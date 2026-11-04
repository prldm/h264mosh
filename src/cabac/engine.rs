//! CABAC arithmetic decoding and encoding engine (9.3.1.2, 9.3.3.2, 9.3.4).

use crate::bits::{BitReader, BitWriter};
use anyhow::{Result, bail};

// Table 9-44: rangeTabLPS[pStateIdx][qCodIRangeIdx]. Transcribed from the spec and checked
// against FFmpeg's libavcodec/cabac.c.
#[rustfmt::skip]
const RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205],
    [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [95, 116, 137, 158], [90, 110, 130, 150], [85, 104, 123, 142], [81, 99, 117, 135],
    [77, 94, 111, 128], [73, 89, 105, 122], [69, 85, 100, 116], [66, 80, 95, 110],
    [62, 76, 90, 104], [59, 72, 86, 99], [56, 69, 81, 94], [53, 65, 77, 89],
    [51, 62, 73, 85], [48, 59, 69, 80], [46, 56, 66, 76], [43, 53, 63, 72],
    [41, 50, 59, 69], [39, 48, 56, 65], [37, 45, 54, 62], [35, 43, 51, 59],
    [33, 41, 48, 56], [32, 39, 46, 53], [30, 37, 43, 50], [29, 35, 41, 48],
    [27, 33, 39, 45], [26, 31, 37, 43], [24, 30, 35, 41], [23, 28, 33, 39],
    [22, 27, 32, 37], [21, 26, 30, 35], [20, 24, 29, 33], [19, 23, 27, 31],
    [18, 22, 26, 30], [17, 21, 25, 28], [16, 20, 23, 27], [15, 19, 22, 25],
    [14, 18, 21, 24], [14, 17, 20, 23], [13, 16, 19, 22], [12, 15, 18, 21],
    [12, 14, 17, 20], [11, 14, 16, 19], [11, 13, 15, 18], [10, 12, 15, 17],
    [10, 12, 14, 16], [9, 11, 13, 15], [9, 11, 12, 14], [8, 10, 12, 14],
    [8, 9, 11, 13], [7, 9, 11, 12], [7, 9, 10, 12], [7, 8, 10, 11],
    [6, 8, 9, 11], [6, 7, 9, 10], [6, 7, 8, 9], [2, 2, 2, 2],
];

// Table 9-45: transIdxLPS and transIdxMPS, indexed by pStateIdx. Checked against FFmpeg's
// libavcodec/cabac.c.
#[rustfmt::skip]
const TRANS_IDX_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

#[rustfmt::skip]
const TRANS_IDX_MPS: [u8; 64] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
    33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48,
    49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

/// A context variable: probability state and most probable symbol (9.3.1.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// pStateIdx, 0..=63
    pub state: u8,
    /// valMPS
    pub mps: bool,
}

impl Context {
    /// Initialisation from a table entry (m, n) and SliceQPY (Equation 9-5).
    pub fn new(m: i8, n: i8, slice_qp: i32) -> Self {
        let pre = (((m as i32 * slice_qp.clamp(0, 51)) >> 4) + n as i32).clamp(1, 126);
        if pre <= 63 { Self { state: (63 - pre) as u8, mps: false } } else { Self { state: (pre - 64) as u8, mps: true } }
    }

    /// The state transition process (9.3.3.2.1.1).
    #[inline]
    fn update(&mut self, bin: bool) {
        let s = self.state as usize & 63;
        if bin == self.mps {
            self.state = TRANS_IDX_MPS[s];
        } else {
            if s == 0 {
                self.mps = !self.mps;
            }
            self.state = TRANS_IDX_LPS[s];
        }
    }
}

/// Arithmetic decoding engine.
pub struct Decoder<'a> {
    data: &'a [u8],
    /// bit position of the next bit to read; may run past the end of `data`
    pos: usize,
    /// codIRange, 256..=510 between calls
    range: u32,
    /// codIOffset, kept below 512 even for invalid streams
    offset: u32,
}

impl<'a> Decoder<'a> {
    /// Initialise the decoding engine at bit `pos` of `data` (9.3.1.2): codIRange = 510,
    /// codIOffset = read_bits(9). Fails if codIOffset is 510 or 511.
    pub fn new(data: &'a [u8], pos: usize) -> Result<Self> {
        let offset = BitReader::at(data, pos).peek(9);
        if offset >= 510 {
            bail!("invalid CABAC initial codIOffset {offset} at bit {pos}");
        }
        Ok(Self { data, pos: pos + 9, range: 510, offset })
    }

