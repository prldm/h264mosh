//! The reference pictures a decoder holds and the reference lists of every slice
//! (8.2.4 and 8.2.5), followed through a stream of frames in decoding order. B slices
//! need them to work out their motion: direct prediction takes the motion of the first
//! picture of list 1 and the picture order counts of the pictures in the lists.

use crate::nal::NAL_IDR;
use crate::poc::PocDecoder;
use crate::slice::{DecRefPicMarking, Mmco, RplmOp, SliceHeader, SliceType};
use crate::stream::AccessUnit;
use anyhow::{Result, bail};

/// A reference picture: the access unit it was decoded from (its index in decoding
/// order), its picture order count and whether it is a long-term reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefPic {
    pub pic: usize,
    pub poc: i64,
    pub long_term: bool,
}

/// Reference lists 0 and 1 of a slice, `num_ref_idx_lX_active` entries long; None
/// where an index has no picture.
pub type RefLists = [Vec<Option<RefPic>>; 2];

/// What the decoder knows when it decodes a picture: its picture order count and the
/// reference lists of its slices.
#[derive(Clone, Debug)]
pub struct PicRefs {
    pub poc: i64,
    pub slices: Vec<RefLists>,
}

/// A reference frame in the decoded picture buffer.
#[derive(Clone, Copy, Debug)]
struct Frame {
    pic: usize,
    frame_num: u32,
    poc: i64,
    /// LongTermFrameIdx of a long-term reference
    long_term: Option<u32>,
}

impl Frame {
    fn ref_pic(&self) -> RefPic {
        RefPic { pic: self.pic, poc: self.poc, long_term: self.long_term.is_some() }
    }
}

#[derive(Default)]
struct Dpb {
    frames: Vec<Frame>,
    prev_ref_frame_num: u32,
}

impl Dpb {
    /// PicNum of a short-term frame for a picture with frame_num `cur` (8.2.4.1).
    fn pic_num(f: &Frame, cur: u32, max_frame_num: u32) -> i64 {
        if f.frame_num > cur { f.frame_num as i64 - max_frame_num as i64 } else { f.frame_num as i64 }
    }

    fn short(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(|f| f.long_term.is_none())
    }

    fn long(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(|f| f.long_term.is_some())
    }

    /// Initial reference lists of a slice (8.2.4.2.1, 8.2.4.2.3), before truncation.
    fn initial(&self, slice_type: SliceType, poc: i64, frame_num: u32, max_frame_num: u32) -> [Vec<Frame>; 2] {
        let mut long: Vec<Frame> = self.long().copied().collect();
        long.sort_by_key(|f| f.long_term);
        match slice_type {
            SliceType::P | SliceType::SP => {
                let mut l0: Vec<Frame> = self.short().copied().collect();
                l0.sort_by_key(|f| std::cmp::Reverse(Self::pic_num(f, frame_num, max_frame_num)));
                l0.extend(long);
                [l0, Vec::new()]
            }
            SliceType::B => {
                let mut before: Vec<Frame> = self.short().filter(|f| f.poc < poc).copied().collect();
                before.sort_by_key(|f| std::cmp::Reverse(f.poc));
                let mut after: Vec<Frame> = self.short().filter(|f| f.poc > poc).copied().collect();
                after.sort_by_key(|f| f.poc);
                let l0: Vec<Frame> = before.iter().chain(&after).chain(&long).copied().collect();
                let mut l1: Vec<Frame> = after.iter().chain(&before).chain(&long).copied().collect();
                if l1.len() > 1 && l1.iter().map(|f| f.pic).eq(l0.iter().map(|f| f.pic)) {
                    l1.swap(0, 1);
                }
                [l0, l1]
            }
            _ => [Vec::new(), Vec::new()],
        }
    }

    /// The reference lists of a slice of the picture with picture order count `poc`.
    fn lists(&self, h: &SliceHeader, poc: i64, max_frame_num: u32) -> Result<RefLists> {
        let slice_type = h.slice_type();
        let initial = self.initial(slice_type, poc, h.frame_num, max_frame_num);
        let mut out: RefLists = [Vec::new(), Vec::new()];
        let lists = if slice_type == SliceType::B {
            2
        } else if slice_type.is_intra() {
            0
        } else {
            1
        };
        for (x, (init, out)) in initial.into_iter().zip(out.iter_mut()).enumerate().take(lists) {
            let active = 1 + if x == 0 { h.num_ref_idx_l0_active_minus1 } else { h.num_ref_idx_l1_active_minus1 } as usize;
            let mut list: Vec<Option<Frame>> = init.into_iter().map(Some).collect();
            list.resize(active, None);
            let ops = if x == 0 { &h.rplm_l0 } else { &h.rplm_l1 };
            if let Some(ops) = ops {
                self.modify(&mut list, ops, h.frame_num, max_frame_num)?;
            }
            *out = list.iter().map(|f| f.map(|f| f.ref_pic())).collect();
        }
        Ok(out)
    }

