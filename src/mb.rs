//! Macroblock layer of CAVLC slices (7.3.4 slice_data, 7.3.5 macroblock_layer):
//! parsing into an editable model and writing it back.
//!
//! Everything that depends on neighbouring macroblocks (nC for coeff_token, QP
//! prediction) is recomputed on write from the model, so edited slices stay
//! consistent. Not supported: CABAC, MBAFF, slice groups (FMO), SP/SI slices.

use crate::bits::{BitReader, BitWriter};
use crate::cavlc::{read_block, write_block};
use crate::cavlc_tables::{CBP_INTER, CBP_INTER_GRAY, CBP_INTRA, CBP_INTRA_GRAY};
use crate::pps::Pps;
use crate::slice::{Slice, SliceHeader, SliceType};
use crate::sps::Sps;
use anyhow::{Result, bail};

/// (x, y) of luma4x4BlkIdx in 4x4-block units (6.4.3)
pub const LUMA4X4_XY: [(usize, usize); 16] =
    [(0, 0), (1, 0), (0, 1), (1, 1), (2, 0), (3, 0), (2, 1), (3, 1), (0, 2), (1, 2), (0, 3), (1, 3), (2, 2), (3, 2), (2, 3), (3, 3)];

fn raster(blk: usize) -> usize {
    let (x, y) = LUMA4X4_XY[blk];
    y * 4 + x
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MbType {
    /// P_Skip / B_Skip
    Skip,
    /// I_4x4, or I_8x8 when transform_8x8 is set
    INxN,
    /// I_16x16 with Intra16x16PredMode; its coded block pattern lives in `Macroblock::cbp`
    I16x16(u8),
    IPcm,
    /// P macroblock type 0..=4 (Table 7-13)
    P(u8),
    /// B macroblock type 0..=22 (Table 7-14)
    B(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pred {
    L0,
    L1,
    Bi,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    S16x16,
    S16x8,
    S8x16,
    S8x8,
}

const B_PART_PREDS: [(Pred, Pred); 9] = [
    (Pred::L0, Pred::L0),
    (Pred::L1, Pred::L1),
    (Pred::L0, Pred::L1),
    (Pred::L1, Pred::L0),
    (Pred::L0, Pred::Bi),
    (Pred::L1, Pred::Bi),
    (Pred::Bi, Pred::L0),
    (Pred::Bi, Pred::L1),
    (Pred::Bi, Pred::Bi),
];

impl MbType {
    pub fn is_intra(self) -> bool {
        matches!(self, MbType::INxN | MbType::I16x16(_) | MbType::IPcm)
    }

    pub fn is_inter(self) -> bool {
        matches!(self, MbType::P(_) | MbType::B(_))
    }

    /// NumMbPart; 0 for B_Direct_16x16
    pub fn num_parts(self) -> usize {
        match self {
            MbType::P(0) | MbType::B(1..=3) => 1,
            MbType::P(1 | 2) | MbType::B(4..=21) => 2,
            MbType::P(3 | 4) | MbType::B(22) => 4,
            _ => 0,
        }
    }

    pub fn shape(self) -> Shape {
        match self {
            MbType::P(1) => Shape::S16x8,
            MbType::P(2) => Shape::S8x16,
            MbType::B(t @ 4..=21) if t % 2 == 0 => Shape::S16x8,
            MbType::B(4..=21) => Shape::S8x16,
            MbType::P(3 | 4) | MbType::B(22) => Shape::S8x8,
            _ => Shape::S16x16,
        }
    }

    /// MbPartPredMode(mb_type, part) for inter types with 1 or 2 partitions
    pub fn part_pred(self, part: usize) -> Pred {
        match self {
            MbType::B(0) => Pred::Direct,
            MbType::B(1) => Pred::L0,
            MbType::B(2) => Pred::L1,
            MbType::B(3) => Pred::Bi,
            MbType::B(t @ 4..=21) => {
                let (a, b) = B_PART_PREDS[(t as usize - 4) / 2];
                if part == 0 { a } else { b }
            }
            _ => Pred::L0,
        }
    }
}

/// (NumSubMbPart, SubMbPredMode, width, height) of a sub_mb_type
pub fn sub_mb_info(slice_type: SliceType, sub: u8) -> (usize, Pred, usize, usize) {
    if slice_type == SliceType::B {
        const T: [(usize, Pred, usize, usize); 13] = [
            (4, Pred::Direct, 4, 4),
            (1, Pred::L0, 8, 8),
            (1, Pred::L1, 8, 8),
            (1, Pred::Bi, 8, 8),
            (2, Pred::L0, 8, 4),
            (2, Pred::L0, 4, 8),
            (2, Pred::L1, 8, 4),
            (2, Pred::L1, 4, 8),
            (2, Pred::Bi, 8, 4),
            (2, Pred::Bi, 4, 8),
            (4, Pred::L0, 4, 4),
            (4, Pred::L1, 4, 4),
            (4, Pred::Bi, 4, 4),
        ];
        T[sub as usize]
    } else {
        const T: [(usize, Pred, usize, usize); 4] = [(1, Pred::L0, 8, 8), (2, Pred::L0, 8, 4), (2, Pred::L0, 4, 8), (4, Pred::L0, 4, 4)];
        T[sub as usize]
    }
}

/// One residual block of a macroblock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blk {
    /// Intra16x16 DC of a plane (0 = Y; 1, 2 = Cb, Cr in 4:4:4)
    LumaDc(u8),
    /// plane, luma4x4BlkIdx (Intra16x16: the AC part)
    Luma(u8, u8),
    /// chroma DC of Cb (0) or Cr (1)
    ChromaDc(u8),
    /// component, chroma4x4BlkIdx
    ChromaAc(u8, u8),
}

/// Transform coefficient levels of one block, in coding (zig-zag) order. Blocks
/// with fewer coefficients (Intra16x16/chroma AC: 15, chroma DC: 4 or 8) leave the
/// tail zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoeffBlock {
    pub blk: Blk,
    pub levels: [i32; 16],
}

#[derive(Clone, Debug)]
pub struct Macroblock {
    pub mb_type: MbType,
    pub transform_8x8: bool,
    /// Intra NxN prediction per block: -1 = prev_intra_pred_mode_flag, 0..=7 = rem_intra_pred_mode
    /// (16 entries for 4x4, the first 4 for 8x8)
    pub intra_modes: [i8; 16],
    pub intra_chroma_pred_mode: u8,
    pub sub_types: [u8; 4],
    /// [list][partition]
    pub ref_idx: [[u8; 4]; 2],
    /// [list][partition][sub-partition][x, y], quarter luma samples
    pub mvd: [[[Mvd; 4]; 4]; 2],
    /// bits 0..3: luma 8x8 blocks, bits 4..5: CodedBlockPatternChroma
    pub cbp: u8,
    /// QP_Y of this macroblock. Only used for macroblocks that carry mb_qp_delta;
    /// the others inherit the predicted QP.
    pub qp: i32,
    /// Residual blocks with at least one non-zero level, in coding order; blocks that
    /// are not listed are all zero. Only blocks covered by `cbp` (or the Intra16x16 DC)
    /// are written.
    pub coeffs: Vec<CoeffBlock>,
    /// I_PCM samples, luma then chroma
    pub pcm: Option<Vec<u16>>,
}

impl Macroblock {
    pub fn new(mb_type: MbType, qp: i32) -> Self {
        Self {
            mb_type,
            transform_8x8: false,
            intra_modes: [0; 16],
            intra_chroma_pred_mode: 0,
            sub_types: [0; 4],
            ref_idx: [[0; 4]; 2],
            mvd: [[[[0; 2]; 4]; 4]; 2],
            cbp: 0,
            qp,
            coeffs: Vec::new(),
            pcm: None,
        }
    }

    /// mb_qp_delta (and residual) present
    pub fn has_residual(&self) -> bool {
        self.cbp != 0 || matches!(self.mb_type, MbType::I16x16(_))
    }

    /// Levels of a residual block (all zero if not stored).
    pub fn block(&self, blk: Blk) -> Option<&CoeffBlock> {
        self.coeffs.iter().find(|b| b.blk == blk)
    }

    /// Remove all residual data (prediction only).
    pub fn clear_residual(&mut self) {
        match self.mb_type {
            MbType::IPcm | MbType::Skip => return,
            MbType::INxN => {}
            _ => self.transform_8x8 = false,
        }
        self.cbp = 0;
        self.coeffs = Vec::new();
    }
}

/// The macroblocks of one slice, in decoding order starting at first_mb_in_slice.
#[derive(Clone, Debug)]
pub struct SliceData {
    pub first_mb: usize,
    pub mbs: Vec<Macroblock>,
}

/// Slice-level parameters needed by the macroblock layer.
pub struct MbContext<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub header: &'a SliceHeader,
    pub slice_type: SliceType,
    pub width: usize,
    pub pic_size: usize,
    pub chroma_array_type: u32,
}