    /// Shift in `n` (<= 32) bits from the stream, zero padded past the end.
    #[inline]
    fn read(&mut self, n: u32) -> u32 {
        let v = BitReader::at(self.data, self.pos).peek(n);
        self.pos += n as usize;
        v
    }

    /// RenormD (9.3.3.2.2) in one step; `range` must be nonzero.
    #[inline]
    fn renorm(&mut self) {
        let shift = self.range.leading_zeros().saturating_sub(23);
        if shift > 0 {
            self.range <<= shift;
            self.offset = ((self.offset << shift) | self.read(shift)) & 0x1ff;
        }
    }

    /// DecodeDecision (9.3.3.2.1), including the state transition and RenormD.
    #[inline]
    pub fn decision(&mut self, ctx: &mut Context) -> bool {
        let lps = RANGE_TAB_LPS[ctx.state as usize & 63][(self.range as usize >> 6) & 3] as u32;
        self.range -= lps;
        let bin = if self.offset >= self.range {
            self.offset -= self.range;
            self.range = lps;
            !ctx.mps
        } else {
            ctx.mps
        };
        ctx.update(bin);
        self.renorm();
        bin
    }

    /// DecodeBypass (9.3.3.2.3).
    #[inline]
    pub fn bypass(&mut self) -> bool {
        self.offset = (self.offset << 1) | self.read(1);
        if self.offset >= self.range {
            self.offset = (self.offset - self.range) & 0x1ff;
            true
        } else {
            false
        }
    }

    /// DecodeTerminate (9.3.3.2.4, ctxIdx 276).
    #[inline]
    pub fn terminate(&mut self) -> bool {
        self.range -= 2;
        if self.offset >= self.range {
            true
        } else {
            self.renorm();
            false
        }
    }

    /// Bit position in `data` of the next bit the engine would read. After terminate()
    /// returned true this is just past the last bit of the arithmetic code word (for
    /// end_of_slice_flag: just past the rbsp_stop_one_bit; for I_PCM: where the
    /// pcm_alignment_zero_bits start).
    pub fn pos(&self) -> usize {
        self.pos
    }
}

/// Arithmetic encoding engine (9.3.4), writing into a BitWriter.
pub struct Encoder {
    w: BitWriter,
    low: u32,
    range: u32,
    first_bit: bool,
    outstanding: u64,
    bins: u64,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    /// InitEncoder (9.3.4.1) over an empty writer.
    pub fn new() -> Self {
        Self { w: BitWriter::new(), low: 0, range: 510, first_bit: true, outstanding: 0, bins: 0 }
    }

    /// PutBit (Figure 9-9): the bit, then the outstanding bits with the opposite value.
    fn put_bit(&mut self, b: bool) {
        if self.first_bit {
            self.first_bit = false;
        } else {
            self.w.flag(b);
        }
        let fill = if b { 0 } else { u32::MAX };
        while self.outstanding > 0 {
            let n = self.outstanding.min(32) as u32;
            self.w.u(n, fill >> (32 - n));
            self.outstanding -= n as u64;
        }
    }

