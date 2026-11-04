//! CABAC slice data (7.3.4 and 7.3.5 with ae(v) descriptors, 9.3): parsing into the
//! macroblock model of [`crate::mb`] and writing it back.
//!
//! Reading and writing share one traversal, generic over the direction of the
//! arithmetic coder ([`Engine`]): every bin either comes from the stream or is the bin
//! of the value being written, so both directions derive each context from the same
//! neighbour data. The writer codes macroblocks as a decoder will see them: elements
//! that are not coded take their inferred values before they feed later contexts.
//!
//! An 8x8 transform block has 64 coefficients in CABAC; the model keeps it as the four
//! interleaved 4x4 blocks of CAVLC (7.3.5.3.1: coefficient 4 * i + j of the 8x8 block is
//! coefficient i of 4x4 block j). Its coded_block_flag is not coded outside 4:4:4, so
//! the writer recodes 8x8 blocks without coefficients in a way that decodes the same.
//!
//! For debugging, [`bin_trace`] lists every bin with its macroblock, syntax element and
//! context, as read or as written.

pub mod binarize;
pub mod engine;
pub mod tables;

use crate::bits::{BitReader, BitWriter};
use crate::mb::{
    self, Blk, CoeffBlock, LUMA4X4_XY, Macroblock, MbContext, MbType, Mvd, Pred, RowCache, Shape, SliceData, sub_mb_info, to_mvd, uses_list,
};
use crate::slice::{Slice, SliceType};
use anyhow::{Result, bail};
use binarize::*;
use engine::{Context, Decoder, Encoder};

/// One direction of the arithmetic coder. Decoding returns the bin read from the
/// stream and ignores `bin`; encoding codes `bin` and returns it.
trait Engine {
    const READ: bool;
    /// a bin coded with context variable `ctx`, number `ctx_idx`
    fn decision(&mut self, ctx_idx: usize, ctx: &mut Context, bin: bool) -> bool;
    fn bypass(&mut self, bin: bool) -> bool;
    fn terminate(&mut self, bin: bool) -> bool;
    /// The raw part of an I_PCM macroblock after its mb_type (alignment and samples:
    /// read into `mb`, or written from `src`), then the engine restarts (9.3.1.2).
    fn pcm(&mut self, ctx: &MbContext, mb: &mut Macroblock, src: Option<&Macroblock>) -> Result<()>;
    /// Bit position in the slice data: of the next bit to read, or bits written so far.
    fn pos(&self) -> usize;
    /// The syntax element the next bins belong to (for tracing).
    #[inline]
    fn element(&mut self, _addr: usize, _name: &'static str) {}
}

struct Reader<'a> {
    dec: Decoder<'a>,
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(slice: &'a Slice) -> Result<Self> {
        let (data, start, _) = slice.slice_data();
        Ok(Self { dec: Decoder::new(data, start)?, data })
    }
}

impl Engine for Reader<'_> {
    const READ: bool = true;

    #[inline]
    fn decision(&mut self, _: usize, ctx: &mut Context, _: bool) -> bool {
        self.dec.decision(ctx)
    }

    #[inline]
    fn bypass(&mut self, _: bool) -> bool {
        self.dec.bypass()
    }

    #[inline]
    fn terminate(&mut self, _: bool) -> bool {
        self.dec.terminate()
    }

    fn pcm(&mut self, ctx: &MbContext, mb: &mut Macroblock, _: Option<&Macroblock>) -> Result<()> {
        let mut r = BitReader::at(self.data, self.dec.pos());
        mb.pcm = Some(mb::read_pcm(&mut r, ctx)?);
        self.dec = Decoder::new(self.data, r.pos())?;
        Ok(())
    }

    fn pos(&self) -> usize {
        self.dec.pos()
    }
}

struct Writer(Encoder);

impl Engine for Writer {
    const READ: bool = false;

    #[inline]
    fn decision(&mut self, _: usize, ctx: &mut Context, bin: bool) -> bool {
        self.0.decision(ctx, bin);
        bin
    }

    #[inline]
    fn bypass(&mut self, bin: bool) -> bool {
        self.0.bypass(bin);
        bin
    }

    #[inline]
    fn terminate(&mut self, bin: bool) -> bool {
        self.0.terminate(bin);
        bin
    }

    fn pcm(&mut self, ctx: &MbContext, _: &mut Macroblock, src: Option<&Macroblock>) -> Result<()> {
        let Some(src) = src else { bail!("I_PCM macroblock without samples") };
        mb::write_pcm(self.0.writer(), ctx, src)?;
        self.0.restart();
        Ok(())
    }

    fn pos(&self) -> usize {
        self.0.bit_len()
    }
}

/// One bin of a [`bin_trace`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracedBin {
    /// macroblock address
    pub mb: usize,
    pub element: &'static str,
    /// ctxIdx; 276 for bins of the terminate process, None for bypass bins
    pub ctx: Option<u16>,
    pub bin: bool,
}

/// An engine that records the bins going through it.
struct Traced<'t, E> {
    e: E,
    addr: usize,
    element: &'static str,
    bins: &'t mut Vec<TracedBin>,
}

impl<E: Engine> Traced<'_, E> {
    fn push(&mut self, ctx: Option<u16>, bin: bool) -> bool {
        self.bins.push(TracedBin { mb: self.addr, element: self.element, ctx, bin });
        bin
    }
}

impl<E: Engine> Engine for Traced<'_, E> {
    const READ: bool = E::READ;

    fn decision(&mut self, ctx_idx: usize, ctx: &mut Context, bin: bool) -> bool {
        let b = self.e.decision(ctx_idx, ctx, bin);
        self.push(Some(ctx_idx as u16), b)
    }

    fn bypass(&mut self, bin: bool) -> bool {
        let b = self.e.bypass(bin);
        self.push(None, b)
    }

    fn terminate(&mut self, bin: bool) -> bool {
        let b = self.e.terminate(bin);
        self.push(Some(276), b)
    }

    fn pcm(&mut self, ctx: &MbContext, mb: &mut Macroblock, src: Option<&Macroblock>) -> Result<()> {
        self.e.pcm(ctx, mb, src)
    }

    fn pos(&self) -> usize {
        self.e.pos()
    }

    fn element(&mut self, addr: usize, name: &'static str) {
        (self.addr, self.element) = (addr, name);
    }
}

/// How a bin is coded: with a context variable (ctxIdx), bypassed, or by the
/// terminate process (ctxIdx 276).
#[derive(Clone, Copy)]
enum Bin {
    Ctx(usize),
    Bypass,
    Terminate,
}

