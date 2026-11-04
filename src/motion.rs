//! Motion vector prediction (8.4.1): turning coded mvd values into the motion the
//! decoder uses and back, in P and B slices of frames. Vectors are in quarter luma
//! samples. B slices also depend on other pictures: direct prediction (8.4.1.2) takes
//! the reference lists and the motion of the first picture of list 1, see
//! [`BContext`] and [`Fields`].

use crate::mb::{self, Macroblock, MbContext, MbType, Pred, RowCache, Shape, SliceData, sub_mb_info, to_mvd, uses_list};
use crate::refs::{PicRefs, RefLists, RefPic};
use crate::slice::SliceType;
use crate::stream::AccessUnit;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::sync::Arc;

pub type Mv = [i32; 2];

/// The motion of one block: (vector, reference index) for lists 0 and 1; reference
/// index -1 (and a zero vector) where a list is not used.
type BlockMotion = [(Mv, i8); 2];

const UNUSED: (Mv, i8) = ([0, 0], -1);

/// Decoded motion of one macroblock, per list and 4x4 block in raster order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbMotion {
    /// vectors in the 16 bits they fit in (8.4.1: at most -2048..2047.75 samples)
    mv: [[[i16; 2]; 16]; 2],
    /// reference index per list, -1 where the list is not used (both lists for intra)
    pub ref_idx: [[i8; 16]; 2],
}

impl MbMotion {
    pub const INTRA: MbMotion = MbMotion { mv: [[[0; 2]; 16]; 2], ref_idx: [[-1; 16]; 2] };

    pub fn is_intra(&self) -> bool {
        self.ref_idx[0][0] < 0 && self.ref_idx[1][0] < 0
    }

    /// The vector of block `b` in list `list`.
    pub fn mv(&self, list: usize, b: usize) -> Mv {
        let [x, y] = self.mv[list][b];
        [x as i32, y as i32]
    }

    /// Set the vector of block `b` in list `list` (out-of-range values saturate).
    pub fn set_mv(&mut self, list: usize, b: usize, mv: Mv) {
        let sat = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        self.mv[list][b] = [sat(mv[0]), sat(mv[1])];
    }

    /// Apply `f` to every vector in use.
    pub fn map_mvs(&mut self, mut f: impl FnMut(Mv) -> Mv) {
        for l in 0..2 {
            for b in 0..16 {
                if self.ref_idx[l][b] >= 0 {
                    self.set_mv(l, b, f(self.mv(l, b)));
                }
            }
        }
    }

    fn block(&self, b: usize) -> BlockMotion {
        [(self.mv(0, b), self.ref_idx[0][b]), (self.mv(1, b), self.ref_idx[1][b])]
    }

    fn set_block(&mut self, b: usize, m: BlockMotion) {
        for (l, (mv, r)) in m.into_iter().enumerate() {
            self.set_mv(l, b, if r >= 0 { mv } else { [0, 0] });
            self.ref_idx[l][b] = r;
        }
    }
}

/// One motion partition of a macroblock in decoding order.
#[derive(Clone, Copy, Debug)]
pub struct Partition {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    /// mbPartIdx and subMbPartIdx (index into Macroblock::mvd)
    pub part: usize,
    pub sub: usize,
    /// Direct for the direct sub-macroblocks of B_8x8
    pub pred: Pred,
    pub ref_idx: [u8; 2],
    shape: Shape,
}

impl Partition {
    fn blocks(&self) -> impl Iterator<Item = usize> + use<> {
        let (x, y, w, h) = (self.x / 4, self.y / 4, self.w / 4, self.h / 4);
        (y..y + h).flat_map(move |by| (x..x + w).map(move |bx| by * 4 + bx))
    }

    fn first_block(&self) -> usize {
        self.y / 4 * 4 + self.x / 4
    }
}

/// The 4x4 blocks (raster order) of 8x8 quadrant `q`.
fn quadrant(q: usize) -> [usize; 4] {
    let b = q / 2 * 8 + q % 2 * 2;
    [b, b + 1, b + 4, b + 5]
}