    /// ref_pic_list_modification of one list (8.2.4.3), frames only.
    fn modify(&self, list: &mut Vec<Option<Frame>>, ops: &[RplmOp], frame_num: u32, max_frame_num: u32) -> Result<()> {
        let active = list.len();
        let (cur, max) = (frame_num as i64, max_frame_num as i64);
        let mut pred = cur;
        list.push(None);
        for (idx, &(idc, value)) in ops.iter().enumerate() {
            let pic = match idc {
                0 | 1 => {
                    let abs = value as i64 + 1;
                    let mut no_wrap = if idc == 0 { pred - abs } else { pred + abs };
                    if no_wrap < 0 {
                        no_wrap += max;
                    } else if no_wrap >= max {
                        no_wrap -= max;
                    }
                    pred = no_wrap;
                    let pic_num = if no_wrap > cur { no_wrap - max } else { no_wrap };
                    self.short().find(|f| Self::pic_num(f, frame_num, max_frame_num) == pic_num)
                }
                2 => self.long().find(|f| f.long_term == Some(value)),
                _ => break,
            };
            let Some(pic) = pic.copied() else { bail!("a reference list modification names a picture the decoder does not hold") };
            if idx >= active {
                bail!("more reference list modifications than list entries");
            }
            list.insert(idx, Some(pic));
            // later entries for the same picture go
            let mut k = idx + 1;
            while k < list.len() {
                if list[k].is_some_and(|f| f.pic == pic.pic) {
                    list.remove(k);
                } else {
                    k += 1;
                }
            }
            list.resize(active + 1, None);
        }
        list.truncate(active);
        Ok(())
    }

    /// Reference picture marking after decoding picture `cur` (8.2.5).
    fn mark(&mut self, mut cur: Frame, idr: bool, marking: Option<&DecRefPicMarking>, max_refs: u32, max_frame_num: u32) {
        let frame_num = cur.frame_num;
        let pic_num = |f: &Frame| Self::pic_num(f, frame_num, max_frame_num);
        if idr {
            self.frames.clear();
            if let Some(DecRefPicMarking::Idr { long_term_reference_flag: true, .. }) = marking {
                cur.long_term = Some(0);
            }
        } else if let Some(DecRefPicMarking::NonIdr(Some(ops))) = marking {
            for op in ops {
                match *op {
                    Mmco::ForgetShort(d) => {
                        let x = frame_num as i64 - (d as i64 + 1);
                        self.frames.retain(|f| f.long_term.is_some() || pic_num(f) != x);
                    }
                    Mmco::ForgetLong(n) => self.frames.retain(|f| f.long_term != Some(n)),
                    Mmco::ShortToLong(d, idx) => {
                        let x = frame_num as i64 - (d as i64 + 1);
                        self.frames.retain(|f| f.long_term != Some(idx));
                        if let Some(f) = self.frames.iter_mut().find(|f| f.long_term.is_none() && pic_num(f) == x) {
                            f.long_term = Some(idx);
                        }
                    }
                    Mmco::MaxLongTermIdx(plus1) => self.frames.retain(|f| f.long_term.is_none_or(|i| i < plus1)),
                    Mmco::Reset => {
                        self.frames.clear();
                        cur.frame_num = 0;
                        cur.poc = 0;
                    }
                    Mmco::CurrentToLong(idx) => {
                        self.frames.retain(|f| f.long_term != Some(idx));
                        cur.long_term = Some(idx);
                    }
                }
            }
        } else if self.frames.len() as u32 >= max_refs.max(1)
            && let Some(oldest) = self.short().min_by_key(|f| pic_num(f)).map(|f| f.pic)
        {
            // sliding window
            self.frames.retain(|f| f.pic != oldest);
        }
        self.prev_ref_frame_num = cur.frame_num;
        self.frames.push(cur);
    }
}

