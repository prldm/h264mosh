//! Macroblock-level effects on single slices (CAVLC or CABAC), and a few slice-level ones
//! that only rewrite the slice header (any stream).

use crate::intra::{self, MbIntra};
use crate::mb::{self, Blk, CoeffBlock, Macroblock, MbContext, MbType, SliceData};
use crate::motion::{self, MbMotion, Mv};
use crate::pps::Pps;
use crate::residual::Scaling;
use crate::slice::{PredWeightTable, Slice, SliceType, WeightEntry};
use crate::sps::Sps;
use crate::stream::AccessUnit;
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MbEffect {
    /// Multiply motion vectors (x, y).
    MvScale(f32, f32),
    /// Add an offset to motion vectors, in quarter luma samples.
    MvAdd(i32, i32),
    /// Random offset per macroblock of up to +-n quarter samples.
    MvJitter(i32),
    /// Multiply the coded motion vector differences instead of the vectors: the error
    /// propagates through motion vector prediction across the picture.
    MvdScale(f32, f32),
    /// Replace intra macroblocks of P slices by skipped ones, so moshed areas never refresh.
    KillIntra,
    /// Drop all residual data: pictures are built from prediction only.
    ZeroResidual,
    /// Code skipped macroblocks explicitly with identical motion (no visual change).
    ExplicitSkips,
    /// Re-partition 16x16/16x8/8x16 macroblocks as P_8x8 with identical motion (no visual change).
    SplitTo8x8,
    /// Set every intra prediction direction (numbered like Intra4x4PredMode: 0 vertical,
    /// 1 horizontal, 2 DC, 3..=8 diagonal); Intra16x16 and chroma take the nearest of
    /// their modes. The residual stays, so pictures smear along the new direction.
    IntraMode(u8),
    /// Turn intra prediction directions by n steps of 22.5 degrees.
    IntraRotate(i32),
    /// Give this fraction (0..=1) of the intra blocks a random direction.
    IntraRandom(f32),
    /// Code every Intra4x4/8x8 direction as rem_intra_pred_mode n (0..=7): which
    /// direction that is depends on the neighbours, so the change cascades.
    IntraCascade(u8),
    /// Add to the quantizer of every macroblock: its residual is scaled by 2^(n/6).
    QpAdd(i32),
    /// Set the quantizer of every macroblock.
    QpSet(i32),
    /// Multiply transform coefficient levels from a frequency on: (from, factor), with
    /// `from` the position in the scan (0 = all, including DC; 0.5 = the upper half).
    Coeffs(f32, f32),
    /// Add to the DC level of every block of luma, Cb and Cr.
    DcShift(i32, i32, i32),
    /// Predict from reference frames n frames further back (clamped to the ones the
    /// slice can use); needs a stream with several reference frames.
    RefShift(i32),
    /// Copy macroblocks of another input frame (decode order) into the same places:
    /// (frame, fraction 0..=1 of the macroblocks, chosen at random).
    Transplant(usize, f32),
    /// Give P-frame macroblocks the motion of another input frame (decode order) at the
    /// same place, mixed with their own: (frame, 0..=1 how much of the other's).
    MotionTransplant(usize, f32),
    /// Deblocking filter offsets FilterOffsetA/B in steps of 2 (-6..=6): positive smooths more.
    Deblock(i32, i32),
    /// Weighted prediction of P slices: luma gain and offset. Applied to every
    /// prediction, it accumulates from frame to frame.
    Weight(f32, i32),
    /// Weighted prediction offsets of Cb and Cr in P slices: the colours drift.
    Tint(i32, i32),
}

impl MbEffect {
    /// Works on decoded motion (and only in P slices).
    fn needs_motion(&self) -> bool {
        matches!(
            self,
            MbEffect::MvScale(..)
                | MbEffect::MvAdd(..)
                | MbEffect::MvJitter(_)
                | MbEffect::KillIntra
                | MbEffect::ExplicitSkips
                | MbEffect::SplitTo8x8
                | MbEffect::RefShift(_)
                | MbEffect::Transplant(..)
                | MbEffect::MotionTransplant(..)
        )
    }

    /// Does nothing outside P slices.
    fn p_only(&self) -> bool {
        self.needs_motion() && !matches!(self, MbEffect::Transplant(..))
    }

    fn needs_intra(&self) -> bool {
        matches!(self, MbEffect::IntraMode(_) | MbEffect::IntraRotate(_) | MbEffect::IntraRandom(_) | MbEffect::IntraCascade(_))
            || matches!(self, MbEffect::Transplant(..))
    }

