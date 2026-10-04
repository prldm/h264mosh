//! Annex B byte stream splitting, NAL unit header, emulation prevention (spec 7.3.1, 7.4.1, B.1).

use anyhow::{Result, bail};

pub const NAL_SLICE: u8 = 1;
pub const NAL_SLICE_DPA: u8 = 2;
pub const NAL_SLICE_DPC: u8 = 4;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;
pub const NAL_END_SEQ: u8 = 10;
pub const NAL_END_STREAM: u8 = 11;
pub const NAL_FILLER: u8 = 12;
pub const NAL_SPS_EXT: u8 = 13;
pub const NAL_PREFIX: u8 = 14;
pub const NAL_SUBSET_SPS: u8 = 15;
pub const NAL_AUX_SLICE: u8 = 19;
pub const NAL_SLICE_EXT: u8 = 20;
pub const NAL_SLICE_EXT_3D: u8 = 21;

pub fn nal_type_name(t: u8) -> &'static str {
    match t {
        1 => "slice",
        2 => "slice DPA",
        3 => "slice DPB",
        4 => "slice DPC",
        5 => "IDR slice",
        6 => "SEI",
        7 => "SPS",
        8 => "PPS",
        9 => "AUD",
        10 => "end of seq",
        11 => "end of stream",
        12 => "filler",
        13 => "SPS ext",
        14 => "prefix",
        15 => "subset SPS",
        16 => "DPS",
        19 => "aux slice",
        20 => "slice ext",
        21 => "slice ext 3D",
        _ => "reserved",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalHeader {
    pub forbidden_zero_bit: u8,
    pub nal_ref_idc: u8,
    pub nal_unit_type: u8,
}

impl NalHeader {
    pub fn parse(b: u8) -> Self {
        Self { forbidden_zero_bit: b >> 7, nal_ref_idc: (b >> 5) & 3, nal_unit_type: b & 0x1f }
    }

    pub fn to_byte(self) -> u8 {
        (self.forbidden_zero_bit << 7) | (self.nal_ref_idc << 5) | self.nal_unit_type
    }

    pub fn is_vcl(self) -> bool {
        (1..=5).contains(&self.nal_unit_type)
    }

    /// NAL types whose header is longer than one byte (SVC/MVC/3D extensions).
    pub fn has_extended_header(self) -> bool {
        matches!(self.nal_unit_type, NAL_PREFIX | NAL_SLICE_EXT | NAL_SLICE_EXT_3D)
    }
}

/// One NAL unit as found in the byte stream. `prefix` holds every byte between the
/// end of the previous NAL unit and this one (trailing_zero_8bits, zero_byte and the
/// start code itself), so `prefix + data` over all units reproduces the file exactly.
#[derive(Clone, Debug)]
pub struct RawNal {
    pub prefix: Vec<u8>,
    pub data: Vec<u8>,
}

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

fn find_start_code(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= buf.len() {
        // fast skip: a start code needs buf[i+2] == 1 and buf[i+1] == 0
        if buf[i + 2] > 1 {
            i += 3;
        } else if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// End of the NAL payload starting at `from`: the first 0x000000 or 0x000001.
fn find_nal_end(buf: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i + 2] > 1 {
            i += 3;
        } else if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] <= 1 {
            return i;
        } else {
            i += 1;
        }
    }
    buf.len()
}

/// Split an Annex B byte stream. Returns the NAL units and any trailing bytes after
/// the last one (normally empty or zero padding).
pub fn split_annexb(buf: &[u8]) -> Result<(Vec<RawNal>, Vec<u8>)> {
    let mut nals = Vec::new();
    let Some(first) = find_start_code(buf, 0) else {
        bail!("no Annex B start code found (is this an MP4/MKV file instead of a raw .h264 stream?)");
    };
    let mut prefix_start = 0;
    let mut sc = first;
    loop {
        let payload_start = sc + 3;
        let mut end = find_nal_end(buf, payload_start);
        if end == buf.len() {
            // zero padding at end of file belongs to the tail, a NAL never ends in 0x00
            while end > payload_start && buf[end - 1] == 0 {
                end -= 1;
            }
        }
        nals.push(RawNal { prefix: buf[prefix_start..payload_start].to_vec(), data: buf[payload_start..end].to_vec() });
        match find_start_code(buf, end) {
            Some(next) => {
                prefix_start = end;
                sc = next;
            }
            None => return Ok((nals, buf[end..].to_vec())),
        }
    }
}

/// Remove emulation_prevention_three_byte (0x000003 -> 0x0000).
pub fn ebsp_to_rbsp(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut zeros = 0;
    for &b in src {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Insert emulation_prevention_three_byte where needed, appending to `out`.
pub fn rbsp_to_ebsp(src: &[u8], out: &mut Vec<u8>) {
    out.reserve(src.len() + src.len() / 64);
    let mut zeros = 0;
    for &b in src {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    // a NAL unit must not end in 0x00 (cabac_zero_words get a final 0x03)
    if zeros >= 2 {
        out.push(3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epb_roundtrip() {
        let cases: &[&[u8]] = &[
            &[0x65, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x80],
            &[0x41, 0x9a, 0x00, 0x00, 0x03, 0x03, 0x12],
            &[0x41, 0x80, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03],
        ];
        for &ebsp in cases {
            let rbsp = ebsp_to_rbsp(ebsp);
            let mut back = Vec::new();
            rbsp_to_ebsp(&rbsp, &mut back);
            assert_eq!(back, ebsp);
        }
    }

    #[test]
    fn split_and_rejoin() {
        let stream: Vec<u8> = vec![
            0, 0, 0, 1, 0x67, 0x42, 0x00, //
            0, 0, 1, 0x68, 0xce, //
            0, 0, 0, 0, 1, 0x65, 0x88, 0x00, 0x00, 0x03, 0x01, 0x80, 0, 0,
        ];
        let (nals, tail) = split_annexb(&stream).unwrap();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].data, vec![0x67, 0x42]);
        assert_eq!(nals[2].prefix, vec![0, 0, 0, 0, 1]);
        assert_eq!(tail, vec![0, 0]);
        assert_eq!(nals[2].data, vec![0x65, 0x88, 0x00, 0x00, 0x03, 0x01, 0x80]);
        let mut joined = Vec::new();
        for n in &nals {
            joined.extend_from_slice(&n.prefix);
            joined.extend_from_slice(&n.data);
        }
        joined.extend_from_slice(&tail);
        assert_eq!(joined, stream);
    }
}
