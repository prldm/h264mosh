//! Datamosh edits.
//!
//! An edit is a [`Timeline`]: the list of output frames, each one showing an input
//! frame (by decode-order index) with optional macroblock effects. Frames can be
//! dropped, repeated and reordered freely. [`render`] turns a timeline into a
//! conformant stream: parameter sets are re-inserted where needed, frame_num (and
//! POC for pic_order_cnt_type 0) are renumbered, and effects are applied.
//! [`MoshPlan`] is the range-based description used by the command line; it is
//! compiled into a timeline.

use crate::effects::{self, EffectStats, MbEffect};
use crate::mb::MbContext;
use crate::nal::{NAL_IDR, NAL_PPS, NAL_SPS, NalHeader, START_CODE};
use crate::poc::PocDecoder;
use crate::pps::Pps;
use crate::range::{DupSpec, RangeSet};
use crate::slice::DecRefPicMarking;
use crate::sps::Sps;
use crate::stream::{AccessUnit, Nal, NalKind, Stream, group_access_units};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

/// A parsed input stream, grouped into frames.
pub struct Source {
    pub aus: Vec<AccessUnit>,
    /// picture order count of each frame
    pub pocs: Vec<Option<i64>>,
    pub tail: Vec<u8>,
    pub warnings: Vec<String>,
}

impl Source {
    pub fn new(stream: Stream) -> Self {
        let aus = group_access_units(stream.nals);
        let pocs = decode_pocs(&aus);
        Self { aus, pocs, tail: stream.tail, warnings: stream.warnings }
    }

    pub fn len(&self) -> usize {
        self.aus.len()
    }

