//! Macroblock-level effects on single slices (CAVLC only).

use crate::mb::{self, Macroblock, MbContext, MbType};
use crate::motion::{self, MbMotion, Mv};
use crate::slice::{Slice, SliceType};
use crate::sps::Sps;
use anyhow::Result;

#[derive(Clone, Debug, PartialEq)]
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
}

impl MbEffect {
    fn needs_motion(&self) -> bool {
        !matches!(self, MbEffect::MvdScale(..) | MbEffect::ZeroResidual)
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

/// Apply `effects` in order to one slice and replace its slice data.
pub fn apply(slice: &mut Slice, effects: &[MbEffect], seed: u64) -> Result<EffectStats> {
    let mut stats = EffectStats { slices: 1, ..Default::default() };
    if effects.is_empty() {
        return Ok(stats);
    }
    let (bytes, bits) = {
        let ctx = MbContext::new(slice)?;
        let mut data = mb::parse_slice_data(slice)?;
        let is_p = ctx.slice_type == SliceType::P;
        let motion = effects.iter().any(MbEffect::needs_motion);
        if motion && !is_p {
            stats.slices_without_motion = 1;
        }
        let mut target: Vec<Option<MbMotion>> =
            if motion && is_p { motion::decode_motion(&ctx, &data)?.into_iter().map(Some).collect() } else { Vec::new() };
        let limits = mv_limits(ctx.sps);
        let mut rng = Rng::new(seed);
        let map = |target: &mut Vec<Option<MbMotion>>, f: &dyn Fn(Mv) -> Mv| {
            for t in target.iter_mut().flatten().filter(|t| !t.is_intra()) {
                t.mv.iter_mut().for_each(|mv| *mv = clamp_mv(f(*mv), limits));
            }
        };
        for fx in effects {
            match *fx {
                MbEffect::ZeroResidual => data.mbs.iter_mut().for_each(Macroblock::clear_residual),
                MbEffect::MvdScale(..) => {}
                _ if !is_p => {}
                MbEffect::KillIntra => {
                    for (mb, t) in data.mbs.iter_mut().zip(target.iter_mut()) {
                        if mb.mb_type.is_intra() {
                            *mb = Macroblock::new(MbType::Skip, mb.qp);
                            *t = None;
                            stats.intra_killed += 1;
                        }
                    }
                }
                MbEffect::ExplicitSkips => {
                    for mb in data.mbs.iter_mut().filter(|m| m.mb_type == MbType::Skip) {
                        *mb = Macroblock::new(MbType::P(0), mb.qp);
                    }
                }
                MbEffect::SplitTo8x8 => data.mbs.iter_mut().for_each(split_to_8x8),
                MbEffect::MvScale(sx, sy) => map(&mut target, &|mv| [scale(mv[0], sx), scale(mv[1], sy)]),
                MbEffect::MvAdd(dx, dy) => map(&mut target, &|mv| [mv[0].saturating_add(dx), mv[1].saturating_add(dy)]),
                MbEffect::MvJitter(n) => {
                    // one offset per macroblock keeps its partitions uniform
                    for t in target.iter_mut().flatten().filter(|t| !t.is_intra()) {
                        let d = [rng.range(n), rng.range(n)];
                        t.mv.iter_mut().for_each(|mv| *mv = clamp_mv([mv[0].saturating_add(d[0]), mv[1].saturating_add(d[1])], limits));
                    }
                }
            }
        }
        if motion && is_p {
            motion::encode_motion(&ctx, &mut data, &target)?;
        }
        for fx in effects {
            if let MbEffect::MvdScale(sx, sy) = *fx {
                for mb in data.mbs.iter_mut().filter(|m| m.mb_type.is_inter()) {
                    for d in mb.mvd.iter_mut().flatten().flatten() {
                        // saturate to the coded range of mvd
                        let sat = |v: i16, s: f32| scale(v as i32, s).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                        *d = [sat(d[0], sx), sat(d[1], sy)];
                    }
                }
            }
        }
        mb::write_slice_data(slice, &data)?
    };
    slice.set_slice_data(bytes, bits)?;
    Ok(stats)
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