/// The partitions of an inter macroblock: one or two of a 16x16 macroblock, or those
/// of each sub-macroblock (a direct sub-macroblock of B_8x8 as one 8x8 partition).
/// None for skipped, B_Direct_16x16 and intra macroblocks.
pub fn partitions(mb: &Macroblock) -> Vec<Partition> {
    let p = |x, y, w, h, part, sub, pred, shape| Partition {
        x,
        y,
        w,
        h,
        part,
        sub,
        pred,
        ref_idx: [mb.ref_idx[0][part], mb.ref_idx[1][part]],
        shape,
    };
    let t = mb.mb_type;
    match t {
        MbType::P(0) | MbType::B(1..=3) => vec![p(0, 0, 16, 16, 0, 0, t.part_pred(0), Shape::S16x16)],
        MbType::P(1 | 2) | MbType::B(4..=21) if t.shape() == Shape::S16x8 => {
            vec![p(0, 0, 16, 8, 0, 0, t.part_pred(0), Shape::S16x8), p(0, 8, 16, 8, 1, 0, t.part_pred(1), Shape::S16x8)]
        }
        MbType::P(1 | 2) | MbType::B(4..=21) => {
            vec![p(0, 0, 8, 16, 0, 0, t.part_pred(0), Shape::S8x16), p(8, 0, 8, 16, 1, 0, t.part_pred(1), Shape::S8x16)]
        }
        MbType::P(3 | 4) | MbType::B(22) => {
            let slice_type = if t == MbType::B(22) { SliceType::B } else { SliceType::P };
            let mut v = Vec::new();
            for i in 0..4 {
                let (n, pred, w, h) = sub_mb_info(slice_type, mb.sub_types[i]);
                let (sx, sy) = (8 * (i % 2), 8 * (i / 2));
                if pred == Pred::Direct {
                    v.push(p(sx, sy, 8, 8, i, 0, pred, Shape::S8x8));
                    continue;
                }
                for j in 0..n {
                    let (dx, dy) = if w == 8 {
                        (0, j * h)
                    } else if h == 8 {
                        (j * w, 0)
                    } else {
                        ((j % 2) * 4, (j / 2) * 4)
                    };
                    let mut part = p(sx + dx, sy + dy, w, h, i, j, pred, Shape::S8x8);
                    if t == MbType::P(4) {
                        part.ref_idx[0] = 0;
                    }
                    v.push(part);
                }
            }
            v
        }
        _ => Vec::new(),
    }
}

/// What the motion of a B slice depends on beyond the slice: its reference lists, the
/// picture order count of its picture and the motion of the first picture of list 1
/// (the colocated picture, 8.4.1.2.1).
#[derive(Clone, Debug)]
pub struct BContext {
    pub lists: RefLists,
    pub poc: i64,
    pub col: Arc<PicMotion>,
}

/// The contexts of a B slice whose motion is edited: effects change the motion it has
/// before any frame is edited (`unedited`), and the result is coded against the
/// pictures it predicts from as they are edited (`edited`).
#[derive(Clone, Debug)]
pub struct BEdit {
    pub unedited: BContext,
    pub edited: BContext,
}

impl BContext {
    fn list1_first(&self) -> Result<RefPic> {
        self.lists[1].first().copied().flatten().context("list 1 of the B slice has no picture")
    }

    /// The colocated block of block `blk` of macroblock `addr`: (mvCol, refIdxCol, the
    /// picture refIdxCol stands for), with refIdxCol -1 for intra.
    fn col_block(&self, ctx: &MbContext, addr: usize, blk: usize) -> (Mv, i8, Option<RefPic>) {
        let blk = if ctx.sps.direct_8x8_inference_flag { [0, 3, 12, 15][blk / 8 * 2 + blk % 4 / 2] } else { blk };
        let Some(m) = self.col.mbs.get(addr).filter(|m| !m.is_intra()) else { return ([0, 0], -1, None) };
        let l = if m.ref_idx[0][blk] >= 0 { 0 } else { 1 };
        let r = m.ref_idx[l][blk];
        (m.mv(l, blk), r, self.col.ref_pic(addr, l, r))
    }
}

/// The motion of a decoded picture, as a colocated picture: every macroblock, and the
/// reference lists of the slice it is in.
#[derive(Debug, Default)]
pub struct PicMotion {
    pub mbs: Vec<MbMotion>,
    slice: Vec<usize>,
    lists: Vec<RefLists>,
}

impl PicMotion {
    fn ref_pic(&self, addr: usize, list: usize, ref_idx: i8) -> Option<RefPic> {
        let lists = self.lists.get(*self.slice.get(addr)?)?;
        lists[list].get(usize::try_from(ref_idx).ok()?).copied().flatten()
    }
}

/// Neighbour state while walking a slice in decoding order.
struct Predictor {
    width: usize,
    /// motion of the neighbouring macroblocks decoded so far in this slice
    done: RowCache<MbMotion>,
    addr: usize,
    cur: MbMotion,
    cur_decoded: [bool; 16],
}