    /// Needs the macroblock layer (not available for MBAFF streams); the others only change
    /// slice headers.
    pub fn needs_macroblocks(&self) -> bool {
        !matches!(self, MbEffect::Deblock(..) | MbEffect::Weight(..) | MbEffect::Tint(..))
    }

    /// Depends on the seed.
    pub fn is_random(&self) -> bool {
        match *self {
            MbEffect::MvJitter(_) | MbEffect::IntraRandom(_) => true,
            MbEffect::Transplant(_, amount) => amount < 1.0,
            _ => false,
        }
    }

    /// Changes the PPS of the whole stream (deblocking control, weighted prediction).
    pub fn pps_needs(&self) -> PpsNeeds {
        match self {
            MbEffect::Deblock(..) => PpsNeeds { deblocking_control: true, weighted_pred: false },
            MbEffect::Weight(..) | MbEffect::Tint(..) => PpsNeeds { deblocking_control: false, weighted_pred: true },
            _ => PpsNeeds::default(),
        }
    }
}

/// PPS flags slice-level effects need.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PpsNeeds {
    pub deblocking_control: bool,
    pub weighted_pred: bool,
}

impl PpsNeeds {
    pub fn any(self) -> bool {
        self.deblocking_control || self.weighted_pred
    }

    pub fn union(self, o: Self) -> Self {
        Self { deblocking_control: self.deblocking_control || o.deblocking_control, weighted_pred: self.weighted_pred || o.weighted_pred }
    }

    /// `pps` with the flags switched on, if that changes it.
    pub fn upgrade(self, pps: &Pps) -> Option<Pps> {
        let new = Pps {
            deblocking_filter_control_present_flag: pps.deblocking_filter_control_present_flag || self.deblocking_control,
            weighted_pred_flag: pps.weighted_pred_flag || self.weighted_pred,
            ..pps.clone()
        };
        (new != *pps).then_some(new)
    }
}

/// The part of the picture an effect is limited to: rectangles (x, y, width, height)
/// in fractions of the picture size. A macroblock is inside when its centre is.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Mask {
    pub rects: Vec<[f32; 4]>,
    /// the effect applies outside the rectangles instead
    #[cfg_attr(feature = "serde", serde(default))]
    pub invert: bool,
}

impl Mask {
    pub fn rect(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { rects: vec![[x, y, w, h]], invert: false }
    }

    /// Whether macroblock `addr` of a picture `width` x `height` macroblocks is inside.
    pub fn contains(&self, addr: usize, width: usize, height: usize) -> bool {
        let cx = ((addr % width) as f32 + 0.5) / width as f32;
        let cy = ((addr / width) as f32 + 0.5) / height as f32;
        let hit = self.rects.iter().any(|&[x, y, w, h]| cx >= x && cx < x + w && cy >= y && cy < y + h);
        hit != self.invert
    }
}

/// An effect, possibly limited to part of the picture. Slice-level effects (deblocking,
/// weighted prediction) ignore the mask.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Fx {
    pub effect: MbEffect,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub mask: Option<Mask>,
}

impl From<MbEffect> for Fx {
    fn from(effect: MbEffect) -> Self {
        Self { effect, mask: None }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct EffectStats {
    pub slices: usize,
    /// slices where motion effects could not apply (I or B slices)
    pub slices_without_motion: usize,
    pub intra_killed: usize,
}

impl std::ops::AddAssign for EffectStats {
    fn add_assign(&mut self, o: Self) {
        self.slices += o.slices;
        self.slices_without_motion += o.slices_without_motion;
        self.intra_killed += o.intra_killed;
    }
}

/// xorshift64*: small deterministic generator for jitter.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, n: i32) -> i32 {
        if n <= 0 { 0 } else { (self.next() % (2 * n as u64 + 1)) as i32 - n }
    }

    /// true with probability `p`
    fn chance(&mut self, p: f32) -> bool {
        ((self.next() >> 40) as f32) < p * (1u64 << 24) as f32
    }

    fn below(&mut self, n: u8) -> u8 {
        (self.next() % n as u64) as u8
    }
}

/// Allowed motion vector range in quarter samples (Table A-1 MaxVmvR, horizontal limit).
fn mv_limits(sps: &Sps) -> (i32, i32) {
    let v = match sps.level_idc {
        0..=10 => 256,
        11..=20 => 512,
        21..=30 => 1024,
        _ => 2048,
    };
    (8192, v)
}

fn clamp_mv(mv: Mv, (h, v): (i32, i32)) -> Mv {
    [mv[0].clamp(-h, h - 1), mv[1].clamp(-v, v - 1)]
}

fn scale(v: i32, s: f32) -> i32 {
    (v as f32 * s).round() as i32
}