    /// RenormE (9.3.4.3).
    #[inline]
    fn renorm(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put_bit(false);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put_bit(true);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    /// EncodeDecision (9.3.4.2) with the state transition and RenormE.
    #[inline]
    pub fn decision(&mut self, ctx: &mut Context, bin: bool) {
        let lps = RANGE_TAB_LPS[ctx.state as usize & 63][(self.range as usize >> 6) & 3] as u32;
        self.range -= lps;
        if bin != ctx.mps {
            self.low += self.range;
            self.range = lps;
        }
        ctx.update(bin);
        self.renorm();
        self.bins += 1;
    }

    /// EncodeBypass (9.3.4.4).
    #[inline]
    pub fn bypass(&mut self, bin: bool) {
        self.low <<= 1;
        if bin {
            self.low += self.range;
        }
        if self.low >= 1024 {
            self.low -= 1024;
            self.put_bit(true);
        } else if self.low < 512 {
            self.put_bit(false);
        } else {
            self.low -= 512;
            self.outstanding += 1;
        }
        self.bins += 1;
    }

    /// EncodeTerminate (9.3.4.5); bin == true also flushes (EncodeFlush), after which the
    /// last bit written is 1 (the rbsp_stop_one_bit when coding end_of_slice_flag).
    pub fn terminate(&mut self, bin: bool) {
        self.range -= 2;
        if bin {
            self.low += self.range;
            self.range = 2;
            self.renorm();
            self.put_bit((self.low >> 9) & 1 == 1);
            self.w.u(2, ((self.low >> 7) & 3) | 1);
        } else {
            self.renorm();
        }
        self.bins += 1;
    }

    /// Bits written so far; bits the engine still holds (codILow, bitsOutstanding) are
    /// not counted until a flush.
    pub fn bit_len(&self) -> usize {
        self.w.bit_len()
    }

    /// Bins coded so far by decision, bypass and terminate (the slice's share of
    /// BinCountsInNALunits, 9.3.4.6).
    pub fn bin_count(&self) -> u64 {
        self.bins
    }

    /// The output writer. Only meaningful right after a flush: raw bits (I_PCM alignment
    /// and samples) may then be written to it, followed by `restart`.
    pub fn writer(&mut self) -> &mut BitWriter {
        &mut self.w
    }

    /// Re-initialise the engine (9.3.4.1: codILow = 0, codIRange = 510, firstBitFlag = 1,
    /// bitsOutstanding = 0) to continue after raw bits; keeps the writer and bin count.
    pub fn restart(&mut self) {
        self.low = 0;
        self.range = 510;
        self.first_bit = true;
        self.outstanding = 0;
    }

    /// The finished output (call after terminate(true)).
    pub fn finish(self) -> BitWriter {
        self.w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            (self.next() >> 11) % n
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Op {
        Dec(usize, bool),
        Bypass(bool),
        Term(bool),
    }

    const NCTX: usize = 40;

    /// Contexts with varied (m, n, qp) initialisations plus skewed bin probabilities
    /// (probability of a 1 out of 1024).
    fn make_contexts(rng: &mut Rng) -> (Vec<Context>, Vec<u64>) {
        let qp = rng.below(60) as i32 - 4;
        let ctxs = (0..NCTX).map(|_| Context::new(rng.below(128) as i8 - 64, rng.below(128) as i8 - 40, qp)).collect();
        let probs = (0..NCTX).map(|_| [1, 20, 300, 512, 800, 1010, 1023][rng.below(7) as usize]).collect();
        (ctxs, probs)
    }

    fn make_ops(rng: &mut Rng, probs: &[u64], count: usize) -> Vec<Op> {
        (0..count)
            .map(|_| match rng.below(100) {
                0..=77 => {
                    let c = rng.below(NCTX as u64) as usize;
                    Op::Dec(c, rng.below(1024) < probs[c])
                }
                78..=94 => Op::Bypass(rng.below(2) == 1),
                _ => Op::Term(false),
            })
            .collect()
    }

    fn encode_ops(enc: &mut Encoder, ctxs: &mut [Context], ops: &[Op]) {
        for &op in ops {
            match op {
                Op::Dec(c, b) => enc.decision(&mut ctxs[c], b),
                Op::Bypass(b) => enc.bypass(b),
                Op::Term(b) => enc.terminate(b),
            }
        }
    }

    /// Decode with the same op kinds and contexts as `shape`, returning the decoded bins.
    fn decode_ops(dec: &mut Decoder, ctxs: &mut [Context], shape: &[Op]) -> Vec<Op> {
        shape
            .iter()
            .map(|&op| match op {
                Op::Dec(c, _) => Op::Dec(c, dec.decision(&mut ctxs[c])),
                Op::Bypass(_) => Op::Bypass(dec.bypass()),
                Op::Term(_) => Op::Term(dec.terminate()),
            })
            .collect()
    }

    fn encode_all(ops: &[Op], ctxs: &mut [Context]) -> (Vec<u8>, usize, u64) {
        let mut enc = Encoder::new();
        encode_ops(&mut enc, ctxs, ops);
        enc.terminate(true);
        let bins = enc.bin_count();
        let w = enc.finish();
        let bits = w.bit_len();
        (w.into_bytes(), bits, bins)
    }

    fn lengths() -> Vec<usize> {
        let mut v: Vec<usize> = (0..40).collect();
        v.extend([100, 1000, 10_000, 150_000, 300_000]);
        v
    }

    #[test]
    fn random_roundtrip() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for (i, len) in lengths().into_iter().enumerate() {
            for rep in 0..if len < 1000 { 20 } else { 2 } {
                let (init, probs) = make_contexts(&mut rng);
                let ops = make_ops(&mut rng, &probs, len);
                let mut enc_ctxs = init.clone();
                let (bytes, bits, bins) = encode_all(&ops, &mut enc_ctxs);
                assert_eq!(bins, len as u64 + 1);
                assert!(bits > 0 && bytes.len() == bits.div_ceil(8));
                assert_eq!((bytes[(bits - 1) / 8] >> (7 - (bits - 1) % 8)) & 1, 1, "last bit must be 1");

                let mut dec_ctxs = init;
                let mut dec = Decoder::new(&bytes, 0).unwrap();
                let got = decode_ops(&mut dec, &mut dec_ctxs, &ops);
                assert_eq!(got, ops, "sequence {i}/{rep} len {len}");
                assert_eq!(dec_ctxs, enc_ctxs);
                assert!(dec.terminate());
                assert_eq!(dec.pos(), bits, "sequence {i}/{rep} len {len}");
            }
        }
    }

    #[test]
    fn reencode_is_identical() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for len in lengths() {
            let (init, probs) = make_contexts(&mut rng);
            let mut shape = make_ops(&mut rng, &probs, len);
            let (bytes, bits, _) = encode_all(&shape, &mut init.clone());

            let mut dec = Decoder::new(&bytes, 0).unwrap();
            let mut dec_ctxs = init.clone();
            shape.push(Op::Term(true));
            let decoded = decode_ops(&mut dec, &mut dec_ctxs, &shape);
            assert_eq!(dec.pos(), bits);

            let mut enc = Encoder::new();
            let mut enc_ctxs = init;
            encode_ops(&mut enc, &mut enc_ctxs, &decoded);
            let w = enc.finish();
            assert_eq!(w.bit_len(), bits);
            assert_eq!(w.into_bytes(), bytes);
        }
    }

