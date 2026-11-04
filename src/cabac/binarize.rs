//! CABAC binarizations (9.3.2) as pure functions; contexts are assigned by the caller.

/// A bin string of at most 64 bins: bin binIdx is bit binIdx of `bits`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bins {
    pub bits: u64,
    pub len: u32,
}

impl Bins {
    /// Append a bin; bins beyond the 64th are dropped (match functions report Invalid by then).
    #[inline]
    pub fn push(&mut self, bin: bool) {
        debug_assert!(self.len < 64, "bin string longer than 64 bins");
        if self.len < 64 {
            self.bits |= (bin as u64) << self.len;
            self.len += 1;
        }
    }

    /// Bin `bin_idx`; zero beyond the 64 storable bins.
    #[inline]
    pub fn get(&self, bin_idx: u32) -> bool {
        bin_idx < 64 && (self.bits >> bin_idx) & 1 == 1
    }

    /// Parse "1 0 0" or "100" (whitespace ignored); for tests and tables.
    pub fn parse(s: &str) -> Self {
        let mut b = Bins::default();
        for c in s.chars().filter(|c| !c.is_whitespace()) {
            match c {
                '0' => b.push(false),
                '1' => b.push(true),
                _ => panic!("bad bin character {c:?}"),
            }
        }
        b
    }

    /// Append the low `n` bits of `v`, most significant first.
    fn push_msb_first(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.push((v >> i) & 1 == 1);
        }
    }

    /// The first `len` bins with anything above them cleared.
    #[inline]
    fn masked(&self) -> u64 {
        if self.len >= 64 { self.bits } else { self.bits & ((1u64 << self.len) - 1) }
    }

    /// Number of leading bins equal to 1.
    #[inline]
    fn leading_ones(&self) -> u32 {
        (!self.masked()).trailing_zeros().min(self.len)
    }
}

/// Result of matching the bins decoded so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match<T> {
    /// a complete bin string of this value
    Done(T),
    /// a proper prefix of at least one bin string
    More,
    /// no bin string starts with these bins
    Invalid,
}

/// A string that needs more bins than a `Bins` can hold can never complete.
#[inline]
fn bounded<T>(m: Match<T>, b: Bins) -> Match<T> {
    if matches!(m, Match::More) && b.len >= 64 { Match::Invalid } else { m }
}

fn ones(n: u32) -> Bins {
    debug_assert!(n <= 64);
    Bins { bits: if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }, len: n.min(64) }
}

/// U (9.3.2.1)
pub fn unary(v: u32) -> Bins {
    debug_assert!(v < 64);
    let mut b = ones(v);
    b.push(false);
    b
}

#[inline]
pub fn match_unary(b: Bins) -> Match<u32> {
    let n = b.leading_ones();
    if n == b.len {
        bounded(Match::More, b)
    } else if n + 1 == b.len {
        Match::Done(n)
    } else {
        Match::Invalid
    }
}

/// TU with cMax (9.3.2.2)
pub fn truncated_unary(v: u32, c_max: u32) -> Bins {
    debug_assert!(v <= c_max);
    if v < c_max { unary(v) } else { ones(c_max) }
}

#[inline]
pub fn match_truncated_unary(b: Bins, c_max: u32) -> Match<u32> {
    let n = b.leading_ones();
    if b.len > c_max {
        Match::Invalid
    } else if n == b.len {
        if b.len == c_max { Match::Done(c_max) } else { bounded(Match::More, b) }
    } else if n + 1 == b.len {
        Match::Done(n)
    } else {
        Match::Invalid
    }
}

/// UEGk (9.3.2.3): TU prefix with cMax = uCoff, k-th order Exp-Golomb suffix, sign
/// (mvd: k = 3, signed, uCoff = 9; coeff_abs_level_minus1: k = 0, unsigned, uCoff = 14)
pub fn ueg(v: i32, k: u32, signed: bool, u_coff: u32) -> Bins {
    debug_assert!(u_coff > 0 && k < 32);
    debug_assert!(signed || v >= 0);
    debug_assert!(v != i32::MIN);
    let abs = v.unsigned_abs();
    let mut b = truncated_unary(abs.min(u_coff), u_coff);
    if abs >= u_coff {
        // Equation 9-6
        let mut suf = abs - u_coff;
        let mut k = k;
        while suf >= (1 << k) {
            b.push(true);
            suf -= 1 << k;
            k += 1;
        }
        b.push(false);
        b.push_msb_first(suf, k);
    }
    if signed && v != 0 {
        b.push(v < 0);
    }
    b
}

