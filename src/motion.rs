//! Motion vector prediction for P slices (8.4.1): turning coded mvd values into
//! actual motion vectors and back. Vectors are in quarter luma samples.

use crate::mb::{Macroblock, MbContext, MbType, RowCache, Shape, SliceData, sub_mb_info, to_mvd};
use crate::slice::SliceType;
use anyhow::{Result, bail};

pub type Mv = [i32; 2];

/// Decoded L0 motion of one macroblock, per 4x4 block in raster order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbMotion {
    pub mv: [Mv; 16],
    /// reference index, -1 for intra
    pub ref_idx: [i8; 16],
}

impl MbMotion {
    pub const INTRA: MbMotion = MbMotion { mv: [[0; 2]; 16], ref_idx: [-1; 16] };

    pub fn is_intra(&self) -> bool {
        self.ref_idx[0] < 0
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
    pub ref_idx: u8,
    shape: Shape,
}

/// The partitions of a P macroblock (P_L0_16x16 .. P_8x8ref0).
pub fn partitions(mb: &Macroblock) -> Vec<Partition> {
    let p = |x, y, w, h, part, sub, ref_idx, shape| Partition { x, y, w, h, part, sub, ref_idx, shape };
    match mb.mb_type {
        MbType::P(0) => vec![p(0, 0, 16, 16, 0, 0, mb.ref_idx[0][0], Shape::S16x16)],
        MbType::P(1) => vec![p(0, 0, 16, 8, 0, 0, mb.ref_idx[0][0], Shape::S16x8), p(0, 8, 16, 8, 1, 0, mb.ref_idx[0][1], Shape::S16x8)],
        MbType::P(2) => vec![p(0, 0, 8, 16, 0, 0, mb.ref_idx[0][0], Shape::S8x16), p(8, 0, 8, 16, 1, 0, mb.ref_idx[0][1], Shape::S8x16)],
        MbType::P(t @ (3 | 4)) => {
            let mut v = Vec::new();
            for i in 0..4 {
                let (n, _, w, h) = sub_mb_info(SliceType::P, mb.sub_types[i]);
                let ref_idx = if t == 4 { 0 } else { mb.ref_idx[0][i] };
                for j in 0..n {
                    let (sx, sy) = (8 * (i % 2), 8 * (i / 2));
                    let (dx, dy) = if w == 8 {
                        (0, j * h)
                    } else if h == 8 {
                        (j * w, 0)
                    } else {
                        ((j % 2) * 4, (j / 2) * 4)
                    };
                    v.push(p(sx + dx, sy + dy, w, h, i, j, ref_idx, Shape::S8x8));
                }
            }
            v
        }
        _ => Vec::new(),
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

    /// Motion at luma location (x, y) relative to the current macroblock (6.4.12);
    /// None if that macroblock/partition is not available.
    fn at(&self, x: i32, y: i32) -> Option<(Mv, i8)> {
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
                return self.cur_decoded[blk].then(|| (self.cur.mv[blk], self.cur.ref_idx[blk]));
            } else {
                return None;
            }
        } else {
            return None;
        };
        let blk = (yw / 4 * 4 + xw / 4) as usize;
        Some((motion.mv[blk], motion.ref_idx[blk]))
    }

    /// mvpLX for a partition (8.4.1.3).
    fn mvp(&self, p: &Partition) -> Mv {
        let (x, y, w) = (p.x as i32, p.y as i32, p.w as i32);
        let refi = p.ref_idx as i8;
        let a = self.at(x - 1, y);
        let b = self.at(x, y - 1);
        let c = self.at(x + w, y - 1).or_else(|| self.at(x - 1, y - 1));
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
        match (self.at(-1, 0), self.at(0, -1)) {
            (Some(a), Some(b)) if !zero(a) && !zero(b) => {
                self.mvp(&Partition { x: 0, y: 0, w: 16, h: 16, part: 0, sub: 0, ref_idx: 0, shape: Shape::S16x16 })
            }
            _ => [0, 0],
        }
    }

