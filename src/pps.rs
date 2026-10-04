//! Picture parameter set (7.3.2.2).

use crate::bits::{BitReader, BitWriter};
use crate::common::{RbspTail, ScalingList, parse_scaling_lists, write_scaling_lists};
use crate::sps::Sps;
use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_slice_groups_minus1: u32,
    pub slice_groups: Option<SliceGroups>,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    /// The optional High-profile part guarded by more_rbsp_data().
    pub ext: Option<PpsExt>,
    pub tail: RbspTail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SliceGroups {
    /// type 0: run_length_minus1 per group
    Interleaved(Vec<u32>),
    /// type 1
    Dispersed,
    /// type 2: (top_left, bottom_right) per group except the last
    Foreground(Vec<(u32, u32)>),
    /// types 3..5: (type, slice_group_change_direction_flag, slice_group_change_rate_minus1)
    Changing(u32, bool, u32),
    /// type 6: pic_size_in_map_units_minus1 + slice_group_id per map unit
    Explicit(Vec<u32>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PpsExt {
    pub transform_8x8_mode_flag: bool,
    pub scaling_lists: Option<Vec<Option<ScalingList>>>,
    pub second_chroma_qp_index_offset: i32,
}

fn ceil_log2(x: u32) -> u32 {
    if x <= 1 { 0 } else { 32 - (x - 1).leading_zeros() }
}

impl Pps {
    /// Just the ids, needed to find the SPS before the full parse.
    pub fn parse_ids(rbsp: &[u8]) -> Result<(u32, u32)> {
        let mut r = BitReader::new(rbsp);
        Ok((r.ue()?, r.ue()?))
    }

    /// `rbsp` excludes the NAL header byte. The SPS is only consulted for
    /// chroma_format_idc when the PPS carries 8x8 scaling lists.
    pub fn parse(rbsp: &[u8], sps: Option<&Sps>) -> Result<Self> {
        let mut r = BitReader::new(rbsp);
        let pps_id = r.ue()?;
        let sps_id = r.ue()?;
        if pps_id > 255 || sps_id > 31 {
            bail!("parameter set id out of range");
        }
        let entropy_coding_mode_flag = r.flag()?;
        let bottom_field_pic_order_in_frame_present_flag = r.flag()?;
        let num_slice_groups_minus1 = r.ue()?;
        if num_slice_groups_minus1 > 7 {
            bail!("num_slice_groups_minus1 {num_slice_groups_minus1} out of range");
        }
        let slice_groups = if num_slice_groups_minus1 > 0 {
            let n = num_slice_groups_minus1 as usize + 1;
            Some(match r.ue()? {
                0 => SliceGroups::Interleaved((0..n).map(|_| r.ue()).collect::<Result<_>>()?),
                1 => SliceGroups::Dispersed,
                2 => SliceGroups::Foreground((0..n - 1).map(|_| Ok((r.ue()?, r.ue()?))).collect::<Result<_>>()?),
                t @ 3..=5 => SliceGroups::Changing(t, r.flag()?, r.ue()?),
                6 => {
                    let units = r.ue()? as usize + 1;
                    if units > 1 << 21 {
                        bail!("slice group map with {units} map units is too large");
                    }
                    let bits = ceil_log2(num_slice_groups_minus1 + 1);
                    SliceGroups::Explicit((0..units).map(|_| r.u(bits)).collect::<Result<_>>()?)
                }
                t => bail!("slice_group_map_type {t} out of range"),
            })
        } else {
            None
        };
        let num_ref_idx_l0_default_active_minus1 = r.ue()?;
        let num_ref_idx_l1_default_active_minus1 = r.ue()?;
        let weighted_pred_flag = r.flag()?;
        let weighted_bipred_idc = r.u(2)?;
        let pic_init_qp_minus26 = r.se()?;
        let pic_init_qs_minus26 = r.se()?;
        let chroma_qp_index_offset = r.se()?;
        let deblocking_filter_control_present_flag = r.flag()?;
        let constrained_intra_pred_flag = r.flag()?;
        let redundant_pic_cnt_present_flag = r.flag()?;
        let ext = if r.more_rbsp_data() {
            let transform_8x8_mode_flag = r.flag()?;
            let scaling_lists = if r.flag()? {
                let chroma_format_idc = match sps {
                    Some(s) => s.chroma_format_idc,
                    None => bail!("PPS {pps_id} has scaling lists but SPS {sps_id} is unknown"),
                };
                let count = 6 + if chroma_format_idc != 3 { 2 } else { 6 } * transform_8x8_mode_flag as usize;
                Some(parse_scaling_lists(&mut r, count)?)
            } else {
                None
            };
            Some(PpsExt { transform_8x8_mode_flag, scaling_lists, second_chroma_qp_index_offset: r.se()? })
        } else {
            None
        };
        let tail = RbspTail::read(&r);
        Ok(Self {
            pps_id,
            sps_id,
            entropy_coding_mode_flag,
            bottom_field_pic_order_in_frame_present_flag,
            num_slice_groups_minus1,
            slice_groups,
            num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1,
            weighted_pred_flag,
            weighted_bipred_idc,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            deblocking_filter_control_present_flag,
            constrained_intra_pred_flag,
            redundant_pic_cnt_present_flag,
            ext,
            tail,
        })
    }

    pub fn write(&self, w: &mut BitWriter) {
        w.ue(self.pps_id);
        w.ue(self.sps_id);
        w.flag(self.entropy_coding_mode_flag);
        w.flag(self.bottom_field_pic_order_in_frame_present_flag);
        w.ue(self.num_slice_groups_minus1);
        if let Some(sg) = &self.slice_groups {
            match sg {
                SliceGroups::Interleaved(runs) => {
                    w.ue(0);
                    runs.iter().for_each(|&v| w.ue(v));
                }
                SliceGroups::Dispersed => w.ue(1),
                SliceGroups::Foreground(rects) => {
                    w.ue(2);
                    for &(tl, br) in rects {
                        w.ue(tl);
                        w.ue(br);
                    }
                }
                SliceGroups::Changing(t, dir, rate) => {
                    w.ue(*t);
                    w.flag(*dir);
                    w.ue(*rate);
                }
                SliceGroups::Explicit(ids) => {
                    w.ue(6);
                    w.ue(ids.len() as u32 - 1);
                    let bits = ceil_log2(self.num_slice_groups_minus1 + 1);
                    ids.iter().for_each(|&v| w.u(bits, v));
                }
            }
        }
        w.ue(self.num_ref_idx_l0_default_active_minus1);
        w.ue(self.num_ref_idx_l1_default_active_minus1);
        w.flag(self.weighted_pred_flag);
        w.u(2, self.weighted_bipred_idc);
        w.se(self.pic_init_qp_minus26);
        w.se(self.pic_init_qs_minus26);
        w.se(self.chroma_qp_index_offset);
        w.flag(self.deblocking_filter_control_present_flag);
        w.flag(self.constrained_intra_pred_flag);
        w.flag(self.redundant_pic_cnt_present_flag);
        if let Some(e) = &self.ext {
            w.flag(e.transform_8x8_mode_flag);
            w.flag(e.scaling_lists.is_some());
            if let Some(l) = &e.scaling_lists {
                write_scaling_lists(w, l);
            }
            w.se(e.second_chroma_qp_index_offset);
        }
        self.tail.write(w);
    }

    pub fn slice_group_map_type(&self) -> Option<u32> {
        self.slice_groups.as_ref().map(|sg| match sg {
            SliceGroups::Interleaved(_) => 0,
            SliceGroups::Dispersed => 1,
            SliceGroups::Foreground(_) => 2,
            SliceGroups::Changing(t, _, _) => *t,
            SliceGroups::Explicit(_) => 6,
        })
    }

    pub fn slice_group_change_rate(&self) -> u32 {
        match self.slice_groups {
            Some(SliceGroups::Changing(_, _, rate)) => rate + 1,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ceil_log2;

    #[test]
    fn ceil_log2_values() {
        assert_eq!([1, 2, 3, 4, 5, 8, 9].map(ceil_log2), [0, 1, 2, 2, 3, 3, 4]);
    }
}