pub fn match_ueg(b: Bins, k: u32, signed: bool, u_coff: u32) -> Match<i32> {
    if k >= 32 {
        return Match::Invalid;
    }
    let n = b.leading_ones();
    if n < u_coff {
        // the prefix ends with a 0 bin (or is still running)
        if n == b.len {
            return bounded(Match::More, b);
        }
        let sign = signed && n > 0;
        let total = n + 1 + sign as u32;
        return match b.len.cmp(&total) {
            std::cmp::Ordering::Less => Match::More,
            std::cmp::Ordering::Greater => Match::Invalid,
            std::cmp::Ordering::Equal if sign && b.get(n + 1) => Match::Done(-(n as i32)),
            std::cmp::Ordering::Equal => Match::Done(n as i32),
        };
    }
    // The prefix is uCoff ones; the Exp-Golomb suffix follows.
    let mut pos = u_coff;
    let mut k = k;
    let mut suf: u64 = 0;
    while b.get(pos) && pos < b.len {
        suf += 1 << k;
        k += 1;
        pos += 1;
        if k >= 32 {
            return Match::Invalid;
        }
    }
    if pos >= b.len {
        return bounded(Match::More, b);
    }
    pos += 1; // the terminating 0
    if pos + k > b.len {
        return bounded(Match::More, b);
    }
    for i in 0..k {
        suf += (b.get(pos + i) as u64) << (k - 1 - i);
    }
    pos += k;
    let abs = u_coff as u64 + suf;
    if abs > i32::MAX as u64 {
        return Match::Invalid;
    }
    let abs = abs as i32;
    if !signed {
        return if b.len == pos { Match::Done(abs) } else { Match::Invalid };
    }
    match b.len.cmp(&(pos + 1)) {
        std::cmp::Ordering::Less => bounded(Match::More, b),
        std::cmp::Ordering::Greater => Match::Invalid,
        std::cmp::Ordering::Equal => Match::Done(if b.get(pos) { -abs } else { abs }),
    }
}

/// Number of bins of FL with cMax: Ceil( Log2( cMax + 1 ) ).
#[inline]
fn fixed_length_bins(c_max: u32) -> u32 {
    32 - c_max.leading_zeros()
}

/// FL with cMax (9.3.2.4); binIdx 0 is the least significant bit
pub fn fixed_length(v: u32, c_max: u32) -> Bins {
    debug_assert!(v <= c_max);
    let n = fixed_length_bins(c_max);
    Bins { bits: v as u64 & ((1u64 << n) - 1), len: n }
}

#[inline]
pub fn match_fixed_length(b: Bins, c_max: u32) -> Match<u32> {
    let n = fixed_length_bins(c_max);
    match b.len.cmp(&n) {
        std::cmp::Ordering::Less => Match::More,
        std::cmp::Ordering::Greater => Match::Invalid,
        std::cmp::Ordering::Equal if b.masked() > c_max as u64 => Match::Invalid,
        std::cmp::Ordering::Equal => Match::Done(b.masked() as u32),
    }
}

/// mb_type in I slices, 0..=25 (Table 9-36); also the suffix of intra types in P and B slices
///
/// Types 1..=24 are I_16x16_<pred>_<chroma>_<luma>: bin 0 = 1, bin 1 = 0 (not I_PCM), bin 2 = luma
/// flag (types 13..=24), then the chroma pattern (0, "1 0", "1 1" for 0, 1, 2) and the two bins of the
/// prediction mode, most significant first.
pub fn mb_type_i(t: u32) -> Bins {
    debug_assert!(t <= 25);
    let mut b = Bins::default();
    match t {
        0 => b.push(false),
        25 => b.push_msb_first(0b11, 2),
        _ => {
            let u = t - 1;
            b.push_msb_first(0b10, 2);
            b.push(u >= 12);
            match (u >> 2) % 3 {
                0 => b.push(false),
                c => b.push_msb_first(0b10 | (c - 1), 2),
            }
            b.push_msb_first(u & 3, 2);
        }
    }
    b
}

pub fn match_mb_type_i(b: Bins) -> Match<u32> {
    let (len, bit) = (b.len, |i| b.get(i) as u32);
    if len == 0 {
        return Match::More;
    }
    if bit(0) == 0 {
        return if len == 1 { Match::Done(0) } else { Match::Invalid };
    }
    if len == 1 {
        return Match::More;
    }
    if bit(1) == 1 {
        return if len == 2 { Match::Done(25) } else { Match::Invalid };
    }
    if len < 4 {
        return Match::More;
    }
    // chroma pattern: a single 0, or 1 followed by 0 / 1
    let (chroma, total) = if bit(3) == 0 { (0, 6) } else { (1 + bit(4), 7) };
    match len.cmp(&total) {
        std::cmp::Ordering::Less => Match::More,
        std::cmp::Ordering::Greater => Match::Invalid,
        std::cmp::Ordering::Equal => {
            let pred = bit(total - 2) << 1 | bit(total - 1);
            Match::Done(1 + pred + 4 * chroma + 12 * bit(2))
        }
    }
}

/// An entry of a bin string table: syntax element value, code (first bin is the most significant
/// bit) and number of bins.
type Code = (u32, u32, u32);

fn code_bins(&(_, code, len): &Code) -> Bins {
    let mut b = Bins::default();
    b.push_msb_first(code, len);
    b
}

/// Match against a table of complete bin strings.
fn match_table(b: Bins, table: &[Code]) -> Match<u32> {
    let mut more = false;
    for &(value, code, len) in table {
        if b.len > len {
            continue;
        }
        // the first b.len bins of the code, most significant first, are the low bits reversed
        if (0..b.len).all(|i| b.get(i) == ((code >> (len - 1 - i)) & 1 == 1)) {
            if b.len == len {
                return Match::Done(value);
            }
            more = true;
        }
    }
    if more { Match::More } else { Match::Invalid }
}