    #[test]
    fn restart_after_raw_bytes() {
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        for round in 0..200 {
            let (init, probs) = make_contexts(&mut rng);
            let len1 = rng.below(if round < 100 { 30 } else { 2000 }) as usize;
            let len2 = rng.below(if round < 100 { 30 } else { 2000 }) as usize;
            let ops1 = make_ops(&mut rng, &probs, len1);
            let ops2 = make_ops(&mut rng, &probs, len2);
            let raw: Vec<u8> = (0..rng.below(40)).map(|_| rng.next() as u8).collect();

            let mut enc_ctxs = init.clone();
            let mut enc = Encoder::new();
            encode_ops(&mut enc, &mut enc_ctxs, &ops1);
            enc.terminate(true);
            let stop = enc.writer().bit_len();
            let pad = (8 - stop % 8) % 8;
            enc.writer().u(pad as u32, 0);
            for &b in &raw {
                enc.writer().u(8, b as u32);
            }
            let raw_end = enc.writer().bit_len();
            enc.restart();
            encode_ops(&mut enc, &mut enc_ctxs, &ops2);
            enc.terminate(true);
            assert_eq!(enc.bin_count(), (len1 + len2 + 2) as u64);
            let w = enc.finish();
            let total = w.bit_len();
            let bytes = w.into_bytes();

            let mut dec_ctxs = init;
            let mut dec = Decoder::new(&bytes, 0).unwrap();
            assert_eq!(decode_ops(&mut dec, &mut dec_ctxs, &ops1), ops1);
            assert!(dec.terminate());
            assert_eq!(dec.pos(), stop);
            let raw_start = dec.pos().next_multiple_of(8);
            assert_eq!(raw_start + raw.len() * 8, raw_end);
            assert_eq!(&bytes[raw_start / 8..raw_end / 8], &raw[..]);

            let mut dec = Decoder::new(&bytes, raw_end).unwrap();
            assert_eq!(decode_ops(&mut dec, &mut dec_ctxs, &ops2), ops2);
            assert!(dec.terminate());
            assert_eq!(dec.pos(), total);
            assert_eq!(dec_ctxs, enc_ctxs);
        }
    }

