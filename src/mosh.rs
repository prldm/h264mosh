//! Datamosh edits.
//!
//! An edit is a [`Timeline`]: the list of output frames in display order, each one
//! showing an input frame (by decode-order index) with optional macroblock effects.
//! Frames can be dropped, repeated and reordered freely, B-frames included. [`render`]
//! turns a timeline into a conformant stream: the frames are put in a decoding order,
//! parameter sets are re-inserted where needed, frame_num, POC (pic_order_cnt_type 0),
//! reference lists and marking are renumbered (see [`crate::renumber`]), and effects
//! are applied.
//! [`MoshPlan`] is the range-based description used by the command line; it is
//! compiled into a timeline.

use crate::effects::{self, Donor, Donors, EffectStats, Fx, Mask, MbEffect, PpsNeeds};
use crate::mb::MbContext;
use crate::nal::{NAL_IDR, NAL_PPS, NAL_SPS, NalHeader, START_CODE};
use crate::poc::PocDecoder;
use crate::pps::Pps;
use crate::range::{DupSpec, RangeSet};
use crate::renumber::{self, SpsNeeds, Targets};
use crate::slice::{DecRefPicMarking, Mmco, SliceType};
use crate::sps::Sps;
use crate::stream::{AccessUnit, Nal, NalKind, Stream, group_access_units};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

/// A parsed input stream, grouped into frames.
pub struct Source {
    pub aus: Vec<AccessUnit>,
    /// picture order count of each frame
    pub pocs: Vec<Option<i64>>,
    /// input frames in display order
    pub display: Vec<usize>,
    /// position of each input frame in `display`
    pub(crate) display_pos: Vec<usize>,
    /// what the reference list modifications and marking of each frame refer to
    pub(crate) refs: Vec<Targets>,
    pub tail: Vec<u8>,
    pub warnings: Vec<String>,
}

impl Source {
    pub fn new(stream: Stream) -> Self {
        let aus = group_access_units(stream.nals);
        let pocs = decode_pocs(&aus);
        let display = display_order(&aus, &pocs);
        let mut display_pos = vec![0; aus.len()];
        for (k, &i) in display.iter().enumerate() {
            display_pos[i] = k;
        }
        let refs = renumber::source_targets(&aus, &pocs);
        Self { aus, pocs, display, display_pos, refs, tail: stream.tail, warnings: stream.warnings }
    }