/// mb_type in P/SP slices (Table 9-37); the last entry is the prefix of the intra types.
const MB_TYPE_P: [Code; 5] = [(0, 0b000, 3), (1, 0b011, 3), (2, 0b010, 3), (3, 0b001, 3), (5, 0b1, 1)];

/// mb_type in B slices (Table 9-37); the last entry is the prefix of the intra types.
const MB_TYPE_B: [Code; 24] = [
    (0, 0b0, 1),
    (1, 0b100, 3),
    (2, 0b101, 3),
    (3, 0b110000, 6),
    (4, 0b110001, 6),
    (5, 0b110010, 6),
    (6, 0b110011, 6),
    (7, 0b110100, 6),
    (8, 0b110101, 6),
    (9, 0b110110, 6),
    (10, 0b110111, 6),
    (11, 0b111110, 6),
    (12, 0b1110000, 7),
    (13, 0b1110001, 7),
    (14, 0b1110010, 7),
    (15, 0b1110011, 7),
    (16, 0b1110100, 7),
    (17, 0b1110101, 7),
    (18, 0b1110110, 7),
    (19, 0b1110111, 7),
    (20, 0b1111000, 7),
    (21, 0b1111001, 7),
    (22, 0b111111, 6),
    (23, 0b111101, 6),
];

/// sub_mb_type in P/SP slices (Table 9-38)
const SUB_MB_TYPE_P: [Code; 4] = [(0, 0b1, 1), (1, 0b00, 2), (2, 0b011, 3), (3, 0b010, 3)];

/// sub_mb_type in B slices (Table 9-38)
const SUB_MB_TYPE_B: [Code; 13] = [
    (0, 0b0, 1),
    (1, 0b100, 3),
    (2, 0b101, 3),
    (3, 0b11000, 5),
    (4, 0b11001, 5),
    (5, 0b11010, 5),
    (6, 0b11011, 5),
    (7, 0b111000, 6),
    (8, 0b111001, 6),
    (9, 0b111010, 6),
    (10, 0b111011, 6),
    (11, 0b11110, 5),
    (12, 0b11111, 5),
];

/// Prefix of mb_type in P/SP slices (Table 9-37): the whole string for 0..=3; for the
/// intra types 5..=30 only the prefix "1" (the caller codes the suffix with mb_type_i(t - 5)).
/// match returns Done(5) for the intra prefix. 4 (P_8x8ref0) has no bin string.
pub fn mb_type_p_prefix(t: u32) -> Bins {
    debug_assert!(t <= 30 && t != 4);
    code_bins(&MB_TYPE_P[if t >= 5 { 4 } else { t as usize & 3 }])
}

pub fn match_mb_type_p_prefix(b: Bins) -> Match<u32> {
    match_table(b, &MB_TYPE_P)
}

/// Prefix of mb_type in B slices (Table 9-37): the whole string for 0..=22; for the intra
/// types 23..=48 the prefix "1 1 1 1 0 1"; match returns Done(23) for it.
pub fn mb_type_b_prefix(t: u32) -> Bins {
    debug_assert!(t <= 48);
    code_bins(&MB_TYPE_B[t.min(23) as usize])
}

pub fn match_mb_type_b_prefix(b: Bins) -> Match<u32> {
    match_table(b, &MB_TYPE_B)
}

/// sub_mb_type in P/SP slices, 0..=3 (Table 9-38)
pub fn sub_mb_type_p(t: u32) -> Bins {
    debug_assert!(t <= 3);
    code_bins(&SUB_MB_TYPE_P[t as usize & 3])
}

pub fn match_sub_mb_type_p(b: Bins) -> Match<u32> {
    match_table(b, &SUB_MB_TYPE_P)
}

/// sub_mb_type in B slices, 0..=12 (Table 9-38)
pub fn sub_mb_type_b(t: u32) -> Bins {
    debug_assert!(t <= 12);
    code_bins(&SUB_MB_TYPE_B[t.min(12) as usize])
}

pub fn match_sub_mb_type_b(b: Bins) -> Match<u32> {
    match_table(b, &SUB_MB_TYPE_B)
}

/// mb_qp_delta (9.3.2.7): Table 9-3 mapping, then U
pub fn mb_qp_delta(d: i32) -> Bins {
    // 0, 1, -1, 2, -2, ... map to 0, 1, 2, 3, 4, ...
    let mapped = if d > 0 { 2 * d as u32 - 1 } else { d.unsigned_abs().wrapping_mul(2) };
    unary(mapped)
}