impl Predictor {
    fn new(ctx: &MbContext) -> Self {
        Self { width: ctx.width, done: RowCache::new(ctx.width), addr: 0, cur: MbMotion::INTRA, cur_decoded: [false; 16] }
    }

    fn begin(&mut self, addr: usize) {
        self.addr = addr;
        self.cur = MbMotion::INTRA;
        self.cur_decoded = [false; 16];
    }

    fn finish(&mut self) -> MbMotion {
        self.done.set(self.addr, self.cur);
        self.cur
    }

    /// Motion in list `list` at luma location (x, y) relative to the current
    /// macroblock (6.4.12); None if that macroblock/partition is not available.
    fn at(&self, x: i32, y: i32, list: usize) -> Option<(Mv, i8)> {
        let (a, w) = (self.addr, self.width);
        let mb = |addr: Option<usize>| addr.and_then(|n| self.done.get(n));
        let (motion, xw, yw) = if y < 0 {
            let top = a >= w;
            if x < 0 {
                (mb((top && a % w != 0).then(|| a - w - 1))?, x + 16, y + 16)
            } else if x < 16 {
                (mb(top.then(|| a - w))?, x, y + 16)
            } else {
                (mb((top && (a + 1) % w != 0).then(|| a - w + 1))?, x - 16, y + 16)
            }
        } else if y < 16 {
            if x < 0 {
                (mb((a % w != 0).then(|| a - 1))?, x + 16, y)
            } else if x < 16 {
                let blk = (y / 4 * 4 + x / 4) as usize;
                return self.cur_decoded[blk].then(|| (self.cur.mv(list, blk), self.cur.ref_idx[list][blk]));
            } else {
                return None;
            }
        } else {
            return None;
        };
        let blk = (yw / 4 * 4 + xw / 4) as usize;
        Some((motion.mv(list, blk), motion.ref_idx[list][blk]))
    }

    /// mvpLX for a partition (8.4.1.3).
    fn mvp(&self, p: &Partition, list: usize) -> Mv {
        let (x, y, w) = (p.x as i32, p.y as i32, p.w as i32);
        let refi = p.ref_idx[list] as i8;
        let a = self.at(x - 1, y, list);
        let b = self.at(x, y - 1, list);
        let c = self.at(x + w, y - 1, list).or_else(|| self.at(x - 1, y - 1, list));
        let unpack = |n: Option<(Mv, i8)>| n.map_or(([0, 0], -1, false), |(mv, r)| (mv, r, true));
        let (mva, ra, avail_a) = unpack(a);
        let (mut mvb, mut rb, avail_b) = unpack(b);
        let (mut mvc, mut rc, avail_c) = unpack(c);
        match p.shape {
            Shape::S16x8 if p.part == 0 && rb == refi => return mvb,
            Shape::S16x8 if p.part == 1 && ra == refi => return mva,
            Shape::S8x16 if p.part == 0 && ra == refi => return mva,
            Shape::S8x16 if p.part == 1 && rc == refi => return mvc,
            _ => {}
        }
        if !avail_b && !avail_c && avail_a {
            (mvb, rb, mvc, rc) = (mva, ra, mva, ra);
        }
        match (ra == refi, rb == refi, rc == refi) {
            (true, false, false) => mva,
            (false, true, false) => mvb,
            (false, false, true) => mvc,
            _ => [median(mva[0], mvb[0], mvc[0]), median(mva[1], mvb[1], mvc[1])],
        }
    }

    /// Motion vector of a P_Skip macroblock (8.4.1.1).
    fn skip_mv(&self) -> Mv {
        let zero = |(mv, r): (Mv, i8)| r == 0 && mv == [0, 0];
        match (self.at(-1, 0, 0), self.at(0, -1, 0)) {
            (Some(a), Some(b)) if !zero(a) && !zero(b) => self.mvp(&whole(Pred::L0, [0, 0]), 0),
            _ => [0, 0],
        }
    }