/// One macroblock of an input frame, ready to be copied into another frame.
#[derive(Clone)]
struct DonorMb {
    mb: Macroblock,
    /// decoded motion, for macroblocks of P slices
    motion: Option<MbMotion>,
    intra: Option<MbIntra>,
}

/// The macroblocks of an input frame, for [`MbEffect::Transplant`] and
/// [`MbEffect::MotionTransplant`].
pub struct Donor {
    sps: Arc<Sps>,
    pps: Arc<Pps>,
    mbs: Vec<Option<DonorMb>>,
}

impl Donor {
    pub fn new(au: &AccessUnit) -> Result<Self> {
        let Some(first) = au.first_slice() else { bail!("the frame has no slices") };
        let (sps, pps) = (first.sps.clone(), first.pps.clone());
        let mut mbs = Vec::new();
        for s in au.slices() {
            let ctx = MbContext::new(s)?;
            if *s.sps != *sps || *s.pps != *pps {
                bail!("the frame's slices use different parameter sets");
            }
            let data = mb::parse_slice_data(s)?;
            let motion = if ctx.slice_type == SliceType::P { Some(motion::decode_motion(&ctx, &data)?) } else { None };
            let intra = intra::decode_intra(&ctx, &data);
            mbs.resize(mbs.len().max(ctx.pic_size), None);
            for (i, mb) in data.mbs.into_iter().enumerate() {
                let inter = !mb.mb_type.is_intra();
                if inter && motion.is_none() {
                    // B macroblocks cannot be copied
                    continue;
                }
                let motion = motion.as_ref().map(|m| m[i]);
                mbs[data.first_mb + i] = Some(DonorMb { mb, motion, intra: intra[i] });
            }
        }
        Ok(Self { sps, pps, mbs })
    }
}

/// The PPS fields macroblocks depend on (not, e.g., the flags slice-level effects switch on).
fn mb_layer(p: &Pps) -> impl PartialEq + use<'_> {
    (p.entropy_coding_mode_flag, p.chroma_qp_index_offset, p.constrained_intra_pred_flag, &p.ext)
}

/// Input frames to copy macroblocks from, by frame index; an error says why a frame
/// cannot be used.
pub type Donors = HashMap<usize, std::result::Result<Donor, String>>;

/// Apply `effects` in order to one slice and replace its slice data. Macroblock
/// effects need the macroblock layer; slice-level effects rewrite the header only.
pub fn apply(slice: &mut Slice, effects: &[Fx], seed: u64, donors: &Donors) -> Result<EffectStats> {
    let mut stats = EffectStats { slices: 1, ..Default::default() };
    if effects.is_empty() {
        return Ok(stats);
    }
    for fx in effects {
        apply_to_header(slice, &fx.effect)?;
    }
    if !effects.iter().any(|f| f.effect.needs_macroblocks()) {
        return Ok(stats);
    }
    let (data, slice_qp) = {
        let ctx = MbContext::new(slice)?;
        let mut data = mb::parse_slice_data(slice)?;
        let mut slice_qp = ctx.slice_qp();
        apply_to_data(&ctx, &mut data, &mut slice_qp, effects, seed, donors, &mut stats)?;
        (data, slice_qp)
    };
    slice.header.slice_qp_delta = slice_qp - 26 - slice.pps.pic_init_qp_minus26;
    let (bytes, bits) = mb::write_slice_data(slice, &data)?;
    slice.set_slice_data(bytes, bits)?;
    Ok(stats)
}

fn apply_to_header(slice: &mut Slice, effect: &MbEffect) -> Result<()> {
    let is_p = matches!(slice.header.slice_type(), SliceType::P | SliceType::SP);
    let chroma = slice.sps.chroma_array_type() != 0;
    let h = &mut slice.header;
    match *effect {
        MbEffect::Deblock(a, b) => {
            if !slice.pps.deblocking_filter_control_present_flag {
                bail!("the PPS does not allow deblocking control");
            }
            if h.disable_deblocking_filter_idc != 1 {
                (h.slice_alpha_c0_offset_div2, h.slice_beta_offset_div2) = (a.clamp(-6, 6), b.clamp(-6, 6));
            }
        }
        MbEffect::Weight(gain, offset) if is_p => {
            let Some(t) = h.pred_weight_table.as_mut() else { bail!("weighted prediction is off in the PPS") };
            // finer weights where the gain needs them
            while t.luma_log2_weight_denom < 5 && t.l0.iter().all(|e| e.luma.is_none_or(|(w, _)| w.abs() <= 63)) {
                t.luma_log2_weight_denom += 1;
                t.l0.iter_mut().filter_map(|e| e.luma.as_mut()).for_each(|(w, _)| *w *= 2);
            }
            let unit = 1 << t.luma_log2_weight_denom;
            for e in t.l0.iter_mut() {
                let (w, o) = e.luma.unwrap_or((unit, 0));
                let new = (scale(w, gain).clamp(-128, 127), (o + offset).clamp(-128, 127));
                e.luma = (e.luma.is_some() || new != (unit, 0)).then_some(new);
            }
        }
        MbEffect::Tint(cb, cr) if is_p && chroma => {
            let Some(t) = h.pred_weight_table.as_mut() else { bail!("weighted prediction is off in the PPS") };
            let unit = 1 << t.chroma_log2_weight_denom;
            for e in t.l0.iter_mut() {
                let [(wb, ob), (wr, or)] = e.chroma.unwrap_or([(unit, 0); 2]);
                let new = [(wb, (ob + cb).clamp(-128, 127)), (wr, (or + cr).clamp(-128, 127))];
                e.chroma = (e.chroma.is_some() || new != [(unit, 0); 2]).then_some(new);
            }
        }
        _ => {}
    }
    Ok(())
}

