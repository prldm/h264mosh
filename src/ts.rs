//! A minimal MPEG transport stream writer (ISO/IEC 13818-1): one H.264 stream with
//! presentation and decoding times. Raw H.264 carries no timestamps, and with
//! B-frames a muxer cannot guess them; this gives ffmpeg a stream to copy from.

use crate::mosh::display_order;
use crate::nal::NAL_AUD;
use crate::stream::{AccessUnit, Stream, group_access_units};
use anyhow::Result;

const PACKET: usize = 188;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x100;
/// 90 kHz ticks before the first frame is shown (decoding times start earlier)
const START: u64 = 90_000;

/// The stream `data` in a transport stream, frames `fps` (num, den) apart, or None
/// if it shows its frames in decoding order (no timestamps needed).
pub fn with_timestamps(data: &[u8], (num, den): (u64, u64)) -> Result<Option<Vec<u8>>> {
    let aus = group_access_units(Stream::parse(data)?.nals);
    let display = display_order(&aus, &crate::mosh::decode_pocs(&aus));
    if display.iter().enumerate().all(|(k, &i)| k == i) {
        return Ok(None);
    }
    let mut shown_at = vec![0; aus.len()];
    for (k, &i) in display.iter().enumerate() {
        shown_at[i] = k as u64;
    }
    let ticks = |n: u64| n * 90_000 * den / num;
    // decoding starts this many frames before the first frame is shown
    let delay = (0..aus.len()).map(|i| (i as u64).saturating_sub(shown_at[i])).max().unwrap_or(0);
    let mut ts = Writer::default();
    ts.tables();
    for (i, au) in aus.iter().enumerate() {
        let (pts, dts) = (START + ticks(shown_at[i] + delay), START + ticks(i as u64));
        ts.pes(&annexb(au)?, pts, dts);
    }
    Ok(Some(ts.out))
}

/// The access unit in Annex B form, with an access unit delimiter in front (which
/// H.264 in a transport stream must have).
fn annexb(au: &AccessUnit) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if au.nals.first().is_none_or(|n| n.nal_type() != NAL_AUD) {
        // primary_pic_type 7: any slice type
        out.extend_from_slice(&[0, 0, 0, 1, NAL_AUD, 0xf0]);
    }
    for n in &au.nals {
        n.write_to(&mut out, false)?;
    }
    Ok(out)
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    /// continuity counters of PAT, PMT and video
    counters: [u8; 3],
}

impl Writer {
    /// Program association and program map table: one program, one H.264 stream.
    fn tables(&mut self) {
        let pat = section(0x00, 1, &[0x00, 0x01, 0xe0 | (PMT_PID >> 8) as u8, PMT_PID as u8]);
        self.packets(0, 0, &[&[0][..], &pat].concat(), None);
        let pid = [0xe0 | (VIDEO_PID >> 8) as u8, VIDEO_PID as u8];
        // PCR PID, no program info, stream type 0x1b (H.264) without descriptors
        let pmt = section(0x02, 1, &[pid[0], pid[1], 0xf0, 0x00, 0x1b, pid[0], pid[1], 0xf0, 0x00]);
        self.packets(PMT_PID, 1, &[&[0][..], &pmt].concat(), None);
    }

    /// One frame as a PES packet with both timestamps; the clock reference goes with it.
    fn pes(&mut self, frame: &[u8], pts: u64, dts: u64) {
        let mut p = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0xc0, 10];
        p.extend(timestamp(0x3, pts));
        p.extend(timestamp(0x1, dts));
        p.extend_from_slice(frame);
        self.packets(VIDEO_PID, 2, &p, Some(dts.saturating_sub(9000)));
    }

    /// Split `payload` into packets; the first starts the unit and may carry a PCR.
    fn packets(&mut self, pid: u16, counter: usize, payload: &[u8], pcr: Option<u64>) {
        let mut rest = payload;
        let mut first = true;
        while first || !rest.is_empty() {
            // adaptation field flags and PCR: 33-bit base, 6 reserved bits, 9-bit extension 0
            let fields: Vec<u8> = match pcr.filter(|_| first) {
                Some(p) => vec![0x10, (p >> 25) as u8, (p >> 17) as u8, (p >> 9) as u8, (p >> 1) as u8, ((p & 1) << 7) as u8 | 0x7e, 0],
                None => Vec::new(),
            };
            let room = PACKET - 4 - if fields.is_empty() { 0 } else { 1 + fields.len() };
            let take = rest.len().min(room);
            // a short last packet is filled up by the adaptation field
            let stuffing = room - take;
            let adaptation = match (fields.is_empty(), stuffing) {
                (true, 0) => Vec::new(),
                (true, 1) => vec![0],
                (true, n) => [vec![(n - 1) as u8, 0], vec![0xff; n - 2]].concat(),
                (false, n) => [vec![(fields.len() + n) as u8], fields, vec![0xff; n]].concat(),
            };
            let control = if adaptation.is_empty() { 0x10 } else { 0x30 };
            let cc = &mut self.counters[counter];
            self.out.extend([0x47, (u8::from(first) << 6) | (pid >> 8) as u8, pid as u8, control | *cc]);
            *cc = (*cc + 1) & 0xf;
            self.out.extend(adaptation);
            self.out.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            first = false;
        }
    }
}

/// A PSI section with its CRC: table `table_id`, `id` (stream or program), version 0.
fn section(table_id: u8, id: u16, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4;
    let mut s = vec![table_id, 0xb0 | (len >> 8) as u8, len as u8, (id >> 8) as u8, id as u8, 0xc1, 0, 0];
    s.extend_from_slice(body);
    let crc = crc32(&s);
    s.extend(crc.to_be_bytes());
    s
}

/// PTS or DTS field with the 4-bit prefix `marker`.
fn timestamp(marker: u8, t: u64) -> [u8; 5] {
    [
        (marker << 4) | (((t >> 30) & 7) << 1) as u8 | 1,
        (t >> 22) as u8,
        (((t >> 15) & 0x7f) << 1) as u8 | 1,
        (t >> 7) as u8,
        ((t & 0x7f) << 1) as u8 | 1,
    ]
}

/// CRC-32/MPEG-2
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_timestamps() {
        // CRC-32/MPEG-2 check value
        assert_eq!(crc32(b"123456789"), 0x0376_e6e7);
        assert_eq!(timestamp(0x2, 0), [0x21, 0, 1, 0, 1]);
        assert_eq!(timestamp(0x2, (1 << 33) - 1), [0x2f, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn packets_are_whole() {
        let mut w = Writer::default();
        w.tables();
        for len in [1, 100, 170, 175, 176, 177, 183, 184, 185, 400] {
            w.pes(&vec![7; len], 0, 0);
        }
        assert_eq!(w.out.len() % PACKET, 0);
        assert!(w.out.chunks(PACKET).all(|p| p[0] == 0x47));
    }
}
