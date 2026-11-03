//! Intra prediction directions: turning the coded values into actual directions and
//! back (8.3.1.1, 8.3.2.1). Intra 4x4/8x8 directions are coded relative to a
//! prediction from the neighbouring blocks, so re-coding a slice keeps every
//! direction that is not changed on purpose. A direction that would read samples a
//! block does not have (e.g. vertical at the top of a slice, 8.3.1.2) is not allowed
//! and becomes DC.

use crate::mb::{LUMA4X4_XY, MbContext, MbType, RowCache, SliceData};
use anyhow::{Result, bail};

/// DC prediction of Intra4x4/8x8 and Intra16x16
pub const DC: u8 = 2;
/// DC prediction of chroma
pub const CHROMA_DC: u8 = 0;

/// A block's direction, or what it should become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Intra4x4PredMode / Intra8x8PredMode (0..=8) or Intra16x16PredMode (0..=3)
    Mode(u8),
    /// code rem_intra_pred_mode with this value (0..=7), whatever direction it gives
    /// next to the final neighbours
    Rem(u8),
}

/// The directions of an intra macroblock: luma per 4x4 block in raster order (an 8x8
/// block repeats its direction in its four 4x4 blocks, Intra16x16 in all of them),
/// and intra_chroma_pred_mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbIntra {
    pub luma: [Dir; 16],
    pub chroma: u8,
}

// samples a block can predict from
const TOP: u8 = 1;
const LEFT: u8 = 2;
const TOP_LEFT: u8 = 4;
const ALL: u8 = TOP | LEFT | TOP_LEFT;

/// samples each direction needs (8.3.1.2, 8.3.2.2)
const NXN_NEEDS: [u8; 9] = [TOP, LEFT, 0, TOP, ALL, ALL, ALL, TOP, LEFT];
const I16_NEEDS: [u8; 4] = [TOP, LEFT, 0, ALL];
const CHROMA_NEEDS: [u8; 4] = [0, LEFT, TOP, ALL];

fn allowed(needs: &[u8], mode: u8, have: u8) -> bool {
    needs.get(mode as usize).is_some_and(|n| n & !have == 0)
}

fn decode_mode(coded: i8, pred: u8) -> u8 {
    match coded {
        c if c < 0 => pred,
        c if (c as u8) < pred => c as u8,
        c => c as u8 + 1,
    }
}

fn encode_mode(mode: u8, pred: u8) -> i8 {
    match mode {
        m if m == pred => -1,
        m if m < pred => m as i8,
        m => m as i8 - 1,
    }
}

/// What later macroblocks need to know about a finished one.
#[derive(Clone, Copy)]
struct Done {
    intra: bool,
    /// Intra4x4/8x8 directions in raster order; DC for any other macroblock (8.3.1.1)
    modes: [u8; 16],
}

/// Neighbour state while walking a slice in decoding order.
struct Walker {
    width: usize,
    constrained: bool,
    done: RowCache<Done>,
    addr: usize,
    cur: [u8; 16],
}

impl Walker {
    fn new(ctx: &MbContext) -> Self {
        Self { width: ctx.width, constrained: ctx.pps.constrained_intra_pred_flag, done: RowCache::new(ctx.width), addr: 0, cur: [DC; 16] }
    }

    fn begin(&mut self, addr: usize) {
        self.addr = addr;
        self.cur = [DC; 16];
    }

    fn finish(&mut self, intra: bool, nxn: bool) {
        self.done.set(self.addr, Done { intra, modes: if nxn { self.cur } else { [DC; 16] } });
    }

    /// The neighbouring macroblock at (dx, dy) in macroblocks, if it is available for
    /// intra prediction (in the slice, and intra when constrained_intra_pred_flag is set).
    fn mb(&self, dx: isize, dy: isize) -> Option<&Done> {
        let (a, w) = (self.addr, self.width);
        let x = (a % w) as isize + dx;
        if x < 0 || x >= w as isize || (dy < 0 && a < w) {
            return None;
        }
        let n = (a as isize + dy * w as isize + dx) as usize;
        self.done.get(n).filter(|d| d.intra || !self.constrained)
    }

    /// predIntra4x4PredMode / predIntra8x8PredMode of the block at (x, y) in 4x4 units
    fn predicted(&self, x: usize, y: usize) -> u8 {
        let a = if x > 0 { Some(self.cur[y * 4 + x - 1]) } else { self.mb(-1, 0).map(|n| n.modes[y * 4 + 3]) };
        let b = if y > 0 { Some(self.cur[(y - 1) * 4 + x]) } else { self.mb(0, -1).map(|n| n.modes[12 + x]) };
        match (a, b) {
            (Some(a), Some(b)) => a.min(b),
            _ => DC,
        }
    }