#[inline]
pub fn match_mb_qp_delta(b: Bins) -> Match<i32> {
    match match_unary(b) {
        Match::Done(k) if k & 1 == 1 => Match::Done(k.div_ceil(2) as i32),
        Match::Done(k) => Match::Done(-((k / 2) as i32)),
        Match::More => Match::More,
        Match::Invalid => Match::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check a matcher against its encoder on every bin string up to `max_len` bins: Done only for
    /// the encoding of the value, More only when some extension leads on, Invalid is final.
    fn check_exhaustive<T: Copy + std::fmt::Debug + PartialEq>(
        max_len: u32,
        matcher: impl Fn(Bins) -> Match<T>,
        encode: impl Fn(T) -> Bins,
    ) {
        fn ext(b: Bins, bin: bool) -> Bins {
            let mut e = b;
            e.push(bin);
            e
        }
        for len in 0..=max_len {
            for bits in 0..(1u64 << len) {
                let b = Bins { bits, len };
                match matcher(b) {
                    Match::Done(v) => assert_eq!(encode(v), b, "{b:?}"),
                    Match::More => {
                        let live = [false, true].iter().any(|&x| matcher(ext(b, x)) != Match::Invalid);
                        assert!(live, "dead end {b:?}");
                    }
                    Match::Invalid => {
                        for x in [false, true] {
                            assert_eq!(matcher(ext(b, x)), Match::Invalid, "{b:?}");
                        }
                    }
                }
            }
        }
    }

    /// Feed `b` bin by bin: every proper prefix must give More and the whole string Done(v).
    fn check_roundtrip<T: Copy + std::fmt::Debug + PartialEq>(b: Bins, v: T, matcher: impl Fn(Bins) -> Match<T>) {
        let mut acc = Bins::default();
        for i in 0..b.len {
            assert_eq!(matcher(acc), Match::More, "{v:?}: prefix of {i} bins of {b:?}");
            acc.push(b.get(i));
        }
        assert_eq!(matcher(acc), Match::Done(v), "{v:?}: {b:?}");
    }

    /// No string is a proper prefix of another one (and all are distinct).
    fn check_prefix_free(strings: &[Bins]) {
        for (i, a) in strings.iter().enumerate() {
            for (j, b) in strings.iter().enumerate() {
                if i != j {
                    let prefix = a.len <= b.len && (0..a.len).all(|k| a.get(k) == b.get(k));
                    assert!(!prefix, "{a:?} is a prefix of {b:?}");
                }
            }
        }
    }

    /// Feed `n` ones and report the first Invalid (or None if the matcher never gave up).
    fn first_invalid<T>(n: u32, matcher: impl Fn(Bins) -> Match<T>) -> Option<u32> {
        let mut b = Bins::default();
        for i in 1..=n {
            b.push(true);
            if matches!(matcher(b), Match::Invalid) {
                return Some(i);
            }
        }
        None
    }

    #[test]
    fn bins_basics() {
        let mut b = Bins::default();
        b.push(true);
        b.push(false);
        b.push(true);
        assert_eq!(b, Bins { bits: 0b101, len: 3 });
        assert!(b.get(0) && !b.get(1) && b.get(2) && !b.get(3) && !b.get(200));
        assert_eq!(Bins::parse("1 0 1"), b);
        assert_eq!(Bins::parse("101"), b);
        assert_eq!(Bins::parse(" "), Bins::default());
    }

    #[test]
    fn unary_table_9_35() {
        for (v, s) in [(0, "0"), (1, "1 0"), (2, "1 1 0"), (3, "1 1 1 0"), (4, "1 1 1 1 0"), (5, "1 1 1 1 1 0")] {
            assert_eq!(unary(v), Bins::parse(s));
        }
    }

    #[test]
    fn unary_roundtrip_and_invalid() {
        for v in 0..64 {
            check_roundtrip(unary(v), v, match_unary);
        }
        check_exhaustive(12, match_unary, unary);
        // a zero ends the string; anything after it is invalid
        assert_eq!(match_unary(Bins::parse("1 1 0 1")), Match::Invalid);
        assert_eq!(match_unary(Bins::parse("0 0")), Match::Invalid);
        assert_eq!(match_unary(Bins::default()), Match::More);
        // value 63 is the longest: 64 ones can only be a prefix of something unrepresentable
        assert_eq!(first_invalid(64, match_unary), Some(64));
        assert_eq!(match_unary(Bins { bits: u64::MAX, len: 64 }), Match::Invalid);
        assert_eq!(match_unary(Bins { bits: u64::MAX >> 1, len: 64 }), Match::Done(63));
    }

    #[test]
    fn truncated_unary_strings() {
        // cMax = 2 (CodedBlockPatternChroma)
        assert_eq!(truncated_unary(0, 2), Bins::parse("0"));
        assert_eq!(truncated_unary(1, 2), Bins::parse("1 0"));
        assert_eq!(truncated_unary(2, 2), Bins::parse("1 1"));
        // cMax = 1 is a single bin
        assert_eq!(truncated_unary(0, 1), Bins::parse("0"));
        assert_eq!(truncated_unary(1, 1), Bins::parse("1"));
        assert_eq!(truncated_unary(5, 5), Bins::parse("1 1 1 1 1"));
        assert_eq!(truncated_unary(64, 64), Bins { bits: u64::MAX, len: 64 });
    }

    #[test]
    fn truncated_unary_roundtrip_and_invalid() {
        for c_max in 1..=40 {
            for v in 0..=c_max {
                check_roundtrip(truncated_unary(v, c_max), v, |b| match_truncated_unary(b, c_max));
            }
        }
        for c_max in [1, 2, 3, 6] {
            check_exhaustive(9, |b| match_truncated_unary(b, c_max), |v| truncated_unary(v, c_max));
        }
        assert_eq!(match_truncated_unary(Bins::parse("1 1 1"), 2), Match::Invalid);
        assert_eq!(match_truncated_unary(Bins::parse("1 1 0"), 2), Match::Invalid);
        assert_eq!(match_truncated_unary(Bins::parse("1 1"), 2), Match::Done(2));
        assert_eq!(match_truncated_unary(Bins::parse("1"), 2), Match::More);
        assert_eq!(first_invalid(64, |b| match_truncated_unary(b, 100)), Some(64));
        assert_eq!(match_truncated_unary(Bins::default(), 0), Match::Done(0));
    }

    #[test]
    fn ueg0_hand_worked() {
        // coeff_abs_level_minus1: UEG0, uCoff = 14, unsigned. Below uCoff only the TU prefix.
        let c = |v| ueg(v, 0, false, 14);
        assert_eq!(c(0), Bins::parse("0"));
        assert_eq!(c(1), Bins::parse("1 0"));
        assert_eq!(c(13), Bins::parse("1111111111111 0"));
        // 14: prefix 14 ones, sufS = 0 < 1: "0", no k bits
        assert_eq!(c(14), Bins::parse(&format!("{}0", "1".repeat(14))));
        // 15: sufS = 1 >= 1: "1", sufS = 0, k = 1; 0 < 2: "0" then 1 bit "0"
        assert_eq!(c(15), Bins::parse(&format!("{}1 0 0", "1".repeat(14))));
        // 16: sufS = 2: "1", sufS = 1, k = 1; 1 < 2: "0" then bit "1"
        assert_eq!(c(16), Bins::parse(&format!("{}1 0 1", "1".repeat(14))));
        // 17: sufS = 3: "1", sufS = 2, k = 1; 2 >= 2: "1", sufS = 0, k = 2; "0" then bits "00"
        assert_eq!(c(17), Bins::parse(&format!("{}1 1 0 00", "1".repeat(14))));
        // 29: sufS = 15: 1 (->14, k=1), 1 (->12, k=2), 1 (->8, k=3), 1 (->0, k=4), "0", 4 bits "0000"
        assert_eq!(c(29), Bins::parse(&format!("{}1111 0 0000", "1".repeat(14))));
        // 30: sufS = 16: 1111 (->1, k=4), 0 (1 < 16), bits 0001
        assert_eq!(c(30), Bins::parse(&format!("{}1111 0 0001", "1".repeat(14))));
    }

    #[test]
    fn ueg3_hand_worked() {
        // mvd: UEG3, uCoff = 9, signed.
        let m = |v| ueg(v, 3, true, 9);
        assert_eq!(m(0), Bins::parse("0"));
        // short prefix: ones, a 0, then the sign (0 = positive, 1 = negative)
        assert_eq!(m(1), Bins::parse("1 0 0"));
        assert_eq!(m(-1), Bins::parse("1 0 1"));
        assert_eq!(m(8), Bins::parse("11111111 0 0"));
        assert_eq!(m(-8), Bins::parse("11111111 0 1"));
        // 9: prefix 9 ones, sufS = 0 < 8: "0" then 3 bits "000", sign
        assert_eq!(m(9), Bins::parse("111111111 0 000 0"));
        assert_eq!(m(-9), Bins::parse("111111111 0 000 1"));
        // 10: sufS = 1: "0", "001"
        assert_eq!(m(10), Bins::parse("111111111 0 001 0"));
        assert_eq!(m(-10), Bins::parse("111111111 0 001 1"));
        // 16: sufS = 7: "0", "111"
        assert_eq!(m(16), Bins::parse("111111111 0 111 0"));
        // 17: sufS = 8 >= 8: "1", sufS = 0, k = 4; "0", then 4 bits "0000"
        assert_eq!(m(17), Bins::parse("111111111 1 0 0000 0"));
        assert_eq!(m(-17), Bins::parse("111111111 1 0 0000 1"));
        // 100: sufS = 91: 1 (->83, k=4), 1 (->67, k=5), 1 (->35, k=6); 35 < 64: "0", 6 bits 100011
        assert_eq!(m(100), Bins::parse("111111111 111 0 100011 0"));
        assert_eq!(m(-100), Bins::parse("111111111 111 0 100011 1"));
        // a large value, from the closed form: n ones where 8 * (2^n - 1) <= sufS < 8 * (2^(n+1) - 1)
        let big = ueg(100_000, 3, true, 9);
        // sufS = 99991; 8 * (2^13 - 1) = 65528 <= 99991 < 8 * (2^14 - 1) = 131064: n = 13,
        // remainder 99991 - 65528 = 34463 in 3 + 13 = 16 bits
        assert_eq!(big.len, 9 + 13 + 1 + 16 + 1);
        assert_eq!(big.leading_ones(), 9 + 13);
        let rem = (0..16).fold(0u32, |a, i| a << 1 | big.get(9 + 13 + 1 + i) as u32);
        assert_eq!(rem, 34463);
        assert!(!big.get(big.len - 1));
    }

    #[test]
    fn ueg_roundtrips() {
        // strings longer than 64 bins cannot be stored: keep the values below 2^24 for most
        // parameters, and use uCoff = 1, k = 0 (the shortest strings) for the largest values
        let mut values: Vec<i32> = (0..3000).collect();
        let mut x = 1u32;
        while x < 1 << 24 {
            values.extend([x as i32 - 1, x as i32, x as i32 + 1]);
            x = x * 3 / 2 + 1;
        }
        for &v in &values {
            for (k, u_coff) in [(0, 14), (3, 9), (0, 1), (2, 5)] {
                check_roundtrip(ueg(v, k, false, u_coff), v, |b| match_ueg(b, k, false, u_coff));
                for s in [v, -v] {
                    check_roundtrip(ueg(s, k, true, u_coff), s, |b| match_ueg(b, k, true, u_coff));
                }
            }
        }
        let mut v = 1i32 << 24;
        while v < i32::MAX / 2 {
            for x in [v - 1, v, v + 1, i32::MAX - v] {
                check_roundtrip(ueg(x, 0, false, 1), x, |b| match_ueg(b, 0, false, 1));
                for s in [x, -x] {
                    check_roundtrip(ueg(s, 0, true, 1), s, |b| match_ueg(b, 0, true, 1));
                }
            }
            v = v / 2 * 3 + 1;
        }
        let max = i32::MAX;
        check_roundtrip(ueg(max, 0, false, 1), max, |b| match_ueg(b, 0, false, 1));
        check_roundtrip(ueg(-max, 0, true, 1), -max, |b| match_ueg(b, 0, true, 1));
    }

    #[test]
    fn ueg_exhaustive_small_strings() {
        check_exhaustive(16, |b| match_ueg(b, 0, false, 14), |v| ueg(v, 0, false, 14));
        check_exhaustive(16, |b| match_ueg(b, 3, true, 9), |v| ueg(v, 3, true, 9));
        check_exhaustive(12, |b| match_ueg(b, 1, true, 2), |v| ueg(v, 1, true, 2));
        check_exhaustive(12, |b| match_ueg(b, 2, false, 3), |v| ueg(v, 2, false, 3));
    }

    #[test]
    fn ueg_garbage_ends_in_invalid() {
        // all ones: the Exp-Golomb part would reach k = 32
        assert_eq!(first_invalid(64, |b| match_ueg(b, 0, false, 14)), Some(14 + 32));
        assert_eq!(first_invalid(64, |b| match_ueg(b, 3, true, 9)), Some(9 + 29));
        // k itself out of range
        assert_eq!(match_ueg(Bins::parse("0"), 32, false, 14), Match::Invalid);
        // trailing bins after a complete string
        assert_eq!(match_ueg(Bins::parse("0 0"), 0, false, 14), Match::Invalid);
        assert_eq!(match_ueg(Bins::parse("1 0 0 0"), 3, true, 9), Match::Invalid);
        assert_eq!(match_ueg(Bins::parse("0 1"), 3, true, 9), Match::Invalid);
        // magnitude beyond i32: uCoff 1, k = 0: 1 + 31 ones, 0, 31 one bits = 64 bins, sufS >= 2^31
        let mut b = Bins::parse(&"1".repeat(32));
        b.push(false);
        for _ in 0..31 {
            b.push(true);
        }
        assert_eq!(match_ueg(b, 0, false, 1), Match::Invalid);
        // the largest magnitude is still accepted
        let b = ueg(i32::MAX, 0, false, 1);
        assert_eq!(match_ueg(b, 0, false, 1), Match::Done(i32::MAX));
        // 64 bins that still need more bins
        let b = Bins { bits: u64::MAX >> 1, len: 64 };
        assert_eq!(match_ueg(b, 0, false, 14), Match::Invalid);
        assert_eq!(match_ueg(Bins { bits: u64::MAX, len: 64 }, 0, false, 64), Match::Invalid);
    }

    #[test]
    fn fixed_length_strings() {
        // FL: binIdx 0 is the least significant bit
        assert_eq!(fixed_length(0, 15), Bins::parse("0 0 0 0"));
        assert_eq!(fixed_length(1, 15), Bins::parse("1 0 0 0"));
        assert_eq!(fixed_length(6, 15), Bins::parse("0 1 1 0"));
        assert_eq!(fixed_length(15, 15), Bins::parse("1 1 1 1"));
        // cMax = 7 and 8 differ in length: Ceil( Log2( cMax + 1 ) )
        assert_eq!(fixed_length(5, 7), Bins::parse("1 0 1"));
        assert_eq!(fixed_length(8, 8), Bins::parse("0 0 0 1"));
        assert_eq!(fixed_length(1, 1), Bins::parse("1"));
        assert_eq!(fixed_length(0, 0), Bins::default());
    }

    #[test]
    fn fixed_length_roundtrip_and_invalid() {
        for c_max in [1, 2, 3, 5, 7, 8, 15, 63, 255] {
            for v in 0..=c_max {
                check_roundtrip(fixed_length(v, c_max), v, |b| match_fixed_length(b, c_max));
            }
        }
        assert_eq!(match_fixed_length(Bins::default(), 0), Match::Done(0));
        // 3 > cMax = 2
        assert_eq!(match_fixed_length(Bins::parse("1 1"), 2), Match::Invalid);
        // too many bins
        assert_eq!(match_fixed_length(Bins::parse("1 0 1 0 0"), 15), Match::Invalid);
        assert_eq!(match_fixed_length(Bins::parse("1"), 15), Match::More);
    }

    #[test]
    fn coded_block_pattern_composition() {
        // 9.3.2.6: FL(luma, 15) followed by TU(chroma, 2)
        let cbp = |luma, chroma| {
            let mut b = fixed_length(luma, 15);
            let t = truncated_unary(chroma, 2);
            for i in 0..t.len {
                b.push(t.get(i));
            }
            b
        };
        assert_eq!(cbp(0, 0), Bins::parse("0000 0"));
        assert_eq!(cbp(15, 0), Bins::parse("1111 0"));
        assert_eq!(cbp(15, 1), Bins::parse("1111 10"));
        assert_eq!(cbp(15, 2), Bins::parse("1111 11"));
        // luma bit 0 (8x8 block 0) is binIdx 0
        assert_eq!(cbp(0b0101, 1), Bins::parse("1010 10"));
        assert_eq!(cbp(0b1000, 2), Bins::parse("0001 11"));
    }

    const TABLE_9_36: [&str; 26] = [
        "0",
        "1 0 0 0 0 0",
        "1 0 0 0 0 1",
        "1 0 0 0 1 0",
        "1 0 0 0 1 1",
        "1 0 0 1 0 0 0",
        "1 0 0 1 0 0 1",
        "1 0 0 1 0 1 0",
        "1 0 0 1 0 1 1",
        "1 0 0 1 1 0 0",
        "1 0 0 1 1 0 1",
        "1 0 0 1 1 1 0",
        "1 0 0 1 1 1 1",
        "1 0 1 0 0 0",
        "1 0 1 0 0 1",
        "1 0 1 0 1 0",
        "1 0 1 0 1 1",
        "1 0 1 1 0 0 0",
        "1 0 1 1 0 0 1",
        "1 0 1 1 0 1 0",
        "1 0 1 1 0 1 1",
        "1 0 1 1 1 0 0",
        "1 0 1 1 1 0 1",
        "1 0 1 1 1 1 0",
        "1 0 1 1 1 1 1",
        "1 1",
    ];

    /// Table 9-37, P and SP slices (rows 0..=3 and the intra prefix, reported as 5).
    const TABLE_9_37_P: [(u32, &str); 5] = [(0, "0 0 0"), (1, "0 1 1"), (2, "0 1 0"), (3, "0 0 1"), (5, "1")];

    /// Table 9-37, B slices (the intra prefix is reported as 23).
    const TABLE_9_37_B: [(u32, &str); 24] = [
        (0, "0"),
        (1, "1 0 0"),
        (2, "1 0 1"),
        (3, "1 1 0 0 0 0"),
        (4, "1 1 0 0 0 1"),
        (5, "1 1 0 0 1 0"),
        (6, "1 1 0 0 1 1"),
        (7, "1 1 0 1 0 0"),
        (8, "1 1 0 1 0 1"),
        (9, "1 1 0 1 1 0"),
        (10, "1 1 0 1 1 1"),
        (11, "1 1 1 1 1 0"),
        (12, "1 1 1 0 0 0 0"),
        (13, "1 1 1 0 0 0 1"),
        (14, "1 1 1 0 0 1 0"),
        (15, "1 1 1 0 0 1 1"),
        (16, "1 1 1 0 1 0 0"),
        (17, "1 1 1 0 1 0 1"),
        (18, "1 1 1 0 1 1 0"),
        (19, "1 1 1 0 1 1 1"),
        (20, "1 1 1 1 0 0 0"),
        (21, "1 1 1 1 0 0 1"),
        (22, "1 1 1 1 1 1"),
        (23, "1 1 1 1 0 1"),
    ];

    /// Table 9-38, P and SP slices (checked against the page image; the text extraction is garbled).
    const TABLE_9_38_P: [&str; 4] = ["1", "0 0", "0 1 1", "0 1 0"];

    const TABLE_9_38_B: [&str; 13] = [
        "0",
        "1 0 0",
        "1 0 1",
        "1 1 0 0 0",
        "1 1 0 0 1",
        "1 1 0 1 0",
        "1 1 0 1 1",
        "1 1 1 0 0 0",
        "1 1 1 0 0 1",
        "1 1 1 0 1 0",
        "1 1 1 0 1 1",
        "1 1 1 1 0",
        "1 1 1 1 1",
    ];

    #[test]
    fn mb_type_i_table_9_36() {
        let all: Vec<Bins> = TABLE_9_36.iter().map(|s| Bins::parse(s)).collect();
        for (t, &b) in all.iter().enumerate() {
            assert_eq!(mb_type_i(t as u32), b, "mb_type {t}");
            check_roundtrip(b, t as u32, match_mb_type_i);
        }
        check_prefix_free(&all);
        check_exhaustive(9, match_mb_type_i, mb_type_i);
        // every string of up to 7 bins is a complete, a partial or an overlong string
        assert_eq!(match_mb_type_i(Bins::parse("0 0")), Match::Invalid);
        assert_eq!(match_mb_type_i(Bins::parse("1 1 0")), Match::Invalid);
        assert_eq!(match_mb_type_i(Bins::parse("1 0 0 0 0 0 0")), Match::Invalid);
        assert_eq!(match_mb_type_i(Bins::parse("1 0 0 1 0 0 0 0")), Match::Invalid);
        assert_eq!(match_mb_type_i(Bins::parse("1 0 0 1")), Match::More);
        assert_eq!(match_mb_type_i(Bins::default()), Match::More);
    }

    #[test]
    fn mb_type_p_table_9_37() {
        let all: Vec<Bins> = TABLE_9_37_P.iter().map(|&(_, s)| Bins::parse(s)).collect();
        for &(t, s) in &TABLE_9_37_P {
            let b = Bins::parse(s);
            assert_eq!(mb_type_p_prefix(t), b, "mb_type {t}");
            check_roundtrip(b, t, match_mb_type_p_prefix);
        }
        // every intra type has the prefix "1"
        for t in 5..=30 {
            assert_eq!(mb_type_p_prefix(t), Bins::parse("1"));
        }
        check_prefix_free(&all);
        check_exhaustive(6, match_mb_type_p_prefix, mb_type_p_prefix);
        // 4 (P_8x8ref0) has no bin string; no bin string decodes to it
        for len in 0..=3 {
            for bits in 0..1u64 << len {
                assert_ne!(match_mb_type_p_prefix(Bins { bits, len }), Match::Done(4));
            }
        }
        assert_eq!(match_mb_type_p_prefix(Bins::parse("1 0")), Match::Invalid);
        assert_eq!(match_mb_type_p_prefix(Bins::parse("0 0 1 0")), Match::Invalid);
        assert_eq!(match_mb_type_p_prefix(Bins::parse("0 0")), Match::More);
    }

    #[test]
    fn mb_type_b_table_9_37() {
        let all: Vec<Bins> = TABLE_9_37_B.iter().map(|&(_, s)| Bins::parse(s)).collect();
        for &(t, s) in &TABLE_9_37_B {
            let b = Bins::parse(s);
            assert_eq!(mb_type_b_prefix(t), b, "mb_type {t}");
            check_roundtrip(b, t, match_mb_type_b_prefix);
        }
        for t in 23..=48 {
            assert_eq!(mb_type_b_prefix(t), Bins::parse("1 1 1 1 0 1"));
        }
        check_prefix_free(&all);
        check_exhaustive(9, match_mb_type_b_prefix, mb_type_b_prefix);
        assert_eq!(match_mb_type_b_prefix(Bins::parse("0 0")), Match::Invalid);
        assert_eq!(match_mb_type_b_prefix(Bins::parse("1 1 1 1 1 1 0")), Match::Invalid);
        assert_eq!(match_mb_type_b_prefix(Bins::parse("1 1 1 1 0")), Match::More);
    }

    #[test]
    fn sub_mb_type_table_9_38() {
        let p: Vec<Bins> = TABLE_9_38_P.iter().map(|s| Bins::parse(s)).collect();
        for (t, &b) in p.iter().enumerate() {
            assert_eq!(sub_mb_type_p(t as u32), b, "P sub_mb_type {t}");
            check_roundtrip(b, t as u32, match_sub_mb_type_p);
        }
        check_prefix_free(&p);
        check_exhaustive(6, match_sub_mb_type_p, sub_mb_type_p);
        assert_eq!(match_sub_mb_type_p(Bins::parse("1 0")), Match::Invalid);
        assert_eq!(match_sub_mb_type_p(Bins::parse("0")), Match::More);

        let bb: Vec<Bins> = TABLE_9_38_B.iter().map(|s| Bins::parse(s)).collect();
        for (t, &b) in bb.iter().enumerate() {
            assert_eq!(sub_mb_type_b(t as u32), b, "B sub_mb_type {t}");
            check_roundtrip(b, t as u32, match_sub_mb_type_b);
        }
        check_prefix_free(&bb);
        check_exhaustive(9, match_sub_mb_type_b, sub_mb_type_b);
        assert_eq!(match_sub_mb_type_b(Bins::parse("0 0")), Match::Invalid);
        assert_eq!(match_sub_mb_type_b(Bins::parse("1 1 1 1 1 1")), Match::Invalid);
        assert_eq!(match_sub_mb_type_b(Bins::parse("1 1 1")), Match::More);
    }

    #[test]
    fn mb_qp_delta_table_9_3() {
        // mapped value 0, 1, 2, 3, 4, ... for 0, 1, -1, 2, -2, ...
        for (d, mapped) in [(0, 0), (1, 1), (-1, 2), (2, 3), (-2, 4), (3, 5), (-3, 6), (26, 51), (-26, 52)] {
            assert_eq!(mb_qp_delta(d), unary(mapped), "{d}");
        }
        assert_eq!(mb_qp_delta(0), Bins::parse("0"));
        assert_eq!(mb_qp_delta(-1), Bins::parse("1 1 0"));
        for d in -31..=31 {
            check_roundtrip(mb_qp_delta(d), d, match_mb_qp_delta);
        }
        check_exhaustive(10, match_mb_qp_delta, mb_qp_delta);
        assert_eq!(match_mb_qp_delta(Bins::parse("1 0 0")), Match::Invalid);
        assert_eq!(first_invalid(64, match_mb_qp_delta), Some(64));
    }

    #[test]
    fn matchers_never_panic_on_arbitrary_input() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x >> 58) as u32; // 0..=63
            let b = Bins { bits: x, len: if x & 0x100 != 0 { 64 } else { len } };
            let _ = (match_unary(b), match_truncated_unary(b, 9), match_ueg(b, 3, true, 9));
            let _ = (match_ueg(b, 0, false, 14), match_fixed_length(b, 15), match_mb_type_i(b));
            let _ = (match_mb_type_p_prefix(b), match_mb_type_b_prefix(b), match_sub_mb_type_p(b));
            let _ = (match_sub_mb_type_b(b), match_mb_qp_delta(b));
        }
    }
}