/// An explicit weighted prediction table with default weights (identical prediction),
/// for P slices whose PPS gets weighted_pred_flag.
pub fn default_weights(slice: &Slice) -> PredWeightTable {
    let n = slice.header.num_ref_idx_l0_active_minus1 as usize + 1;
    PredWeightTable {
        luma_log2_weight_denom: 5,
        chroma_log2_weight_denom: 5,
        l0: vec![WeightEntry { luma: None, chroma: None }; n],
        l1: Vec::new(),
    }
}

/// The macroblock effects, on parsed slice data. `slice_qp` is SliceQPY; QP effects
/// change it.
fn apply_to_data(
    ctx: &MbContext,
    data: &mut SliceData,
    slice_qp: &mut i32,
    effects: &[Fx],
    seed: u64,
    donors: &Donors,
    stats: &mut EffectStats,
) -> Result<()> {
    let is_p = ctx.slice_type == SliceType::P;
    let height = ctx.pic_size / ctx.width;
    let first = data.first_mb;
    let inside = |fx: &Fx, i: usize| fx.mask.as_ref().is_none_or(|m| m.contains(first + i, ctx.width, height));
    let motion = effects.iter().any(|f| f.effect.needs_motion());
    if !is_p && effects.iter().any(|f| f.effect.p_only()) {
        stats.slices_without_motion = 1;
    }
    let mut target: Vec<Option<MbMotion>> =
        if motion && is_p { motion::decode_motion(ctx, data)?.into_iter().map(Some).collect() } else { Vec::new() };
    // killing only some intra macroblocks changes how the others are predicted
    let intra = effects.iter().any(|f| f.effect.needs_intra() || (f.effect == MbEffect::KillIntra && f.mask.is_some()));
    let mut intra_target = if intra { intra::decode_intra(ctx, data) } else { Vec::new() };
    let mut scaling = None;
    // QP_Y of every macroblock (uncoded ones inherit it), for macroblocks outside the masks
    let qps: Vec<i32> = data.mbs.iter().map(|m| m.qp).collect();
    let limits = mv_limits(ctx.sps);
    let mut rng = Rng::new(seed);
    let map = |target: &mut Vec<Option<MbMotion>>, fx: &Fx, f: &dyn Fn(Mv) -> Mv| {
        for (_, t) in target.iter_mut().enumerate().filter(|(i, _)| inside(fx, *i)) {
            if let Some(t) = t.as_mut().filter(|t| !t.is_intra()) {
                t.mv.iter_mut().for_each(|mv| *mv = clamp_mv(f(*mv), limits));
            }
        }
    };
    for (k, fx) in effects.iter().enumerate() {
        // randomized effects other than jitter draw from their own sequence
        let mut own_rng = Rng::new(seed ^ (k as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03));
        match fx.effect {
            MbEffect::ZeroResidual => {
                data.mbs.iter_mut().enumerate().filter(|(i, _)| inside(fx, *i)).for_each(|(_, mb)| mb.clear_residual())
            }
            MbEffect::MvdScale(..) | MbEffect::Deblock(..) | MbEffect::Weight(..) | MbEffect::Tint(..) => {}
            MbEffect::IntraMode(d) => {
                for_intra(data, &mut intra_target, |i| inside(fx, i), |t, nxn| t.map(nxn, |kind, _| kind.from_nxn(d)))
            }
            MbEffect::IntraRotate(n) => {
                for_intra(data, &mut intra_target, |i| inside(fx, i), |t, nxn| t.map(nxn, |kind, m| kind.turn(m, n)))
            }
            MbEffect::IntraRandom(p) => for_intra(
                data,
                &mut intra_target,
                |i| inside(fx, i),
                |t, nxn| t.map(nxn, |kind, m| if own_rng.chance(p) { own_rng.below(kind.modes()) } else { m }),
            ),
            MbEffect::IntraCascade(r) => for_intra(
                data,
                &mut intra_target,
                |i| inside(fx, i),
                |t, nxn| {
                    if nxn {
                        t.luma = [intra::Dir::Rem(r.min(7)); 16];
                    }
                },
            ),
            MbEffect::QpAdd(_) | MbEffect::QpSet(_) => {
                let scaling = scaling.get_or_insert_with(|| Scaling::new(ctx));
                let (lo, hi) = (-ctx.qp_bd_offset(), 51);
                let want = |qp: i32| match fx.effect {
                    MbEffect::QpAdd(d) => (qp + d).clamp(lo, hi),
                    MbEffect::QpSet(q) => q.clamp(lo, hi),
                    _ => qp,
                };
                for (_, mb) in data.mbs.iter_mut().enumerate().filter(|(i, m)| inside(fx, *i) && m.has_residual()) {
                    fit_qp(mb, want(mb.qp), scaling);
                }
                if fx.mask.is_none() {
                    *slice_qp = want(*slice_qp);
                }
            }
            MbEffect::Coeffs(from, factor) => {
                let scaling = scaling.get_or_insert_with(|| Scaling::new(ctx));
                for (_, mb) in data.mbs.iter_mut().enumerate().filter(|(i, m)| inside(fx, *i) && !m.coeffs.is_empty()) {
                    let mut new = mb.clone();
                    for b in new.coeffs.iter_mut() {
                        scale_band(b, mb.mb_type, mb.transform_8x8, from, factor);
                    }
                    fit_levels(ctx, mb, new, scaling);
                }
            }
            MbEffect::DcShift(y, cb, cr) => {
                let scaling = scaling.get_or_insert_with(|| Scaling::new(ctx));
                for (_, mb) in data.mbs.iter_mut().enumerate().filter(|(i, m)| inside(fx, *i) && m.mb_type != MbType::Skip) {
                    if let Some(new) = dc_shifted(mb, [y, cb, cr], ctx.chroma_array_type) {
                        fit_levels(ctx, mb, new, scaling);
                    }
                }
            }
            _ if !is_p && fx.effect.p_only() => {}
            MbEffect::KillIntra => {
                for (i, (mb, t)) in data.mbs.iter_mut().zip(target.iter_mut()).enumerate() {
                    if mb.mb_type.is_intra() && inside(fx, i) {
                        *mb = Macroblock::new(MbType::Skip, mb.qp);
                        *t = None;
                        if intra {
                            intra_target[i] = None;
                        }
                        stats.intra_killed += 1;
                    }
                }
            }
            MbEffect::ExplicitSkips => {
                for (_, mb) in data.mbs.iter_mut().enumerate().filter(|(i, m)| m.mb_type == MbType::Skip && inside(fx, *i)) {
                    *mb = Macroblock::new(MbType::P(0), mb.qp);
                }
            }
            MbEffect::SplitTo8x8 => data.mbs.iter_mut().enumerate().filter(|(i, _)| inside(fx, *i)).for_each(|(_, mb)| split_to_8x8(mb)),
            MbEffect::MvScale(sx, sy) => map(&mut target, fx, &|mv| [scale(mv[0], sx), scale(mv[1], sy)]),
            MbEffect::MvAdd(dx, dy) => map(&mut target, fx, &|mv| [mv[0].saturating_add(dx), mv[1].saturating_add(dy)]),
            MbEffect::MvJitter(n) => {
                // one offset per macroblock keeps its partitions uniform
                for (i, t) in target.iter_mut().enumerate() {
                    let Some(t) = t.as_mut().filter(|t| !t.is_intra()) else { continue };
                    let d = [rng.range(n), rng.range(n)];
                    if inside(fx, i) {
                        t.mv.iter_mut().for_each(|mv| *mv = clamp_mv([mv[0].saturating_add(d[0]), mv[1].saturating_add(d[1])], limits));
                    }
                }
            }
            MbEffect::RefShift(n) => {
                let max = ctx.header.num_ref_idx_l0_active_minus1 as i32;
                let shift = |r: u8| (r as i32 + n).clamp(0, max) as u8;
                for (_, mb) in data.mbs.iter_mut().enumerate().filter(|(i, _)| inside(fx, *i)) {
                    match mb.mb_type {
                        MbType::Skip if shift(0) != 0 => {
                            *mb = Macroblock::new(MbType::P(0), mb.qp);
                            mb.ref_idx[0][0] = shift(0);
                        }
                        MbType::P(4) if shift(0) != 0 => {
                            mb.mb_type = MbType::P(3);
                            mb.ref_idx[0] = [shift(0); 4];
                        }
                        MbType::P(0..=3) => {
                            let parts = mb.mb_type.num_parts();
                            mb.ref_idx[0][..parts].iter_mut().for_each(|r| *r = shift(*r));
                        }
                        _ => {}
                    }
                }
            }
            MbEffect::MotionTransplant(from, mix) => {
                let donor = donor(donors, from, ctx)?;
                let blend = |own: i32, other: i32| own + ((other - own) as f32 * mix.clamp(0.0, 1.0)).round() as i32;
                for (i, t) in target.iter_mut().enumerate().filter(|(i, _)| inside(fx, *i)) {
                    let own = t.as_mut().filter(|t| !t.is_intra());
                    let other = donor.mbs.get(first + i).and_then(|d| d.as_ref()?.motion).filter(|m| !m.is_intra());
                    if let (Some(t), Some(m)) = (own, other) {
                        for (mv, d) in t.mv.iter_mut().zip(m.mv) {
                            *mv = clamp_mv([blend(mv[0], d[0]), blend(mv[1], d[1])], limits);
                        }
                    }
                }
            }
            MbEffect::Transplant(from, amount) => {
                if ctx.slice_type == SliceType::B {
                    continue;
                }
                let donor = donor(donors, from, ctx)?;
                let max_ref = ctx.header.num_ref_idx_l0_active_minus1 as u8;
                for i in 0..data.mbs.len() {
                    // draw for every macroblock, so the choice does not depend on the mask
                    if !own_rng.chance(amount) || !inside(fx, i) {
                        continue;
                    }
                    let Some(dm) = donor.mbs.get(first + i).and_then(Option::as_ref) else { continue };
                    if !dm.mb.mb_type.is_intra() && !is_p {
                        continue;
                    }
                    let mut mb = dm.mb.clone();
                    mb.ref_idx[0].iter_mut().for_each(|r| *r = (*r).min(max_ref));
                    data.mbs[i] = mb;
                    if is_p {
                        target[i] = Some(dm.motion.unwrap_or(MbMotion::INTRA));
                    }
                    intra_target[i] = dm.intra;
                }
            }
        }
    }
    if motion && is_p {
        motion::encode_motion(ctx, data, &target)?;
    }
    for fx in effects {
        if let MbEffect::MvdScale(sx, sy) = fx.effect {
            for (_, mb) in data.mbs.iter_mut().enumerate().filter(|(i, m)| m.mb_type.is_inter() && inside(fx, *i)) {
                for d in mb.mvd.iter_mut().flatten().flatten() {
                    // saturate to the coded range of mvd
                    let sat = |v: i16, s: f32| scale(v as i32, s).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    *d = [sat(d[0], sx), sat(d[1], sy)];
                }
            }
        }
    }
    if intra {
        intra::encode_intra(ctx, data, &intra_target)?;
    }
    let untouched: Vec<bool> = (0..data.mbs.len()).map(|i| !effects.iter().any(|f| f.effect.needs_macroblocks() && inside(f, i))).collect();
    if untouched.iter().any(|&u| u) {
        keep_quantizers(ctx, data, *slice_qp, &qps, &untouched)?;
    }
    Ok(())
}

