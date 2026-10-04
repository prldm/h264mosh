//! residual_block_cavlc() (7.3.5.3.2, 9.2): reading and writing one block of
//! transform coefficient levels.

use crate::bits::{BitReader, BitWriter};
use crate::cavlc_tables::*;
use anyhow::{Result, bail};
use std::sync::LazyLock;

/// A CAVLC code table plus a lookup table for decoding: indexed by the next `bits`
/// bits of the stream, each entry holds `code length << 8 | symbol`.
struct Vlc {
    codes: &'static [Code],
    bits: u32,
    lut: Box<[u16]>,
}

impl Vlc {
    const NONE: u16 = u16::MAX;

    fn new(codes: &'static [Code]) -> Self {
        let bits = codes.iter().map(|c| c.0 as u32).max().unwrap_or(0);
        let mut lut = vec![Self::NONE; 1 << bits];
        for (symbol, &(len, code)) in codes.iter().enumerate().filter(|(_, c)| c.0 > 0) {
            // every bit pattern that starts with this code
            let shift = bits - len as u32;
            let start = (code as usize) << shift;
            lut[start..start + (1 << shift)].fill((len as u16) << 8 | symbol as u16);
        }
        Self { codes, bits, lut: lut.into_boxed_slice() }
    }

    fn read(&self, r: &mut BitReader) -> Result<usize> {
        let entry = self.lut[r.peek(self.bits) as usize];
        if entry == Self::NONE {
            bail!("invalid CAVLC code at bit {}", r.pos());
        }
        r.skip((entry >> 8) as usize)?;
        Ok((entry & 0xff) as usize)
    }

    fn write(&self, w: &mut BitWriter, symbol: usize) -> Result<()> {
        match self.codes.get(symbol) {
            Some(&(len, code)) if len > 0 => {
                w.u(len as u32, code as u32);
                Ok(())
            }
            _ => bail!("value {symbol} cannot be coded with this CAVLC table"),
        }
    }
}

struct Tables {
    coeff_token: [Vlc; 4],
    coeff_token_chroma_dc_420: Vlc,
    coeff_token_chroma_dc_422: Vlc,
    total_zeros_4x4: Vec<Vlc>,
    total_zeros_chroma_dc_420: Vec<Vlc>,
    total_zeros_chroma_dc_422: Vec<Vlc>,
    run_before: Vec<Vlc>,
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    let all = |tables: &'static [&'static [Code]]| tables.iter().map(|t| Vlc::new(t)).collect();
    Tables {
        coeff_token: [0, 1, 2, 3].map(|i| Vlc::new(&COEFF_TOKEN[i])),
        coeff_token_chroma_dc_420: Vlc::new(&COEFF_TOKEN_CHROMA_DC_420),
        coeff_token_chroma_dc_422: Vlc::new(&COEFF_TOKEN_CHROMA_DC_422),
        total_zeros_4x4: all(&TOTAL_ZEROS_4X4),
        total_zeros_chroma_dc_420: all(&TOTAL_ZEROS_CHROMA_DC_420),
        total_zeros_chroma_dc_422: all(&TOTAL_ZEROS_CHROMA_DC_422),
        run_before: all(&RUN_BEFORE),
    }
});

fn coeff_token_table(nc: i32) -> &'static Vlc {
    let t = &*TABLES;
    match nc {
        -1 => &t.coeff_token_chroma_dc_420,
        -2 => &t.coeff_token_chroma_dc_422,
        0..=1 => &t.coeff_token[0],
        2..=3 => &t.coeff_token[1],
        4..=7 => &t.coeff_token[2],
        _ => &t.coeff_token[3],
    }
}

fn total_zeros_table(max_coeff: usize, total_coeff: usize) -> &'static Vlc {
    let t = &*TABLES;
    match max_coeff {
        4 => &t.total_zeros_chroma_dc_420[total_coeff - 1],
        8 => &t.total_zeros_chroma_dc_422[total_coeff - 1],
        _ => &t.total_zeros_4x4[total_coeff - 1],
    }
}

fn run_before_table(zeros_left: usize) -> &'static Vlc {
    &TABLES.run_before[zeros_left.min(7) - 1]
}

fn next_suffix_length(suffix_length: u32, level: i32) -> u32 {
    let sl = suffix_length.max(1);
    if level.unsigned_abs() > (3 << (sl - 1)) && sl < 6 { sl + 1 } else { sl }
}