// ctxIdxOffset values (Table 9-34)
const MB_SKIP_P: usize = 11;
const MB_SKIP_B: usize = 24;
const MB_TYPE_I: usize = 3;
const MB_TYPE_P: usize = 14;
const MB_TYPE_P_INTRA: usize = 17;
const SUB_MB_TYPE_P: usize = 21;
const MB_TYPE_B: usize = 27;
const MB_TYPE_B_INTRA: usize = 32;
const SUB_MB_TYPE_B: usize = 36;
const MVD: [usize; 2] = [40, 47];
const REF_IDX: usize = 54;
const MB_QP_DELTA: usize = 60;
const CHROMA_PRED: usize = 64;
const PREV_INTRA_PRED: usize = 68;
const REM_INTRA_PRED: usize = 69;
const CBP_LUMA: usize = 73;
const CBP_CHROMA: usize = 77;
const TRANSFORM_8X8: usize = 399;

// Residual contexts by ctxBlockCat 0..=13 (Table 9-42): ctxIdxOffset (Table 9-34) plus
// ctxIdxBlockCatOffset (Table 9-40). Significance maps have frame and field variants.
const CODED_BLOCK_FLAG: [usize; 14] = [85, 89, 93, 97, 101, 1012, 460, 464, 468, 1016, 472, 476, 480, 1020];
const SIGNIFICANT: [[usize; 14]; 2] = [
    [105, 120, 134, 149, 152, 402, 484, 499, 513, 660, 528, 543, 557, 718],
    [277, 292, 306, 321, 324, 436, 776, 791, 805, 675, 820, 835, 849, 733],
];
const LAST_SIGNIFICANT: [[usize; 14]; 2] = [
    [166, 181, 195, 210, 213, 417, 572, 587, 601, 690, 616, 631, 645, 748],
    [338, 353, 367, 382, 385, 451, 864, 879, 893, 699, 908, 923, 937, 757],
];
const ABS_LEVEL: [usize; 14] = [227, 237, 247, 257, 266, 426, 952, 962, 972, 708, 982, 992, 1002, 766];

/// ctxIdxInc of significant_coeff_flag in 8x8 blocks for frame and field macroblocks,
/// and of last_significant_coeff_flag, by scanning position (Table 9-43).
#[rustfmt::skip]
const SIGNIFICANT_8X8: [[u8; 63]; 2] = [
    [
        0, 1, 2, 3, 4, 5, 5, 4, 4, 3, 3, 4, 4, 4, 5, 5, 4, 4, 4, 4, 3, 3, 6, 7, 7, 7, 8, 9, 10, 9, 8, 7,
        7, 6, 11, 12, 13, 11, 6, 7, 8, 9, 14, 10, 9, 8, 6, 11, 12, 13, 11, 6, 9, 14, 10, 9, 11, 12, 13, 11, 14, 10, 12,
    ],
    [
        0, 1, 1, 2, 2, 3, 3, 4, 5, 6, 7, 7, 7, 8, 4, 5, 6, 9, 10, 10, 8, 11, 12, 11, 9, 9, 10, 10, 8, 11, 12, 11,
        9, 9, 10, 10, 8, 11, 12, 11, 9, 9, 10, 10, 8, 13, 13, 9, 9, 10, 10, 8, 13, 13, 9, 9, 10, 10, 14, 14, 14, 14, 14,
    ],
];
#[rustfmt::skip]
const LAST_SIGNIFICANT_8X8: [u8; 63] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8,
];

/// What context derivation needs of a coded macroblock, as a decoder sees it.
#[derive(Clone, Copy, Default)]
struct Nb {
    skip: bool,
    pcm: bool,
    intra: bool,
    /// I_NxN
    nxn: bool,
    /// B_Skip or B_Direct_16x16
    direct: bool,
    transform_8x8: bool,
    /// CodedBlockPatternLuma | CodedBlockPatternChroma << 4
    cbp: u8,
    intra_chroma_pred_mode: u8,
    /// per list: 8x8 blocks predicted from the list with ref_idx > 0 (9.3.3.1.1.6)
    ref_gt0: [u8; 2],
    /// per list, raster 4x4 block and component: Abs(mvd), saturated (9.3.3.1.1.7)
    mvd: [[[u8; 2]; 16]; 2],
    /// coded_block_flag per plane and raster 4x4 block; an 8x8 transform block's flag
    /// sits at each of its four 4x4 positions
    cbf: [u16; 3],
    /// coded_block_flag of DC blocks: bits 0..=2 Intra16x16 luma planes, bits 3, 4 Cb, Cr
    cbf_dc: u8,
    /// coded_block_flag of chroma AC blocks per component, by chroma4x4BlkIdx
    cbf_ac: [u8; 2],
}

/// The 8x8 block containing raster 4x4 block (x, y).
fn blk8(x: usize, y: usize) -> usize {
    (y / 2) * 2 + x / 2
}

/// Top-left 4x4 block and size in 4x4 blocks of macroblock partition `part`.
fn part_rect(shape: Shape, part: usize) -> (usize, usize, usize, usize) {
    match shape {
        Shape::S16x16 => (0, 0, 4, 4),
        Shape::S16x8 => (0, 2 * part, 4, 2),
        Shape::S8x16 => (2 * part, 0, 2, 4),
        Shape::S8x8 => (2 * (part % 2), 2 * (part / 2), 2, 2),
    }
}

/// Top-left 4x4 block and size of sub-macroblock partition `sub` of 8x8 block `part`,
/// for a sub-macroblock type with partitions of `w` x `h` samples.
fn sub_rect(part: usize, sub: usize, w: usize, h: usize) -> (usize, usize, usize, usize) {
    let (x, y) = (2 * (part % 2), 2 * (part / 2));
    match (w, h) {
        (8, 8) => (x, y, 2, 2),
        (8, 4) => (x, y + sub, 2, 1),
        (4, 8) => (x + sub, y, 1, 2),
        _ => (x + sub % 2, y + sub / 2, 1, 1),
    }
}

fn levels_of(coeffs: &[CoeffBlock], blk: Blk) -> &[i32; 16] {
    coeffs.iter().find(|b| b.blk == blk).map_or(&[0; 16], |b| &b.levels)
}