    /// Every input frame once, unchanged, in display order.
    pub fn timeline(&self) -> Timeline {
        Timeline { entries: self.display.iter().map(|&i| TimelineEntry::new(i)).collect() }
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
    pub effects: Vec<Fx>,
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
    /// Every input frame once, unchanged, for a stream without B-frames (see
    /// [`Source::timeline`]).
    pub fn identity(frames: usize) -> Self {
        Self { entries: (0..frames).map(TimelineEntry::new).collect() }
    }

    /// Shows every input frame of `src` once, in its display order.
    fn keeps_order(&self, src: &Source) -> bool {
        self.entries.len() == src.len() && self.entries.iter().zip(&src.display).all(|(e, &i)| e.src == i)
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
    /// its NAL units in the output stream (see `Stream::write_with_offsets`); with
    /// B-frames the stream is in decoding order, not in display order
    pub nals: Range<usize>,
}

#[derive(Default, Debug)]
pub struct RenderReport {
    pub frames_out: usize,
    /// the output frames in display order (the timeline's)
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
    /// timeline entry
    pos: usize,
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
    let reordered = !timeline.keeps_order(src);
    let (order, plan) = if reordered {
        check_editable(src, &mut report.warnings)?;
        if !src.aus[first.src].is_idr() {
            bail!("the output must start with an IDR frame (frame {} is not one)", first.src);
        }
        let (order, plan) = plan_decoding(src, timeline)?;
        (order, Some(plan))
    } else {
        (src.display_pos.clone(), None)
    };

    // first uses in decoding order take the whole access unit
    let mut used = vec![false; src.len()];
    let mut params = ParamSets::default();
    let mut out = Vec::with_capacity(timeline.entries.len());
    for &pos in &order {
        let e = &timeline.entries[pos];
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
        out.push(OutFrame { src: e.src, pos, au });
    }

    if let Some(plan) = plan {
        for (f, headers) in out.iter_mut().zip(plan.headers) {
            let mut headers = headers.into_iter();
            for n in f.au.nals.iter_mut() {
                if let Some(s) = n.slice_mut()
                    && let Some(h) = headers.next()
                    && s.header != h
                {
                    s.header = h;
                    n.dirty = true;
                }
            }
        }
        upgrade_sps(&mut out, plan.needs);
    }
    // stream-level preconditions (e.g. MBAFF) fail the whole render, not every frame
    if let Some(f) = out.iter().find(|f| timeline.entries[f.pos].effects.iter().any(|x| x.effect.needs_macroblocks()))
        && let Some(s) = f.au.first_slice()
    {
        MbContext::new(s)?;
    }
    let needs = timeline.entries.iter().flat_map(|e| &e.effects).fold(PpsNeeds::default(), |n, x| n.union(x.effect.pps_needs()));
    if needs.any() {
        upgrade_pps(&mut out, needs);
    }
    let donors = donors(src, timeline);
    // frames are independent at the bitstream level: apply effects in parallel; seeds
    // belong to the entries, so the result does not depend on scheduling
    let results: Vec<_> = out
        .par_iter()
        .enumerate()
        .filter(|(_, f)| !timeline.entries[f.pos].effects.is_empty())
        .map(|(j, f)| {
            let e = &timeline.entries[f.pos];
            (j, apply_frame_effects(&f.au, &e.effects, e.seed, &donors))
        })
        .collect();
    for (j, result) in results {
        match result {
            Ok((au, stats)) => {
                out[j].au = au;
                report.effects += stats;
            }
            Err(reason) => report.failed_effects.push((out[j].pos, format!("frame {}: {reason}", out[j].src))),
        }
    }
    report.failed_effects.sort_by_key(|(k, _)| *k);
    if opts.no_deblock {
        disable_deblocking(&mut out);
    }

    report.frames_out = out.len();
    report.rewritten = out.iter().filter(|f| f.au.nals.iter().any(|n| n.dirty)).count();
    let mut nals = Vec::new();
    let mut frames = vec![None; out.len()];
    for f in out {
        let start = nals.len();
        nals.extend(f.au.nals);
        frames[f.pos] =
            Some(RenderedFrame { src: f.src, idr: nals[start..].iter().any(|n| n.nal_type() == NAL_IDR), nals: start..nals.len() });
    }
    report.frames = frames.into_iter().map(|f| f.expect("every timeline entry is rendered")).collect();
    Ok((Stream { nals, tail: src.tail.clone(), warnings: Vec::new() }, report))
}

/// The decoding order of an edited timeline and its new headers. B-frames wait for the
/// frame after them as long as the decoder can hold the frames that have to wait to be
/// shown; with too many of them in a row, fewer wait.
fn plan_decoding(src: &Source, timeline: &Timeline) -> Result<(Vec<usize>, renumber::Plan)> {
    let sps = &src.aus[timeline.entries[0].src].first_slice().context("the first frame has no slices")?.sps;
    let limit = sps.max_dpb_frames().max(sps.reorder_limits().1).max(sps.max_num_ref_frames);
    let mut max_pending = timeline.entries.len();
    loop {
        let order = renumber::schedule(src, timeline, max_pending);
        let plan = renumber::plan(src, timeline, &order)?;
        if plan.needs.dpb_frames <= limit || max_pending == 0 {
            return Ok((order, plan));
        }
        max_pending /= 2;
    }
}

/// Bytes of the rendered output frames `range`, decodable on their own when
/// `range.start` is an IDR frame (see `RenderReport::restart_point`): the parameter
/// sets in effect at that point are written first. With B-frames, frames decoded
/// together with the range (the one after its last B-frames) come along.
pub fn segment(stream: &Stream, frames: &[RenderedFrame], range: Range<usize>) -> Result<Vec<u8>> {
    if range.is_empty() || range.end > frames.len() {
        bail!("frame range {range:?} is outside the {} rendered frames", frames.len());
    }
    let start = frames[range.clone()].iter().map(|f| f.nals.start).min().unwrap_or(0);
    let end = frames[range].iter().map(|f| f.nals.end).max().unwrap_or(0);
    let mut active: HashMap<(u8, u32), &Nal> = HashMap::new();
    for n in &stream.nals[..start] {
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
    for n in &stream.nals[start..end] {
        n.write_to(&mut out, false)?;
    }
    Ok(out)
}

/// Apply effects to every slice of a copy of the frame; all or nothing. Errors and
/// panics are reported instead of aborting the render.
fn apply_frame_effects(au: &AccessUnit, fx: &[Fx], seed: u64, donors: &Donors) -> std::result::Result<(AccessUnit, EffectStats), String> {
    let run = || -> Result<(AccessUnit, EffectStats)> {
        let mut au = au.clone();
        let mut stats = EffectStats::default();
        for (i, n) in au.nals.iter_mut().enumerate() {
            if let NalKind::Slice(s) = &mut n.kind {
                stats += effects::apply(s, fx, splitmix64(seed ^ i as u64), donors)?;
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

/// The input frames that macroblocks are copied from, parsed once for all frames.
fn donors(src: &Source, timeline: &Timeline) -> Donors {
    let frames: BTreeSet<usize> = timeline
        .entries
        .iter()
        .flat_map(|e| &e.effects)
        .filter_map(|x| match x.effect {
            MbEffect::Transplant(from, _) | MbEffect::MotionTransplant(from, _) => Some(from),
            _ => None,
        })
        .collect();
    frames
        .into_par_iter()
        .map(|i| {
            let donor = match src.aus.get(i) {
                Some(au) => Donor::new(au).map_err(|e| format!("{e:#}")),
                None => Err(format!("the input has {} frames", src.len())),
            };
            (i, donor)
        })
        .collect()
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

/// Input frames in display order: by picture order count, which starts over at IDR
/// frames and at frames with MMCO 5 (8.2.1).
pub fn display_order(aus: &[AccessUnit], pocs: &[Option<i64>]) -> Vec<usize> {
    let mut period = 0;
    let mut last = 0;
    let mut keys: Vec<(usize, i64, usize)> = aus
        .iter()
        .enumerate()
        .map(|(i, au)| {
            let reset = au.first_slice().is_some_and(|s| s.header.has_mmco5());
            if au.is_idr() || reset {
                period += 1;
            }
            // a frame with MMCO 5 counts as 0 once it is decoded
            last = if reset { 0 } else { pocs[i].unwrap_or(last) };
            (period, last, i)
        })
        .collect();
    keys.sort_unstable();
    keys.into_iter().map(|(_, _, i)| i).collect()
}

/// Renumbering needs progressive frames, and moving B-frames a picture order count of
/// type 0. Long-term reference frames are not followed.
fn check_editable(src: &Source, warnings: &mut Vec<String>) -> Result<()> {
    let mut long_term_warned = false;
    for (i, au) in src.aus.iter().enumerate() {
        let long_term = au.slices().any(|s| match &s.header.dec_ref_pic_marking {
            Some(DecRefPicMarking::NonIdr(Some(ops))) => ops.iter().any(|op| !matches!(op, Mmco::ForgetShort(_) | Mmco::Reset)),
            Some(DecRefPicMarking::Idr { long_term_reference_flag, .. }) => *long_term_reference_flag,
            _ => false,
        });
        if long_term && !long_term_warned {
            warnings.push(format!("frame {i} uses long-term reference frames; after edits these may point at other frames than intended"));
            long_term_warned = true;
        }
        if au.slices().any(|s| !s.sps.frame_mbs_only_flag) {
            bail!("interlaced streams are not supported (frame {i}); re-encode progressive");
        }
        if au.picture_type() == 'B'
            && let Some(s) = au.slices().find(|s| s.sps.pic_order_cnt_type != 0)
        {
            bail!(
                "frame {i} is a B-frame in a stream with picture order count type {}; editing B-frames needs type 0",
                s.sps.pic_order_cnt_type
            );
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

/// Raise the limits of every SPS to what the renumbered stream needs; slices are
/// re-serialized when their header syntax changes with it.
fn upgrade_sps(out: &mut [OutFrame], needs: SpsNeeds) {
    // the old Arc is kept in the map so its address cannot be reused while we key on it
    let mut replaced: HashMap<*const Sps, (Arc<Sps>, Arc<Sps>)> = HashMap::new();
    let mut upgrade = |sps: &Arc<Sps>| -> Arc<Sps> {
        let new = || needs.upgrade(sps).map_or_else(|| sps.clone(), Arc::new);
        replaced.entry(Arc::as_ptr(sps)).or_insert_with(|| (sps.clone(), new())).1.clone()
    };
    for f in out.iter_mut() {
        for n in f.au.nals.iter_mut() {
            match &mut n.kind {
                NalKind::Sps(s) => {
                    let new = upgrade(s);
                    if !Arc::ptr_eq(s, &new) {
                        *s = new;
                        n.dirty = true;
                    }
                }
                NalKind::Slice(s) => {
                    let new = upgrade(&s.sps);
                    if !Arc::ptr_eq(&s.sps, &new) {
                        n.dirty |= new.log2_max_poc_lsb() != s.sps.log2_max_poc_lsb();
                        s.sps = new;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Switch PPS flags on in the whole stream (`needs`); slices referring to a changed
/// PPS are re-serialized against the new one. P slices that get weighted prediction
/// get a table with default weights, so they decode as before.
fn upgrade_pps(out: &mut [OutFrame], needs: PpsNeeds) {
    // the old Arc is kept in the map so its address cannot be reused while we key on it
    let mut replaced: HashMap<*const Pps, (Arc<Pps>, Arc<Pps>)> = HashMap::new();
    let mut upgrade = |pps: &Arc<Pps>| -> Arc<Pps> {
        let Some(new) = needs.upgrade(pps) else { return pps.clone() };
        replaced.entry(Arc::as_ptr(pps)).or_insert_with(|| (pps.clone(), Arc::new(new))).1.clone()
    };
    for f in out.iter_mut() {
        for n in f.au.nals.iter_mut() {
            match &mut n.kind {
                NalKind::Pps(p) => {
                    let new = upgrade(p);
                    if !Arc::ptr_eq(p, &new) {
                        *p = new;
                        n.dirty = true;
                    }
                }
                NalKind::Slice(s) => {
                    let new = upgrade(&s.pps);
                    if !Arc::ptr_eq(&s.pps, &new) {
                        s.pps = new;
                        n.dirty = true;
                    }
                    if s.pps.weighted_pred_flag
                        && s.header.pred_weight_table.is_none()
                        && matches!(s.header.slice_type(), SliceType::P | SliceType::SP)
                    {
                        s.header.pred_weight_table = Some(effects::default_weights(s));
                        n.dirty = true;
                    }
                }
                _ => {}
            }
        }
    }
}

fn disable_deblocking(out: &mut [OutFrame]) {
    upgrade_pps(out, PpsNeeds { deblocking_control: true, weighted_pred: false });
    for f in out.iter_mut() {
        for n in f.au.nals.iter_mut() {
            if let NalKind::Slice(s) = &mut n.kind
                && s.header.disable_deblocking_filter_idc != 1
            {
                s.header.disable_deblocking_filter_idc = 1;
                n.dirty = true;
            }
        }
    }
}

/// Range-based edit description (the command line options). Frame numbers are display
/// positions of the input frames, also those of the transplant effects.
#[derive(Default)]
pub struct MoshPlan {
    /// drop I-frames (IDR or not) whose index is in the set
    pub drop_keyframes: Option<RangeSet>,
    /// drop any frame whose index is in the set
    pub drop: Option<RangeSet>,
    pub dup: Vec<DupSpec>,
    /// macroblock effects, applied to `effect_frames` (all frames if None), within `mask`
    pub effects: Vec<MbEffect>,
    pub effect_frames: Option<RangeSet>,
    pub mask: Option<Mask>,
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
        // effects name their donor frames by display position, the timeline by input frame
        let effects: Vec<Fx> = self
            .effects
            .iter()
            .map(|e| {
                let input = |k: usize| src.display.get(k).copied().unwrap_or(k);
                let effect = match *e {
                    MbEffect::Transplant(k, amount) => MbEffect::Transplant(input(k), amount),
                    MbEffect::MotionTransplant(k, mix) => MbEffect::MotionTransplant(input(k), mix),
                    _ => e.clone(),
                };
                Fx { effect, mask: self.mask.clone() }
            })
            .collect();
        for (k, &i) in src.display.iter().enumerate() {
            let au = &src.aus[i];
            let in_drop = self.drop.as_ref().is_some_and(|r| r.contains(k));
            let drop = in_drop || (au.is_keyframe() && self.drop_keyframes.as_ref().is_some_and(|r| r.contains(k)));
            if drop && k == 0 {
                if in_drop {
                    report.warnings.push("frame 0 kept: the first frame must be decodable".into());
                }
            } else if drop {
                report.dropped.push(k);
                continue;
            }
            let effects = if self.effect_frames.as_ref().is_none_or(|r| r.contains(k)) { effects.clone() } else { Vec::new() };
            let copies: usize = self.dup.iter().filter(|d| d.frames.contains(k)).map(|d| d.count).sum();
            for copy in 0..=copies {
                entries.push(TimelineEntry { src: i, effects: effects.clone(), seed: entry_seed(self.seed, i, copy) });
            }
            if copies > 0 {
                report.duplicated.push((k, copies));
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