    fn set(&mut self, x: usize, y: usize, w: usize, h: usize, mv: Mv, ref_idx: i8) {
        for by in y / 4..(y + h) / 4 {
            for bx in x / 4..(x + w) / 4 {
                self.cur.mv[by * 4 + bx] = mv;
                self.cur.ref_idx[by * 4 + bx] = ref_idx;
                self.cur_decoded[by * 4 + bx] = true;
            }
        }
    }
}

fn median(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

fn check_p(ctx: &MbContext) -> Result<()> {
    if ctx.slice_type != SliceType::P {
        bail!("motion vector editing is only supported in P slices");
    }
    Ok(())
}

/// Actual motion of every macroblock of a P slice.
pub fn decode_motion(ctx: &MbContext, data: &SliceData) -> Result<Vec<MbMotion>> {
    check_p(ctx)?;
    let mut pr = Predictor::new(ctx);
    let mut out = Vec::with_capacity(data.mbs.len());
    for (i, mb) in data.mbs.iter().enumerate() {
        pr.begin(data.first_mb + i);
        match mb.mb_type {
            MbType::Skip => {
                let mv = pr.skip_mv();
                pr.set(0, 0, 16, 16, mv, 0);
            }
            MbType::P(_) => {
                for p in partitions(mb) {
                    let mvp = pr.mvp(&p);
                    let d = mb.mvd[0][p.part][p.sub];
                    pr.set(p.x, p.y, p.w, p.h, [mvp[0] + d[0] as i32, mvp[1] + d[1] as i32], p.ref_idx as i8);
                }
            }
            _ => {}
        }
        out.push(pr.finish());
    }
    Ok(out)
}

/// Re-code the motion of a P slice so that it decodes to `target`. For skipped
/// macroblocks `None` means "whatever the prediction gives"; a skipped macroblock
/// whose target differs from its predicted motion becomes a P_L0_16x16 without residual.
/// Inter macroblocks take the motion of the first 4x4 block of each partition.
pub fn encode_motion(ctx: &MbContext, data: &mut SliceData, target: &[Option<MbMotion>]) -> Result<()> {
    check_p(ctx)?;
    if target.len() != data.mbs.len() {
        bail!("motion target has {} macroblocks, slice has {}", target.len(), data.mbs.len());
    }
    let mut pr = Predictor::new(ctx);
    for (i, mb) in data.mbs.iter_mut().enumerate() {
        pr.begin(data.first_mb + i);
        match (mb.mb_type, &target[i]) {
            (MbType::Skip, None) => {
                let mv = pr.skip_mv();
                pr.set(0, 0, 16, 16, mv, 0);
            }
            (MbType::Skip, Some(t)) => {
                let mv = t.mv[0];
                if pr.skip_mv() != mv {
                    *mb = Macroblock::new(MbType::P(0), mb.qp);
                    let p = &partitions(mb)[0];
                    let mvp = pr.mvp(p);
                    mb.mvd[0][0][0] = to_mvd([mv[0] - mvp[0], mv[1] - mvp[1]])?;
                }
                pr.set(0, 0, 16, 16, mv, 0);
            }
            (MbType::P(_), Some(t)) => {
                for p in partitions(mb) {
                    let mv = t.mv[p.y / 4 * 4 + p.x / 4];
                    let mvp = pr.mvp(&p);
                    mb.mvd[0][p.part][p.sub] = to_mvd([mv[0] - mvp[0], mv[1] - mvp[1]])?;
                    pr.set(p.x, p.y, p.w, p.h, mv, p.ref_idx as i8);
                }
            }
            (MbType::P(_), None) => bail!("no motion target for inter macroblock {}", data.first_mb + i),
            _ => {}
        }
        pr.finish();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::median;

    #[test]
    fn median_of_three() {
        assert_eq!(median(1, 2, 3), 2);
        assert_eq!(median(3, 1, 2), 2);
        assert_eq!(median(-5, 7, 0), 0);
        assert_eq!(median(4, 4, -1), 4);
    }
}