impl<'a> MbContext<'a> {
    pub fn new(slice: &'a Slice) -> Result<Self> {
        let (sps, pps, header) = (slice.sps.as_ref(), slice.pps.as_ref(), &slice.header);
        if pps.entropy_coding_mode_flag {
            bail!("macroblock editing needs CAVLC streams (this one is CABAC; use `prep --cavlc`)");
        }
        if sps.mb_adaptive_frame_field_flag && !header.field_pic_flag {
            bail!("MBAFF streams are not supported");
        }
        if pps.num_slice_groups_minus1 > 0 {
            bail!("slice groups (FMO) are not supported");
        }
        let slice_type = header.slice_type();
        if matches!(slice_type, SliceType::SP | SliceType::SI) {
            bail!("SP/SI slices are not supported");
        }
        let width = sps.pic_width_in_mbs() as usize;
        let height = sps.frame_height_in_mbs() as usize / (1 + header.field_pic_flag as usize);
        Ok(Self { sps, pps, header, slice_type, width, pic_size: width * height, chroma_array_type: sps.chroma_array_type() })
    }

    fn qp_bd_offset(&self) -> i32 {
        6 * self.sps.bit_depth_luma_minus8 as i32
    }

    fn slice_qp(&self) -> i32 {
        26 + self.pps.pic_init_qp_minus26 + self.header.slice_qp_delta
    }