    #[test]
    fn garbage_input_never_panics() {
        let mut rng = Rng(0x0bad_5eed_0bad_5eed);
        let mut buffers: Vec<Vec<u8>> = vec![vec![], vec![0], vec![0xff], vec![0x7f], vec![0xff; 64], vec![0; 64]];
        for _ in 0..200 {
            let n = rng.below(40) as usize;
            buffers.push((0..n).map(|_| rng.next() as u8).collect());
        }
        for buf in &buffers {
            for start in [0, 1, 7, 8, buf.len() * 8, buf.len() * 8 + 20] {
                let Ok(mut dec) = Decoder::new(buf, start) else { continue };
                let mut ctxs = [Context::default(); NCTX];
                for i in 0..5000 {
                    match rng.below(10) {
                        0..=6 => {
                            dec.decision(&mut ctxs[i % NCTX]);
                        }
                        7 | 8 => {
                            dec.bypass();
                        }
                        _ => {
                            dec.terminate();
                        }
                    }
                }
                let _ = dec.pos();
            }
        }
        let bad = [0xffu8, 0xff];
        assert!(Decoder::new(&bad, 0).is_err());
        assert!(Decoder::new(&[0xff, 0x80], 0).is_err());
        assert!(Decoder::new(&[0xfe, 0x00], 0).is_ok());
    }

    #[test]
    fn tables() {
        assert_eq!(RANGE_TAB_LPS[0], [128, 176, 208, 240]);
        assert_eq!(RANGE_TAB_LPS[62], [6, 7, 8, 9]);
        assert_eq!(RANGE_TAB_LPS[63], [2, 2, 2, 2]);
        for s in 0..64 {
            let row = RANGE_TAB_LPS[s];
            if s < 63 {
                assert!(row.windows(2).all(|w| w[0] < w[1]), "row {s} increasing");
            }
            if s > 0 && s < 63 {
                for q in 0..4 {
                    assert!(row[q] <= RANGE_TAB_LPS[s - 1][q], "column {q} at state {s}");
                }
            }
        }
        for (i, &t) in TRANS_IDX_MPS.iter().enumerate().take(62) {
            assert_eq!(t, i as u8 + 1);
        }
        assert_eq!(TRANS_IDX_MPS[62], 62);
        assert_eq!(TRANS_IDX_MPS[63], 63);
        assert_eq!(TRANS_IDX_LPS[0], 0);
        assert_eq!(TRANS_IDX_LPS[62], 38);
        assert_eq!(TRANS_IDX_LPS[63], 63);
        for i in 1..63 {
            assert!(TRANS_IDX_LPS[i] < i as u8, "LPS moves towards state 0 at {i}");
            assert!(TRANS_IDX_LPS[i] >= TRANS_IDX_LPS[i - 1], "monotonic at {i}");
        }
    }

    #[test]
    fn context_initialisation() {
        let c = |m, n, qp| Context::new(m, n, qp);
        // (20 * 26) >> 4 = 32, + (-15) = 17 -> state 46, MPS 0
        assert_eq!(c(20, -15, 26), Context { state: 46, mps: false });
        // preCtxState 63 -> state 0, MPS 0; 64 -> state 0, MPS 1
        assert_eq!(c(0, 63, 20), Context { state: 0, mps: false });
        assert_eq!(c(0, 64, 20), Context { state: 0, mps: true });
        // SliceQPY clipped to 51: (10 * 51) >> 4 = 31 -> state 32, MPS 0
        assert_eq!(c(10, 0, 100), Context { state: 32, mps: false });
        // SliceQPY clipped to 0: preCtxState = n = 80 -> state 16, MPS 1
        assert_eq!(c(10, 80, -5), Context { state: 16, mps: true });
        // preCtxState clipped to 1 -> state 62, MPS 0; clipped to 126 -> state 62, MPS 1
        assert_eq!(c(0, -20, 30), Context { state: 62, mps: false });
        assert_eq!(c(0, 127, 30), Context { state: 62, mps: true });
        // negative products round down: (-16 * 51) >> 4 = -51, + 70 = 19 -> state 44
        assert_eq!(c(-16, 70, 51), Context { state: 44, mps: false });
        // (-3 * 10) >> 4 = -2 (floor), + 65 = 63 -> state 0, MPS 0
        assert_eq!(c(-3, 65, 10), Context { state: 0, mps: false });
    }

    #[test]
    fn state_transitions() {
        let mut c = Context { state: 0, mps: false };
        c.update(true);
        assert_eq!(c, Context { state: 0, mps: true });
        c.update(true);
        assert_eq!(c, Context { state: 1, mps: true });
        c.update(false);
        assert_eq!(c, Context { state: 0, mps: true });
        let mut c = Context { state: 62, mps: true };
        c.update(true);
        assert_eq!(c.state, 62);
        c.update(false);
        assert_eq!(c, Context { state: 38, mps: true });
    }
}