    pub fn is_empty(&self) -> bool {
        self.aus.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TimelineEntry {
    /// input frame index (decode order)
    pub src: usize,
    pub effects: Vec<MbEffect>,
    /// seed of randomized effects; belongs to the entry, so editing other parts of the
    /// timeline never changes this frame
    pub seed: u64,
}

impl TimelineEntry {
    pub fn new(src: usize) -> Self {
        Self { src, effects: Vec::new(), seed: entry_seed(0, src, 0) }
    }
}

/// Seed for the `copy`-th occurrence of input frame `src`, derived from a base seed.
pub fn entry_seed(base: u64, src: usize, copy: usize) -> u64 {
    splitmix64(splitmix64(base ^ splitmix64(src as u64)) ^ copy as u64)
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Timeline {
    pub entries: Vec<TimelineEntry>,
}

impl Timeline {
    /// Every input frame once, unchanged.
    pub fn identity(frames: usize) -> Self {
        Self { entries: (0..frames).map(TimelineEntry::new).collect() }
    }

    fn keeps_order(&self, frames: usize) -> bool {
        self.entries.len() == frames && self.entries.iter().enumerate().all(|(i, e)| e.src == i)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    /// switch the deblocking filter off in every slice
    pub no_deblock: bool,
}

/// Where an output frame ended up in the rendered stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedFrame {
    /// input frame shown
    pub src: usize,
    /// decoding can (re)start at this frame
    pub idr: bool,
    /// its NAL units in the output stream (see `Stream::write_with_offsets`)
    pub nals: Range<usize>,
}

#[derive(Default, Debug)]
pub struct RenderReport {
    pub frames_out: usize,
    pub frames: Vec<RenderedFrame>,
    /// frames whose headers or data were rewritten
    pub rewritten: usize,
    pub effects: EffectStats,
    /// (output frame, reason) for frames whose effects failed; those frames are left unchanged
    pub failed_effects: Vec<(usize, String)>,
    pub warnings: Vec<String>,
}

impl RenderReport {
    /// The output frame where decoding has to start in order to show frame `k`:
    /// the last IDR frame at or before it.
    pub fn restart_point(&self, k: usize) -> usize {
        self.frames[..=k.min(self.frames.len().saturating_sub(1))].iter().rposition(|f| f.idr).unwrap_or(0)
    }
}

struct OutFrame {
    src: usize,
    orig_poc: Option<i64>,
    au: AccessUnit,
}

/// Build the output stream for `timeline`.
pub fn render(src: &Source, timeline: &Timeline, opts: &RenderOptions) -> Result<(Stream, RenderReport)> {
    let mut report = RenderReport::default();
    let Some(first) = timeline.entries.first() else { bail!("the timeline is empty") };
    if let Some(e) = timeline.entries.iter().find(|e| e.src >= src.len()) {
        bail!("timeline refers to frame {} but the input has {} frames", e.src, src.len());
    }
    if let Some(i) = src.aus.iter().position(|au| au.has_unparsed_vcl()) {
        bail!("frame {i} contains slices that could not be parsed; cannot edit this stream");
    }
    let reordered = !timeline.keeps_order(src.len());
    if reordered {
        check_editable(&src.aus, &src.pocs, &mut report.warnings)?;
        if !src.aus[first.src].is_idr() {
            bail!("the output must start with an IDR frame (frame {} is not one)", first.src);
        }
    }

    let mut used = vec![false; src.len()];
    let mut params = ParamSets::default();
    let mut out = Vec::with_capacity(timeline.entries.len());
    for e in &timeline.entries {
        let orig = &src.aus[e.src];
        // the first use takes the whole access unit, repeats only the coded picture
        let mut au = if std::mem::replace(&mut used[e.src], true) {
            AccessUnit {
                nals: orig.nals.iter().filter(|n| n.header.is_vcl()).map(|n| Nal { prefix: START_CODE.to_vec(), ..n.clone() }).collect(),
            }
        } else {
            orig.clone()
        };
        params.prepare(&mut au).with_context(|| format!("frame {}", e.src))?;
        out.push(OutFrame { src: e.src, orig_poc: src.pocs[e.src], au });
    }

    if reordered {
        renumber(&mut out);
    }
    // stream-level preconditions (e.g. CABAC) fail the whole render, not every frame
    if let Some((f, _)) = out.iter().zip(&timeline.entries).find(|(_, e)| !e.effects.is_empty())
        && let Some(s) = f.au.first_slice()
    {
        MbContext::new(s)?;
    }
    // frames are independent at the bitstream level: apply effects in parallel; seeds
    // belong to the entries, so the result does not depend on scheduling
    let results: Vec<_> = out
        .par_iter()
        .zip(timeline.entries.par_iter())
        .enumerate()
        .filter(|(_, (_, e))| !e.effects.is_empty())
        .map(|(k, (f, e))| (k, apply_frame_effects(&f.au, &e.effects, e.seed)))
        .collect();
    for (k, result) in results {
        match result {
            Ok((au, stats)) => {
                out[k].au = au;
                report.effects += stats;
            }
            Err(reason) => report.failed_effects.push((k, format!("frame {}: {reason}", timeline.entries[k].src))),
        }
    }
    if opts.no_deblock {
        disable_deblocking(&mut out);
    }

    report.frames_out = out.len();
    report.rewritten = out.iter().filter(|f| f.au.nals.iter().any(|n| n.dirty)).count();
    let mut nals = Vec::new();
    for f in out {
        let start = nals.len();
        nals.extend(f.au.nals);
        report.frames.push(RenderedFrame {
            src: f.src,
            idr: nals[start..].iter().any(|n| n.nal_type() == NAL_IDR),
            nals: start..nals.len(),
        });
    }
    Ok((Stream { nals, tail: src.tail.clone(), warnings: Vec::new() }, report))
}

/// Bytes of the rendered output frames `range`, decodable on their own when
/// `range.start` is an IDR frame (see `RenderReport::restart_point`): the parameter
/// sets in effect at that point are written first.
pub fn segment(stream: &Stream, frames: &[RenderedFrame], range: Range<usize>) -> Result<Vec<u8>> {
    let (Some(first), Some(last)) = (frames.get(range.start), range.end.checked_sub(1).and_then(|e| frames.get(e))) else {
        bail!("frame range {range:?} is outside the {} rendered frames", frames.len());
    };
    let mut active: HashMap<(u8, u32), &Nal> = HashMap::new();
    for n in &stream.nals[..first.nals.start] {
        match &n.kind {
            NalKind::Sps(s) => active.insert((NAL_SPS, s.sps_id), n),
            NalKind::Pps(p) => active.insert((NAL_PPS, p.pps_id), n),
            _ => None,
        };
    }
    let mut sets: Vec<_> = active.into_iter().collect();
    sets.sort_by_key(|(key, _)| *key);
    let mut out = Vec::new();
    for (_, n) in sets {
        n.write_to(&mut out, false)?;
    }
    for n in &stream.nals[first.nals.start..last.nals.end] {
        n.write_to(&mut out, false)?;
    }
    Ok(out)
}

/// Apply effects to every slice of a copy of the frame; all or nothing. Errors and
/// panics are reported instead of aborting the render.
fn apply_frame_effects(au: &AccessUnit, fx: &[MbEffect], seed: u64) -> std::result::Result<(AccessUnit, EffectStats), String> {
    let run = || -> Result<(AccessUnit, EffectStats)> {
        let mut au = au.clone();
        let mut stats = EffectStats::default();
        for (i, n) in au.nals.iter_mut().enumerate() {
            if let NalKind::Slice(s) = &mut n.kind {
                stats += effects::apply(s, fx, splitmix64(seed ^ i as u64))?;
                n.dirty = true;
            }
        }
        Ok((au, stats))
    };
    match std::panic::catch_unwind(AssertUnwindSafe(run)) {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(panic) => {
            let msg = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned());
            Err(format!("internal error: {}", msg.unwrap_or_else(|| "panic".into())))
        }
    }
}

pub fn decode_pocs(aus: &[AccessUnit]) -> Vec<Option<i64>> {
    let mut dec = PocDecoder::new();
    aus.iter()
        .map(|au| {
            let n = au.first_vcl()?;
            let s = n.slice()?;
            Some(dec.next(n.header, &s.header, &s.sps))
        })
        .collect()
}

/// Renumbering assumes progressive frames in display order.
fn check_editable(aus: &[AccessUnit], pocs: &[Option<i64>], warnings: &mut Vec<String>) -> Result<()> {
    let mut mmco_warned = false;
    for (i, au) in aus.iter().enumerate() {
        if !mmco_warned
            && au.slices().any(|s| matches!(&s.header.dec_ref_pic_marking, Some(DecRefPicMarking::NonIdr(Some(ops))) if !ops.is_empty()))
        {
            warnings.push(format!(
                "frame {i} uses explicit reference marking (MMCO); after edits these may point at other frames than intended"
            ));
            mmco_warned = true;
        }
        if au.slices().any(|s| !s.sps.frame_mbs_only_flag) {
            bail!("interlaced streams are not supported (frame {i}); re-encode progressive");
        }
        if au.picture_type() == 'B' {
            bail!("frame {i} is a B-frame; frame editing needs a stream without B-frames (x264 --bframes 0, see `h264mosh prep`)");
        }
        if i > 0
            && !au.is_idr()
            && let (Some(a), Some(b)) = (pocs[i - 1], pocs[i])
            && b <= a
        {
            bail!("frame {i} is reordered (POC {b} after {a}); frame editing needs display order == decode order");
        }
    }
    Ok(())
}

/// Tracks the SPS/PPS a decoder has seen, and inserts the ones a frame was coded
/// with whenever they are not the current ones (e.g. after the frame that carried
/// them was dropped or moved).
#[derive(Default)]
struct ParamSets {
    sps: HashMap<u32, Arc<Sps>>,
    pps: HashMap<u32, Arc<Pps>>,
}

impl ParamSets {
    fn activate_sps(&mut self, sps: &Arc<Sps>, idr: bool) -> Result<()> {
        if let Some(prev) = self.sps.get(&sps.sps_id)
            && **prev != **sps
            && !idr
        {
            bail!(
                "switching to a different SPS ({}) needs an IDR frame. \
                 All clips must be encoded with identical settings, e.g. with `h264mosh prep`.",
                prev.differences(sps)
            );
        }
        self.sps.insert(sps.sps_id, sps.clone());
        Ok(())
    }

    fn prepare(&mut self, au: &mut AccessUnit) -> Result<()> {
        let idr = au.is_idr();
        for n in &au.nals {
            match &n.kind {
                NalKind::Sps(s) => self.activate_sps(s, idr)?,
                NalKind::Pps(p) => {
                    self.pps.insert(p.pps_id, p.clone());
                }
                _ => {}
            }
        }
        let mut missing = Vec::new();
        let slices: Vec<_> = au.slices().map(|s| (s.sps.clone(), s.pps.clone())).collect();
        for (sps, pps) in slices {
            if self.sps.get(&sps.sps_id).is_none_or(|cur| **cur != *sps) {
                self.activate_sps(&sps, idr)?;
                missing.push(param_set_nal(NAL_SPS, NalKind::Sps(sps)));
            }
            if self.pps.get(&pps.pps_id).is_none_or(|cur| **cur != *pps) {
                self.pps.insert(pps.pps_id, pps.clone());
                missing.push(param_set_nal(NAL_PPS, NalKind::Pps(pps)));
            }
        }
        if !missing.is_empty() {
            let at = au.nals.iter().position(|n| n.header.is_vcl()).unwrap_or(au.nals.len());
            au.nals.splice(at..at, missing);
        }
        Ok(())
    }
}

fn param_set_nal(nal_unit_type: u8, kind: NalKind) -> Nal {
    Nal {
        prefix: START_CODE.to_vec(),
        header: NalHeader { forbidden_zero_bit: 0, nal_ref_idc: 3, nal_unit_type },
        raw: Arc::new(Vec::new()),
        kind,
        dirty: true,
    }
}

fn renumber(out: &mut [OutFrame]) {
    let mut prev_ref_frame_num = 0u32;
    let mut poc = 0i64;
    let mut prev_src: Option<(usize, i64)> = None;
    let mut last_step = 2i64;
    let mut prev_idr_pic_id: Option<u32> = None;
    for f in out.iter_mut() {
        let Some(first) = f.au.first_slice() else { continue };
        let sps = first.sps.clone();
        let idr = f.au.is_idr();
        let is_ref = f.au.is_reference();
        let mmco5 = first.header.has_mmco5();
        // consecutive IDR pictures need different idr_pic_id values
        let idr_pic_id = match prev_idr_pic_id {
            Some(p) if idr && p == first.header.idr_pic_id => (p + 1) % 65536,
            _ => first.header.idr_pic_id,
        };
        if idr {
            prev_ref_frame_num = 0;
            poc = f.orig_poc.unwrap_or(0);
        } else {
            // keep the original POC spacing between frames that are still adjacent
            if let (Some((ps, pp)), Some(p)) = (prev_src, f.orig_poc)
                && ps + 1 == f.src
                && p > pp
            {
                last_step = p - pp;
            }
            poc += last_step;
        }
        prev_src = f.orig_poc.map(|p| (f.src, p));
        prev_idr_pic_id = idr.then_some(idr_pic_id);
        let frame_num = if idr { 0 } else { (prev_ref_frame_num + 1) % sps.max_frame_num() };
        let poc_lsb = poc.rem_euclid(sps.max_poc_lsb() as i64) as u32;
        for n in f.au.nals.iter_mut() {
            let Some(s) = n.slice_mut() else { continue };
            let h = &mut s.header;
            let mut changed = false;
            if !idr && h.frame_num != frame_num {
                h.frame_num = frame_num;
                changed = true;
            }
            if !idr && s.sps.pic_order_cnt_type == 0 && h.pic_order_cnt_lsb != poc_lsb {
                h.pic_order_cnt_lsb = poc_lsb;
                changed = true;
            }
            if idr && h.idr_pic_id != idr_pic_id {
                h.idr_pic_id = idr_pic_id;
                changed = true;
            }
            n.dirty |= changed;
        }
        if is_ref && !idr {
            prev_ref_frame_num = frame_num;
        }
        if mmco5 {
            prev_ref_frame_num = 0;
            poc = 0;
        }
    }
}

fn disable_deblocking(out: &mut [OutFrame]) {
    // PPSs that don't allow per-slice deblocking control get the flag switched on;
    // slices referring to them are re-serialized against the new PPS.
    // the old Arc is kept in the map so its address cannot be reused while we key on it
    let mut replaced: HashMap<*const Pps, (Arc<Pps>, Arc<Pps>)> = HashMap::new();
    let mut upgrade = |pps: &Arc<Pps>| -> Arc<Pps> {
        if pps.deblocking_filter_control_present_flag {
            return pps.clone();
        }
        let new = Arc::new(Pps { deblocking_filter_control_present_flag: true, ..(**pps).clone() });
        replaced.entry(Arc::as_ptr(pps)).or_insert_with(|| (pps.clone(), new)).1.clone()
    };
    for f in out.iter_mut() {
        for n in f.au.nals.iter_mut() {
            match &mut n.kind {
                NalKind::Pps(p) if !p.deblocking_filter_control_present_flag => {
                    *p = upgrade(p);
                    n.dirty = true;
                }
                NalKind::Slice(s) => {
                    if !Arc::ptr_eq(&s.pps, &upgrade(&s.pps)) {
                        s.pps = upgrade(&s.pps);
                        n.dirty = true;
                    }
                    if s.header.disable_deblocking_filter_idc != 1 {
                        s.header.disable_deblocking_filter_idc = 1;
                        n.dirty = true;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Range-based edit description (the command line options).
#[derive(Default)]
pub struct MoshPlan {
    /// drop I-frames (IDR or not) whose index is in the set
    pub drop_keyframes: Option<RangeSet>,
    /// drop any frame whose index is in the set
    pub drop: Option<RangeSet>,
    pub dup: Vec<DupSpec>,
    /// macroblock effects, applied to `effect_frames` (all frames if None)
    pub effects: Vec<MbEffect>,
    pub effect_frames: Option<RangeSet>,
    pub no_deblock: bool,
    pub seed: u64,
}

#[derive(Default, Debug)]
pub struct MoshReport {
    pub frames_in: usize,
    pub dropped: Vec<usize>,
    /// (frame, extra copies)
    pub duplicated: Vec<(usize, usize)>,
    pub render: RenderReport,
    pub warnings: Vec<String>,
}

impl MoshPlan {
    pub fn timeline(&self, src: &Source, report: &mut MoshReport) -> Timeline {
        let mut entries = Vec::new();
        for (i, au) in src.aus.iter().enumerate() {
            let in_drop = self.drop.as_ref().is_some_and(|r| r.contains(i));
            let drop = in_drop || (au.is_keyframe() && self.drop_keyframes.as_ref().is_some_and(|r| r.contains(i)));
            if drop && i == 0 {
                if in_drop {
                    report.warnings.push("frame 0 kept: the first frame must be decodable".into());
                }
            } else if drop {
                report.dropped.push(i);
                continue;
            }
            let effects = if self.effect_frames.as_ref().is_none_or(|r| r.contains(i)) { self.effects.clone() } else { Vec::new() };
            let copies: usize = self.dup.iter().filter(|d| d.frames.contains(i)).map(|d| d.count).sum();
            for copy in 0..=copies {
                entries.push(TimelineEntry { src: i, effects: effects.clone(), seed: entry_seed(self.seed, i, copy) });
            }
            if copies > 0 {
                report.duplicated.push((i, copies));
            }
        }
        Timeline { entries }
    }
}

/// Compile `plan` into a timeline and render it.
pub fn mosh(stream: Stream, plan: &MoshPlan) -> Result<(Stream, MoshReport)> {
    let src = Source::new(stream);
    let mut report = MoshReport { frames_in: src.len(), warnings: src.warnings.clone(), ..Default::default() };
    let timeline = plan.timeline(&src, &mut report);
    let (out, render_report) = render(&src, &timeline, &RenderOptions { no_deblock: plan.no_deblock })?;
    report.render = render_report;
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn edit_model_can_move_to_worker_threads() {
        assert_send_sync::<Source>();
        assert_send_sync::<Timeline>();
        assert_send_sync::<Stream>();
    }
}
