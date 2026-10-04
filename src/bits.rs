//! Bit-level reader/writer for RBSP data (H.264 spec 7.2: u(n), ue(v), se(v)).

use anyhow::{Result, bail};

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn at(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Current position in bits from the start of the buffer.
    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    pub fn byte_aligned(&self) -> bool {
        self.pos.is_multiple_of(8)
    }

    pub fn flag(&mut self) -> Result<bool> {
        if self.pos >= self.data.len() * 8 {
            bail!("bitstream overrun at bit {}", self.pos);
        }
        let bit = (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(bit == 1)
    }

    /// The next `n` (<= 32) bits without consuming them; zero padded past the end.
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let word = match self.data.get(byte..byte + 8) {
            Some(b) => u64::from_be_bytes(b.try_into().unwrap()),
            None => {
                let mut b = [0u8; 8];
                let tail = self.data.get(byte..).unwrap_or(&[]);
                b[..tail.len()].copy_from_slice(tail);
                u64::from_be_bytes(b)
            }
        };
        // at most 7 + 32 bits are needed, all within the 64-bit word
        ((word << (self.pos & 7)) >> (64 - n)) as u32
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.bits_left() < n {
            bail!("bitstream overrun: need {} bits at bit {}, have {}", n, self.pos, self.bits_left());
        }
        self.pos += n;
        Ok(())
    }

    /// u(n), n <= 32
    pub fn u(&mut self, n: u32) -> Result<u32> {
        let v = self.peek(n);
        self.skip(n as usize)?;
        Ok(v)
    }

    /// ue(v): unsigned Exp-Golomb
    pub fn ue(&mut self) -> Result<u32> {
        let p = self.peek(32);
        if p == 0 {
            bail!("invalid Exp-Golomb code at bit {}", self.pos);
        }
        let zeros = p.leading_zeros();
        self.skip(zeros as usize + 1)?;
        Ok(((1u64 << zeros) - 1 + self.u(zeros)? as u64) as u32)
    }

    /// Number of zero bits before the next one bit, consuming them and the one bit
    /// (level_prefix, 9.2.2.1).
    pub fn leading_zeros_and_one(&mut self) -> Result<u32> {
        let mut zeros = 0;
        loop {
            let p = self.peek(32);
            if p != 0 {
                zeros += p.leading_zeros();
                self.skip(p.leading_zeros() as usize + 1)?;
                return Ok(zeros);
            }
            self.skip(32)?;
            zeros += 32;
        }
    }

    /// se(v): signed Exp-Golomb
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        Ok(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }

    /// more_rbsp_data(): true while there is data before the rbsp_stop_one_bit.
    pub fn more_rbsp_data(&self) -> bool {
        match last_one_bit(self.data) {
            Some(stop) => self.pos < stop,
            None => false,
        }
    }
}

/// Bit position of the last set bit in the buffer (the rbsp_stop_one_bit for a
/// buffer without cabac_zero_words).
pub fn last_one_bit(data: &[u8]) -> Option<usize> {
    let i = data.iter().rposition(|&b| b != 0)?;
    Some(i * 8 + 7 - data[i].trailing_zeros() as usize)
}

#[derive(Default)]
pub struct BitWriter {
    /// completed bytes
    buf: Vec<u8>,
    /// the last `pending` (< 8) bits, right-aligned
    acc: u64,
    pending: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bit_len(&self) -> usize {
        self.buf.len() * 8 + self.pending as usize
    }

    pub fn byte_aligned(&self) -> bool {
        self.pending == 0
    }

    pub fn flag(&mut self, b: bool) {
        self.u(1, b as u32);
    }

    /// u(n), n <= 32
    pub fn u(&mut self, n: u32, v: u32) {
        debug_assert!(n <= 32);
        debug_assert!(n == 32 || (v as u64) < (1u64 << n), "value {v} does not fit in {n} bits");
        // at most 7 + 32 bits are pending here
        self.acc = (self.acc << n) | v as u64;
        self.pending += n;
        while self.pending >= 8 {
            self.pending -= 8;
            self.buf.push((self.acc >> self.pending) as u8);
        }
        self.acc &= (1 << self.pending) - 1;
    }

