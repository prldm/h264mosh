//! Picture order count decoding (8.2.1), used to report display order and to detect
//! frame reordering (B-frames).

use crate::nal::NalHeader;
use crate::slice::SliceHeader;
use crate::sps::Sps;

#[derive(Default)]
pub struct PocDecoder {
    prev_poc_msb: i64,
    prev_poc_lsb: i64,
    prev_frame_num_offset: i64,
    prev_frame_num: u32,
}

impl PocDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// POC of the picture whose first slice is given; must be called in decoding order.
    pub fn next(&mut self, nal: NalHeader, h: &SliceHeader, sps: &Sps) -> i64 {
        let idr = nal.nal_unit_type == crate::nal::NAL_IDR;
        let is_ref = nal.nal_ref_idc != 0;
        let mmco5 = h.has_mmco5();
        let (top, bottom) = match sps.pic_order_cnt_type {
            0 => {
                if idr {
                    self.prev_poc_msb = 0;
                    self.prev_poc_lsb = 0;
                }
                let max = sps.max_poc_lsb() as i64;
                let lsb = h.pic_order_cnt_lsb as i64;
                let (pm, pl) = (self.prev_poc_msb, self.prev_poc_lsb);
                let msb = if lsb < pl && pl - lsb >= max / 2 {
                    pm + max
                } else if lsb > pl && lsb - pl > max / 2 {
                    pm - max
                } else {
                    pm
                };
                let top = msb + lsb;
                let bottom = if h.field_pic_flag { top } else { top + h.delta_pic_order_cnt_bottom as i64 };
                if is_ref {
                    if mmco5 {
                        self.prev_poc_msb = 0;
                        self.prev_poc_lsb = if h.bottom_field_flag { 0 } else { top - top.min(bottom) };
                    } else {
                        self.prev_poc_msb = msb;
                        self.prev_poc_lsb = lsb;
                    }
                }
                (top, bottom)
            }
            t => {
                let max_fn = sps.max_frame_num() as i64;
                let fno = if idr {
                    0
                } else if self.prev_frame_num > h.frame_num {
                    self.prev_frame_num_offset + max_fn
                } else {
                    self.prev_frame_num_offset
                };
                let r = if t == 1 { poc_type1(sps, h, is_ref, fno) } else { poc_type2(idr, is_ref, fno + h.frame_num as i64) };
                self.prev_frame_num_offset = if mmco5 { 0 } else { fno };
                self.prev_frame_num = if mmco5 { 0 } else { h.frame_num };
                r
            }
        };
        if !h.field_pic_flag {
            top.min(bottom)
        } else if h.bottom_field_flag {
            bottom
        } else {
            top
        }
    }
}

fn poc_type1(sps: &Sps, h: &SliceHeader, is_ref: bool, frame_num_offset: i64) -> (i64, i64) {
    let cycle = &sps.offset_for_ref_frame;
    let mut abs_frame_num = if cycle.is_empty() { 0 } else { frame_num_offset + h.frame_num as i64 };
    if !is_ref && abs_frame_num > 0 {
        abs_frame_num -= 1;
    }
    let mut expected = 0i64;
    if abs_frame_num > 0 {
        let n = cycle.len() as i64;
        let cycle_cnt = (abs_frame_num - 1) / n;
        let in_cycle = ((abs_frame_num - 1) % n) as usize;
        let per_cycle: i64 = cycle.iter().map(|&o| o as i64).sum();
        expected = cycle_cnt * per_cycle + cycle[..=in_cycle].iter().map(|&o| o as i64).sum::<i64>();
    }
    if !is_ref {
        expected += sps.offset_for_non_ref_pic as i64;
    }
    let d = h.delta_pic_order_cnt;
    if !h.field_pic_flag {
        let top = expected + d[0] as i64;
        (top, top + sps.offset_for_top_to_bottom_field as i64 + d[1] as i64)
    } else if !h.bottom_field_flag {
        let top = expected + d[0] as i64;
        (top, top)
    } else {
        let bottom = expected + sps.offset_for_top_to_bottom_field as i64 + d[0] as i64;
        (bottom, bottom)
    }
}

fn poc_type2(idr: bool, is_ref: bool, abs: i64) -> (i64, i64) {
    let t = if idr {
        0
    } else if is_ref {
        2 * abs
    } else {
        2 * abs - 1
    };
    (t, t)
}