/// Uncoded macroblocks take the QP of the macroblock before them. Where effects inside
/// a mask changed that, an untouched macroblock outside gets its QP back by carrying
/// mb_qp_delta with an all-zero residual, which changes neither its picture nor its
/// deblocking (an I_8x8 macroblock carries it with an empty chroma DC, see
/// [`mb::without_empty_8x8`]).
fn keep_quantizers(ctx: &MbContext, data: &mut SliceData, slice_qp: i32, qps: &[i32], untouched: &[bool]) -> Result<()> {
    let mut pred = slice_qp;
    let mut skips = Vec::new();
    for (i, mb) in data.mbs.iter_mut().enumerate() {
        if mb.mb_type == MbType::IPcm {
            continue;
        }
        if mb.has_residual() {
            pred = mb.qp;
            continue;
        }
        if !untouched[i] || pred == qps[i] || !matches!(mb.mb_type, MbType::INxN | MbType::P(_) | MbType::Skip) {
            mb.qp = pred;
            continue;
        }
        if mb.mb_type == MbType::Skip {
            if ctx.slice_type != SliceType::P {
                continue;
            }
            skips.push(i);
        }
        mb.cbp = 1;
        (mb.cbp, mb.transform_8x8) = mb::without_empty_8x8(ctx, mb);
        mb.qp = qps[i];
        pred = qps[i];
    }
    if !skips.is_empty() {
        // coded skips: P_L0_16x16 with the motion the skip had
        let motion = motion::decode_motion(ctx, data)?;
        for &i in &skips {
            let qp = data.mbs[i].qp;
            data.mbs[i] = Macroblock::new(MbType::P(0), qp);
            data.mbs[i].cbp = 1;
        }
        motion::encode_motion(ctx, data, &motion.into_iter().map(Some).collect::<Vec<_>>())?;
    }
    Ok(())
}