/// The picture order count and reference lists of every access unit of a stream in
/// decoding order. A picture whose references cannot be followed (field pictures, gaps
/// in frame_num, lists naming pictures that are not there) gets an error, and so do
/// the ones after it until the next IDR picture.
pub fn follow<'a>(aus: impl IntoIterator<Item = &'a AccessUnit>) -> Vec<Result<PicRefs, String>> {
    let mut pocs = PocDecoder::new();
    let mut dpb = Dpb::default();
    let mut broken: Option<String> = None;
    let mut out = Vec::new();
    for (i, au) in aus.into_iter().enumerate() {
        let Some(nal) = au.first_vcl() else {
            out.push(Err("the frame has no slices".into()));
            continue;
        };
        let Some(first) = nal.slice() else {
            out.push(Err("the frame has slices that could not be parsed".into()));
            continue;
        };
        let idr = nal.nal_type() == NAL_IDR;
        let poc = pocs.next(nal.header, &first.header, &first.sps);
        if idr {
            dpb = Dpb::default();
            broken = None;
        }
        let max_frame_num = first.sps.max_frame_num();
        let h = &first.header;
        let result = (|| -> Result<PicRefs> {
            if let Some(reason) = &broken {
                bail!("{reason}");
            }
            if h.field_pic_flag {
                bail!("field pictures are not followed");
            }
            let next = (dpb.prev_ref_frame_num + 1) % max_frame_num;
            if !idr && h.frame_num != dpb.prev_ref_frame_num && h.frame_num != next {
                bail!("frame_num jumps from {} to {}", dpb.prev_ref_frame_num, h.frame_num);
            }
            let slices = au.slices().map(|s| dpb.lists(&s.header, poc, max_frame_num)).collect::<Result<Vec<_>>>()?;
            Ok(PicRefs { poc, slices })
        })();
        if let Err(e) = &result {
            broken = Some(format!("{e:#}"));
        }
        out.push(result.map_err(|e| format!("{e:#}")));
        if nal.header.nal_ref_idc != 0 && broken.is_none() {
            let cur = Frame { pic: i, frame_num: h.frame_num, poc, long_term: None };
            dpb.mark(cur, idr, h.dec_ref_pic_marking.as_ref(), first.sps.max_num_ref_frames, max_frame_num);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pic: usize, frame_num: u32, poc: i64) -> Frame {
        Frame { pic, frame_num, poc, long_term: None }
    }

    fn pics(list: &[Frame]) -> Vec<usize> {
        list.iter().map(|f| f.pic).collect()
    }

    #[test]
    fn p_lists_start_with_the_latest_frame_num() {
        // frame_num wraps at 16: 15 and 14 are older than 1
        let dpb = Dpb { frames: vec![frame(0, 14, 0), frame(1, 15, 2), frame(2, 1, 6)], ..Default::default() };
        let [l0, l1] = dpb.initial(SliceType::P, 8, 2, 16);
        assert_eq!((pics(&l0), l1.len()), (vec![2, 1, 0], 0));
    }

    #[test]
    fn b_lists_go_out_from_the_current_picture_order_count() {
        let dpb = Dpb { frames: vec![frame(0, 0, 0), frame(1, 1, 8), frame(2, 2, 4)], ..Default::default() };
        let [l0, l1] = dpb.initial(SliceType::B, 2, 3, 16);
        assert_eq!((pics(&l0), pics(&l1)), (vec![0, 2, 1], vec![2, 1, 0]));
        // all before it: list 1 would be list 0, so its first two swap
        let [l0, l1] = dpb.initial(SliceType::B, 10, 3, 16);
        assert_eq!((pics(&l0), pics(&l1)), (vec![1, 2, 0], vec![2, 1, 0]));
    }

    #[test]
    fn modifications_can_repeat_a_picture() {
        // x264's weighted duplicates: the newest frame twice, by wrapping around
        let dpb = Dpb { frames: vec![frame(0, 0, 0), frame(1, 1, 6)], ..Default::default() };
        let mut list = vec![Some(frame(1, 1, 6)), Some(frame(0, 0, 0)), None];
        dpb.modify(&mut list, &[(0, 0), (1, 15)], 2, 16).unwrap();
        assert_eq!(list.iter().map(|f| f.map(|f| f.pic)).collect::<Vec<_>>(), [Some(1), Some(1), Some(0)]);
        // a picture the decoder does not hold
        assert!(dpb.modify(&mut list, &[(0, 5)], 2, 16).is_err());
    }

    #[test]
    fn marking_slides_and_follows_mmco() {
        let mut dpb = Dpb::default();
        for (pic, frame_num) in [(0, 0), (1, 1), (2, 2)] {
            dpb.mark(frame(pic, frame_num, 2 * pic as i64), pic == 0, None, 2, 16);
        }
        assert_eq!(pics(&dpb.frames), [1, 2], "the oldest frame slid out");
        // MMCO 1 forgets frame_num 1, MMCO 6 keeps the current one as long-term 0
        let ops = DecRefPicMarking::NonIdr(Some(vec![Mmco::ForgetShort(1), Mmco::CurrentToLong(0)]));
        dpb.mark(frame(3, 3, 6), false, Some(&ops), 4, 16);
        assert_eq!(pics(&dpb.frames), [2, 3]);
        assert_eq!(dpb.frames[1].long_term, Some(0));
        let [l0, _] = dpb.initial(SliceType::P, 8, 4, 16);
        assert_eq!(pics(&l0), [2, 3], "long-term references come last");
    }
}