    /// Motion of every block of the current macroblock if it was direct predicted
    /// (8.4.1.2); direct sub-macroblocks of B_8x8 take their quadrant of it.
    fn direct(&self, ctx: &MbContext, b: &BContext) -> Result<MbMotion> {
        let pic1 = b.list1_first()?;
        let mut m = MbMotion::INTRA;
        if ctx.header.direct_spatial_mv_pred_flag {
            // 8.4.1.2.2: reference indices and vectors from the neighbours of the macroblock
            let mut refs = [-1i8; 2];
            for (l, r) in refs.iter_mut().enumerate() {
                let ref_of = |n: Option<(Mv, i8)>| n.map_or(-1, |(_, r)| r);
                let c = self.at(16, -1, l).or_else(|| self.at(-1, -1, l));
                *r = min_positive(ref_of(self.at(-1, 0, l)), min_positive(ref_of(self.at(0, -1, l)), ref_of(c)));
            }
            let zero = refs == [-1, -1];
            if zero {
                refs = [0, 0];
            }
            let mut mvp = [[0; 2]; 2];
            for l in 0..2 {
                if !zero && refs[l] >= 0 {
                    mvp[l] = self.mvp(&whole(Pred::Bi, [refs[0].max(0) as u8, refs[1].max(0) as u8]), l);
                }
            }
            for blk in 0..16 {
                let (mv_col, ref_col, _) = b.col_block(ctx, self.addr, blk);
                let col_zero = !pic1.long_term && ref_col == 0 && mv_col[0].abs() <= 1 && mv_col[1].abs() <= 1;
                let mut motion = [UNUSED; 2];
                for l in 0..2 {
                    if refs[l] >= 0 {
                        let mv = if zero || (refs[l] == 0 && col_zero) { [0, 0] } else { mvp[l] };
                        motion[l] = (mv, refs[l]);
                    }
                }
                m.set_block(blk, motion);
            }
        } else {
            // 8.4.1.2.3: the colocated vector, scaled by picture order count distances
            for blk in 0..16 {
                let (mv_col, ref_col, pic_col) = b.col_block(ctx, self.addr, blk);
                let ref0 = match (ref_col, pic_col) {
                    (r, _) if r < 0 => 0,
                    (_, Some(pc)) => b.lists[0]
                        .iter()
                        .position(|r| r.is_some_and(|r| r.pic == pc.pic))
                        .context("temporal direct: list 0 lacks the picture the colocated block predicts from")?,
                    (_, None) => bail!("temporal direct: the colocated block predicts from no picture"),
                };
                let pic0 = b.lists[0].get(ref0).copied().flatten().context("temporal direct: list 0 has no picture")?;
                let (mv0, mv1) = if pic0.long_term || pic1.poc == pic0.poc {
                    (mv_col, [0, 0])
                } else {
                    let tb = (b.poc - pic0.poc).clamp(-128, 127) as i32;
                    let td = (pic1.poc - pic0.poc).clamp(-128, 127) as i32;
                    let tx = (16384 + (td / 2).abs()) / td;
                    let scale = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                    let mv0 = [(scale * mv_col[0] + 128) >> 8, (scale * mv_col[1] + 128) >> 8];
                    (mv0, [mv0[0] - mv_col[0], mv0[1] - mv_col[1]])
                };
                m.set_block(blk, [(mv0, ref0 as i8), (mv1, 0)]);
            }
        }
        Ok(m)
    }

    fn set(&mut self, p: &Partition, motion: BlockMotion) {
        for b in p.blocks() {
            self.cur.set_block(b, motion);
            self.cur_decoded[b] = true;
        }
    }

    fn set_from(&mut self, blocks: impl IntoIterator<Item = usize>, m: &MbMotion) {
        for b in blocks {
            self.cur.set_block(b, m.block(b));
            self.cur_decoded[b] = true;
        }
    }
}

/// A 16x16 partition (for P_Skip and direct prediction).
fn whole(pred: Pred, ref_idx: [u8; 2]) -> Partition {
    Partition { x: 0, y: 0, w: 16, h: 16, part: 0, sub: 0, pred, ref_idx, shape: Shape::S16x16 }
}