/// The input frame `from`, if its macroblocks fit into slices like `ctx`'s.
fn donor<'a>(donors: &'a Donors, from: usize, ctx: &MbContext) -> Result<&'a Donor> {
    let donor = match donors.get(&from) {
        Some(Ok(d)) => d,
        Some(Err(reason)) => bail!("cannot copy from frame {from}: {reason}"),
        None => bail!("frame {from} is not available to copy from"),
    };
    if *donor.sps != *ctx.sps || mb_layer(&donor.pps) != mb_layer(ctx.pps) {
        bail!("frame {from} is coded with other parameter sets");
    }
    Ok(donor)
}

/// Change the directions of the intra macroblocks `pick` selects.
fn for_intra(data: &SliceData, targets: &mut [Option<MbIntra>], pick: impl Fn(usize) -> bool, mut f: impl FnMut(&mut MbIntra, bool)) {
    for (i, t) in targets.iter_mut().enumerate() {
        if let Some(t) = t.as_mut().filter(|_| pick(i)) {
            f(t, data.mbs[i].mb_type == MbType::INxN);
        }
    }
}

/// Set the QP of a macroblock with residual as close to `want` as the value ranges
/// of its levels allow.
fn fit_qp(mb: &mut Macroblock, want: i32, scaling: &Scaling) {
    let mut qp = want;
    while qp != mb.qp && !scaling.fits(mb, qp) {
        qp += (mb.qp - qp).signum();
    }
    mb.qp = qp;
}

