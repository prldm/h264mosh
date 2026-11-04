//! Self checks: bit-exact re-serialization, frame_num and picture order count conformance.

use crate::common::RbspTail;
use crate::mb;
use crate::mosh::{decode_pocs, display_order};
use crate::stream::{AccessUnit, NalKind, Stream};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Default)]
pub struct RoundtripReport {
    /// per NAL type name: (re-serialized ok, mismatched)
    pub by_type: BTreeMap<&'static str, (usize, usize)>,
    pub passthrough: usize,
    /// SPS/PPS whose data after the parsed syntax is not plain rbsp_trailing_bits
    pub raw_tails: usize,
    /// (NAL index, description) of the first few mismatches
    pub mismatches: Vec<(usize, String)>,
    pub total_mismatches: usize,
    /// passthrough write reproduces the input file
    pub file_identical: bool,
    /// forced re-serialization of every parsed unit reproduces the input file
    pub reserialized_identical: bool,
}

pub fn roundtrip(data: &[u8], stream: &Stream) -> Result<RoundtripReport> {
    let mut rep = RoundtripReport { file_identical: stream.write(false)? == data, ..Default::default() };
    // per NAL unit, in parallel: (type, has unparsed trailing data, mismatch)
    let checks: Vec<Option<(&'static str, bool, Option<String>)>> = stream
        .nals
        .par_iter()
        .map(|n| {
            let (name, raw_tail) = match &n.kind {
                NalKind::Sps(s) => ("SPS", s.tail != RbspTail::Trailing),
                NalKind::Pps(p) => ("PPS", p.tail != RbspTail::Trailing),
                NalKind::Slice(_) => ("slice", false),
                NalKind::Other => return Ok(None),
            };
            let new = n.ebsp(true)?;
            let mismatch = (new.as_ref() != n.raw.as_slice()).then(|| {
                let at = new.iter().zip(n.raw.iter()).position(|(a, b)| a != b).unwrap_or(new.len().min(n.raw.len()));
                format!("first difference at byte {at} (original {} bytes, rewritten {} bytes)", n.raw.len(), new.len())
            });
            Ok(Some((name, raw_tail, mismatch)))
        })
        .collect::<Result<_>>()?;
    for (i, check) in checks.into_iter().enumerate() {
        let Some((name, raw_tail, mismatch)) = check else {
            rep.passthrough += 1;
            continue;
        };
        rep.raw_tails += raw_tail as usize;
        let entry = rep.by_type.entry(name).or_default();
        match mismatch {
            None => entry.0 += 1,
            Some(m) => {
                entry.1 += 1;
                rep.total_mismatches += 1;
                if rep.mismatches.len() < 10 {
                    rep.mismatches.push((i, format!("{name}: {m}")));
                }
            }
        }
    }
    rep.reserialized_identical = stream.write(true)? == data;
    Ok(rep)
}

/// Frames whose frame_num breaks 7.4.3 (gap or repeat without gaps_in_frame_num_value_allowed_flag).
pub fn frame_num_violations(aus: &[AccessUnit]) -> Vec<usize> {
    let mut bad = Vec::new();
    let mut prev_ref: Option<u32> = None;
    for (i, au) in aus.iter().enumerate() {
        let Some(s) = au.first_slice() else { continue };
        let h = &s.header;
        if au.is_idr() {
            if h.frame_num != 0 {
                bad.push(i);
            }
        } else if let Some(p) = prev_ref {
            let expected = (p + 1) % s.sps.max_frame_num();
            let second_field = h.field_pic_flag && h.frame_num == p;
            if h.frame_num != expected && !second_field && !s.sps.gaps_in_frame_num_value_allowed_flag {
                bad.push(i);
            }
        }
        if au.is_reference() {
            prev_ref = Some(if h.has_mmco5() { 0 } else { h.frame_num });
        }
    }
    bad
}

/// Display order as a decoder derives it from the picture order counts.
#[derive(Default, Debug)]
pub struct PocReport {
    /// frames (decoding order) in display order
    pub display: Vec<usize>,
    /// frames with the picture order count of an earlier frame of the same IDR period
    pub duplicates: Vec<usize>,
    /// frames preceded in decoding order by more frames shown after them than the
    /// SPS allows (max_num_reorder_frames)
    pub reordered_too_far: Vec<usize>,
}

pub fn poc_check(aus: &[AccessUnit]) -> PocReport {
    let pocs = decode_pocs(aus);
    let display = display_order(aus, &pocs);
    let mut rep = PocReport::default();
    let mut seen = HashSet::new();
    for (i, au) in aus.iter().enumerate() {
        if au.is_idr() || au.first_slice().is_some_and(|s| s.header.has_mmco5()) {
            seen.clear();
        }
        if let Some(p) = pocs[i]
            && !seen.insert(p)
        {
            rep.duplicates.push(i);
        }
    }
    let mut shown_at = vec![0; aus.len()];
    for (k, &i) in display.iter().enumerate() {
        shown_at[i] = k;
    }
    let mut decoded = BTreeSet::new();
    for (i, au) in aus.iter().enumerate() {
        let later = decoded.range(shown_at[i] + 1..).count();
        if let Some(s) = au.first_slice()
            && later > s.sps.reorder_limits().0 as usize
        {
            rep.reordered_too_far.push(i);
        }
        decoded.insert(shown_at[i]);
    }
    rep.display = display;
    rep
}

#[derive(Default)]
pub struct MbRoundtripReport {
    pub identical: usize,
    pub different: usize,
    /// MBAFF or otherwise unsupported slices
    pub skipped: usize,
    pub macroblocks: usize,
    /// first few failures: (NAL index, description)
    pub failures: Vec<(usize, String)>,
}

enum MbCheck {
    NotSlice,
    Unsupported,
    Checked { macroblocks: usize, problem: Option<String> },
}

/// Where the bins of a CABAC slice as read and as written back first differ.
fn first_bin_difference(slice: &crate::slice::Slice) -> String {
    let (read, _) = crate::cabac::bin_trace(slice, false);
    let (written, err) = crate::cabac::bin_trace(slice, true);
    match read.iter().zip(&written).position(|(a, b)| a != b) {
        Some(i) => {
            let (a, b) = (read[i], written[i]);
            let ctx = |c: Option<u16>| c.map_or("bypass".into(), |c| format!("ctxIdx {c}"));
            format!(
                "bin {i} differs: read macroblock {} {} ({}) = {}, written macroblock {} {} ({}) = {}",
                a.mb,
                a.element,
                ctx(a.ctx),
                a.bin as u8,
                b.mb,
                b.element,
                ctx(b.ctx),
                b.bin as u8
            )
        }
        None if read.len() != written.len() => {
            format!("{} bins read, {} written{}", read.len(), written.len(), err.err().map_or(String::new(), |e| format!(" ({e:#})")))
        }
        None => format!("the {} bins are the same: the arithmetic coder differs", read.len()),
    }
}

/// Parse the macroblock layer of every slice and write it back; the slice data
/// must come out bit for bit identical. Slices are checked in parallel.
pub fn mb_roundtrip(stream: &Stream) -> MbRoundtripReport {
    let checks: Vec<MbCheck> = stream
        .nals
        .par_iter()
        .map(|n| {
            let Some(slice) = n.slice() else { return MbCheck::NotSlice };
            if mb::MbContext::new(slice).is_err() {
                return MbCheck::Unsupported;
            }
            let mut macroblocks = 0;
            let result = mb::parse_slice_data(slice).and_then(|data| {
                macroblocks = data.mbs.len();
                mb::write_slice_data(slice, &data)
            });
            let (buf, start, end) = slice.slice_data();
            let problem = match result {
                Err(e) => Some(format!("{e:#}")),
                Ok(written) => {
                    // both placed at the slice data's offset within a byte, so that the
                    // I_PCM samples are aligned as in the slice
                    let at_offset = |append: &dyn Fn(&mut crate::bits::BitWriter)| {
                        let mut w = crate::bits::BitWriter::new();
                        (0..start % 8).for_each(|_| w.flag(false));
                        append(&mut w);
                        (w.bit_len() - start % 8, w.into_bytes())
                    };
                    let (bits, bytes) = at_offset(&|w| written.append_to(w));
                    let (orig_bits, orig) = at_offset(&|w| w.append_bits(buf, start, end));
                    (bits != orig_bits || bytes != orig).then(|| {
                        let mut msg = format!("rewritten slice data differs ({bits} bits vs {orig_bits})");
                        if slice.pps.entropy_coding_mode_flag {
                            msg = format!("{msg}; {}", first_bin_difference(slice));
                        }
                        msg
                    })
                }
            };
            MbCheck::Checked { macroblocks, problem }
        })
        .collect();
    let mut rep = MbRoundtripReport::default();
    for (i, check) in checks.into_iter().enumerate() {
        match check {
            MbCheck::NotSlice => {}
            MbCheck::Unsupported => rep.skipped += 1,
            MbCheck::Checked { macroblocks, problem } => {
                rep.macroblocks += macroblocks;
                match problem {
                    None => rep.identical += 1,
                    Some(p) => {
                        rep.different += 1;
                        if rep.failures.len() < 10 {
                            rep.failures.push((i, p));
                        }
                    }
                }
            }
        }
    }
    rep
}