    /// The neighbouring samples a block at (x, y) in 4x4 units can predict from
    /// (Intra16x16 and chroma: (0, 0)).
    fn samples(&self, x: usize, y: usize) -> u8 {
        let top = y > 0 || self.mb(0, -1).is_some();
        let left = x > 0 || self.mb(-1, 0).is_some();
        let top_left = match (x > 0, y > 0) {
            (true, true) => true,
            (true, false) => self.mb(0, -1).is_some(),
            (false, true) => self.mb(-1, 0).is_some(),
            (false, false) => self.mb(-1, -1).is_some(),
        };
        (top as u8 * TOP) | (left as u8 * LEFT) | (top_left as u8 * TOP_LEFT)
    }

    fn set(&mut self, x: usize, y: usize, size: usize, mode: u8) {
        for by in y..y + size {
            for bx in x..x + size {
                self.cur[by * 4 + bx] = mode;
            }
        }
    }
}

/// The luma blocks of an I_NxN macroblock in decoding order: (index into
/// `Macroblock::intra_modes`, x, y, size), in 4x4 units.
fn blocks(transform_8x8: bool) -> impl Iterator<Item = (usize, usize, usize, usize)> {
    let n = if transform_8x8 { 4 } else { 16 };
    (0..n).map(move |k| if transform_8x8 { (k, 2 * (k % 2), 2 * (k / 2), 2) } else { (k, LUMA4X4_XY[k].0, LUMA4X4_XY[k].1, 1) })
}

fn has_chroma(ctx: &MbContext) -> bool {
    matches!(ctx.chroma_array_type, 1 | 2)
}

/// The directions of every intra macroblock of a slice (None for the others).
pub fn decode_intra(ctx: &MbContext, data: &SliceData) -> Vec<Option<MbIntra>> {
    let mut w = Walker::new(ctx);
    let mut out = Vec::with_capacity(data.mbs.len());
    for (i, mb) in data.mbs.iter().enumerate() {
        w.begin(data.first_mb + i);
        let chroma = mb.intra_chroma_pred_mode;
        let intra = match mb.mb_type {
            MbType::INxN => {
                let mut luma = [Dir::Mode(DC); 16];
                for (k, x, y, size) in blocks(mb.transform_8x8) {
                    let mode = decode_mode(mb.intra_modes[k], w.predicted(x, y));
                    w.set(x, y, size, mode);
                }
                for (d, &m) in luma.iter_mut().zip(&w.cur) {
                    *d = Dir::Mode(m);
                }
                Some(MbIntra { luma, chroma })
            }
            MbType::I16x16(m) => Some(MbIntra { luma: [Dir::Mode(m); 16], chroma }),
            _ => None,
        };
        w.finish(mb.mb_type.is_intra(), mb.mb_type == MbType::INxN);
        out.push(intra);
    }
    out
}

/// Re-code the intra macroblocks of a slice so that they decode to `targets`, one per
/// macroblock (as from `decode_intra`; I_PCM and inter macroblocks are left alone).
/// Directions the position does not allow become DC.
pub fn encode_intra(ctx: &MbContext, data: &mut SliceData, targets: &[Option<MbIntra>]) -> Result<()> {
    if targets.len() != data.mbs.len() {
        bail!("intra target has {} macroblocks, slice has {}", targets.len(), data.mbs.len());
    }
    let mut w = Walker::new(ctx);
    for (i, (mb, target)) in data.mbs.iter_mut().zip(targets).enumerate() {
        w.begin(data.first_mb + i);
        let nxn = mb.mb_type == MbType::INxN;
        if matches!(mb.mb_type, MbType::INxN | MbType::I16x16(_)) {
            let Some(t) = target else { bail!("no intra target for macroblock {}", data.first_mb + i) };
            if nxn {
                for (k, x, y, size) in blocks(mb.transform_8x8) {
                    let pred = w.predicted(x, y);
                    let want = match t.luma[y * 4 + x] {
                        Dir::Mode(m) => m,
                        Dir::Rem(r) => decode_mode(r.min(7) as i8, pred),
                    };
                    let mode = if allowed(&NXN_NEEDS, want, w.samples(x, y)) { want } else { DC };
                    mb.intra_modes[k] = encode_mode(mode, pred);
                    w.set(x, y, size, mode);
                }
            } else if let Dir::Mode(want) = t.luma[0] {
                let mode = if allowed(&I16_NEEDS, want, w.samples(0, 0)) { want } else { DC };
                mb.mb_type = MbType::I16x16(mode);
            }
            if has_chroma(ctx) {
                let want = t.chroma;
                mb.intra_chroma_pred_mode = if allowed(&CHROMA_NEEDS, want, w.samples(0, 0)) { want } else { CHROMA_DC };
            }
        }
        w.finish(mb.mb_type.is_intra(), nxn);
    }
    Ok(())
}