    fn luma_planes(&self) -> usize {
        if self.chroma_array_type == 3 { 3 } else { 1 }
    }

    /// chroma 4x4 blocks per component (4:2:0: 4, 4:2:2: 8)
    fn chroma_blocks(&self) -> usize {
        if self.chroma_array_type == 2 { 8 } else { 4 }
    }

    fn cbp_tables(&self) -> (&'static [u8], &'static [u8]) {
        if matches!(self.chroma_array_type, 1 | 2) { (&CBP_INTRA, &CBP_INTER) } else { (&CBP_INTRA_GRAY, &CBP_INTER_GRAY) }
    }

    fn num_ref_minus1(&self, list: usize) -> u32 {
        if list == 0 { self.header.num_ref_idx_l0_active_minus1 } else { self.header.num_ref_idx_l1_active_minus1 }
    }

    fn intra_offset(&self) -> u32 {
        match self.slice_type {
            SliceType::P => 5,
            SliceType::B => 23,
            _ => 0,
        }
    }
}

/// Per-macroblock values of the current and the previous macroblock row, which is
/// all that neighbour derivation (left, above, above-left, above-right) looks at.
/// A slice starts with an empty cache, so only macroblocks of the same slice count
/// as available; entries remember their address so that a slot reused by a later
/// row is never mistaken for a neighbour.
pub(crate) struct RowCache<T> {
    width: usize,
    slots: Vec<Option<(usize, T)>>,
}

impl<T> RowCache<T> {
    pub(crate) fn new(width: usize) -> Self {
        Self { width, slots: (0..2 * width).map(|_| None).collect() }
    }

    pub(crate) fn get(&self, addr: usize) -> Option<&T> {
        match &self.slots[addr % (2 * self.width)] {
            Some((a, v)) if *a == addr => Some(v),
            _ => None,
        }
    }

    pub(crate) fn set(&mut self, addr: usize, value: T) {
        self.slots[addr % (2 * self.width)] = Some((addr, value));
    }
}

/// TotalCoeff per 4x4 block of a decoded macroblock (for nC, 9.2.1).
#[derive(Clone, Copy, Default)]
struct TotalCoeffs {
    /// [plane][raster 4x4 index]
    luma: [[u8; 16]; 3],
    /// [Cb/Cr][chroma4x4BlkIdx]
    chroma: [[u8; 8]; 2],
}

impl TotalCoeffs {
    fn filled(n: u8) -> Self {
        Self { luma: [[n; 16]; 3], chroma: [[n; 8]; 2] }
    }
}

struct Neighbours {
    tcs: RowCache<TotalCoeffs>,
    width: usize,
}