/// The CABAC state of one slice.
struct Coder<'a, E> {
    e: E,
    ctx: &'a MbContext<'a>,
    cx: [Context; 1024],
    nbs: RowCache<Nb>,
    left: Option<Nb>,
    top: Option<Nb>,
    cur: Nb,
    /// address of the current macroblock
    addr: usize,
    /// field picture: field variants of the significance map contexts
    field: usize,
    qp: i32,
    /// mb_qp_delta of the previous macroblock was coded and non-zero (9.3.3.1.1.5)
    prev_qp_delta: bool,
}

impl<'a, E: Engine> Coder<'a, E> {
    /// 9.3.1.1
    fn new(e: E, ctx: &'a MbContext<'a>) -> Result<Self> {
        let idc = ctx.header.cabac_init_idc as usize;
        let table = if ctx.slice_type == SliceType::I {
            &tables::INIT_I
        } else if idc <= 2 {
            &tables::INIT_PB[idc]
        } else {
            bail!("cabac_init_idc {idc} out of range")
        };
        let qp = ctx.slice_qp();
        let cx = std::array::from_fn(|i| Context::new(table[i].0, table[i].1, qp));
        Ok(Self {
            e,
            ctx,
            cx,
            nbs: RowCache::new(ctx.width),
            left: None,
            top: None,
            cur: Nb::default(),
            addr: ctx.header.first_mb_in_slice as usize,
            field: ctx.header.field_pic_flag as usize,
            qp,
            prev_qp_delta: false,
        })
    }

    /// Name the syntax element the next bins belong to (for tracing).
    #[inline]
    fn se(&mut self, name: &'static str) {
        self.e.element(self.addr, name);
    }

    #[inline]
    fn flag(&mut self, ctx_idx: usize, bin: bool) -> bool {
        self.e.decision(ctx_idx, &mut self.cx[ctx_idx], bin)
    }

    #[inline]
    fn bin(&mut self, how: Bin, bin: bool) -> bool {
        match how {
            Bin::Ctx(i) => self.e.decision(i, &mut self.cx[i], bin),
            Bin::Bypass => self.e.bypass(bin),
            Bin::Terminate => self.e.terminate(bin),
        }
    }

    /// Code the bin string `value` (ignored when reading) bin by bin; `how(binIdx, bins
    /// so far)` says how each bin is coded and `matches` recognizes complete strings.
    #[inline]
    fn string<T>(&mut self, value: Bins, how: impl Fn(u32, Bins) -> Bin, matches: impl Fn(Bins) -> Match<T>) -> Result<T> {
        let mut bins = Bins::default();
        loop {
            let i = bins.len;
            let b = self.bin(how(i, bins), value.get(i));
            bins.push(b);
            match matches(bins) {
                Match::Done(v) => return Ok(v),
                Match::More => {}
                Match::Invalid => bail!("invalid bin string"),
            }
        }
    }

    /// U binarization (9.3.2.1) of a value up to `max`, with the context of bin binIdx
    /// from `ctx_of(binIdx)`.
    fn unary(&mut self, value: u32, max: u32, ctx_of: impl Fn(u32) -> usize) -> Result<u32> {
        let mut n = 0;
        while self.flag(ctx_of(n), n < value) {
            n += 1;
            if n > max {
                bail!("unary value exceeds {max}");
            }
        }
        Ok(n)
    }

    /// condTermFlagA + condTermFlagB for conditions that are 0 for unavailable neighbours.
    fn inc_ab(&self, f: impl Fn(&Nb) -> bool) -> usize {
        self.left.as_ref().is_some_and(&f) as usize + self.top.as_ref().is_some_and(&f) as usize
    }

    /// condTermFlagN of coded_block_flag (9.3.3.1.1.9): `bit` is the flag of the
    /// neighbouring block in an available, non-I_PCM macroblock.
    fn cbf_cond(&self, n: Option<&Nb>, bit: impl Fn(&Nb) -> bool) -> usize {
        match n {
            None => self.cur.intra as usize,
            Some(n) if n.pcm => 1,
            Some(n) => bit(n) as usize,
        }
    }

