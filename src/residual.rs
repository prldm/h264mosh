//! Scaling and inverse transform of residual blocks (8.5.6–8.5.13), as far as needed
//! to tell whether coefficient levels stay within the value ranges a conforming
//! stream may produce. Effects that raise levels or the quantizer check their result
//! here, so that every decoder still computes the same picture.

use crate::common::ScalingList;
use crate::mb::{Blk, LUMA4X4_XY, Macroblock, MbContext, MbType};
use crate::pps::Pps;
use crate::sps::Sps;

/// Raster position (y * 4 + x) of each coefficient in the frame zig-zag scan (Table 8-13).
pub const ZIGZAG4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const FIELD4: [usize; 16] = [0, 4, 1, 8, 12, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15];
/// Raster position (y * 8 + x) of each coefficient in the 8x8 frame zig-zag scan (Table 8-14).
pub const ZIGZAG8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56,
    57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
const FIELD8: [usize; 64] = [
    0, 8, 16, 1, 9, 24, 32, 17, 2, 25, 40, 48, 56, 33, 10, 3, 18, 41, 49, 57, 26, 11, 4, 19, 34, 42, 50, 58, 27, 12, 5, 20, 35, 43, 51, 59,
    28, 13, 6, 21, 36, 44, 52, 60, 29, 14, 22, 37, 45, 53, 61, 30, 7, 15, 38, 46, 54, 62, 23, 31, 39, 47, 55, 63,
];

/// Default scaling lists (Table 7-3, 7-4), in raster order.
const DEFAULT4: [[u8; 16]; 2] =
    [[6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42], [10, 14, 20, 24, 14, 20, 24, 27, 20, 24, 27, 30, 24, 27, 30, 34]];
const DEFAULT8: [[u8; 64]; 2] = [
    [
        6, 10, 13, 16, 18, 23, 25, 27, 10, 11, 16, 18, 23, 25, 27, 29, 13, 16, 18, 23, 25, 27, 29, 31, 16, 18, 23, 25, 27, 29, 31, 33, 18,
        23, 25, 27, 29, 31, 33, 36, 23, 25, 27, 29, 31, 33, 36, 38, 25, 27, 29, 31, 33, 36, 38, 40, 27, 29, 31, 33, 36, 38, 40, 42,
    ],
    [
        9, 13, 15, 17, 19, 21, 22, 24, 13, 13, 17, 19, 21, 22, 24, 25, 15, 17, 19, 21, 22, 24, 25, 27, 17, 19, 21, 22, 24, 25, 27, 28, 19,
        21, 22, 24, 25, 27, 28, 30, 21, 22, 24, 25, 27, 28, 30, 32, 22, 24, 25, 27, 28, 30, 32, 33, 24, 25, 27, 28, 30, 32, 33, 35,
    ],
];

