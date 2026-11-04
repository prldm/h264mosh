//! Renumbering an edited stream.
//!
//! A timeline lists frames in display order. [`schedule`] picks the order they are
//! decoded in: B-frames after the frame that follows them on screen, so they predict
//! from both sides as encoded. [`plan`] then gives every slice the header that makes
//! the edited stream decode in that order and show in the timeline's: frame_num,
//! picture order count (type 0), IDR ids, reference list modifications and reference
//! marking. Lists and marking keep pointing at the pictures they were coded against
//! while the decoder still holds them; otherwise they are repaired so that every
//! reference index has a picture and the decoder never holds too many.

use crate::mosh::{Source, Timeline, TimelineEntry};
use crate::slice::{DecRefPicMarking, Mmco, RplmOp, SliceHeader, SliceType};
use crate::sps::{BitstreamRestriction, Sps, Vui};
use crate::stream::AccessUnit;
use anyhow::{Result, bail};
use std::collections::BTreeSet;

/// The pictures a frame's reference list modifications and MMCO 1 operations refer to,
/// as input frame indices (None for operations on long-term pictures).
#[derive(Clone, Debug, Default)]
pub(crate) struct Targets {
    /// per slice: list 0 and list 1, one entry per modification
    lists: Vec<[Vec<Option<usize>>; 2]>,
    /// one entry per memory_management_control_operation
    marking: Vec<Option<usize>>,
    /// per slice: the pictures its reference lists hold (temporal direct depends on them)
    held: Vec<Lists>,
}

/// Reference lists 0 and 1 as frames (input frames or timeline entries), None where an
/// index has no picture.
type Lists = [Vec<Option<usize>>; 2];

/// A short-term reference frame held by the decoder.
#[derive(Clone, Copy, Debug)]
struct Ref {
    frame_num: u32,
    poc: i64,
    /// input frame
    src: usize,
    /// timeline entry (output frames only)
    pos: usize,
}

/// The short-term reference frames, in decoding order (8.2.5: sliding window, MMCO 1
/// and 5; long-term pictures are not tracked).
#[derive(Default)]
struct Dpb {
    refs: Vec<Ref>,
}

impl Dpb {
    fn find(&self, frame_num: u32) -> Option<&Ref> {
        self.refs.iter().find(|r| r.frame_num == frame_num)
    }

    /// The most recent copy of input frame `src`.
    fn latest(&self, src: usize) -> Option<&Ref> {
        self.refs.iter().rev().find(|r| r.src == src)
    }

    /// Store the reference frame `cur` as marked by `marking`.
    fn mark(&mut self, mut cur: Ref, idr: bool, marking: Option<&DecRefPicMarking>, max_refs: usize, max_frame_num: u32) {
        if idr {
            self.refs.clear();
        } else if let Some(DecRefPicMarking::NonIdr(Some(ops))) = marking {
            for op in ops {
                match *op {
                    Mmco::ForgetShort(d) | Mmco::ShortToLong(d, _) => {
                        let f = pic_before(cur.frame_num, d, max_frame_num);
                        self.refs.retain(|r| r.frame_num != f);
                    }
                    Mmco::Reset => {
                        self.refs.clear();
                        cur.frame_num = 0;
                        cur.poc = 0;
                    }
                    _ => {}
                }
            }
        }
        // the sliding window; with adaptive marking only for streams that free too little
        if self.refs.len() >= max_refs {
            self.refs.remove(0);
        }
        self.refs.push(cur);
    }
}

/// frame_num of the picture `difference_of_pic_nums_minus1` before frame `cur`.
fn pic_before(cur: u32, diff_minus1: u32, max_frame_num: u32) -> u32 {
    (cur + max_frame_num - (diff_minus1 + 1) % max_frame_num) % max_frame_num
}