    /// One macroblock: mb_skip_flag (P and B slices) and macroblock_layer(). Reading
    /// fills `m`; writing codes `m`, a copy of `src` without residual and samples, which
    /// are taken from `src`.
    fn macroblock(&mut self, addr: usize, m: &mut Macroblock, src: Option<&Macroblock>) -> Result<()> {
        let ctx = self.ctx;
        let st = ctx.slice_type;
        self.left = if addr.is_multiple_of(ctx.width) { None } else { self.nbs.get(addr - 1).copied() };
        self.top = if addr < ctx.width { None } else { self.nbs.get(addr - ctx.width).copied() };
        self.cur = Nb::default();
        self.addr = addr;
        if st != SliceType::I {
            let off = if st == SliceType::B { MB_SKIP_B } else { MB_SKIP_P };
            let inc = self.inc_ab(|n| !n.skip);
            self.se("mb_skip_flag");
            if self.flag(off + inc, m.mb_type == MbType::Skip) {
                m.mb_type = MbType::Skip;
                m.qp = self.qp;
                self.cur.skip = true;
                self.cur.direct = st == SliceType::B;
                self.prev_qp_delta = false;
                self.nbs.set(addr, self.cur);
                return Ok(());
            }
        } else if !E::READ && m.mb_type == MbType::Skip {
            bail!("skipped macroblock in an I slice");
        }
        if !E::READ && m.mb_type == MbType::P(4) {
            // P_8x8ref0 has no CABAC bin string; P_8x8 with all ref_idx 0 is the same
            m.mb_type = MbType::P(3);
            m.ref_idx[0] = [0; 4];
        }
        let raw = if E::READ { 0 } else { mb::mb_type_to_raw(ctx, m)? };
        self.se("mb_type");
        let (mb_type, intra_cbp) = mb::mb_type_from_raw(ctx, self.mb_type(raw)?)?;
        m.mb_type = mb_type;
        self.cur.intra = mb_type.is_intra();
        self.cur.nxn = mb_type == MbType::INxN;
        self.cur.direct = mb_type == MbType::B(0);
        if mb_type == MbType::IPcm {
            self.e.pcm(ctx, m, src)?;
            m.qp = self.qp;
            self.cur.pcm = true;
            self.prev_qp_delta = false;
            self.nbs.set(addr, self.cur);
            return Ok(());
        }
        let t8_mode = mb::transform_8x8_mode(ctx);
        let mut no_sub_lt8 = true;
        if mb_type.num_parts() == 4 {
            self.sub_mb_pred(m)?;
            for i in 0..4 {
                let (n, pred, _, _) = sub_mb_info(st, m.sub_types[i]);
                if (pred == Pred::Direct && !ctx.sps.direct_8x8_inference_flag) || (pred != Pred::Direct && n > 1) {
                    no_sub_lt8 = false;
                }
            }
        } else {
            if t8_mode && mb_type == MbType::INxN {
                m.transform_8x8 = self.transform_8x8(m.transform_8x8);
            } else if mb_type == MbType::INxN {
                m.transform_8x8 = false;
            }
            self.mb_pred(m)?;
        }
        if let MbType::I16x16(_) = mb_type {
            m.cbp = intra_cbp;
            m.transform_8x8 = false;
        } else {
            let t8_allowed =
                t8_mode && mb_type != MbType::INxN && no_sub_lt8 && (mb_type != MbType::B(0) || ctx.sps.direct_8x8_inference_flag);
            if let Some(src) = src.filter(|_| ctx.chroma_array_type != 3)
                && m.transform_8x8
                && (mb_type == MbType::INxN || t8_allowed)
            {
                // An 8x8 block without coefficients cannot be coded under its
                // CodedBlockPatternLuma bit. Keep the picture and the QP: without any luma
                // coefficients an inter macroblock can take the 4x4 transform instead;
                // otherwise the empty blocks go, and an empty chroma DC keeps mb_qp_delta.
                let empty = (0..4)
                    .filter(|&b8| (0..4).all(|j| levels_of(&src.coeffs, Blk::Luma(0, 4 * b8 + j)).iter().all(|&l| l == 0)))
                    .fold(0, |e, b8| e | 1 << b8)
                    & m.cbp;
                if empty != 0 && mb_type != MbType::INxN && empty == m.cbp & 15 {
                    m.transform_8x8 = false;
                } else if empty != 0 {
                    m.cbp &= !empty;
                    if m.cbp == 0 && ctx.chroma_array_type != 0 {
                        m.cbp = 0x10;
                    }
                }
            }
            m.cbp = self.coded_block_pattern(m.cbp)?;
            if m.cbp & 15 != 0 && t8_allowed {
                m.transform_8x8 = self.transform_8x8(m.transform_8x8);
            } else if mb_type != MbType::INxN {
                m.transform_8x8 = false;
            }
        }
        self.cur.cbp = m.cbp;
        self.cur.transform_8x8 = m.transform_8x8;
        if m.has_residual() {
            let off = ctx.qp_bd_offset();
            let delta = if E::READ {
                0
            } else {
                let modulus = 52 + off;
                let d = (m.qp - self.qp).rem_euclid(modulus);
                if d > 25 + off / 2 { d - modulus } else { d }
            };
            self.se("mb_qp_delta");
            let delta = self.mb_qp_delta(delta)?;
            self.qp = mb::wrap_qp(self.qp + delta, off);
            self.prev_qp_delta = delta != 0;
            let mut coeffs = Vec::new();
            self.residual(m, src.map_or(&[], |s| &s.coeffs), &mut coeffs)?;
            if E::READ {
                m.coeffs = coeffs;
            }
        } else {
            self.prev_qp_delta = false;
        }
        m.qp = self.qp;
        self.nbs.set(addr, self.cur);
        Ok(())
    }

    /// mb_type (9.3.2.5, contexts 9.3.3.1.1.3 and 9.3.3.1.2): the coded value `raw`.
    fn mb_type(&mut self, raw: u32) -> Result<u32> {
        match self.ctx.slice_type {
            SliceType::P => {
                let how = |i, b: Bins| {
                    Bin::Ctx(
                        MB_TYPE_P
                            + match i {
                                0 => 0,
                                1 => 1,
                                _ => 2 + b.get(1) as usize,
                            },
                    )
                };
                match self.string(mb_type_p_prefix(raw), how, match_mb_type_p_prefix)? {
                    5 => Ok(5 + self.mb_type_i(raw.saturating_sub(5), Some(MB_TYPE_P_INTRA))?),
                    t => Ok(t),
                }
            }
            SliceType::B => {
                let inc = self.inc_ab(|n| !n.direct);
                let how = move |i, b: Bins| {
                    Bin::Ctx(
                        MB_TYPE_B
                            + match i {
                                0 => inc,
                                1 => 3,
                                2 => 5 - b.get(1) as usize,
                                _ => 5,
                            },
                    )
                };
                match self.string(mb_type_b_prefix(raw), how, match_mb_type_b_prefix)? {
                    23 => Ok(23 + self.mb_type_i(raw.saturating_sub(23), Some(MB_TYPE_B_INTRA))?),
                    t => Ok(t),
                }
            }
            _ => self.mb_type_i(raw, None),
        }
    }

    /// mb_type of an I slice, or the suffix of an intra mb_type in P and B slices
    /// coded at ctxIdxOffset `suffix`.
    fn mb_type_i(&mut self, t: u32, suffix: Option<usize>) -> Result<u32> {
        let (off, inc) = match suffix {
            Some(off) => (off, 0),
            None => (MB_TYPE_I, self.inc_ab(|n| !n.nxn)),
        };
        let prefix = suffix.is_none();
        let how = move |i, b: Bins| {
            let b3 = b.get(3) as usize;
            Bin::Ctx(
                off + match (i, prefix) {
                    (0, _) => inc,
                    (1, _) => return Bin::Terminate,
                    (2, true) => 3,
                    (3, true) => 4,
                    (4, true) => 6 - b3,
                    (5, true) => 7 - b3,
                    (_, true) => 7,
                    (2, false) => 1,
                    (3, false) => 2,
                    (4, false) => 3 - b3,
                    (_, false) => 3,
                },
            )
        };
        self.string(mb_type_i(t), how, match_mb_type_i)
    }

    fn transform_8x8(&mut self, v: bool) -> bool {
        let inc = self.inc_ab(|n| n.transform_8x8);
        self.se("transform_size_8x8_flag");
        self.flag(TRANSFORM_8X8 + inc, v)
    }