/// normAdjust4x4 (8-315): [qP % 6][position class]
const NORM4: [[i32; 3]; 6] = [[10, 16, 13], [11, 18, 14], [13, 20, 16], [14, 23, 18], [16, 25, 20], [18, 29, 23]];
/// normAdjust8x8 (8-318): [qP % 6][position class]
const NORM8: [[i32; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];
/// position class of normAdjust8x8, indexed by (x % 4) * 4 + y % 4
const CLASS8: [usize; 16] = [0, 3, 4, 3, 3, 1, 5, 1, 4, 5, 2, 5, 3, 1, 5, 1];

/// QP_C for qPI = 30..=51 (Table 8-15)
const QPC: [i32; 22] = [29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39];

/// The values of a scaling list (7.3.2.1.1.1) in raster order; None means the default list.
fn list_values<const N: usize>(list: &ScalingList, scan: &[usize; N]) -> Option<[u8; N]> {
    let mut out = [0; N];
    let (mut last, mut next) = (8i32, 8i32);
    let mut deltas = list.deltas.iter();
    for (j, &pos) in scan.iter().enumerate() {
        if next != 0 {
            next = (last + deltas.next().copied().unwrap_or(0)).rem_euclid(256);
            if j == 0 && next == 0 {
                return None;
            }
        }
        if next != 0 {
            last = next;
        }
        out[pos] = last as u8;
    }
    Some(out)
}

/// Weight matrices: the six 4x4 lists (Intra Y, Cb, Cr, Inter Y, Cb, Cr) and the six
/// 8x8 lists (Intra Y, Inter Y, Intra Cb, Inter Cb, Intra Cr, Inter Cr), raster order.
type Weights = ([[u8; 16]; 6], [[u8; 64]; 6]);

/// Resolve signalled lists with the fall-back rules of Table 7-2: rule A when `seq` is
/// None, rule B (the sequence-level lists) otherwise.
fn resolve(lists: &[Option<ScalingList>], seq: Option<&Weights>) -> Weights {
    let (mut w4, mut w8) = ([[16; 16]; 6], [[16; 64]; 6]);
    for i in 0..6 {
        let default = DEFAULT4[i / 3];
        w4[i] = match lists.get(i).and_then(Option::as_ref) {
            Some(l) => list_values(l, &ZIGZAG4).unwrap_or(default),
            None if i % 3 == 0 => seq.map_or(default, |s| s.0[i]),
            None => w4[i - 1],
        };
    }
    for k in 0..6 {
        let default = DEFAULT8[k % 2];
        w8[k] = match lists.get(6 + k).and_then(Option::as_ref) {
            Some(l) => list_values(l, &ZIGZAG8).unwrap_or(default),
            None if k < 2 => seq.map_or(default, |s| s.1[k]),
            None => w8[k - 2],
        };
    }
    (w4, w8)
}

fn weights(sps: &Sps, pps: &Pps) -> Weights {
    let flat = ([[16; 16]; 6], [[16; 64]; 6]);
    let seq = sps.scaling_lists.as_ref().map_or(flat, |l| resolve(l, None));
    match pps.ext.as_ref().and_then(|e| e.scaling_lists.as_ref()) {
        Some(lists) => resolve(lists, sps.scaling_lists.is_some().then_some(&seq)),
        None => seq,
    }
}

/// Everything about a slice that dequantization depends on.
pub struct Scaling {
    /// LevelScale4x4 [list][qP % 6][raster position]
    ls4: Box<[[[i64; 16]; 6]; 6]>,
    /// LevelScale8x8 [list][qP % 6][raster position]
    ls8: Box<[[[i64; 64]; 6]; 6]>,
    scan4: &'static [usize; 16],
    scan8: &'static [usize; 64],
    /// luma, chroma
    bit_depth: [u32; 2],
    qp_bd_offset: [i32; 2],
    chroma_qp_offset: [i32; 2],
    bypass: bool,
    chroma_array_type: u32,
    /// level_prefix may not exceed 15 in Baseline, Main and Extended streams; smaller
    /// levels can always be coded that way
    max_level: Option<i32>,
}

impl Scaling {
    pub fn new(ctx: &MbContext) -> Self {
        let (sps, pps) = (ctx.sps, ctx.pps);
        let (w4, w8) = weights(sps, pps);
        let mut ls4 = Box::new([[[0; 16]; 6]; 6]);
        let mut ls8 = Box::new([[[0; 64]; 6]; 6]);
        for m in 0..6 {
            for list in 0..6 {
                for pos in 0..16 {
                    let (x, y) = (pos % 4, pos / 4);
                    let class = if x % 2 == 0 && y % 2 == 0 {
                        0
                    } else if x % 2 == 1 && y % 2 == 1 {
                        1
                    } else {
                        2
                    };
                    ls4[list][m][pos] = (w4[list][pos] as i32 * NORM4[m][class]) as i64;
                }
                for pos in 0..64 {
                    let (x, y) = (pos % 8, pos / 8);
                    ls8[list][m][pos] = (w8[list][pos] as i32 * NORM8[m][CLASS8[(x % 4) * 4 + y % 4]]) as i64;
                }
            }
        }
        let field = ctx.header.field_pic_flag;
        let second = pps.ext.as_ref().map_or(pps.chroma_qp_index_offset, |e| e.second_chroma_qp_index_offset);
        Self {
            ls4,
            ls8,
            scan4: if field { &FIELD4 } else { &ZIGZAG4 },
            scan8: if field { &FIELD8 } else { &ZIGZAG8 },
            bit_depth: [sps.bit_depth_luma_minus8 + 8, sps.bit_depth_chroma_minus8 + 8],
            qp_bd_offset: [6 * sps.bit_depth_luma_minus8 as i32, 6 * sps.bit_depth_chroma_minus8 as i32],
            chroma_qp_offset: [pps.chroma_qp_index_offset, second],
            bypass: sps.qpprime_y_zero_transform_bypass_flag,
            chroma_array_type: ctx.chroma_array_type,
            max_level: matches!(sps.profile_idc, 66 | 77 | 88).then_some(2063),
        }
    }

    /// QP'C of component `c` (0 = Cb, 1 = Cr) for luma QP `qp` (8.5.8).
    fn chroma_qp(&self, qp: i32, c: usize) -> i32 {
        let off = self.qp_bd_offset[1];
        let qpi = (qp + self.chroma_qp_offset[c]).clamp(-off, 51);
        let qpc = if qpi < 30 { qpi } else { QPC[qpi as usize - 30] };
        qpc + off
    }

    /// Whether the residual of `mb`, decoded at QP_Y `qp`, keeps every scaled
    /// coefficient and every intermediate value of the inverse transforms within
    /// -2^(7 + bitDepth) .. 2^(7 + bitDepth) - 1 (8.5.10–8.5.13), and can be coded.
    pub fn fits(&self, mb: &Macroblock, qp: i32) -> bool {
        if mb.pcm.is_some() || !mb.has_residual() || mb.coeffs.is_empty() {
            return true;
        }
        if let Some(max) = self.max_level
            && mb.coeffs.iter().any(|b| b.levels.iter().any(|l| l.abs() > max))
        {
            return false;
        }
        let qp_y = qp + self.qp_bd_offset[0];
        if self.bypass && qp_y == 0 {
            // transform bypass: the levels are the residual samples
            let lim = 1i64 << (7 + self.bit_depth[0].min(self.bit_depth[1]));
            return mb.coeffs.iter().all(|b| b.levels.iter().all(|&l| (l as i64) < lim && (l as i64) >= -lim));
        }
        let intra = mb.mb_type.is_intra();
        let planes = if self.chroma_array_type == 3 { 3 } else { 1 };
        for p in 0..planes {
            let (qp, bd) = if p == 0 { (qp_y, self.bit_depth[0]) } else { (self.chroma_qp(qp, p - 1), self.bit_depth[1]) };
            if !self.plane_fits(mb, p, qp, 1 << (7 + bd), intra) {
                return false;
            }
        }
        if matches!(self.chroma_array_type, 1 | 2) && mb.cbp >> 4 != 0 {
            for c in 0..2 {
                if !self.chroma_fits(mb, c, self.chroma_qp(qp, c), 1 << (7 + self.bit_depth[1]), intra) {
                    return false;
                }
            }
        }
        true
    }

    /// Luma, or a 4:4:4 chroma plane coded like luma.
    fn plane_fits(&self, mb: &Macroblock, p: usize, qp: i32, lim: i64, intra: bool) -> bool {
        let i16 = matches!(mb.mb_type, MbType::I16x16(_));
        let ls = &self.ls4[if intra { 0 } else { 3 } + p][qp as usize % 6];
        let mut dc = [0i64; 16];
        if i16 && let Some(b) = mb.block(Blk::LumaDc(p as u8)) {
            let mut c = [0i64; 16];
            for (k, &l) in b.levels.iter().enumerate() {
                c[self.scan4[k]] = l as i64;
            }
            let f = mul4(&mul4(&H4, &c), &H4);
            if !in_range(&f, lim) {
                return false;
            }
            for (d, &f) in dc.iter_mut().zip(&f) {
                *d = scale(f, ls[0], qp, 6);
            }
        }
        let coded = |blk: usize| mb.cbp & (1 << (blk / 4)) != 0;
        if mb.transform_8x8 {
            let ls8 = &self.ls8[2 * p + !intra as usize][qp as usize % 6];
            for b8 in 0..4 {
                if !coded(4 * b8) {
                    continue;
                }
                let mut c = [0i64; 64];
                for k in 0..4 {
                    if let Some(b) = mb.block(Blk::Luma(p as u8, (4 * b8 + k) as u8)) {
                        for (i, &l) in b.levels.iter().enumerate() {
                            c[self.scan8[4 * i + k]] = l as i64;
                        }
                    }
                }
                if !block8_fits(&c, ls8, qp, lim) {
                    return false;
                }
            }
            return true;
        }
        for (blk, &(x, y)) in LUMA4X4_XY.iter().enumerate() {
            let mut c = [0i64; 16];
            if coded(blk)
                && let Some(b) = mb.block(Blk::Luma(p as u8, blk as u8))
            {
                // Intra16x16 AC blocks start at the second scan position
                for (i, &l) in b.levels.iter().take(16 - i16 as usize).enumerate() {
                    c[self.scan4[i + i16 as usize]] = l as i64;
                }
            }
            if !block4_fits(&c, i16.then_some(dc[y * 4 + x]), ls, qp, lim) {
                return false;
            }
        }
        true
    }

    /// Chroma DC and AC of component `comp` (0 = Cb, 1 = Cr) for ChromaArrayType 1
    /// and 2 (8.5.11), at QP'C `qp`.
    fn chroma_fits(&self, mb: &Macroblock, comp: usize, qp: i32, lim: i64, intra: bool) -> bool {
        let list = if intra { 1 } else { 4 } + comp;
        let ls = &self.ls4[list][qp as usize % 6];
        let l = mb.block(Blk::ChromaDc(comp as u8)).map_or([0; 16], |b| b.levels).map(|v| v as i64);
        // dcC per chroma 4x4 block, in raster order
        let mut dc = [0i64; 8];
        let blocks = if self.chroma_array_type == 1 {
            // c = [c0 c1; c2 c3], f = A c A
            let f = [l[0] + l[1] + l[2] + l[3], l[0] - l[1] + l[2] - l[3], l[0] + l[1] - l[2] - l[3], l[0] - l[1] - l[2] + l[3]];
            if !in_range(&f, lim) {
                return false;
            }
            for (d, &f) in dc.iter_mut().zip(&f) {
                *d = ((f * ls[0]) << (qp / 6)) >> 5;
            }
            4
        } else {
            // 4 rows of 2 coefficients (8-330), f = H4 c A
            let c = [[l[0], l[2]], [l[1], l[5]], [l[3], l[6]], [l[4], l[7]]];
            let mut f = [0i64; 8];
            for i in 0..4 {
                let row: [i64; 2] = std::array::from_fn(|j| (0..4).map(|k| H4[i * 4 + k] * c[k][j]).sum());
                f[2 * i] = row[0] + row[1];
                f[2 * i + 1] = row[0] - row[1];
            }
            if !in_range(&f, lim) {
                return false;
            }
            let qp_dc = qp + 3;
            let ls_dc = self.ls4[list][qp_dc as usize % 6][0];
            for (d, &f) in dc.iter_mut().zip(&f) {
                *d = scale(f, ls_dc, qp_dc, 6);
            }
            8
        };
        for (b, &dc) in dc.iter().enumerate().take(blocks) {
            let mut c = [0i64; 16];
            if mb.cbp >> 4 == 2
                && let Some(ac) = mb.block(Blk::ChromaAc(comp as u8, b as u8))
            {
                for (i, &l) in ac.levels.iter().take(15).enumerate() {
                    c[self.scan4[i + 1]] = l as i64;
                }
            }
            if !block4_fits(&c, Some(dc), ls, qp, lim) {
                return false;
            }
        }
        true
    }
}

/// The 4x4 Hadamard matrix of the DC transforms (8-320, 8-329)
const H4: [i64; 16] = [1, 1, 1, 1, 1, 1, -1, -1, 1, -1, -1, 1, 1, -1, 1, -1];

fn mul4(a: &[i64; 16], b: &[i64; 16]) -> [i64; 16] {
    std::array::from_fn(|k| (0..4).map(|n| a[k / 4 * 4 + n] * b[n * 4 + k % 4]).sum())
}

fn in_range(v: &[i64], lim: i64) -> bool {
    v.iter().all(|&x| x >= -lim && x < lim)
}

/// Scaling of one coefficient with a normalisation of `shift` bits (4 for 4x4 blocks,
/// 6 for 8x8 blocks and the Intra16x16 and 4:2:2 DC): a rounded right shift while
/// qP / 6 < shift, a left shift from there on.
fn scale(c: i64, ls: i64, qp: i32, shift: i32) -> i64 {
    let s = qp / 6 - shift;
    if c == 0 {
        0
    } else if s >= 0 {
        (c * ls) << s
    } else {
        (c * ls + (1 << (-s - 1))) >> -s
    }
}

/// One 4x4 block (8.5.12): `c` in raster order; `dc` is the already scaled DC of
/// Intra16x16 and chroma blocks.
fn block4_fits(c: &[i64; 16], dc: Option<i64>, ls: &[i64; 16], qp: i32, lim: i64) -> bool {
    if dc.is_none_or(|d| d == 0) && c.iter().all(|&v| v == 0) {
        return true;
    }
    let mut d: [i64; 16] = std::array::from_fn(|pos| scale(c[pos], ls[pos], qp, 4));
    if let Some(dc) = dc {
        d[0] = dc;
    }
    if !in_range(&d, lim) {
        return false;
    }
    // rows, then columns
    let mut f = [0i64; 16];
    for i in 0..4 {
        let r = &d[i * 4..i * 4 + 4];
        let e = [r[0] + r[2], r[0] - r[2], (r[1] >> 1) - r[3], r[1] + (r[3] >> 1)];
        let o = [e[0] + e[3], e[1] + e[2], e[1] - e[2], e[0] - e[3]];
        if !in_range(&e, lim) || !in_range(&o, lim) {
            return false;
        }
        f[i * 4..i * 4 + 4].copy_from_slice(&o);
    }
    (0..4).all(|j| {
        let col = [f[j], f[4 + j], f[8 + j], f[12 + j]];
        let g = [col[0] + col[2], col[0] - col[2], (col[1] >> 1) - col[3], col[1] + (col[3] >> 1)];
        let h = [g[0] + g[3], g[1] + g[2], g[1] - g[2], g[0] - g[3]];
        in_range(&g, lim) && in_range(&h, lim)
    })
}

/// One-dimensional 8-point inverse transform (8.5.13.2); None if an intermediate value
/// leaves the range.
fn idct8(d: [i64; 8], lim: i64) -> Option<[i64; 8]> {
    let g = [
        d[0] + d[4],
        -d[3] + d[5] - d[7] - (d[7] >> 1),
        d[0] - d[4],
        d[1] + d[7] - d[3] - (d[3] >> 1),
        (d[2] >> 1) - d[6],
        -d[1] + d[7] + d[5] + (d[5] >> 1),
        d[2] + (d[6] >> 1),
        d[3] + d[5] + d[1] + (d[1] >> 1),
    ];
    let h = [
        g[0] + g[6],
        g[1] + (g[7] >> 2),
        g[2] + g[4],
        g[3] + (g[5] >> 2),
        g[2] - g[4],
        (g[3] >> 2) - g[5],
        g[0] - g[6],
        g[7] - (g[1] >> 2),
    ];
    let f = [h[0] + h[7], h[2] + h[5], h[4] + h[3], h[6] + h[1], h[6] - h[1], h[4] - h[3], h[2] - h[5], h[0] - h[7]];
    (in_range(&g, lim) && in_range(&h, lim) && in_range(&f, lim)).then_some(f)
}

/// One 8x8 block (8.5.13), `c` in raster order.
fn block8_fits(c: &[i64; 64], ls: &[i64; 64], qp: i32, lim: i64) -> bool {
    if c.iter().all(|&v| v == 0) {
        return true;
    }
    let d: [i64; 64] = std::array::from_fn(|pos| scale(c[pos], ls[pos], qp, 6));
    if !in_range(&d, lim) {
        return false;
    }
    let mut rows = [0i64; 64];
    for i in 0..8 {
        let Some(f) = idct8(std::array::from_fn(|j| d[i * 8 + j]), lim) else { return false };
        rows[i * 8..i * 8 + 8].copy_from_slice(&f);
    }
    (0..8).all(|j| idct8(std::array::from_fn(|i| rows[i * 8 + j]), lim).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zig-zag scans built by walking the anti-diagonals.
    fn zigzag(n: usize) -> Vec<usize> {
        let mut v = Vec::new();
        for s in 0..2 * n - 1 {
            let xs: Vec<usize> = (0..n).filter(|&x| s >= x && s - x < n).collect();
            let ordered: Vec<usize> = if s % 2 == 0 { xs } else { xs.into_iter().rev().collect() };
            v.extend(ordered.into_iter().map(|x| (s - x) * n + x));
        }
        v
    }

    #[test]
    fn scans_are_zig_zag_and_permutations() {
        assert_eq!(ZIGZAG4.to_vec(), zigzag(4));
        assert_eq!(ZIGZAG8.to_vec(), zigzag(8));
        for scan in [&FIELD4[..], &FIELD8[..]] {
            let mut s = scan.to_vec();
            s.sort();
            assert_eq!(s, (0..scan.len()).collect::<Vec<_>>());
        }
        // the field scans start down the first column
        assert_eq!(FIELD4[..4], [0, 4, 1, 8]);
        assert_eq!(FIELD8[..4], [0, 8, 16, 1]);
    }

    #[test]
    fn scaling_lists_decode_with_defaults_and_fall_back() {
        // all deltas 0: flat 8
        let flat = ScalingList { deltas: vec![0; 16] };
        assert_eq!(list_values(&flat, &ZIGZAG4), Some([8; 16]));
        // a first value of 0 selects the default list
        assert_eq!(list_values(&ScalingList { deltas: vec![-8] }, &ZIGZAG4), None);
        // a later 0 repeats the last value to the end
        let v = list_values(&ScalingList { deltas: vec![8, 4, -20] }, &ZIGZAG4).unwrap();
        assert_eq!((v[ZIGZAG4[0]], v[ZIGZAG4[1]], v[ZIGZAG4[2]], v[ZIGZAG4[15]]), (16, 20, 20, 20));
        // rule A: missing lists take the defaults, then the previous list
        let (w4, w8) = resolve(&[None, None, None, Some(flat), None, None], None);
        assert_eq!((w4[0], w4[2], w4[3], w4[5]), (DEFAULT4[0], DEFAULT4[0], [8; 16], [8; 16]));
        assert_eq!((w8[0], w8[1], w8[5]), (DEFAULT8[0], DEFAULT8[1], DEFAULT8[1]));
        // rule B: the first lists of each kind come from the sequence
        let seq = ([[7; 16]; 6], [[9; 64]; 6]);
        let (w4, w8) = resolve(&[], Some(&seq));
        assert_eq!((w4[0], w4[4], w8[1], w8[3]), ([7; 16], [7; 16], [9; 64], [9; 64]));
    }

    #[test]
    fn transforms_bound_their_values() {
        let lim = 1 << 15;
        let ls = [16 * 16; 16];
        // qP 28: d = c * 256; a lone DC of 100 gives 25600 and fits, 128 does not
        let mut c = [0i64; 16];
        c[0] = 100;
        assert!(block4_fits(&c, None, &ls, 28, lim));
        c[0] = 128;
        assert!(!block4_fits(&c, None, &ls, 28, lim));
        // two coefficients that fit on their own add up in the transform
        let mut c = [0i64; 16];
        c[0] = 100;
        c[2] = 100;
        assert!(!block4_fits(&c, None, &ls, 28, lim));
        // below qP 24 the scaling rounds
        assert_eq!(scale(1, 256, 6, 4), (256 + 4) >> 3);
        assert_eq!(scale(-3, 256, 40, 4), (-3 * 256) << 2);
        assert_eq!(scale(5, 320, 30, 6), (5 * 320 + 1) >> 1);
        assert!(block8_fits(&[0; 64], &[16 * 20; 64], 30, lim));
        let mut c = [0i64; 64];
        c[0] = 2000;
        assert!(!block8_fits(&c, &[16 * 20; 64], 30, lim));
    }
}