impl Neighbours {
    fn new(width: usize) -> Self {
        Self { tcs: RowCache::new(width), width }
    }

    fn left(&self, addr: usize) -> Option<&TotalCoeffs> {
        if addr.is_multiple_of(self.width) { None } else { self.tcs.get(addr - 1) }
    }

    fn top(&self, addr: usize) -> Option<&TotalCoeffs> {
        if addr < self.width { None } else { self.tcs.get(addr - self.width) }
    }

    fn combine(a: Option<u8>, b: Option<u8>) -> i32 {
        match (a, b) {
            (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
            (Some(n), None) | (None, Some(n)) => n as i32,
            (None, None) => 0,
        }
    }

    fn nc_luma(&self, cur: &TotalCoeffs, addr: usize, plane: usize, blk: usize) -> i32 {
        let (x, y) = LUMA4X4_XY[blk];
        let a = if x > 0 { Some(cur.luma[plane][y * 4 + x - 1]) } else { self.left(addr).map(|t| t.luma[plane][y * 4 + 3]) };
        let b = if y > 0 { Some(cur.luma[plane][(y - 1) * 4 + x]) } else { self.top(addr).map(|t| t.luma[plane][12 + x]) };
        Self::combine(a, b)
    }

    fn nc_chroma(&self, cur: &TotalCoeffs, addr: usize, c: usize, blk: usize, rows: usize) -> i32 {
        let (x, y) = (blk % 2, blk / 2);
        let a = if x > 0 { Some(cur.chroma[c][y * 2]) } else { self.left(addr).map(|t| t.chroma[c][y * 2 + 1]) };
        let b = if y > 0 { Some(cur.chroma[c][(y - 1) * 2 + x]) } else { self.top(addr).map(|t| t.chroma[c][(rows - 1) * 2 + x]) };
        Self::combine(a, b)
    }
}

fn wrap_qp(qp: i32, off: i32) -> i32 {
    (qp + 52 + 2 * off).rem_euclid(52 + off) - off
}

fn read_te(r: &mut BitReader, cmax: u32) -> Result<u8> {
    let v = if cmax == 1 { !r.flag()? as u32 } else { r.ue()? };
    if v > cmax {
        bail!("ref_idx {v} exceeds num_ref_idx_active_minus1 {cmax}");
    }
    Ok(v as u8)
}

fn write_te(w: &mut BitWriter, cmax: u32, v: u8) {
    if cmax == 1 { w.flag(v == 0) } else { w.ue(v as u32) }
}

fn decode_intra_type(t: u32) -> Result<(MbType, u8)> {
    Ok(match t {
        0 => (MbType::INxN, 0),
        1..=24 => {
            let k = t - 1;
            let cbp = (if t >= 13 { 15 } else { 0 }) | ((((k / 4) % 3) as u8) << 4);
            (MbType::I16x16((k % 4) as u8), cbp)
        }
        25 => (MbType::IPcm, 0),
        _ => bail!("intra mb_type {t} out of range"),
    })
}

/// Parse slice_data() of a CAVLC slice.
pub fn parse_slice_data(slice: &Slice) -> Result<SliceData> {
    let ctx = MbContext::new(slice)?;
    let (buf, start, end) = slice.slice_data();
    let mut r = BitReader::at(buf, start);
    let first_mb = ctx.header.first_mb_in_slice as usize;
    let mut nb = Neighbours::new(ctx.width);
    let mut qp = ctx.slice_qp();
    let mut mbs = Vec::with_capacity(ctx.pic_size.saturating_sub(first_mb));
    let mut addr = first_mb;
    let mut more = true;
    while more {
        if ctx.slice_type != SliceType::I {
            let run = r.ue()? as usize;
            if addr + run > ctx.pic_size {
                bail!("mb_skip_run runs past the end of the picture");
            }
            for _ in 0..run {
                mbs.push(Macroblock::new(MbType::Skip, qp));
                nb.tcs.set(addr, TotalCoeffs::filled(0));
                addr += 1;
            }
            if run > 0 {
                more = r.pos() < end;
            }
        }
        if more {
            if addr >= ctx.pic_size {
                bail!("slice data runs past the end of the picture");
            }
            let mb = parse_mb(&mut r, &ctx, &mut nb, addr, &mut qp).map_err(|e| e.context(format!("macroblock {addr}")))?;
            mbs.push(mb);
            addr += 1;
        }
        more = r.pos() < end;
    }
    if r.pos() != end {
        bail!("slice data overran the rbsp_stop_one_bit");
    }
    Ok(SliceData { first_mb, mbs })
}

fn parse_mb(r: &mut BitReader, ctx: &MbContext, nb: &mut Neighbours, addr: usize, qp: &mut i32) -> Result<Macroblock> {
    let raw = r.ue()?;
    let off = ctx.intra_offset();
    let (mb_type, intra_cbp) = match ctx.slice_type {
        SliceType::P if raw < 5 => (MbType::P(raw as u8), 0),
        SliceType::B if raw < 23 => (MbType::B(raw as u8), 0),
        _ => decode_intra_type(raw - off)?,
    };
    let mut mb = Macroblock::new(mb_type, *qp);
    if mb_type == MbType::IPcm {
        while !r.byte_aligned() {
            if r.flag()? {
                bail!("pcm_alignment_zero_bit is one");
            }
        }
        let (n_luma, n_chroma) = pcm_sizes(ctx);
        let bd_y = ctx.sps.bit_depth_luma_minus8 + 8;
        let bd_c = ctx.sps.bit_depth_chroma_minus8 + 8;
        let mut samples = Vec::with_capacity(n_luma + n_chroma);
        for i in 0..n_luma + n_chroma {
            samples.push(r.u(if i < n_luma { bd_y } else { bd_c })? as u16);
        }
        mb.pcm = Some(samples);
        nb.tcs.set(addr, TotalCoeffs::filled(16));
        return Ok(mb);
    }
    let mut no_sub_lt8 = true;
    if mb_type.num_parts() == 4 {
        parse_sub_mb_pred(r, ctx, &mut mb)?;
        for i in 0..4 {
            let (n, pred, _, _) = sub_mb_info(ctx.slice_type, mb.sub_types[i]);
            if (pred == Pred::Direct && !ctx.sps.direct_8x8_inference_flag) || (pred != Pred::Direct && n > 1) {
                no_sub_lt8 = false;
            }
        }
    } else {
        if transform_8x8_mode(ctx) && mb_type == MbType::INxN {
            mb.transform_8x8 = r.flag()?;
        }
        parse_mb_pred(r, ctx, &mut mb)?;
    }
    if let MbType::I16x16(_) = mb_type {
        mb.cbp = intra_cbp;
    } else {
        let code = r.ue()? as usize;
        let (intra, inter) = ctx.cbp_tables();
        let table = if mb_type == MbType::INxN { intra } else { inter };
        mb.cbp = *table.get(code).ok_or_else(|| anyhow::anyhow!("coded_block_pattern {code} out of range"))?;
        if mb.cbp & 15 != 0
            && transform_8x8_mode(ctx)
            && mb_type != MbType::INxN
            && no_sub_lt8
            && (mb_type != MbType::B(0) || ctx.sps.direct_8x8_inference_flag)
        {
            mb.transform_8x8 = r.flag()?;
        }
    }
    let mut tc = TotalCoeffs::default();
    if mb.has_residual() {
        let delta = r.se()?;
        let range = 26 + ctx.qp_bd_offset() / 2;
        if delta < -range || delta > range - 1 {
            bail!("mb_qp_delta {delta} out of range");
        }
        *qp = wrap_qp(*qp + delta, ctx.qp_bd_offset());
        let mut coeffs = Vec::new();
        residual(&mut Coder::Read(r, &mut coeffs), ctx, nb, addr, &mb, &mut tc)?;
        mb.coeffs = coeffs;
    }
    mb.qp = *qp;
    nb.tcs.set(addr, tc);
    Ok(mb)
}

fn transform_8x8_mode(ctx: &MbContext) -> bool {
    ctx.pps.ext.as_ref().is_some_and(|e| e.transform_8x8_mode_flag)
}

fn pcm_sizes(ctx: &MbContext) -> (usize, usize) {
    let chroma = match ctx.chroma_array_type {
        1 => 2 * 64,
        2 => 2 * 128,
        3 => 2 * 256,
        _ => 0,
    };
    (256, chroma)
}

fn parse_mb_pred(r: &mut BitReader, ctx: &MbContext, mb: &mut Macroblock) -> Result<()> {
    if mb.mb_type.is_intra() {
        if mb.mb_type == MbType::INxN {
            for m in mb.intra_modes.iter_mut().take(if mb.transform_8x8 { 4 } else { 16 }) {
                *m = if r.flag()? { -1 } else { r.u(3)? as i8 };
            }
        }
        if matches!(ctx.chroma_array_type, 1 | 2) {
            let m = r.ue()?;
            if m > 3 {
                bail!("intra_chroma_pred_mode {m} out of range");
            }
            mb.intra_chroma_pred_mode = m as u8;
        }
        return Ok(());
    }
    let n = mb.mb_type.num_parts();
    for list in 0..2 {
        let cmax = ctx.num_ref_minus1(list);
        for i in 0..n {
            if cmax > 0 && uses_list(mb.mb_type.part_pred(i), list) {
                mb.ref_idx[list][i] = read_te(r, cmax)?;
            }
        }
    }
    for list in 0..2 {
        for i in 0..n {
            if uses_list(mb.mb_type.part_pred(i), list) {
                mb.mvd[list][i][0] = read_mvd(r)?;
            }
        }
    }
    Ok(())
}

/// A motion vector difference. Conformant streams keep mvd within 16 bits
/// (quarter samples); anything else is rejected rather than truncated.
pub type Mvd = [i16; 2];

/// Checked conversion of a motion vector difference to the stored type.
pub fn to_mvd(d: [i32; 2]) -> Result<Mvd> {
    match (i16::try_from(d[0]), i16::try_from(d[1])) {
        (Ok(x), Ok(y)) => Ok([x, y]),
        _ => bail!("motion vector difference {d:?} out of range"),
    }
}

fn read_mvd(r: &mut BitReader) -> Result<Mvd> {
    to_mvd([r.se()?, r.se()?])
}

fn write_mvd(w: &mut BitWriter, d: Mvd) {
    w.se(d[0] as i32);
    w.se(d[1] as i32);
}

fn uses_list(pred: Pred, list: usize) -> bool {
    match pred {
        Pred::Direct => false,
        Pred::Bi => true,
        Pred::L0 => list == 0,
        Pred::L1 => list == 1,
    }
}

fn parse_sub_mb_pred(r: &mut BitReader, ctx: &MbContext, mb: &mut Macroblock) -> Result<()> {
    let max = if ctx.slice_type == SliceType::B { 13 } else { 4 };
    for s in mb.sub_types.iter_mut() {
        let t = r.ue()?;
        if t >= max {
            bail!("sub_mb_type {t} out of range");
        }
        *s = t as u8;
    }
    for list in 0..2 {
        let cmax = ctx.num_ref_minus1(list);
        for i in 0..4 {
            let (_, pred, _, _) = sub_mb_info(ctx.slice_type, mb.sub_types[i]);
            if cmax > 0 && mb.mb_type != MbType::P(4) && uses_list(pred, list) {
                mb.ref_idx[list][i] = read_te(r, cmax)?;
            }
        }
    }
    for list in 0..2 {
        for i in 0..4 {
            let (n, pred, _, _) = sub_mb_info(ctx.slice_type, mb.sub_types[i]);
            if uses_list(pred, list) {
                for j in 0..n {
                    mb.mvd[list][i][j] = read_mvd(r)?;
                }
            }
        }
    }
    Ok(())
}

/// Shared residual traversal for reading and writing.
enum Coder<'r, 'b> {
    Read(&'r mut BitReader<'b>, &'r mut Vec<CoeffBlock>),
    Write(&'r mut BitWriter, &'r [CoeffBlock]),
}

impl Coder<'_, '_> {
    fn block(&mut self, blk: Blk, nc: i32, max: usize) -> Result<u8> {
        match self {
            Coder::Read(r, out) => {
                let mut levels = [0; 16];
                let n = read_block(r, nc, &mut levels, max)?;
                if n > 0 {
                    out.push(CoeffBlock { blk, levels });
                }
                Ok(n)
            }
            Coder::Write(w, blocks) => {
                let levels = blocks.iter().find(|b| b.blk == blk).map_or(&[0; 16], |b| &b.levels);
                write_block(w, nc, levels, max)
            }
        }
    }
}

fn residual(coder: &mut Coder, ctx: &MbContext, nb: &Neighbours, addr: usize, mb: &Macroblock, tc: &mut TotalCoeffs) -> Result<()> {
    let i16 = matches!(mb.mb_type, MbType::I16x16(_));
    for p in 0..ctx.luma_planes() {
        if i16 {
            let nc = nb.nc_luma(tc, addr, p, 0);
            coder.block(Blk::LumaDc(p as u8), nc, 16)?;
        }
        for blk in 0..16 {
            if mb.cbp & (1 << (blk / 4)) != 0 {
                let nc = nb.nc_luma(tc, addr, p, blk);
                let max = if i16 { 15 } else { 16 };
                tc.luma[p][raster(blk)] = coder.block(Blk::Luma(p as u8, blk as u8), nc, max)?;
            }
        }
    }
    if matches!(ctx.chroma_array_type, 1 | 2) {
        let n = ctx.chroma_blocks();
        let cbp_chroma = mb.cbp >> 4;
        if cbp_chroma != 0 {
            let nc = if ctx.chroma_array_type == 1 { -1 } else { -2 };
            for c in 0..2 {
                coder.block(Blk::ChromaDc(c as u8), nc, n)?;
            }
        }
        if cbp_chroma & 2 != 0 {
            for c in 0..2 {
                for blk in 0..n {
                    let nc = nb.nc_chroma(tc, addr, c, blk, n / 2);
                    tc.chroma[c][blk] = coder.block(Blk::ChromaAc(c as u8, blk as u8), nc, 15)?;
                }
            }
        }
    }
    Ok(())
}

/// Serialize slice data; returns the bytes and the length in bits (without trailing bits).
pub fn write_slice_data(slice: &Slice, data: &SliceData) -> Result<(Vec<u8>, usize)> {
    let ctx = MbContext::new(slice)?;
    if data.first_mb != ctx.header.first_mb_in_slice as usize {
        bail!("slice data does not start at first_mb_in_slice");
    }
    if data.first_mb + data.mbs.len() > ctx.pic_size {
        bail!("slice data has more macroblocks than the picture");
    }
    let mut w = BitWriter::new();
    let mut nb = Neighbours::new(ctx.width);
    let mut qp = ctx.slice_qp();
    let mut skips = 0u32;
    for (i, mb) in data.mbs.iter().enumerate() {
        let addr = data.first_mb + i;
        if mb.mb_type == MbType::Skip {
            if ctx.slice_type == SliceType::I {
                bail!("skipped macroblock in an I slice");
            }
            skips += 1;
            nb.tcs.set(addr, TotalCoeffs::filled(0));
            continue;
        }
        if ctx.slice_type != SliceType::I {
            w.ue(skips);
            skips = 0;
        }
        write_mb(&mut w, &ctx, &mut nb, addr, mb, &mut qp).map_err(|e| e.context(format!("macroblock {addr}")))?;
    }
    if skips > 0 {
        w.ue(skips);
    }
    let bits = w.bit_len();
    Ok((w.into_bytes(), bits))
}

fn write_mb(w: &mut BitWriter, ctx: &MbContext, nb: &mut Neighbours, addr: usize, mb: &Macroblock, qp: &mut i32) -> Result<()> {
    let off = ctx.intra_offset();
    let raw = match mb.mb_type {
        MbType::P(t) if ctx.slice_type == SliceType::P => t as u32,
        MbType::B(t) if ctx.slice_type == SliceType::B => t as u32,
        MbType::INxN => off,
        MbType::IPcm => off + 25,
        MbType::I16x16(pred) => {
            let luma = mb.cbp & 15;
            if luma != 0 && luma != 15 {
                bail!("Intra16x16 needs a luma coded block pattern of 0 or 15");
            }
            off + 1 + pred as u32 + 4 * (mb.cbp >> 4) as u32 + if luma == 15 { 12 } else { 0 }
        }
        t => bail!("{t:?} macroblock in a {:?} slice", ctx.slice_type),
    };
    w.ue(raw);
    if mb.mb_type == MbType::IPcm {
        while !w.byte_aligned() {
            w.flag(false);
        }
        let (n_luma, n_chroma) = pcm_sizes(ctx);
        let Some(samples) = mb.pcm.as_ref().filter(|s| s.len() == n_luma + n_chroma) else { bail!("I_PCM macroblock without samples") };
        let bd_y = ctx.sps.bit_depth_luma_minus8 + 8;
        let bd_c = ctx.sps.bit_depth_chroma_minus8 + 8;
        for (i, &s) in samples.iter().enumerate() {
            w.u(if i < n_luma { bd_y } else { bd_c }, s as u32);
        }
        nb.tcs.set(addr, TotalCoeffs::filled(16));
        return Ok(());
    }
    let mut no_sub_lt8 = true;
    if mb.mb_type.num_parts() == 4 {
        write_sub_mb_pred(w, ctx, mb);
        for i in 0..4 {
            let (n, pred, _, _) = sub_mb_info(ctx.slice_type, mb.sub_types[i]);
            if (pred == Pred::Direct && !ctx.sps.direct_8x8_inference_flag) || (pred != Pred::Direct && n > 1) {
                no_sub_lt8 = false;
            }
        }
    } else {
        if transform_8x8_mode(ctx) && mb.mb_type == MbType::INxN {
            w.flag(mb.transform_8x8);
        }
        write_mb_pred(w, ctx, mb);
    }
    if !matches!(mb.mb_type, MbType::I16x16(_)) {
        let (intra, inter) = ctx.cbp_tables();
        let table = if mb.mb_type == MbType::INxN { intra } else { inter };
        let Some(code) = table.iter().position(|&c| c == mb.cbp) else { bail!("coded_block_pattern {} not codable", mb.cbp) };
        w.ue(code as u32);
        if mb.cbp & 15 != 0
            && transform_8x8_mode(ctx)
            && mb.mb_type != MbType::INxN
            && no_sub_lt8
            && (mb.mb_type != MbType::B(0) || ctx.sps.direct_8x8_inference_flag)
        {
            w.flag(mb.transform_8x8);
        }
    }
    let mut tc = TotalCoeffs::default();
    if mb.has_residual() {
        let m = 52 + ctx.qp_bd_offset();
        let range = 26 + ctx.qp_bd_offset() / 2;
        let mut delta = (mb.qp - *qp).rem_euclid(m);
        if delta > range - 1 {
            delta -= m;
        }
        w.se(delta);
        *qp = wrap_qp(*qp + delta, ctx.qp_bd_offset());
        residual(&mut Coder::Write(w, &mb.coeffs), ctx, nb, addr, mb, &mut tc)?;
    }
    nb.tcs.set(addr, tc);
    Ok(())
}

fn write_mb_pred(w: &mut BitWriter, ctx: &MbContext, mb: &Macroblock) {
    if mb.mb_type.is_intra() {
        if mb.mb_type == MbType::INxN {
            for &m in mb.intra_modes.iter().take(if mb.transform_8x8 { 4 } else { 16 }) {
                w.flag(m < 0);
                if m >= 0 {
                    w.u(3, m as u32);
                }
            }
        }
        if matches!(ctx.chroma_array_type, 1 | 2) {
            w.ue(mb.intra_chroma_pred_mode as u32);
        }
        return;
    }
    let n = mb.mb_type.num_parts();
    for list in 0..2 {
        let cmax = ctx.num_ref_minus1(list);
        for i in 0..n {
            if cmax > 0 && uses_list(mb.mb_type.part_pred(i), list) {
                write_te(w, cmax, mb.ref_idx[list][i]);
            }
        }
    }
    for list in 0..2 {
        for i in 0..n {
            if uses_list(mb.mb_type.part_pred(i), list) {
                write_mvd(w, mb.mvd[list][i][0]);
            }
        }
    }
}

fn write_sub_mb_pred(w: &mut BitWriter, ctx: &MbContext, mb: &Macroblock) {
    for &s in &mb.sub_types {
        w.ue(s as u32);
    }
    for list in 0..2 {
        let cmax = ctx.num_ref_minus1(list);
        for i in 0..4 {
            let (_, pred, _, _) = sub_mb_info(ctx.slice_type, mb.sub_types[i]);
            if cmax > 0 && mb.mb_type != MbType::P(4) && uses_list(pred, list) {
                write_te(w, cmax, mb.ref_idx[list][i]);
            }
        }
    }
    for list in 0..2 {
        for i in 0..4 {
            let (n, pred, _, _) = sub_mb_info(ctx.slice_type, mb.sub_types[i]);
            if uses_list(pred, list) {
                for j in 0..n {
                    write_mvd(w, mb.mvd[list][i][j]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slices hold thousands of these; their size drives memory traffic.
    #[test]
    fn macroblock_stays_small() {
        let size = std::mem::size_of::<Macroblock>();
        println!("Macroblock: {size} bytes");
        assert!(size <= 224, "Macroblock grew to {size} bytes");
    }
}