    pub fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        // len - 1 zero bits, then x in len bits (up to 33 bits for the largest values)
        let zeros = len - 1;
        self.u(zeros.min(32), 0);
        if zeros > 32 {
            self.u(zeros - 32, 0);
        }
        if len > 32 {
            self.u(len - 32, (x >> 32) as u32);
        }
        self.u(len.min(32), x as u32 & (u32::MAX >> (32 - len.min(32))));
    }

    pub fn se(&mut self, v: i32) {
        debug_assert!(v != i32::MIN, "se(v) cannot code i32::MIN");
        let v = v as i64;
        let k = if v > 0 { 2 * v - 1 } else { -2 * v };
        self.ue(k as u32);
    }

    /// rbsp_trailing_bits(): stop bit + zero alignment bits
    pub fn trailing_bits(&mut self) {
        self.flag(true);
        if self.pending > 0 {
            self.u(8 - self.pending, 0);
        }
    }

    /// Copy bits [start, end) from `src`.
    pub fn append_bits(&mut self, src: &[u8], start: usize, end: usize) {
        debug_assert!(start <= end && end <= src.len() * 8);
        let mut p = start;
        // up to 7 bits until the source is byte aligned
        let head = ((8 - p % 8) % 8).min(end - p);
        if head > 0 {
            self.u(head as u32, BitReader::at(src, p).peek(head as u32));
            p += head;
        }
        let whole = (end - p) / 8;
        let bytes = &src[p / 8..p / 8 + whole];
        if self.pending == 0 {
            self.buf.extend_from_slice(bytes);
        } else {
            let s = self.pending;
            self.buf.reserve(whole);
            for &b in bytes {
                // the pending bits followed by the high bits of b make a full byte
                self.buf.push((self.acc << (8 - s)) as u8 | (b >> s));
                self.acc = b as u64 & ((1 << s) - 1);
            }
        }
        p += whole * 8;
        if p < end {
            self.u((end - p) as u32, BitReader::at(src, p).peek((end - p) as u32));
        }
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.pending > 0 {
            self.buf.push((self.acc << (8 - self.pending)) as u8);
        }
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_roundtrip() {
        let ues = [0u32, 1, 2, 3, 7, 8, 254, 255, 65535, 1 << 20, u32::MAX - 1];
        let ses = [0i32, 1, -1, 2, -2, 127, -128, 100000, -100000];
        let mut w = BitWriter::new();
        for &v in &ues {
            w.ue(v);
        }
        for &v in &ses {
            w.se(v);
        }
        w.u(5, 0b10110);
        w.trailing_bits();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for &v in &ues {
            assert_eq!(r.ue().unwrap(), v);
        }
        for &v in &ses {
            assert_eq!(r.se().unwrap(), v);
        }
        assert_eq!(r.u(5).unwrap(), 0b10110);
        assert!(!r.more_rbsp_data());
    }

    #[test]
    fn known_codes() {
        // ue: 0 -> 1, 1 -> 010, 2 -> 011, 3 -> 00100
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(1);
        w.ue(2);
        w.ue(3);
        assert_eq!(w.bit_len(), 1 + 3 + 3 + 5);
        assert_eq!(w.into_bytes(), vec![0b1010_0110, 0b0100_0000]);
    }

    #[test]
    fn append_bits_unaligned() {
        let src = [0b1011_0011u8, 0b0101_1100, 0b1111_0000, 0b1000_0001];
        for start in 0..12 {
            for end in start..32 {
                for pre in 0..9 {
                    let mut w = BitWriter::new();
                    for i in 0..pre {
                        w.flag(i % 2 == 0);
                    }
                    w.append_bits(&src, start, end);
                    let out = w.into_bytes();
                    let mut r = BitReader::new(&out);
                    for i in 0..pre {
                        assert_eq!(r.flag().unwrap(), i % 2 == 0);
                    }
                    let mut s = BitReader::at(&src, start);
                    for _ in start..end {
                        assert_eq!(r.flag().unwrap(), s.flag().unwrap());
                    }
                }
            }
        }
    }

    /// Bit-by-bit reference for `peek`, zero padded past the end.
    fn peek_slow(data: &[u8], pos: usize, n: u32) -> u32 {
        (0..n as usize).fold(0, |v, i| {
            let p = pos + i;
            let bit = data.get(p >> 3).map_or(0, |b| (b >> (7 - (p & 7))) & 1);
            (v << 1) | bit as u32
        })
    }

    #[test]
    fn peek_matches_bitwise_reference() {
        let data: Vec<u8> = (0..23u32).map(|i| (i.wrapping_mul(0x9E37_79B9) >> 13) as u8).collect();
        for pos in 0..data.len() * 8 + 16 {
            for n in 0..=32 {
                assert_eq!(BitReader::at(&data, pos).peek(n), peek_slow(&data, pos, n), "pos {pos} n {n}");
            }
        }
    }

    #[test]
    fn mixed_width_roundtrip() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut w = BitWriter::new();
        let mut items = Vec::new();
        for _ in 0..20_000 {
            let n = (next() % 33) as u32;
            let v = if n == 0 { 0 } else { (next() as u32) >> (32 - n) };
            w.u(n, v);
            items.push((n, v));
        }
        let bits = w.bit_len();
        let bytes = w.into_bytes();
        assert_eq!(bytes.len(), bits.div_ceil(8));
        let mut r = BitReader::new(&bytes);
        for (n, v) in items {
            assert_eq!(r.u(n).unwrap(), v);
        }
    }

    #[test]
    fn stop_bit() {
        assert_eq!(last_one_bit(&[0x80]), Some(0));
        assert_eq!(last_one_bit(&[0x12, 0x40, 0x00]), Some(9));
        assert_eq!(last_one_bit(&[0, 0]), None);
    }
}