/// Replace the levels of `mb` by those of `new` (same macroblock, more coded blocks
/// allowed), or, if they do not fit the value ranges, by the largest step from the old
/// levels towards them that does. 8x8 blocks left empty are coded otherwise
/// ([`mb::without_empty_8x8`]: the same with CAVLC and CABAC, deblocked alike by all
/// decoders).
fn fit_levels(ctx: &MbContext, mb: &mut Macroblock, new: Macroblock, scaling: &Scaling) {
    let blend = |t: f32| {
        let mut m = new.clone();
        for b in m.coeffs.iter_mut() {
            let old = mb.block(b.blk).map_or([0; 16], |o| o.levels);
            for (l, o) in b.levels.iter_mut().zip(old) {
                *l = o + ((*l - o) as f32 * t).round() as i32;
            }
        }
        m.coeffs.retain(|b| b.levels.iter().any(|&l| l != 0));
        m
    };
    let mut best = blend(1.0);
    if !scaling.fits(&best, new.qp) {
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..8 {
            let mid = (lo + hi) / 2.0;
            if scaling.fits(&blend(mid), new.qp) { lo = mid } else { hi = mid }
        }
        if lo == 0.0 {
            return;
        }
        best = blend(lo);
    }
    *mb = best;
    (mb.cbp, mb.transform_8x8) = mb::without_empty_8x8(ctx, mb);
}

/// Multiply the levels of a block from scan position `from` (a fraction of the scan
/// length) on. DC blocks count as frequency 0; Intra16x16 and chroma AC blocks start
/// at the second position; 8x8 blocks are held as four interleaved 4x4 blocks
/// (level i of the k-th one is coefficient 4i + k of the 8x8 scan).
fn scale_band(b: &mut CoeffBlock, mb_type: MbType, transform_8x8: bool, from: f32, factor: f32) {
    let pos = |i: usize| -> f32 {
        match b.blk {
            Blk::LumaDc(_) | Blk::ChromaDc(_) => 0.0,
            Blk::ChromaAc(..) => (i + 1) as f32 / 16.0,
            Blk::Luma(_, blk) if transform_8x8 => (4 * i + blk as usize % 4) as f32 / 64.0,
            Blk::Luma(..) if matches!(mb_type, MbType::I16x16(_)) => (i + 1) as f32 / 16.0,
            Blk::Luma(..) => i as f32 / 16.0,
        }
    };
    for (i, l) in b.levels.iter_mut().enumerate() {
        if *l != 0 && pos(i) >= from {
            *l = scale(*l, factor);
        }
    }
}