/// Resolve the reference list modifications and MMCO 1 operations of every input frame.
pub(crate) fn source_targets(aus: &[AccessUnit], pocs: &[Option<i64>]) -> Vec<Targets> {
    let mut dpb = Dpb::default();
    aus.iter()
        .enumerate()
        .map(|(i, au)| {
            let Some(first) = au.first_slice() else { return Targets::default() };
            let (h, sps) = (&first.header, &first.sps);
            let max = sps.max_frame_num();
            if au.is_idr() {
                dpb.refs.clear();
            }
            let resolve = |ops: &Option<Vec<RplmOp>>| {
                let mut pred = h.frame_num;
                ops.iter()
                    .flatten()
                    .map(|&(idc, v)| {
                        pred = match idc {
                            0 => (pred + max - (v + 1) % max) % max,
                            1 => (pred + v + 1) % max,
                            _ => return None,
                        };
                        dpb.find(pred).map(|r| r.src)
                    })
                    .collect()
            };
            let lists = au.slices().map(|s| [resolve(&s.header.rplm_l0), resolve(&s.header.rplm_l1)]).collect();
            let poc = pocs[i].unwrap_or(0);
            let held = au
                .slices()
                .map(|s| slice_lists(&dpb, &s.header, poc, h.frame_num, max).map(|l| l.iter().map(|f| f.map(|r| r.src)).collect()))
                .collect();
            let marking = match &h.dec_ref_pic_marking {
                Some(DecRefPicMarking::NonIdr(Some(ops))) => ops
                    .iter()
                    .map(|op| match *op {
                        Mmco::ForgetShort(d) => dpb.find(pic_before(h.frame_num, d, max)).map(|r| r.src),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            if au.is_reference() {
                let cur = Ref { frame_num: h.frame_num, poc, src: i, pos: i };
                dpb.mark(cur, au.is_idr(), h.dec_ref_pic_marking.as_ref(), sps.max_num_ref_frames.max(1) as usize, max);
            }
            Targets { lists, marking, held }
        })
        .collect()
}

fn resets(au: &AccessUnit) -> bool {
    au.is_idr() || au.first_slice().is_some_and(|s| s.header.has_mmco5())
}

/// The order to decode the timeline's frames in (timeline indices). Frames other than
/// B-frames are decoded in display order, and the B-frames shown before one of them
/// right after it. IDR frames (and MMCO 5) start over, so the B-frames waiting for
/// them are decoded first. At most `max_pending` B-frames wait for the next frame, and
/// fewer reference B-frames than the decoder holds reference frames, so that the frame
/// they wait for stays a reference frame until they are shown.
pub(crate) fn schedule(src: &Source, timeline: &Timeline, max_pending: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(timeline.entries.len());
    let mut pending = Vec::new();
    let is_ref = |k: usize| src.aus[timeline.entries[k].src].is_reference();
    for (k, e) in timeline.entries.iter().enumerate() {
        let au = &src.aus[e.src];
        if au.picture_type() == 'B' {
            let max_refs = au.first_slice().map_or(1, |s| s.sps.max_num_ref_frames.max(1) as usize);
            if is_ref(k) && pending.iter().filter(|&&p| is_ref(p)).count() + 1 >= max_refs {
                flush(src, timeline, &mut pending, &mut order);
            }
            pending.push(k);
            if pending.len() > max_pending || pending.iter().filter(|&&p| is_ref(p)).count() >= max_refs {
                flush(src, timeline, &mut pending, &mut order);
            }
        } else if resets(au) {
            flush(src, timeline, &mut pending, &mut order);
            order.push(k);
        } else {
            order.push(k);
            flush(src, timeline, &mut pending, &mut order);
        }
    }
    flush(src, timeline, &mut pending, &mut order);
    order
}

/// Decode the waiting B-frames `pending` (in display order): reference B-frames in
/// input order, as the encoder had them; the others in display order, each as soon as
/// everything shown before it is decoded, so that none of them has to be stored.
fn flush(src: &Source, timeline: &Timeline, pending: &mut Vec<usize>, order: &mut Vec<usize>) {
    let shown = std::mem::take(pending);
    let mut by_input = shown.clone();
    by_input.sort_by_key(|&k| (timeline.entries[k].src, k));
    let mut decoded = vec![false; shown.len()];
    // shown[..low] are decoded
    let mut low = 0;
    let mut waiting = BTreeSet::new();
    let mut decode = |i: usize, order: &mut Vec<usize>, low: &mut usize| {
        order.push(shown[i]);
        decoded[i] = true;
        while *low < shown.len() && decoded[*low] {
            *low += 1;
        }
    };
    for k in by_input {
        let i = shown.binary_search(&k).expect("pending frames are in display order");
        if src.aus[timeline.entries[k].src].is_reference() {
            decode(i, order, &mut low);
        } else {
            waiting.insert(i);
        }
        while let Some(&w) = waiting.first()
            && w == low
        {
            waiting.remove(&w);
            decode(w, order, &mut low);
        }
    }
    for w in waiting {
        decode(w, order, &mut low);
    }
}

/// New slice headers for the timeline's frames decoded in `order`.
pub(crate) struct Plan {
    /// per frame in decoding order: one header per slice
    pub headers: Vec<Vec<SliceHeader>>,
    pub needs: SpsNeeds,
}

/// What the sequence parameter set has to allow for the renumbered stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SpsNeeds {
    /// bits of pic_order_cnt_lsb
    pub log2_max_poc_lsb: u32,
    /// frames that precede a frame in decoding order and follow it in display order
    pub reorder_frames: u32,
    /// frames the decoder holds at once (reference frames and frames waiting to be shown)
    pub dpb_frames: u32,
}

impl SpsNeeds {
    /// `sps` with the limits raised as far as needed, or None if it allows the stream.
    pub fn upgrade(&self, sps: &Sps) -> Option<Sps> {
        let mut new = sps.clone();
        if sps.pic_order_cnt_type == 0 && self.log2_max_poc_lsb > sps.log2_max_poc_lsb() {
            new.log2_max_pic_order_cnt_lsb_minus4 = self.log2_max_poc_lsb - 4;
        }
        let restriction = sps.vui.as_ref().and_then(|v| v.bitstream_restriction.as_ref());
        let (reorder, dpb) = sps.reorder_limits();
        // without the restriction, decoders guess the reordering; say it when there is some
        if self.reorder_frames > reorder || self.dpb_frames > dpb || (restriction.is_none() && self.reorder_frames > 0) {
            let mut b = restriction.cloned().unwrap_or(BitstreamRestriction {
                motion_vectors_over_pic_boundaries_flag: true,
                max_bytes_per_pic_denom: 2,
                max_bits_per_mb_denom: 1,
                log2_max_mv_length_horizontal: 16,
                log2_max_mv_length_vertical: 16,
                max_num_reorder_frames: 0,
                max_dec_frame_buffering: 0,
            });
            b.max_num_reorder_frames = b.max_num_reorder_frames.max(self.reorder_frames);
            b.max_dec_frame_buffering =
                b.max_dec_frame_buffering.max(self.dpb_frames).max(b.max_num_reorder_frames).max(sps.max_num_ref_frames);
            new.vui.get_or_insert_with(Vui::default).bitstream_restriction = Some(b);
        }
        (new != *sps).then_some(new)
    }
}

/// Picture order counts of the timeline's frames, in display order: increasing from
/// each IDR frame, with the input's spacing between frames that are still neighbours.
fn display_pocs(src: &Source, timeline: &Timeline) -> Vec<i64> {
    let mut poc = 0i64;
    let mut prev: Option<(usize, i64)> = None;
    let mut last_step = 2i64;
    timeline
        .entries
        .iter()
        .map(|e| {
            let au = &src.aus[e.src];
            let Some(first) = au.first_slice() else { return poc };
            let orig = src.pocs[e.src];
            if au.is_idr() {
                poc = orig.unwrap_or(0);
            } else {
                if let (Some((ps, pp)), Some(p)) = (prev, orig)
                    && src.display_pos[ps] + 1 == src.display_pos[e.src]
                    && p > pp
                {
                    last_step = p - pp;
                }
                poc += last_step;
            }
            prev = orig.map(|p| (e.src, p));
            let this = poc;
            if first.header.has_mmco5() {
                poc = 0;
            }
            this
        })
        .collect()
}

/// Headers for the timeline's frames decoded in `order` (see the module docs).
pub(crate) fn plan(src: &Source, timeline: &Timeline, order: &[usize]) -> Result<Plan> {
    let entries = &timeline.entries;
    let pocs = display_pocs(src, timeline);
    let mut headers = Vec::with_capacity(order.len());
    let mut needs = SpsNeeds::default();
    let mut dpb = Dpb::default();
    let mut prev_ref_frame_num = 0u32;
    let mut prev_idr_pic_id: Option<u32> = None;
    // the picture order count is coded relative to the previous reference frame
    let (mut prev_ref_poc, mut max_poc_step, mut max_idr_poc) = (0i64, 0i64, 0i64);
    // display positions decoded but not shown yet, with frames shown as early as possible
    let mut decoded = vec![false; entries.len()];
    let mut waiting = BTreeSet::new();
    let mut next_shown = 0;
    // the lists of each slice of the frames decoded so far, as timeline entries
    let mut lists_of: Vec<Vec<Lists>> = vec![Vec::new(); entries.len()];
    for &k in order {
        let e = &entries[k];
        let au = &src.aus[e.src];
        let first = au.first_slice();
        if let Some(first) = first {
            let sps = first.sps.clone();
            let max_fn = sps.max_frame_num();
            let max_refs = sps.max_num_ref_frames.max(1) as usize;
            let idr = au.is_idr();
            let is_ref = au.is_reference();
            // consecutive IDR pictures need different idr_pic_id values
            let idr_pic_id = match prev_idr_pic_id {
                Some(p) if idr && p == first.header.idr_pic_id => (p + 1) % 65536,
                _ => first.header.idr_pic_id,
            };
            prev_idr_pic_id = idr.then_some(idr_pic_id);
            if idr {
                prev_ref_frame_num = 0;
                dpb.refs.clear();
            }
            let frame_num = if idr { 0 } else { (prev_ref_frame_num + 1) % max_fn };
            let poc = pocs[k];
            if idr {
                if poc < 0 {
                    bail!("frame {}: an IDR frame with a negative picture order count", e.src);
                }
                max_idr_poc = max_idr_poc.max(poc);
            } else {
                max_poc_step = max_poc_step.max((poc - prev_ref_poc).abs());
            }
            let targets = &src.refs[e.src];
            let marking = if is_ref && !idr {
                remap_marking(&first.header.dec_ref_pic_marking, &targets.marking, &dpb, frame_num, max_fn, max_refs)
            } else {
                first.header.dec_ref_pic_marking.clone()
            };
            let mut frame = Vec::new();
            for (i, s) in au.slices().enumerate() {
                let mut h = s.header.clone();
                if idr {
                    h.idr_pic_id = idr_pic_id;
                } else {
                    h.frame_num = frame_num;
                    h.dec_ref_pic_marking.clone_from(&marking);
                }
                let st = h.slice_type();
                let no_targets = [Vec::new(), Vec::new()];
                let [t0, t1] = targets.lists.get(i).unwrap_or(&no_targets);
                if !st.is_intra() && !dpb.refs.is_empty() {
                    let active0 = h.num_ref_idx_l0_active_minus1 as usize + 1;
                    if st == SliceType::B {
                        let (l0, l1) = b_lists(&dpb, poc);
                        let active1 = h.num_ref_idx_l1_active_minus1 as usize + 1;
                        h.rplm_l0 = fix_list(&l0, h.rplm_l0.take(), t0, &dpb, frame_num, active0, max_fn);
                        h.rplm_l1 = fix_list(&l1, h.rplm_l1.take(), t1, &dpb, frame_num, active1, max_fn);
                    } else {
                        let l0: Vec<u32> = dpb.refs.iter().rev().map(|r| r.frame_num).collect();
                        h.rplm_l0 = fix_list(&l0, h.rplm_l0.take(), t0, &dpb, frame_num, active0, max_fn);
                    }
                }
                let lists = slice_lists(&dpb, &h, poc, frame_num, max_fn).map(|l| l.iter().map(|f| f.map(|r| r.pos)).collect());
                if st == SliceType::B && !h.direct_spatial_mv_pred_flag && !temporal_direct_holds(src, entries, &lists, i, e.src, &lists_of)
                {
                    h.direct_spatial_mv_pred_flag = true;
                }
                lists_of[k].push(lists);
                frame.push(h);
            }
            headers.push(frame);
            if is_ref {
                let cur = Ref { frame_num, poc, src: e.src, pos: k };
                dpb.mark(cur, idr, marking.as_ref(), max_refs, max_fn);
                prev_ref_poc = if first.header.has_mmco5() { 0 } else { poc };
                prev_ref_frame_num = if first.header.has_mmco5() { 0 } else { frame_num };
            }
            needs.reorder_frames = needs.reorder_frames.max(waiting.range(k + 1..).count() as u32);
        } else {
            headers.push(Vec::new());
        }
        decoded[k] = true;
        waiting.insert(k);
        while next_shown < entries.len() && decoded[next_shown] {
            waiting.remove(&next_shown);
            next_shown += 1;
        }
        if let Some(first) = first {
            // a stream without reference frames (all intra) never refers to the ones it marks
            let refs = dpb.refs.len().min(first.sps.max_num_ref_frames as usize);
            let held = refs + waiting.iter().filter(|&&p| !dpb.refs.iter().any(|r| r.pos == p)).count();
            needs.dpb_frames = needs.dpb_frames.max(held as u32);
        }
    }

    // pic_order_cnt_lsb must tell the steps apart from wraps (8.2.1.1)
    let Some(bits) = (4..=16).find(|b| max_poc_step < 1 << (b - 1) && max_idr_poc < 1 << b) else {
        bail!("the edit puts frames too far apart from their reference frames for the picture order count");
    };
    needs.log2_max_poc_lsb = bits;
    for (frame, &k) in headers.iter_mut().zip(order) {
        let au = &src.aus[entries[k].src];
        if au.is_idr() {
            continue;
        }
        for (h, s) in frame.iter_mut().zip(au.slices()) {
            if s.sps.pic_order_cnt_type == 0 {
                h.pic_order_cnt_lsb = pocs[k].rem_euclid(1 << bits.max(s.sps.log2_max_poc_lsb())) as u32;
            }
        }
    }
    Ok(Plan { headers, needs })
}

/// Temporal direct prediction (8.4.1.2.3) takes the motion of the first picture of
/// list 1 and the pictures that one predicts from, which have to be in list 0. That
/// holds as encoded when slice `slice` of input frame `frame` (now with `lists`) and
/// that picture hold the same frames as in the input; otherwise spatial direct
/// prediction stands in, which needs nothing but the lists.
fn temporal_direct_holds(src: &Source, entries: &[TimelineEntry], lists: &Lists, slice: usize, frame: usize, held: &[Vec<Lists>]) -> bool {
    let as_input = |l: &Lists| l.clone().map(|v| v.iter().map(|p| p.map(|p| entries[p].src)).collect::<Vec<_>>());
    let same = |out: &[Lists], input: &[Lists]| out.len() == input.len() && out.iter().zip(input).all(|(o, i)| as_input(o) == *i);
    let Some(col) = lists[1].first().copied().flatten() else { return false };
    src.refs[frame].held.get(slice).is_some_and(|orig| as_input(lists) == *orig) && same(&held[col], &src.refs[entries[col].src].held)
}

/// The reference lists of a slice of frame `frame_num` (picture order count `poc`)
/// whose decoder holds `dpb`: list 1 only for B slices, empty for intra slices or
/// lists with long-term pictures.
fn slice_lists<'a>(dpb: &'a Dpb, h: &SliceHeader, poc: i64, frame_num: u32, max_frame_num: u32) -> [Vec<Option<&'a Ref>>; 2] {
    let st = h.slice_type();
    if st.is_intra() || dpb.refs.is_empty() {
        return [Vec::new(), Vec::new()];
    }
    let (l0, l1) = if st == SliceType::B { b_lists(dpb, poc) } else { (dpb.refs.iter().rev().map(|r| r.frame_num).collect(), Vec::new()) };
    let list = |initial: &[u32], ops: &Option<Vec<RplmOp>>, active: u32| {
        let frames = modified_list(initial, ops.as_deref(), frame_num, active as usize + 1, max_frame_num).unwrap_or_default();
        frames.into_iter().map(|f| f.and_then(|f| dpb.find(f))).collect()
    };
    let l1 = if st == SliceType::B { list(&l1, &h.rplm_l1, h.num_ref_idx_l1_active_minus1) } else { Vec::new() };
    [list(&l0, &h.rplm_l0, h.num_ref_idx_l0_active_minus1), l1]
}

/// Initial reference lists of a B frame with picture order count `poc` (8.2.4.2.3), as
/// frame_num values.
fn b_lists(dpb: &Dpb, poc: i64) -> (Vec<u32>, Vec<u32>) {
    let mut before: Vec<&Ref> = dpb.refs.iter().filter(|r| r.poc < poc).collect();
    let mut after: Vec<&Ref> = dpb.refs.iter().filter(|r| r.poc > poc).collect();
    before.sort_by_key(|r| std::cmp::Reverse(r.poc));
    after.sort_by_key(|r| r.poc);
    let l0: Vec<u32> = before.iter().chain(&after).map(|r| r.frame_num).collect();
    let mut l1: Vec<u32> = after.iter().chain(&before).map(|r| r.frame_num).collect();
    if l1.len() > 1 && l1 == l0 {
        l1.swap(0, 1);
    }
    (l0, l1)
}

/// The reference marking of frame `frame_num`, its MMCO 1 operations pointing at the
/// frames they were meant for (the oldest copy). Operations whose frame the decoder no
/// longer holds are left out; when what remains would keep too many reference frames,
/// the sliding window takes over.
fn remap_marking(
    marking: &Option<DecRefPicMarking>,
    targets: &[Option<usize>],
    dpb: &Dpb,
    frame_num: u32,
    max_frame_num: u32,
    max_refs: usize,
) -> Option<DecRefPicMarking> {
    let Some(DecRefPicMarking::NonIdr(Some(ops))) = marking else { return marking.clone() };
    let mut kept = Vec::new();
    let mut only_short_term = true;
    for (i, op) in ops.iter().enumerate() {
        match op {
            Mmco::ForgetShort(_) => {
                if let Some(r) = targets.get(i).copied().flatten().and_then(|t| dpb.refs.iter().find(|r| r.src == t)) {
                    kept.push(Mmco::ForgetShort((frame_num + max_frame_num - r.frame_num) % max_frame_num - 1));
                }
            }
            other => {
                only_short_term = false;
                kept.push(other.clone());
            }
        }
    }
    if only_short_term && (kept.is_empty() || dpb.refs.len() - kept.len() + 1 > max_refs) {
        return Some(DecRefPicMarking::NonIdr(None));
    }
    Some(DecRefPicMarking::NonIdr(Some(kept)))
}

/// ref_pic_list_modification for one list of a slice of frame `frame_num`, whose
/// initial list is `initial` (frame_num values) and which uses `active` indices. The
/// coded modifications are kept if they still point at the pictures they were coded
/// against (`targets`), or else at least at pictures the decoder has.
fn fix_list(
    initial: &[u32],
    ops: Option<Vec<RplmOp>>,
    targets: &[Option<usize>],
    dpb: &Dpb,
    frame_num: u32,
    active: usize,
    max_frame_num: u32,
) -> Option<Vec<RplmOp>> {
    if let Some(ops) = &ops
        && targets.len() == ops.len()
        && let Some(pics) = targets.iter().map(|t| t.and_then(|t| dpb.latest(t)).map(|r| r.frame_num)).collect::<Option<Vec<_>>>()
    {
        let new = encode_list(ops, &pics, frame_num, max_frame_num);
        if list_complete(initial, Some(&new), frame_num, active, max_frame_num) {
            return Some(new);
        }
    }
    if list_complete(initial, ops.as_deref(), frame_num, active, max_frame_num) {
        return ops;
    }
    (active > initial.len()).then(|| fill_list(initial, frame_num, active, max_frame_num))
}

/// Modifications putting `pics` at the front of a list of frame `frame_num`, each coded
/// in the direction (modification_of_pic_nums_idc 0 or 1) of the corresponding `ops`.
fn encode_list(ops: &[RplmOp], pics: &[u32], frame_num: u32, max_frame_num: u32) -> Vec<RplmOp> {
    let mut pred = frame_num;
    ops.iter()
        .zip(pics)
        .map(|(&(idc, _), &pic)| {
            let diff = (if idc == 0 { pred + max_frame_num - pic } else { pic + max_frame_num - pred }) % max_frame_num;
            pred = pic;
            // a difference of MaxPicNum wraps around to the same picture
            (idc, if diff == 0 { max_frame_num } else { diff } - 1)
        })
        .collect()
}

/// Whether a list starting as `initial` (frame_num values; 8.2.4.2) and modified by
/// `ops` (8.2.4.3.1) has a picture for each of its `active` entries. Lists with
/// long-term pictures are not checked.
fn list_complete(initial: &[u32], ops: Option<&[RplmOp]>, frame_num: u32, active: usize, max_frame_num: u32) -> bool {
    if ops.unwrap_or_default().iter().any(|&(idc, _)| idc > 1) {
        return true;
    }
    modified_list(initial, ops, frame_num, active, max_frame_num).is_some_and(|l| l.iter().all(Option::is_some))
}

/// The list starting as `initial` (frame_num values) modified by `ops`, `active`
/// entries long, None where an entry has no picture; None if a modification refers to
/// no picture of `initial` or to a long-term picture.
fn modified_list(initial: &[u32], ops: Option<&[RplmOp]>, frame_num: u32, active: usize, max_frame_num: u32) -> Option<Vec<Option<u32>>> {
    let mut list: Vec<Option<u32>> = initial.iter().map(|&f| Some(f)).take(active).collect();
    list.resize(active, None);
    let mut pred = frame_num;
    for (i, &(idc, v)) in ops.unwrap_or_default().iter().enumerate() {
        let pic = match idc {
            _ if v >= max_frame_num => return None,
            0 => (pred + max_frame_num - (v + 1)) % max_frame_num,
            1 => (pred + v + 1) % max_frame_num,
            _ => return None,
        };
        if !initial.contains(&pic) || i >= active {
            return None;
        }
        pred = pic;
        // insert at index i, drop the later copies of the same picture
        list.insert(i, Some(pic));
        let rest: Vec<_> = list.drain(i + 1..).filter(|&p| p != Some(pic)).collect();
        list.extend(rest);
        list.resize(active, None);
    }
    Some(list)
}

/// ref_pic_list_modification for a list that uses `active` indices but starts as only
/// `initial` (frame_num values): the initial order, the missing entries repeating the
/// last picture.
fn fill_list(initial: &[u32], frame_num: u32, active: usize, max_frame_num: u32) -> Vec<RplmOp> {
    let pics: Vec<u32> = (0..active).map(|i| initial[i.min(initial.len() - 1)]).collect();
    encode_list(&vec![(0, 0); active], &pics, frame_num, max_frame_num)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// initial list of a P frame when the decoder holds `dpb` (oldest first)
    fn p_list(dpb: &[u32]) -> Vec<u32> {
        dpb.iter().rev().copied().collect()
    }

    #[test]
    fn missing_references_repeat_the_oldest_frame() {
        // frames 6 and 7 held, frame 8 refers to four: 7, 6, 6, 6
        assert_eq!(fill_list(&p_list(&[6, 7]), 8, 4, 16), [(0, 0), (0, 0), (0, 15), (0, 15)]);
        // across the frame_num wrap: 15 then 0, current 1
        assert_eq!(fill_list(&p_list(&[15, 0]), 1, 2, 16), [(0, 0), (0, 0)]);
        assert_eq!(fill_list(&p_list(&[14]), 2, 2, 16), [(0, 3), (0, 15)]);
        for (dpb, cur, active) in [(&[6, 7][..], 8, 4), (&[15, 0], 1, 3), (&[14], 2, 2)] {
            let initial = p_list(dpb);
            assert!(!list_complete(&initial, None, cur, active, 16));
            assert!(list_complete(&initial, Some(&fill_list(&initial, cur, active, 16)), cur, active, 16));
        }
    }

    #[test]
    fn reference_lists_are_checked_against_the_held_frames() {
        // frames 3, 4, 5 held, current 6: the initial list is 5, 4, 3
        assert!(list_complete(&p_list(&[3, 4, 5]), None, 6, 3, 16));
        assert!(!list_complete(&p_list(&[4, 5]), None, 6, 3, 16));
        // move 3 to the front: 3, 5, 4
        assert!(list_complete(&p_list(&[3, 4, 5]), Some(&[(0, 2)]), 6, 3, 16));
        // refers to frame 2, which is gone
        assert!(!list_complete(&p_list(&[3, 4, 5]), Some(&[(0, 3)]), 6, 3, 16));
        // the same picture twice, then the next one: 5, 5, 4
        assert!(list_complete(&p_list(&[4, 5]), Some(&[(0, 0), (0, 15), (0, 0)]), 6, 3, 16));
        // two copies of 5 and nothing for the third entry
        assert!(!list_complete(&p_list(&[5]), Some(&[(0, 0), (0, 15)]), 6, 3, 16));
    }

    #[test]
    fn modifications_are_coded_in_their_own_direction() {
        // x264 with weighted prediction: previous P twice, the B-frame after it, the IDR
        let ops = [(0, 1), (0, 15), (1, 0), (0, 1)];
        assert_eq!(encode_list(&ops, &[1, 1, 2, 0], 3, 16), ops);
        // the same pictures across the frame_num wrap
        assert_eq!(encode_list(&ops, &[15, 15, 0, 14], 1, 16), ops);
        assert_eq!(encode_list(&ops, &[5, 5, 7, 4], 9, 16), [(0, 3), (0, 15), (1, 1), (0, 2)]);
    }

    fn r(frame_num: u32, poc: i64) -> Ref {
        Ref { frame_num, poc, src: frame_num as usize, pos: 0 }
    }

    #[test]
    fn b_lists_are_ordered_by_picture_order_count() {
        let dpb = Dpb { refs: vec![r(0, 0), r(1, 8), r(2, 4)] };
        assert_eq!(b_lists(&dpb, 2), (vec![0, 2, 1], vec![2, 1, 0]));
        assert_eq!(b_lists(&dpb, 6), (vec![2, 0, 1], vec![1, 2, 0]));
        // only earlier pictures: list 1 starts with the second one
        assert_eq!(b_lists(&dpb, 10), (vec![1, 2, 0], vec![2, 1, 0]));
        assert_eq!(b_lists(&Dpb { refs: vec![r(0, 0)] }, 2), (vec![0], vec![0]));
    }

    #[test]
    fn marking_follows_the_frames_it_was_meant_for() {
        let mut dpb = Dpb::default();
        for (f, src) in [(0, 10), (1, 11), (2, 12), (3, 13)] {
            dpb.refs.push(Ref { frame_num: f, poc: 0, src, pos: 0 });
        }
        let ops = Some(DecRefPicMarking::NonIdr(Some(vec![Mmco::ForgetShort(9), Mmco::ForgetShort(9)])));
        // frame 4 forgets input frames 10 and 12 (frame_num 0 and 2)
        let new = remap_marking(&ops, &[Some(10), Some(12)], &dpb, 4, 16, 4);
        assert_eq!(new, Some(DecRefPicMarking::NonIdr(Some(vec![Mmco::ForgetShort(3), Mmco::ForgetShort(1)]))));
        // input frame 12 is gone: one frame forgotten, the reference frames fit
        let new = remap_marking(&ops, &[Some(10), Some(99)], &dpb, 4, 16, 4);
        assert_eq!(new, Some(DecRefPicMarking::NonIdr(Some(vec![Mmco::ForgetShort(3)]))));
        // nothing forgotten and the decoder full: sliding window
        let new = remap_marking(&ops, &[Some(98), Some(99)], &dpb, 4, 16, 4);
        assert_eq!(new, Some(DecRefPicMarking::NonIdr(None)));
    }
}