/// Read one block into `coeffs[..max_coeff]` (which must be zeroed). `nc` selects the
/// coeff_token table (-1/-2 for chroma DC). Returns TotalCoeff.
pub fn read_block(r: &mut BitReader, nc: i32, coeffs: &mut [i32], max_coeff: usize) -> Result<u8> {
    let token = coeff_token_table(nc).read(r)?;
    let (total, trailing_ones) = (token / 4, token % 4);
    if total == 0 {
        return Ok(0);
    }
    if total > max_coeff {
        bail!("TotalCoeff {total} exceeds block size {max_coeff}");
    }
    let mut levels = [0i32; 16];
    let mut suffix_length = if total > 10 && trailing_ones < 3 { 1 } else { 0 };
    for (i, level) in levels.iter_mut().enumerate().take(total) {
        if i < trailing_ones {
            *level = if r.flag()? { -1 } else { 1 };
            continue;
        }
        let prefix = r.leading_zeros_and_one()?;
        if prefix > 28 {
            bail!("level_prefix {prefix} out of range");
        }
        let mut code = (prefix.min(15) << suffix_length) as i64;
        if suffix_length > 0 || prefix >= 14 {
            let size = if prefix == 14 && suffix_length == 0 {
                4
            } else if prefix >= 15 {
                prefix - 3
            } else {
                suffix_length
            };
            code += r.u(size)? as i64;
        }
        if prefix >= 15 && suffix_length == 0 {
            code += 15;
        }
        if prefix >= 16 {
            code += (1i64 << (prefix - 3)) - 4096;
        }
        if i == trailing_ones && trailing_ones < 3 {
            code += 2;
        }
        *level = (if code % 2 == 0 { (code + 2) >> 1 } else { (-code - 1) >> 1 }) as i32;
        suffix_length = next_suffix_length(suffix_length, *level);
    }
    let mut zeros_left = if total < max_coeff { total_zeros_table(max_coeff, total).read(r)? } else { 0 };
    if total + zeros_left > max_coeff {
        bail!("total_zeros {zeros_left} too large");
    }
    // levels[0] is the highest-frequency coefficient; place them from the lowest up
    let mut runs = [0usize; 16];
    for run in runs.iter_mut().take(total - 1) {
        if zeros_left == 0 {
            break;
        }
        *run = run_before_table(zeros_left).read(r)?;
        if *run > zeros_left {
            bail!("run_before {} exceeds zeros left {zeros_left}", *run);
        }
        zeros_left -= *run;
    }
    runs[total - 1] = zeros_left;
    let mut pos: isize = -1;
    for i in (0..total).rev() {
        pos += runs[i] as isize + 1;
        coeffs[pos as usize] = levels[i];
    }
    Ok(total as u8)
}

/// Write `coeffs[..max_coeff]` as one block. Returns TotalCoeff.
pub fn write_block(w: &mut BitWriter, nc: i32, coeffs: &[i32], max_coeff: usize) -> Result<u8> {
    // coding order: highest frequency first
    let mut nonzero = [0usize; 16];
    let mut levels = [0i32; 16];
    let mut total = 0;
    for i in (0..max_coeff).rev().filter(|&i| coeffs[i] != 0) {
        nonzero[total] = i;
        levels[total] = coeffs[i];
        total += 1;
    }
    let levels = &levels[..total];
    let trailing_ones = levels.iter().take(3).take_while(|l| l.abs() == 1).count();
    coeff_token_table(nc).write(w, total * 4 + trailing_ones)?;
    if total == 0 {
        return Ok(0);
    }
    let mut suffix_length = if total > 10 && trailing_ones < 3 { 1 } else { 0 };
    for (i, &level) in levels.iter().enumerate() {
        if i < trailing_ones {
            w.flag(level < 0);
            continue;
        }
        let mut code = if level > 0 { 2 * level as i64 - 2 } else { -2 * level as i64 - 1 };
        if i == trailing_ones && trailing_ones < 3 {
            code -= 2;
        }
        write_level_code(w, code, suffix_length)?;
        suffix_length = next_suffix_length(suffix_length, level);
    }
    let total_zeros = nonzero[0] + 1 - total;
    if total < max_coeff {
        total_zeros_table(max_coeff, total).write(w, total_zeros)?;
    }
    let mut zeros_left = total_zeros;
    for k in 0..total - 1 {
        if zeros_left == 0 {
            break;
        }
        let run = nonzero[k] - nonzero[k + 1] - 1;
        run_before_table(zeros_left).write(w, run)?;
        zeros_left -= run;
    }
    Ok(total as u8)
}