/// `mb` with `shift` (Y, Cb, Cr) added to the DC level of every block, the coded
/// block pattern extended where needed; None if nothing changes.
fn dc_shifted(mb: &Macroblock, shift: [i32; 3], chroma_array_type: u32) -> Option<Macroblock> {
    if mb.mb_type == MbType::IPcm || shift == [0; 3] {
        return None;
    }
    let mut m = mb.clone();
    let add = |m: &mut Macroblock, blk: Blk, v: i32| {
        if v == 0 {
            return;
        }
        match m.coeffs.iter_mut().find(|b| b.blk == blk) {
            Some(b) => b.levels[0] += v,
            None => {
                let mut levels = [0; 16];
                levels[0] = v;
                m.coeffs.push(CoeffBlock { blk, levels });
            }
        }
    };
    let planes = if chroma_array_type == 3 { 3 } else { 1 };
    let i16 = matches!(m.mb_type, MbType::I16x16(_));
    for (p, &s) in shift.iter().enumerate().take(planes) {
        if i16 {
            add(&mut m, Blk::LumaDc(p as u8), s);
        } else if s != 0 {
            let step = if m.transform_8x8 { 4 } else { 1 };
            for blk in (0..16).step_by(step) {
                add(&mut m, Blk::Luma(p as u8, blk as u8), s);
            }
            m.cbp |= 15;
        }
    }
    if matches!(chroma_array_type, 1 | 2) {
        for c in 0..2 {
            if shift[1 + c] != 0 {
                add(&mut m, Blk::ChromaDc(c as u8), shift[1 + c]);
                m.cbp = (m.cbp & 15) | (m.cbp >> 4).max(1) << 4;
            }
        }
    }
    // keep the blocks in coding order, like the parser
    m.coeffs.sort_by_key(|b| block_order(b.blk));
    Some(m)
}

fn block_order(blk: Blk) -> (u8, u8, u8) {
    match blk {
        Blk::LumaDc(p) => (p, 0, 0),
        Blk::Luma(p, b) => (p, 1, b),
        Blk::ChromaDc(c) => (3, c, 0),
        Blk::ChromaAc(c, b) => (4, c, b),
    }
}

/// P_L0_16x16/16x8/8x16 -> P_8x8 with four 8x8 sub-macroblocks; motion is re-coded later.
fn split_to_8x8(mb: &mut Macroblock) {
    let refs = mb.ref_idx[0];
    let per_8x8: [u8; 4] = match mb.mb_type {
        MbType::P(0) => [refs[0]; 4],
        MbType::P(1) => [refs[0], refs[0], refs[1], refs[1]],
        MbType::P(2) => [refs[0], refs[1], refs[0], refs[1]],
        _ => return,
    };
    mb.mb_type = MbType::P(3);
    mb.sub_types = [0; 4];
    mb.ref_idx[0] = per_8x8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_test_macroblock_centres() {
        let m = Mask::rect(0.0, 0.0, 0.5, 0.5);
        // 4x2 macroblocks: the top-left two
        let inside: Vec<usize> = (0..8).filter(|&a| m.contains(a, 4, 2)).collect();
        assert_eq!(inside, [0, 1]);
        let inv = Mask { invert: true, ..m };
        assert_eq!((0..8).filter(|&a| inv.contains(a, 4, 2)).count(), 6);
        assert!(!Mask::default().contains(0, 4, 2));
    }

    #[test]
    fn chance_follows_the_probability() {
        let mut r = Rng::new(5);
        let hits = (0..10000).filter(|_| r.chance(0.25)).count();
        assert!((2200..2800).contains(&hits), "{hits}");
        assert!((0..100).all(|_| r.chance(1.0)));
        assert!((0..100).all(|_| !r.chance(0.0)));
    }

    #[test]
    fn dc_shift_extends_the_coded_block_pattern() {
        let mut mb = Macroblock::new(MbType::P(0), 26);
        mb.coeffs.push(CoeffBlock { blk: Blk::ChromaAc(1, 2), levels: [1; 16] });
        let m = dc_shifted(&mb, [2, 0, -1], 1).unwrap();
        assert_eq!(m.cbp, 15 | (1 << 4));
        assert_eq!(m.block(Blk::Luma(0, 5)).unwrap().levels[0], 2);
        assert_eq!(m.block(Blk::ChromaDc(1)).unwrap().levels[0], -1);
        assert!(m.block(Blk::ChromaDc(0)).is_none());
        let order: Vec<_> = m.coeffs.iter().map(|b| block_order(b.blk)).collect();
        assert!(order.is_sorted());
        assert!(dc_shifted(&mb, [0; 3], 1).is_none());
    }
}