    fn mb_pred(&mut self, m: &mut Macroblock) -> Result<()> {
        if m.mb_type.is_intra() {
            if m.mb_type == MbType::INxN {
                for k in 0..if m.transform_8x8 { 4 } else { 16 } {
                    let v = m.intra_modes[k];
                    self.se("prev_intra_pred_mode_flag");
                    m.intra_modes[k] = if self.flag(PREV_INTRA_PRED, v < 0) {
                        -1
                    } else {
                        self.se("rem_intra_pred_mode");
                        let how = |_, _| Bin::Ctx(REM_INTRA_PRED);
                        self.string(fixed_length(v.clamp(0, 7) as u32, 7), how, |b| match_fixed_length(b, 7))? as i8
                    };
                }
            }
            if matches!(self.ctx.chroma_array_type, 1 | 2) {
                let inc = self.inc_ab(|n| n.intra_chroma_pred_mode != 0);
                let how = move |i, _| Bin::Ctx(CHROMA_PRED + if i == 0 { inc } else { 3 });
                let v = m.intra_chroma_pred_mode.min(3) as u32;
                self.se("intra_chroma_pred_mode");
                m.intra_chroma_pred_mode = self.string(truncated_unary(v, 3), how, |b| match_truncated_unary(b, 3))? as u8;
                self.cur.intra_chroma_pred_mode = m.intra_chroma_pred_mode;
            }
            return Ok(());
        }
        let (n, shape) = (m.mb_type.num_parts(), m.mb_type.shape());
        for list in 0..2 {
            for i in 0..n {
                let (x, y, w, h) = part_rect(shape, i);
                if self.ctx.num_ref_minus1(list) > 0 && uses_list(m.mb_type.part_pred(i), list) {
                    m.ref_idx[list][i] = self.ref_idx(list, x, y, w, h, m.ref_idx[list][i])?;
                }
            }
        }
        for list in 0..2 {
            for i in 0..n {
                if uses_list(m.mb_type.part_pred(i), list) {
                    m.mvd[list][i][0] = self.mvd(list, part_rect(shape, i), m.mvd[list][i][0])?;
                }
            }
        }
        Ok(())
    }

    fn sub_mb_pred(&mut self, m: &mut Macroblock) -> Result<()> {
        let st = self.ctx.slice_type;
        for i in 0..4 {
            let v = m.sub_types[i] as u32;
            self.se("sub_mb_type");
            m.sub_types[i] = if st == SliceType::B {
                let how = |i, b: Bins| {
                    Bin::Ctx(
                        SUB_MB_TYPE_B
                            + match i {
                                0 => 0,
                                1 => 1,
                                2 => 3 - b.get(1) as usize,
                                _ => 3,
                            },
                    )
                };
                self.string(sub_mb_type_b(v.min(12)), how, match_sub_mb_type_b)?
            } else {
                let how = |i: u32, _| Bin::Ctx(SUB_MB_TYPE_P + i.min(2) as usize);
                self.string(sub_mb_type_p(v.min(3)), how, match_sub_mb_type_p)?
            } as u8;
        }
        for list in 0..2 {
            for i in 0..4 {
                let (_, pred, _, _) = sub_mb_info(st, m.sub_types[i]);
                if self.ctx.num_ref_minus1(list) > 0 && m.mb_type != MbType::P(4) && uses_list(pred, list) {
                    let (x, y, w, h) = part_rect(Shape::S8x8, i);
                    m.ref_idx[list][i] = self.ref_idx(list, x, y, w, h, m.ref_idx[list][i])?;
                }
            }
        }
        for list in 0..2 {
            for i in 0..4 {
                let (n, pred, w, h) = sub_mb_info(st, m.sub_types[i]);
                if uses_list(pred, list) {
                    for j in 0..n {
                        m.mvd[list][i][j] = self.mvd(list, sub_rect(i, j, w, h), m.mvd[list][i][j])?;
                    }
                }
            }
        }
        Ok(())
    }

    /// ref_idx_lX of the partition at 4x4 block (x, y), w x h blocks (9.3.3.1.1.6).
    fn ref_idx(&mut self, list: usize, x: usize, y: usize, w: usize, h: usize, v: u8) -> Result<u8> {
        let bit = |n: &Nb, x, y| (n.ref_gt0[list] >> blk8(x, y)) & 1 == 1;
        let a = if x > 0 { bit(&self.cur, x - 1, y) } else { self.left.as_ref().is_some_and(|n| bit(n, 3, y)) };
        let b = if y > 0 { bit(&self.cur, x, y - 1) } else { self.top.as_ref().is_some_and(|n| bit(n, x, 3)) };
        let inc = a as usize + 2 * b as usize;
        let max = self.ctx.num_ref_minus1(list);
        self.se(["ref_idx_l0", "ref_idx_l1"][list]);
        let v = self.unary(v as u32, max, |i| REF_IDX + [inc, 4, 5][i.min(2) as usize])?;
        if v > 0 {
            for yy in (y..y + h).step_by(2) {
                for xx in (x..x + w).step_by(2) {
                    self.cur.ref_gt0[list] |= 1 << blk8(xx, yy);
                }
            }
        }
        Ok(v as u8)
    }

    /// mvd_lX of the (sub-)partition `rect` (9.3.3.1.1.7): UEG3, signedValFlag 1, uCoff 9.
    fn mvd(&mut self, list: usize, (x, y, w, h): (usize, usize, usize, usize), v: Mvd) -> Result<Mvd> {
        let mut d = [0; 2];
        for comp in 0..2 {
            let a = if x > 0 { self.cur.mvd[list][y * 4 + x - 1][comp] } else { self.left.map_or(0, |n| n.mvd[list][y * 4 + 3][comp]) };
            let b = if y > 0 { self.cur.mvd[list][(y - 1) * 4 + x][comp] } else { self.top.map_or(0, |n| n.mvd[list][12 + x][comp]) };
            let sum = a as u32 + b as u32;
            let inc = if sum > 32 {
                2
            } else if sum > 2 {
                1
            } else {
                0
            };
            let off = MVD[comp];
            self.se([["mvd_l0 x", "mvd_l0 y"], ["mvd_l1 x", "mvd_l1 y"]][list][comp]);
            let how = move |i: u32, b: Bins| {
                if i < 9 && b.bits.trailing_ones() >= i { Bin::Ctx(off + [inc, 3, 4, 5, 6, 6, 6, 6, 6][i as usize]) } else { Bin::Bypass }
            };
            d[comp] = self.string(ueg(v[comp] as i32, 3, true, 9), how, |b| match_ueg(b, 3, true, 9))?;
        }
        let d = to_mvd(d)?;
        let abs = [d[0].unsigned_abs().min(255) as u8, d[1].unsigned_abs().min(255) as u8];
        for yy in y..y + h {
            for xx in x..x + w {
                self.cur.mvd[list][yy * 4 + xx] = abs;
            }
        }
        Ok(d)
    }