/// Intra4x4/8x8 directions in the order of their angles (Figure 8-2); DC has none.
const ANGULAR: [u8; 8] = [8, 1, 6, 4, 5, 0, 7, 3];
/// Intra16x16 vertical, horizontal, plane
const I16_TURN: [u8; 3] = [0, 1, 3];
/// chroma vertical, horizontal, plane
const CHROMA_TURN: [u8; 3] = [2, 1, 3];

/// Kinds of intra blocks, with their own sets of directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Intra4x4 and Intra8x8: 9 directions
    NxN,
    /// Intra16x16: vertical, horizontal, DC, plane
    I16x16,
    /// chroma: DC, horizontal, vertical, plane
    Chroma,
}

impl Kind {
    pub fn modes(self) -> u8 {
        if self == Kind::NxN { 9 } else { 4 }
    }

    /// The nearest mode of this kind to Intra4x4 direction `d`.
    pub fn from_nxn(self, d: u8) -> u8 {
        match (self, d) {
            (Kind::NxN, d) => d.min(8),
            (Kind::I16x16, 0..=2) => d,
            (Kind::Chroma, 0) => 2,
            (Kind::Chroma, 1) => 1,
            (Kind::Chroma, 2) => CHROMA_DC,
            _ => 3,
        }
    }

    /// Turn a direction by `steps`: Intra4x4/8x8 in steps of 22.5 degrees, the others
    /// through vertical, horizontal and plane. DC stays.
    pub fn turn(self, mode: u8, steps: i32) -> u8 {
        let cycle: &[u8] = match self {
            Kind::NxN => &ANGULAR,
            Kind::I16x16 => &I16_TURN,
            Kind::Chroma => &CHROMA_TURN,
        };
        match cycle.iter().position(|&m| m == mode) {
            Some(i) => cycle[(i as i64 + steps as i64).rem_euclid(cycle.len() as i64) as usize],
            None => mode,
        }
    }
}

impl MbIntra {
    /// Change every luma direction (Intra4x4/8x8 or Intra16x16, as `nxn` says) and the
    /// chroma direction.
    pub fn map(&mut self, nxn: bool, mut f: impl FnMut(Kind, u8) -> u8) {
        let kind = if nxn { Kind::NxN } else { Kind::I16x16 };
        for d in self.luma.iter_mut() {
            if let Dir::Mode(m) = *d {
                *d = Dir::Mode(f(kind, m));
            }
        }
        self.chroma = f(Kind::Chroma, self.chroma);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coded_modes_skip_the_predicted_one() {
        for pred in 0..9 {
            for mode in 0..9 {
                let c = encode_mode(mode, pred);
                assert!((-1..8).contains(&c));
                assert_eq!(decode_mode(c, pred), mode);
            }
        }
    }

    #[test]
    fn turning_keeps_dc_and_cycles() {
        assert_eq!(Kind::NxN.turn(DC, 3), DC);
        assert_eq!(Kind::NxN.turn(0, 1), 7);
        assert_eq!(Kind::NxN.turn(0, -1), 5);
        assert_eq!(Kind::NxN.turn(3, 1), 8);
        assert_eq!((0..8).fold(4, |m, _| Kind::NxN.turn(m, 1)), 4);
        assert_eq!(Kind::I16x16.turn(0, 1), 1);
        assert_eq!(Kind::Chroma.turn(CHROMA_DC, 1), CHROMA_DC);
        assert_eq!(Kind::Chroma.turn(3, 1), 2);
    }

    #[test]
    fn directions_need_their_samples() {
        // nothing around: only DC, horizontal-free and vertical-free modes fail
        assert!(allowed(&NXN_NEEDS, DC, 0));
        assert!(!allowed(&NXN_NEEDS, 0, LEFT));
        assert!(allowed(&NXN_NEEDS, 8, LEFT));
        assert!(!allowed(&NXN_NEEDS, 4, TOP | LEFT));
        assert!(allowed(&I16_NEEDS, 3, ALL));
        assert!(!allowed(&I16_NEEDS, 4, ALL));
        assert!(allowed(&CHROMA_NEEDS, 2, TOP));
    }
}