/// level_prefix / level_suffix for one levelCode (inverse of 9.2.2.1).
fn write_level_code(w: &mut BitWriter, code: i64, suffix_length: u32) -> Result<()> {
    let (prefix, suffix, suffix_size);
    if suffix_length == 0 && code < 14 {
        (prefix, suffix, suffix_size) = (code as u32, 0, 0);
    } else if suffix_length == 0 && code < 30 {
        (prefix, suffix, suffix_size) = (14, (code - 14) as u32, 4);
    } else if suffix_length > 0 && code < (15 << suffix_length) {
        (prefix, suffix, suffix_size) = ((code >> suffix_length) as u32, (code & ((1 << suffix_length) - 1)) as u32, suffix_length);
    } else {
        // escape: level_prefix >= 15
        let base = if suffix_length == 0 { 30 } else { 15i64 << suffix_length };
        let offset = code - base + 4096;
        let bits = 63 - offset.leading_zeros(); // floor(log2)
        if bits > 25 {
            bail!("coefficient level too large to code");
        }
        (prefix, suffix, suffix_size) = (bits + 3, (offset - (1 << bits)) as u32, bits);
    }
    for _ in 0..prefix {
        w.flag(false);
    }
    w.flag(true);
    w.u(suffix_size, suffix);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix_free(table: &[Code]) {
        let codes: Vec<&Code> = table.iter().filter(|c| c.0 > 0).collect();
        for (i, a) in codes.iter().enumerate() {
            for b in codes.iter().skip(i + 1) {
                let n = a.0.min(b.0);
                assert_ne!(a.1 >> (a.0 - n), b.1 >> (b.0 - n), "codes {a:?} and {b:?} overlap");
            }
        }
    }

    /// The plain linear search over the code table, as reference for the lookup tables.
    fn read_linear(r: &mut BitReader, table: &[Code]) -> Result<usize> {
        let bits = r.peek(16);
        for (i, &(len, code)) in table.iter().enumerate() {
            if len > 0 && (bits >> (16 - len as u32)) as u16 == code {
                r.skip(len as usize)?;
                return Ok(i);
            }
        }
        bail!("invalid code")
    }

    #[test]
    fn lookup_tables_match_linear_search() {
        let t = &*TABLES;
        let vlcs = t
            .coeff_token
            .iter()
            .chain([&t.coeff_token_chroma_dc_420, &t.coeff_token_chroma_dc_422])
            .chain(&t.total_zeros_4x4)
            .chain(&t.total_zeros_chroma_dc_420)
            .chain(&t.total_zeros_chroma_dc_422)
            .chain(&t.run_before);
        let mut checked = 0;
        for vlc in vlcs {
            // every bit pattern of the table's maximum code length, followed by padding
            for pattern in 0u32..1 << vlc.bits {
                let bytes = ((pattern << (32 - vlc.bits)) as u64).to_be_bytes();
                let bytes = &bytes[4..];
                let (mut a, mut b) = (BitReader::new(bytes), BitReader::new(bytes));
                let (x, y) = (vlc.read(&mut a).ok(), read_linear(&mut b, vlc.codes).ok());
                assert_eq!(x, y, "pattern {pattern:b}");
                assert_eq!(a.pos(), b.pos());
                checked += 1;
            }
        }
        // the nC < 2 coeff_token table alone has 16-bit codes
        assert!(checked > 65_536);
    }

    #[test]
    fn tables_are_prefix_free() {
        COEFF_TOKEN.iter().for_each(|t| prefix_free(t));
        prefix_free(&COEFF_TOKEN_CHROMA_DC_420);
        prefix_free(&COEFF_TOKEN_CHROMA_DC_422);
        TOTAL_ZEROS_4X4
            .iter()
            .chain(&TOTAL_ZEROS_CHROMA_DC_420)
            .chain(&TOTAL_ZEROS_CHROMA_DC_422)
            .chain(&RUN_BEFORE)
            .for_each(|t| prefix_free(t));
    }

    fn roundtrip(coeffs: &[i32], nc: i32) {
        let max = coeffs.len();
        let mut w = BitWriter::new();
        let total = write_block(&mut w, nc, coeffs, max).unwrap();
        w.trailing_bits();
        let bytes = w.into_bytes();
        let mut back = vec![0; max];
        let mut r = BitReader::new(&bytes);
        assert_eq!(read_block(&mut r, nc, &mut back, max).unwrap(), total);
        assert_eq!(back, coeffs, "nc={nc}");
    }

    #[test]
    fn block_roundtrip() {
        let mut seed = 12345u64;
        let mut rnd = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for &max in &[4usize, 8, 15, 16] {
            for _ in 0..3000 {
                let mut c = vec![0i32; max];
                let n = rnd(max as u64 + 1) as usize;
                for _ in 0..n {
                    let pos = rnd(max as u64) as usize;
                    let mag = match rnd(4) {
                        0 => 1,
                        1 => 1 + rnd(4) as i32,
                        2 => 1 + rnd(100) as i32,
                        _ => 1 + rnd(40000) as i32,
                    };
                    c[pos] = if rnd(2) == 0 { mag } else { -mag };
                }
                let ncs: &[i32] = match max {
                    4 => &[-1],
                    8 => &[-2],
                    _ => &[0, 1, 2, 3, 5, 8, 16],
                };
                for &nc in ncs {
                    roundtrip(&c, nc);
                }
            }
        }
        roundtrip(&[0; 16], 0);
        roundtrip(&[1; 16], 0);
        roundtrip(&[-1, 1, -1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 3);
    }
}