    /// coded_block_pattern (9.3.2.6, 9.3.3.1.1.4)
    fn coded_block_pattern(&mut self, v: u8) -> Result<u8> {
        self.se("coded_block_pattern");
        let mut luma = 0u8;
        for b8 in 0..4 {
            // condTermFlagN is 1 when the neighbouring 8x8 block has no coded residual
            let a =
                if b8 % 2 == 1 { (luma >> (b8 - 1)) & 1 == 0 } else { self.left.is_some_and(|n| !n.pcm && (n.cbp >> (b8 + 1)) & 1 == 0) };
            let b = if b8 >= 2 { (luma >> (b8 - 2)) & 1 == 0 } else { self.top.is_some_and(|n| !n.pcm && (n.cbp >> (b8 + 2)) & 1 == 0) };
            let bin = self.flag(CBP_LUMA + a as usize + 2 * b as usize, (v >> b8) & 1 == 1);
            luma |= (bin as u8) << b8;
        }
        if !matches!(self.ctx.chroma_array_type, 1 | 2) {
            return Ok(luma);
        }
        let cond = |n: &Option<Nb>, bin: usize| n.is_some_and(|n| n.pcm || if bin == 0 { n.cbp >> 4 != 0 } else { n.cbp >> 4 == 2 });
        let inc = [0, 1].map(|bin| cond(&self.left, bin) as usize + 2 * cond(&self.top, bin) as usize + 4 * bin);
        let how = move |i: u32, _| Bin::Ctx(CBP_CHROMA + inc[i.min(1) as usize]);
        let chroma = self.string(truncated_unary((v >> 4).min(2) as u32, 2), how, |b| match_truncated_unary(b, 2))?;
        Ok(luma | (chroma as u8) << 4)
    }