fn median(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// MinPositive (8.4.1.2.2).
fn min_positive(x: i8, y: i8) -> i8 {
    if x >= 0 && y >= 0 { x.min(y) } else { x.max(y) }
}

/// Whether macroblock `mb` of a B slice has direct-predicted parts.
pub fn has_direct(mb: &Macroblock) -> bool {
    match mb.mb_type {
        MbType::Skip | MbType::B(0) => true,
        MbType::B(22) => mb.sub_types.contains(&0),
        _ => false,
    }
}

/// The motion of partition `p` of `mb`, its vectors predicted by `pr`.
fn partition_motion(pr: &Predictor, mb: &Macroblock, p: &Partition) -> BlockMotion {
    let mut motion = [UNUSED; 2];
    for (l, m) in motion.iter_mut().enumerate() {
        if uses_list(p.pred, l) {
            let mvp = pr.mvp(p, l);
            let d = mb.mvd[l][p.part][p.sub];
            *m = ([mvp[0] + d[0] as i32, mvp[1] + d[1] as i32], p.ref_idx[l] as i8);
        }
    }
    motion
}

/// Actual motion of every macroblock of a slice; B slices need their `b` context.
/// Intra slices have intra macroblocks only.
pub fn decode_motion(ctx: &MbContext, data: &SliceData, b: Option<&BContext>) -> Result<Vec<MbMotion>> {
    let mut pr = Predictor::new(ctx);
    let mut out = Vec::with_capacity(data.mbs.len());
    let is_b = ctx.slice_type == SliceType::B;
    for (i, mb) in data.mbs.iter().enumerate() {
        pr.begin(data.first_mb + i);
        let direct = if is_b && has_direct(mb) {
            Some(pr.direct(ctx, b.context("the motion of a B slice depends on other pictures")?)?)
        } else {
            None
        };
        match (mb.mb_type, &direct) {
            (MbType::Skip, None) => {
                let mv = pr.skip_mv();
                pr.set(&whole(Pred::L0, [0, 0]), [(mv, 0), UNUSED]);
            }
            (MbType::Skip | MbType::B(0), Some(d)) => pr.set_from(0..16, d),
            (t, _) if t.is_inter() => {
                for p in partitions(mb) {
                    match &direct {
                        Some(d) if p.pred == Pred::Direct => pr.set_from(p.blocks(), d),
                        _ => {
                            let motion = partition_motion(&pr, mb, &p);
                            pr.set(&p, motion);
                        }
                    }
                }
            }
            _ => {}
        }
        out.push(pr.finish());
    }
    Ok(out)
}

/// Whether the partitions of inter macroblock `mb` can carry the motion `t` (with other
/// vectors and reference indices): each one uses the lists its prediction mode says,
/// and direct parts already come out as `t`.
fn fits(mb: &Macroblock, t: &MbMotion, direct: Option<&MbMotion>) -> bool {
    if !mb.mb_type.is_inter() || mb.mb_type == MbType::B(0) {
        return false;
    }
    partitions(mb).iter().all(|p| match direct {
        _ if p.pred != Pred::Direct => p.blocks().all(|b| pred_of(&t.block(b)) == Some(p.pred)),
        Some(d) => p.blocks().all(|b| d.block(b) == t.block(b)),
        None => false,
    })
}

/// The block whose reference indices partition `p` takes: the first of its
/// sub-macroblock (which has one per list for all its partitions), else its own first.
fn ref_block(mb: &Macroblock, p: &Partition) -> usize {
    if matches!(mb.mb_type, MbType::P(3 | 4) | MbType::B(22)) { quadrant(p.part)[0] } else { p.first_block() }
}

fn pred_of(m: &BlockMotion) -> Option<Pred> {
    match (m[0].1 >= 0, m[1].1 >= 0) {
        (true, false) => Some(Pred::L0),
        (false, true) => Some(Pred::L1),
        (true, true) => Some(Pred::Bi),
        (false, false) => None,
    }
}

/// Give macroblock `mb` of a B slice partitions coded explicitly that carry the motion
/// `t`, as few as it takes; its residual stays. Where an 8x8 block would need smaller
/// partitions that cannot be coded (its blocks predict from different pictures, or the
/// residual uses the 8x8 transform), its first 4x4 block stands for it. The vectors are
/// coded by [`encode_motion`].
pub fn code_explicitly(ctx: &MbContext, mb: &mut Macroblock, t: &MbMotion) -> Result<()> {
    let uniform = |blocks: &[usize]| blocks.iter().all(|&b| t.block(b) == t.block(blocks[0]));
    let pred = |b: usize| pred_of(&t.block(b)).context("a block of an inter macroblock has no motion");
    let half = |blocks: [usize; 8]| uniform(&blocks);
    let (top, bottom) = ([0, 1, 2, 3, 4, 5, 6, 7], [8, 9, 10, 11, 12, 13, 14, 15]);
    let (left, right) = ([0, 1, 4, 5, 8, 9, 12, 13], [2, 3, 6, 7, 10, 11, 14, 15]);
    let two = |shape: Shape, a: usize, b: usize| -> Result<MbType> {
        let (pa, pb) = (pred(a)?, pred(b)?);
        Ok((4..=21)
            .map(MbType::B)
            .find(|t| t.shape() == shape && t.part_pred(0) == pa && t.part_pred(1) == pb)
            .expect("every pair of modes"))
    };
    let (mb_type, firsts): (MbType, Vec<usize>) = if uniform(&(0..16).collect::<Vec<_>>()) {
        let t = match pred(0)? {
            Pred::L0 => 1,
            Pred::L1 => 2,
            _ => 3,
        };
        (MbType::B(t), vec![0])
    } else if half(top) && half(bottom) {
        (two(Shape::S16x8, 0, 8)?, vec![0, 8])
    } else if half(left) && half(right) {
        (two(Shape::S8x16, 0, 2)?, vec![0, 2])
    } else {
        let mut subs = [0u8; 4];
        for (q, sub) in subs.iter_mut().enumerate() {
            let [a, b, c, d] = quadrant(q);
            let p = pred(a)?;
            let first = t.block(a);
            let one_ref = [b, c, d].iter().all(|&k| (0..2).all(|l| t.ref_idx[l][k] == first[l].1));
            let same = |x: usize, y: usize| t.block(x) == t.block(y);
            let (n, w, h) = if !one_ref || mb.transform_8x8 || (same(a, b) && same(a, c) && same(a, d)) {
                (1, 8, 8)
            } else if same(a, b) && same(c, d) {
                (2, 8, 4)
            } else if same(a, c) && same(b, d) {
                (2, 4, 8)
            } else {
                (4, 4, 4)
            };
            *sub = (1..13).find(|&s| sub_mb_info(SliceType::B, s) == (n, p, w, h)).expect("every sub-macroblock shape");
        }
        mb.sub_types = subs;
        (MbType::B(22), (0..4).map(|q| quadrant(q)[0]).collect())
    };
    if mb.mb_type == MbType::Skip {
        mb.cbp = 0;
        mb.coeffs.clear();
        mb.transform_8x8 = false;
    }
    mb.mb_type = mb_type;
    for (part, &b) in firsts.iter().enumerate() {
        for l in 0..2 {
            mb.ref_idx[l][part] = t.ref_idx[l][b].max(0) as u8;
        }
    }
    mb.mvd = Default::default();
    debug_assert!(!mb.transform_8x8 || mb::inter_transform_8x8(ctx, mb));
    Ok(())
}

/// Re-code the motion of a P or B slice so that it decodes to `target`. For skipped
/// and direct-predicted macroblocks `None` means "whatever the prediction gives"; one
/// whose target differs from its predicted motion is coded explicitly (a P_Skip as a
/// P_L0_16x16 without residual). Inter macroblocks take the motion of the first 4x4
/// block of each partition; in B slices their partitions change when they cannot carry
/// the target.
pub fn encode_motion(ctx: &MbContext, data: &mut SliceData, target: &[Option<MbMotion>], b: Option<&BContext>) -> Result<()> {
    if target.len() != data.mbs.len() {
        bail!("motion target has {} macroblocks, slice has {}", target.len(), data.mbs.len());
    }
    match ctx.slice_type {
        SliceType::P => encode_p(ctx, data, target),
        SliceType::B => encode_b(ctx, data, target, b.context("the motion of a B slice depends on other pictures")?),
        _ => bail!("motion vector editing needs a P or B slice"),
    }
}

fn encode_p(ctx: &MbContext, data: &mut SliceData, target: &[Option<MbMotion>]) -> Result<()> {
    let mut pr = Predictor::new(ctx);
    for (i, mb) in data.mbs.iter_mut().enumerate() {
        pr.begin(data.first_mb + i);
        match (mb.mb_type, &target[i]) {
            (MbType::Skip, None) => {
                let mv = pr.skip_mv();
                pr.set(&whole(Pred::L0, [0, 0]), [(mv, 0), UNUSED]);
            }
            (MbType::Skip, Some(t)) => {
                let mv = t.mv(0, 0);
                if pr.skip_mv() != mv {
                    *mb = Macroblock::new(MbType::P(0), mb.qp);
                    let p = &partitions(mb)[0];
                    let mvp = pr.mvp(p, 0);
                    mb.mvd[0][0][0] = to_mvd([mv[0] - mvp[0], mv[1] - mvp[1]])?;
                }
                pr.set(&whole(Pred::L0, [0, 0]), [(mv, 0), UNUSED]);
            }
            (MbType::P(_), Some(t)) => {
                for p in partitions(mb) {
                    let mv = t.mv(0, p.first_block());
                    let mvp = pr.mvp(&p, 0);
                    mb.mvd[0][p.part][p.sub] = to_mvd([mv[0] - mvp[0], mv[1] - mvp[1]])?;
                    pr.set(&p, [(mv, p.ref_idx[0] as i8), UNUSED]);
                }
            }
            (MbType::P(_), None) => bail!("no motion target for inter macroblock {}", data.first_mb + i),
            _ => {}
        }
        pr.finish();
    }
    Ok(())
}

fn encode_b(ctx: &MbContext, data: &mut SliceData, target: &[Option<MbMotion>], b: &BContext) -> Result<()> {
    let mut pr = Predictor::new(ctx);
    for (i, mb) in data.mbs.iter_mut().enumerate() {
        let addr = data.first_mb + i;
        pr.begin(addr);
        // temporal direct prediction fails where the colocated picture now predicts
        // from a picture list 0 does not have: such macroblocks are coded explicitly
        let direct = if has_direct(mb) { pr.direct(ctx, b).ok() } else { None };
        let mut target = target[i];
        if target.is_none() && direct.is_none() && matches!(mb.mb_type, MbType::Skip | MbType::B(0)) {
            // no motion to keep: both lists' first pictures, unmoved
            let mut t = MbMotion::INTRA;
            (0..16).for_each(|blk| t.set_block(blk, [([0, 0], 0), ([0, 0], 0)]));
            target = Some(t);
        }
        match (&target, &direct) {
            (_, Some(d)) if matches!(mb.mb_type, MbType::Skip | MbType::B(0)) && target.is_none_or(|t| t == *d) => {
                pr.set_from(0..16, d);
            }
            (None, _) if mb.mb_type.is_inter() => bail!("no motion target for inter macroblock {addr}"),
            (Some(t), _) if mb.mb_type.is_inter() || mb.mb_type == MbType::Skip => {
                if !fits(mb, t, direct.as_ref()) {
                    code_explicitly(ctx, mb, t).with_context(|| format!("macroblock {addr}"))?;
                }
                for p in partitions(mb) {
                    if p.pred == Pred::Direct {
                        pr.set_from(p.blocks(), direct.as_ref().expect("a direct sub-macroblock"));
                        continue;
                    }
                    let mut p = p;
                    let (first, refs) = (t.block(p.first_block()), t.block(ref_block(mb, &p)));
                    let mut motion = [UNUSED; 2];
                    for (l, m) in motion.iter_mut().enumerate() {
                        if uses_list(p.pred, l) {
                            let (mv, r) = (first[l].0, refs[l].1);
                            if r < 0 {
                                bail!("macroblock {addr}: a partition has no motion in a list it predicts from");
                            }
                            p.ref_idx[l] = r as u8;
                            mb.ref_idx[l][p.part] = r as u8;
                            let mvp = pr.mvp(&p, l);
                            mb.mvd[l][p.part][p.sub] = to_mvd([mv[0] - mvp[0], mv[1] - mvp[1]])?;
                            *m = (mv, r);
                        }
                    }
                    pr.set(&p, motion);
                }
            }
            _ => {}
        }
        pr.finish();
    }
    Ok(())
}

fn slice_macroblocks(s: &crate::slice::Slice) -> Result<(MbContext<'_>, SliceData)> {
    let ctx = MbContext::new(s)?;
    let data = mb::parse_slice_data(s)?;
    Ok((ctx, data))
}

/// The motion of the pictures of a stream in decoding order, worked out when asked for
/// and kept: a B picture needs the motion of its colocated pictures first.
pub struct Fields {
    refs: Vec<std::result::Result<PicRefs, String>>,
    done: HashMap<usize, std::result::Result<Arc<PicMotion>, String>>,
}

impl Fields {
    /// For the pictures `refs` describes (see [`crate::refs::follow`]).
    pub fn new(refs: Vec<std::result::Result<PicRefs, String>>) -> Self {
        Self { refs, done: HashMap::new() }
    }

    /// The colocated pictures of picture `i`: the first of list 1 of each B slice.
    pub fn colocated(&self, i: usize) -> Vec<usize> {
        let Some(Ok(refs)) = self.refs.get(i) else { return Vec::new() };
        let mut pics: Vec<usize> = refs.slices.iter().filter_map(|l| l[1].first().copied().flatten()).map(|p| p.pic).collect();
        pics.dedup();
        pics
    }

    /// Work out the motion of pictures `pics` and of the colocated pictures they need,
    /// in parallel where they do not depend on each other. `au` gives the pictures by
    /// index.
    pub fn prepare<'a>(&mut self, pics: impl IntoIterator<Item = usize>, au: &(dyn Fn(usize) -> &'a AccessUnit + Sync)) {
        let mut need = std::collections::BTreeSet::new();
        let mut stack: Vec<usize> = pics.into_iter().collect();
        while let Some(i) = stack.pop() {
            if !self.done.contains_key(&i) && need.insert(i) {
                stack.extend(self.colocated(i));
            }
        }
        while !need.is_empty() {
            let level: Vec<usize> = need.iter().copied().filter(|&i| self.colocated(i).iter().all(|c| self.done.contains_key(c))).collect();
            if level.is_empty() {
                break;
            }
            let results: Vec<_> = {
                use rayon::prelude::*;
                level.par_iter().map(|&i| (i, self.compute(i, au))).collect()
            };
            for (i, m) in results {
                need.remove(&i);
                self.done.insert(i, m);
            }
        }
    }

    /// The context of each slice of picture `i`; None for slices other than B.
    pub fn contexts<'a>(
        &mut self,
        i: usize,
        au: &(dyn Fn(usize) -> &'a AccessUnit + Sync),
    ) -> std::result::Result<Vec<Option<BContext>>, String> {
        self.prepare(self.colocated(i), au);
        self.contexts_of(i, au(i))
    }

    /// The motion of picture `i`.
    pub fn motion<'a>(&mut self, i: usize, au: &(dyn Fn(usize) -> &'a AccessUnit + Sync)) -> std::result::Result<Arc<PicMotion>, String> {
        self.prepare([i], au);
        self.done.get(&i).cloned().unwrap_or_else(|| Err("the motion of the picture could not be worked out".into()))
    }

    /// Like [`Fields::contexts`], with the colocated pictures' motion worked out.
    fn contexts_of(&self, i: usize, au: &AccessUnit) -> std::result::Result<Vec<Option<BContext>>, String> {
        let refs = self.refs.get(i).ok_or("the picture is not in the stream")?.clone()?;
        let mut out = Vec::new();
        for (k, s) in au.slices().enumerate() {
            if s.header.slice_type() != SliceType::B {
                out.push(None);
                continue;
            }
            let lists = refs.slices.get(k).cloned().ok_or("the picture has more slices than reference lists")?;
            let Some(col) = lists[1].first().copied().flatten() else { return Err("list 1 of a B slice has no picture".into()) };
            let col = self.done.get(&col.pic).cloned().unwrap_or_else(|| Err("the colocated picture's motion is missing".into()))?;
            out.push(Some(BContext { lists, poc: refs.poc, col }));
        }
        Ok(out)
    }

    /// The motion of picture `i`, whose colocated pictures' motion is worked out.
    fn compute<'a>(&self, i: usize, au: &dyn Fn(usize) -> &'a AccessUnit) -> std::result::Result<Arc<PicMotion>, String> {
        let au = au(i);
        let contexts = self.contexts_of(i, au)?;
        let refs = self.refs[i].as_ref().map_err(Clone::clone)?;
        let mut m = PicMotion { mbs: Vec::new(), slice: Vec::new(), lists: refs.slices.clone() };
        for (k, (s, b)) in au.slices().zip(&contexts).enumerate() {
            let (ctx, data) = slice_macroblocks(s).map_err(|e| format!("{e:#}"))?;
            let motion = decode_motion(&ctx, &data, b.as_ref()).map_err(|e| format!("{e:#}"))?;
            if m.mbs.len() < ctx.pic_size {
                m.mbs.resize(ctx.pic_size, MbMotion::INTRA);
                m.slice.resize(ctx.pic_size, usize::MAX);
            }
            for (j, mm) in motion.into_iter().enumerate() {
                m.mbs[data.first_mb + j] = mm;
                m.slice[data.first_mb + j] = k;
            }
        }
        Ok(Arc::new(m))
    }

    /// Forget the motion of picture `i`, whose slices changed.
    pub fn forget(&mut self, i: usize) {
        self.done.remove(&i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_three() {
        assert_eq!(median(1, 2, 3), 2);
        assert_eq!(median(3, 1, 2), 2);
        assert_eq!(median(-5, 7, 0), 0);
        assert_eq!(median(4, 4, -1), 4);
    }

    #[test]
    fn min_positive_prefers_the_smaller_reference() {
        assert_eq!(min_positive(2, 1), 1);
        assert_eq!(min_positive(-1, 3), 3);
        assert_eq!(min_positive(-1, -1), -1);
    }

    #[test]
    fn quadrants_cover_the_macroblock() {
        let mut all: Vec<usize> = (0..4).flat_map(quadrant).collect();
        all.sort();
        assert_eq!(all, (0..16).collect::<Vec<_>>());
        assert_eq!(quadrant(3), [10, 11, 14, 15]);
    }
}