    /// mb_qp_delta (9.3.2.7, 9.3.3.1.1.5)
    fn mb_qp_delta(&mut self, d: i32) -> Result<i32> {
        let range = 26 + self.ctx.qp_bd_offset() / 2;
        let mapped = if d > 0 { 2 * d - 1 } else { -2 * d } as u32;
        let inc = self.prev_qp_delta as usize;
        let k = self.unary(mapped, 2 * range as u32, |i| MB_QP_DELTA + [inc, 2, 3][i.min(2) as usize])?;
        let d = if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) };
        if d < -range || d > range - 1 {
            bail!("mb_qp_delta {d} out of range");
        }
        Ok(d)
    }

    /// residual() (7.3.5.3): reads into `out` or writes the blocks of `src`.
    fn residual(&mut self, m: &Macroblock, src: &[CoeffBlock], out: &mut Vec<CoeffBlock>) -> Result<()> {
        let ctx = self.ctx;
        let i16 = matches!(m.mb_type, MbType::I16x16(_));
        for p in 0..ctx.luma_planes() {
            // ctxBlockCat of DC, AC, 4x4 and 8x8 blocks of this plane (Table 9-42)
            let [cat_dc, cat_ac, cat_4x4, cat_8x8] = [[0, 1, 2, 5], [6, 7, 8, 9], [10, 11, 12, 13]][p];
            if i16 {
                let inc = self.cbf_cond(self.left.as_ref(), |n| (n.cbf_dc >> p) & 1 == 1)
                    + 2 * self.cbf_cond(self.top.as_ref(), |n| (n.cbf_dc >> p) & 1 == 1);
                let blk = Blk::LumaDc(p as u8);
                if self.block_4x4(cat_dc, 16, Some(inc), blk, src, out)? {
                    self.cur.cbf_dc |= 1 << p;
                }
            }
            for b8 in 0..4 {
                if m.cbp & (1 << b8) == 0 {
                    continue;
                }
                if m.transform_8x8 {
                    let (x, y) = (2 * (b8 % 2), 2 * (b8 / 2));
                    let inc = (ctx.chroma_array_type == 3).then(|| {
                        let t8 = |n: &Nb, x: usize, y: usize| n.transform_8x8 && (n.cbf[p] >> (y * 4 + x)) & 1 == 1;
                        let a = if x > 0 { t8(&self.cur, x - 1, y) as usize } else { self.cbf_cond(self.left.as_ref(), |n| t8(n, 3, y)) };
                        let b = if y > 0 { t8(&self.cur, x, y - 1) as usize } else { self.cbf_cond(self.top.as_ref(), |n| t8(n, x, 3)) };
                        a + 2 * b
                    });
                    if self.block_8x8(cat_8x8, inc, p as u8, b8 as u8, src, out)? {
                        self.cur.cbf[p] |= 0x33 << (y * 4 + x);
                    }
                } else {
                    for (blk, &(x, y)) in LUMA4X4_XY.iter().enumerate().skip(4 * b8).take(4) {
                        let bit = |n: &Nb, x: usize, y: usize| (n.cbf[p] >> (y * 4 + x)) & 1 == 1;
                        let a = if x > 0 { bit(&self.cur, x - 1, y) as usize } else { self.cbf_cond(self.left.as_ref(), |n| bit(n, 3, y)) };
                        let b = if y > 0 { bit(&self.cur, x, y - 1) as usize } else { self.cbf_cond(self.top.as_ref(), |n| bit(n, x, 3)) };
                        let (cat, n) = if i16 { (cat_ac, 15) } else { (cat_4x4, 16) };
                        if self.block_4x4(cat, n, Some(a + 2 * b), Blk::Luma(p as u8, blk as u8), src, out)? {
                            self.cur.cbf[p] |= 1 << (y * 4 + x);
                        }
                    }
                }
            }
        }
        if matches!(ctx.chroma_array_type, 1 | 2) {
            let blocks = ctx.chroma_blocks();
            let cbp_chroma = m.cbp >> 4;
            if cbp_chroma != 0 {
                for c in 0..2 {
                    let bit = |n: &Nb| (n.cbf_dc >> (3 + c)) & 1 == 1;
                    let inc = self.cbf_cond(self.left.as_ref(), bit) + 2 * self.cbf_cond(self.top.as_ref(), bit);
                    if self.block_4x4(3, blocks, Some(inc), Blk::ChromaDc(c as u8), src, out)? {
                        self.cur.cbf_dc |= 1 << (3 + c);
                    }
                }
            }
            if cbp_chroma & 2 != 0 {
                let rows = blocks / 2;
                for c in 0..2 {
                    for blk in 0..blocks {
                        let (x, y) = (blk % 2, blk / 2);
                        let bit = |n: &Nb, x: usize, y: usize| (n.cbf_ac[c] >> (y * 2 + x)) & 1 == 1;
                        let a = if x > 0 { bit(&self.cur, 0, y) as usize } else { self.cbf_cond(self.left.as_ref(), |n| bit(n, 1, y)) };
                        let b = if y > 0 {
                            bit(&self.cur, x, y - 1) as usize
                        } else {
                            self.cbf_cond(self.top.as_ref(), |n| bit(n, x, rows - 1))
                        };
                        if self.block_4x4(4, 15, Some(a + 2 * b), Blk::ChromaAc(c as u8, blk as u8), src, out)? {
                            self.cur.cbf_ac[c] |= 1 << blk;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A block of at most 16 coefficients; returns coded_block_flag.
    fn block_4x4(
        &mut self,
        cat: usize,
        n: usize,
        cbf_inc: Option<usize>,
        blk: Blk,
        src: &[CoeffBlock],
        out: &mut Vec<CoeffBlock>,
    ) -> Result<bool> {
        let mut levels = [0; 16];
        if !E::READ {
            levels = *levels_of(src, blk);
        }
        let coded = self.residual_block(cat, cbf_inc, &mut levels[..n])?;
        if E::READ && coded {
            out.push(CoeffBlock { blk, levels });
        }
        Ok(coded)
    }

    /// The 64 coefficients of 8x8 block `b8` of plane `p`, held as four interleaved 4x4
    /// blocks; returns coded_block_flag.
    fn block_8x8(
        &mut self,
        cat: usize,
        cbf_inc: Option<usize>,
        p: u8,
        b8: u8,
        src: &[CoeffBlock],
        out: &mut Vec<CoeffBlock>,
    ) -> Result<bool> {
        let mut levels = [0; 64];
        if !E::READ {
            for j in 0..4 {
                for (i, &l) in levels_of(src, Blk::Luma(p, 4 * b8 + j)).iter().enumerate() {
                    levels[4 * i + j as usize] = l;
                }
            }
            if cbf_inc.is_none() && levels.iter().all(|&l| l == 0) {
                bail!("8x8 block without coefficients");
            }
        }
        let coded = self.residual_block(cat, cbf_inc, &mut levels)?;
        if E::READ && coded {
            for j in 0..4 {
                let mut l = [0; 16];
                for (i, v) in l.iter_mut().enumerate() {
                    *v = levels[4 * i + j as usize];
                }
                if l.iter().any(|&v| v != 0) {
                    out.push(CoeffBlock { blk: Blk::Luma(p, 4 * b8 + j), levels: l });
                }
            }
        }
        Ok(coded)
    }

    /// residual_block_cabac (7.3.5.3.3) over `levels` (the input when writing, zeros to
    /// fill when reading). coded_block_flag is coded with `cbf_inc` when given, otherwise
    /// inferred to be 1. Returns coded_block_flag.
    fn residual_block(&mut self, cat: usize, cbf_inc: Option<usize>, levels: &mut [i32]) -> Result<bool> {
        let n = levels.len();
        if let Some(inc) = cbf_inc {
            let any = levels.iter().any(|&l| l != 0);
            self.se("coded_block_flag");
            if !self.flag(CODED_BLOCK_FLAG[cat] + inc, any) {
                return Ok(false);
            }
        }
        let (sig, last) = (SIGNIFICANT[self.field][cat], LAST_SIGNIFICANT[self.field][cat]);
        // ctxIdxInc of the significance map (9.3.3.1.3)
        let num_c8x8 = if self.ctx.chroma_array_type == 2 { 2 } else { 1 };
        let sig_8x8 = &SIGNIFICANT_8X8[self.field];
        let incs = |i: usize| match cat {
            3 => ((i / num_c8x8).min(2), (i / num_c8x8).min(2)),
            5 | 9 | 13 => (sig_8x8[i] as usize, LAST_SIGNIFICANT_8X8[i] as usize),
            _ => (i, i),
        };
        let last_nz = levels.iter().rposition(|&l| l != 0).unwrap_or(0);
        let mut num = n;
        for (i, l) in levels.iter_mut().enumerate().take(n - 1) {
            let (si, li) = incs(i);
            self.se("significant_coeff_flag");
            if self.flag(sig + si, *l != 0) {
                if E::READ {
                    // significant; the level follows below
                    *l = 1;
                }
                self.se("last_significant_coeff_flag");
                if self.flag(last + li, i == last_nz) {
                    num = i + 1;
                    break;
                }
            }
        }
        if E::READ {
            levels[num - 1] = 1;
        }
        // levels in reverse scanning order (9.3.3.1.3)
        let abs_ctx = ABS_LEVEL[cat];
        let gt1_max = if cat == 3 { 3 } else { 4 };
        let (mut eq1, mut gt1) = (0, 0);
        for i in (0..num).rev() {
            let l = levels[i];
            if l == 0 {
                continue;
            }
            let inc0 = if gt1 != 0 { 0 } else { (1 + eq1).min(4) };
            let inc1 = 5 + gt1.min(gt1_max);
            let how = move |j: u32, _| match j {
                0 => Bin::Ctx(abs_ctx + inc0),
                1..14 => Bin::Ctx(abs_ctx + inc1),
                _ => Bin::Bypass,
            };
            let v = l.unsigned_abs().saturating_sub(1).min(i32::MAX as u32 - 1) as i32;
            self.se("coeff_abs_level_minus1");
            let Some(abs) = self.string(ueg(v, 0, false, 14), how, |b| match_ueg(b, 0, false, 14))?.checked_add(1) else {
                bail!("coeff_abs_level_minus1 out of range");
            };
            self.se("coeff_sign_flag");
            let neg = self.e.bypass(l < 0);
            levels[i] = if neg { -abs } else { abs };
            if abs == 1 {
                eq1 += 1;
            } else {
                gt1 += 1;
            }
        }
        Ok(true)
    }
}

/// Parse slice_data() of a CABAC slice.
pub(crate) fn parse_slice_data(slice: &Slice, ctx: &MbContext) -> Result<SliceData> {
    let mut c = Coder::new(Reader::new(slice)?, ctx)?;
    let mbs = read_mbs(&mut c, slice)?;
    // the arithmetic code word ends with the rbsp_stop_one_bit; then alignment bits and
    // cabac_zero_words (0x0000) up to the end of the RBSP. The alignment bits are not
    // checked: x264 sets the last one in about half of its frames.
    let (buf, _, end) = slice.slice_data();
    let stop = c.e.pos();
    if stop > end || !BitReader::at(buf, stop - 1).flag()? {
        bail!("slice data does not end with the rbsp_stop_one_bit");
    }
    if buf[stop.div_ceil(8)..end / 8].iter().any(|&b| b != 0) {
        bail!("unexpected data after the end of the slice");
    }
    Ok(SliceData { first_mb: ctx.header.first_mb_in_slice as usize, mbs })
}

/// The macroblocks up to end_of_slice_flag.
fn read_mbs<E: Engine>(c: &mut Coder<E>, slice: &Slice) -> Result<Vec<Macroblock>> {
    let ctx = c.ctx;
    let end = slice.slice_data().2;
    let mut addr = ctx.header.first_mb_in_slice as usize;
    let mut mbs = Vec::with_capacity(ctx.pic_size.saturating_sub(addr));
    loop {
        if addr >= ctx.pic_size {
            bail!("slice data runs past the end of the picture");
        }
        let mut m = Macroblock::new(MbType::Skip, c.qp);
        c.macroblock(addr, &mut m, None).map_err(|e| e.context(format!("macroblock {addr}")))?;
        mbs.push(m);
        if c.e.pos() > end {
            bail!("slice data overran the end of the slice at macroblock {addr}");
        }
        addr += 1;
        c.se("end_of_slice_flag");
        if c.e.terminate(false) {
            return Ok(mbs);
        }
    }
}

/// Code the macroblocks of `data`, each followed by end_of_slice_flag.
fn write_mbs<E: Engine>(c: &mut Coder<E>, data: &SliceData) -> Result<()> {
    let ctx = c.ctx;
    if data.first_mb != ctx.header.first_mb_in_slice as usize {
        bail!("slice data does not start at first_mb_in_slice");
    }
    if data.mbs.is_empty() || data.first_mb + data.mbs.len() > ctx.pic_size {
        bail!("slice data has no or more macroblocks than the picture");
    }
    for (i, src) in data.mbs.iter().enumerate() {
        let addr = data.first_mb + i;
        let mut m = header_copy(src);
        c.macroblock(addr, &mut m, Some(src)).map_err(|e| e.context(format!("macroblock {addr}")))?;
        c.se("end_of_slice_flag");
        c.e.terminate(i + 1 == data.mbs.len());
    }
    Ok(())
}

/// Serialize the slice data of a CABAC slice, through the rbsp_stop_one_bit, alignment
/// and cabac_zero_words. When the arithmetic code word comes out as in the slice's
/// current data, the rest is kept as it was too; otherwise the alignment bits are zero
/// and the cabac_zero_words are those of the current data, or more if the bin count
/// needs them.
pub(crate) fn write_slice_data(slice: &Slice, ctx: &MbContext, data: &SliceData) -> Result<(Vec<u8>, usize)> {
    let mut c = Coder::new(Writer(Encoder::new()), ctx)?;
    write_mbs(&mut c, data)?;
    let bins = c.e.0.bin_count();
    let code_bits = c.e.0.writer().bit_len();
    let code = c.e.0.finish().into_bytes();
    let (orig, start, end) = slice.slice_data();
    let orig = &orig[start / 8..end / 8];
    let (whole, part) = (code_bits / 8, code_bits % 8);
    let same =
        orig.len() * 8 >= code_bits && orig[..whole] == code[..whole] && (part == 0 || (orig[whole] ^ code[whole]) >> (8 - part) == 0);
    let mut w = BitWriter::new();
    w.append_bits(&code, 0, code_bits);
    if same {
        w.append_bits(orig, code_bits, orig.len() * 8);
    } else {
        while !w.byte_aligned() {
            w.flag(false);
        }
        let orig_words = orig.iter().rev().take_while(|&&b| b == 0).count() / 2;
        let words = orig_words.max(zero_words_needed(ctx, data.mbs.len(), bins, w.bit_len() / 8));
        for _ in 0..words {
            w.u(16, 0);
        }
    }
    let bits = w.bit_len();
    Ok((w.into_bytes(), bits))
}

/// Every bin of a CABAC slice, as read from it, or as written back from the parsed
/// macroblocks (`written`). The bins up to an error are returned with it.
pub fn bin_trace(slice: &Slice, written: bool) -> (Vec<TracedBin>, Result<()>) {
    let mut bins = Vec::new();
    let result = (|| {
        let ctx = MbContext::new(slice)?;
        if !ctx.pps.entropy_coding_mode_flag {
            bail!("not a CABAC slice");
        }
        if written {
            let data = parse_slice_data(slice, &ctx)?;
            let e = Traced { e: Writer(Encoder::new()), addr: 0, element: "", bins: &mut bins };
            write_mbs(&mut Coder::new(e, &ctx)?, &data)
        } else {
            let e = Traced { e: Reader::new(slice)?, addr: 0, element: "", bins: &mut bins };
            read_mbs(&mut Coder::new(e, &ctx)?, slice).map(|_| ())
        }
    })();
    (bins, result)
}

/// The macroblock without its residual blocks and PCM samples.
fn header_copy(mb: &Macroblock) -> Macroblock {
    Macroblock {
        mb_type: mb.mb_type,
        transform_8x8: mb.transform_8x8,
        intra_modes: mb.intra_modes,
        intra_chroma_pred_mode: mb.intra_chroma_pred_mode,
        sub_types: mb.sub_types,
        ref_idx: mb.ref_idx,
        mvd: mb.mvd,
        cbp: mb.cbp,
        qp: mb.qp,
        coeffs: Vec::new(),
        pcm: None,
    }
}

/// cabac_zero_words that keep this slice within the bin limit of 7.4.2.10 on its own
/// (9.3.4.6 applied per slice: if every slice of a picture keeps it, the picture does).
fn zero_words_needed(ctx: &MbContext, mbs: usize, bins: u64, data_bytes: usize) -> usize {
    let mb_width_c = if ctx.chroma_array_type == 0 { 0 } else { 16 / if ctx.chroma_array_type == 3 { 1 } else { 2 } };
    let mb_height_c = if ctx.chroma_array_type == 0 { 0 } else { 16 / if ctx.chroma_array_type == 1 { 2 } else { 1 } };
    let raw_mb_bits =
        256 * (ctx.sps.bit_depth_luma_minus8 as i64 + 8) + 2 * mb_width_c * mb_height_c * (ctx.sps.bit_depth_chroma_minus8 as i64 + 8);
    // only the NAL header byte and the slice data: the slice header may still change
    // (frame renumbering), and leaving it and emulation prevention out errs on the safe side
    let bytes = 1 + data_bytes as i64;
    let ceil_div = |a: i64, b: i64| -((-a).div_euclid(b));
    ceil_div(ceil_div(3 * (32 * bins as i64 - raw_mb_bits * mbs as i64), 1024) - bytes, 3).max(0) as usize
}
