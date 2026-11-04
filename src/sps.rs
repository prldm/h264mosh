//! Sequence parameter set (7.3.2.1) including VUI (E.1.1) and HRD (E.1.2).

use crate::bits::{BitReader, BitWriter};
use crate::common::{RbspTail, ScalingList, parse_scaling_lists, write_scaling_lists};
use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    /// constraint_set0..5_flag + reserved_zero_2bits, as the raw byte
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub sps_id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub qpprime_y_zero_transform_bypass_flag: bool,
    /// Some(..) when seq_scaling_matrix_present_flag is set
    pub scaling_lists: Option<Vec<Option<ScalingList>>>,
    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    /// left, right, top, bottom
    pub frame_crop: Option<[u32; 4]>,
    pub vui: Option<Vui>,
    pub tail: RbspTail,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Vui {
    /// aspect_ratio_idc, and sar_width/sar_height when idc == 255
    pub aspect_ratio: Option<(u8, u16, u16)>,
    pub overscan_appropriate: Option<bool>,
    pub video_signal_type: Option<VideoSignalType>,
    /// chroma_sample_loc_type_top_field, bottom_field
    pub chroma_loc: Option<(u32, u32)>,
    pub timing: Option<Timing>,
    pub nal_hrd: Option<Hrd>,
    pub vcl_hrd: Option<Hrd>,
    pub low_delay_hrd_flag: bool,
    pub pic_struct_present_flag: bool,
    pub bitstream_restriction: Option<BitstreamRestriction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSignalType {
    pub video_format: u8,
    pub video_full_range_flag: bool,
    /// colour_primaries, transfer_characteristics, matrix_coefficients
    pub colour_description: Option<(u8, u8, u8)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timing {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub fixed_frame_rate_flag: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hrd {
    pub bit_rate_scale: u8,
    pub cpb_size_scale: u8,
    /// bit_rate_value_minus1, cpb_size_value_minus1, cbr_flag
    pub cpbs: Vec<(u32, u32, bool)>,
    pub initial_cpb_removal_delay_length_minus1: u8,
    pub cpb_removal_delay_length_minus1: u8,
    pub dpb_output_delay_length_minus1: u8,
    pub time_offset_length: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitstreamRestriction {
    pub motion_vectors_over_pic_boundaries_flag: bool,
    pub max_bytes_per_pic_denom: u32,
    pub max_bits_per_mb_denom: u32,
    pub log2_max_mv_length_horizontal: u32,
    pub log2_max_mv_length_vertical: u32,
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
}

fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
}

impl Sps {
    /// `rbsp` excludes the NAL header byte.
    pub fn parse(rbsp: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(rbsp);
        let profile_idc = r.u(8)? as u8;
        let constraint_flags = r.u(8)? as u8;
        let level_idc = r.u(8)? as u8;
        let sps_id = r.ue()?;
        if sps_id > 31 {
            bail!("seq_parameter_set_id {sps_id} out of range");
        }
        let (mut chroma_format_idc, mut separate_colour_plane_flag) = (1, false);
        let (mut bit_depth_luma_minus8, mut bit_depth_chroma_minus8) = (0, 0);
        let mut qpprime_y_zero_transform_bypass_flag = false;
        let mut scaling_lists = None;
        if has_chroma_info(profile_idc) {
            chroma_format_idc = r.ue()?;
            if chroma_format_idc > 3 {
                bail!("chroma_format_idc {chroma_format_idc} out of range");
            }
            if chroma_format_idc == 3 {
                separate_colour_plane_flag = r.flag()?;
            }
            bit_depth_luma_minus8 = r.ue()?;
            bit_depth_chroma_minus8 = r.ue()?;
            if bit_depth_luma_minus8 > 6 || bit_depth_chroma_minus8 > 6 {
                bail!("bit depth above 14 is not allowed");
            }
            qpprime_y_zero_transform_bypass_flag = r.flag()?;
            if r.flag()? {
                let count = if chroma_format_idc != 3 { 8 } else { 12 };
                scaling_lists = Some(parse_scaling_lists(&mut r, count)?);
            }
        }
        let log2_max_frame_num_minus4 = r.ue()?;
        if log2_max_frame_num_minus4 > 12 {
            bail!("log2_max_frame_num_minus4 {log2_max_frame_num_minus4} out of range");
        }
        let pic_order_cnt_type = r.ue()?;
        let mut log2_max_pic_order_cnt_lsb_minus4 = 0;
        let mut delta_pic_order_always_zero_flag = false;
        let mut offset_for_non_ref_pic = 0;
        let mut offset_for_top_to_bottom_field = 0;
        let mut offset_for_ref_frame = Vec::new();
        match pic_order_cnt_type {
            0 => {
                log2_max_pic_order_cnt_lsb_minus4 = r.ue()?;
                if log2_max_pic_order_cnt_lsb_minus4 > 12 {
                    bail!("log2_max_pic_order_cnt_lsb_minus4 out of range");
                }
            }
            1 => {
                delta_pic_order_always_zero_flag = r.flag()?;
                offset_for_non_ref_pic = r.se()?;
                offset_for_top_to_bottom_field = r.se()?;
                let n = r.ue()?;
                if n > 255 {
                    bail!("num_ref_frames_in_pic_order_cnt_cycle {n} out of range");
                }
                for _ in 0..n {
                    offset_for_ref_frame.push(r.se()?);
                }
            }
            2 => {}
            t => bail!("pic_order_cnt_type {t} out of range"),
        }
        let max_num_ref_frames = r.ue()?;
        let gaps_in_frame_num_value_allowed_flag = r.flag()?;
        let pic_width_in_mbs_minus1 = r.ue()?;
        let pic_height_in_map_units_minus1 = r.ue()?;
        // 16384 pixels per side: far beyond any level, and keeps per-picture buffers bounded
        if pic_width_in_mbs_minus1 >= 1024 || pic_height_in_map_units_minus1 >= 1024 {
            bail!("picture size {}x{} macroblocks is not supported", pic_width_in_mbs_minus1 + 1, pic_height_in_map_units_minus1 + 1);
        }
        let frame_mbs_only_flag = r.flag()?;
        let mb_adaptive_frame_field_flag = if !frame_mbs_only_flag { r.flag()? } else { false };
        let direct_8x8_inference_flag = r.flag()?;
        let frame_crop = if r.flag()? { Some([r.ue()?, r.ue()?, r.ue()?, r.ue()?]) } else { None };
        let vui = if r.flag()? { Some(Vui::parse(&mut r)?) } else { None };
        let tail = RbspTail::read(&r);
        Ok(Self {
            profile_idc,
            constraint_flags,
            level_idc,
            sps_id,
            chroma_format_idc,
            separate_colour_plane_flag,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            qpprime_y_zero_transform_bypass_flag,
            scaling_lists,
            log2_max_frame_num_minus4,
            pic_order_cnt_type,
            log2_max_pic_order_cnt_lsb_minus4,
            delta_pic_order_always_zero_flag,
            offset_for_non_ref_pic,
            offset_for_top_to_bottom_field,
            offset_for_ref_frame,
            max_num_ref_frames,
            gaps_in_frame_num_value_allowed_flag,
            pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1,
            frame_mbs_only_flag,
            mb_adaptive_frame_field_flag,
            direct_8x8_inference_flag,
            frame_crop,
            vui,
            tail,
        })
    }

    pub fn write(&self, w: &mut BitWriter) {
        w.u(8, self.profile_idc as u32);
        w.u(8, self.constraint_flags as u32);
        w.u(8, self.level_idc as u32);
        w.ue(self.sps_id);
        if has_chroma_info(self.profile_idc) {
            w.ue(self.chroma_format_idc);
            if self.chroma_format_idc == 3 {
                w.flag(self.separate_colour_plane_flag);
            }
            w.ue(self.bit_depth_luma_minus8);
            w.ue(self.bit_depth_chroma_minus8);
            w.flag(self.qpprime_y_zero_transform_bypass_flag);
            w.flag(self.scaling_lists.is_some());
            if let Some(l) = &self.scaling_lists {
                write_scaling_lists(w, l);
            }
        }
        w.ue(self.log2_max_frame_num_minus4);
        w.ue(self.pic_order_cnt_type);
        match self.pic_order_cnt_type {
            0 => w.ue(self.log2_max_pic_order_cnt_lsb_minus4),
            1 => {
                w.flag(self.delta_pic_order_always_zero_flag);
                w.se(self.offset_for_non_ref_pic);
                w.se(self.offset_for_top_to_bottom_field);
                w.ue(self.offset_for_ref_frame.len() as u32);
                for &o in &self.offset_for_ref_frame {
                    w.se(o);
                }
            }
            _ => {}
        }
        w.ue(self.max_num_ref_frames);
        w.flag(self.gaps_in_frame_num_value_allowed_flag);
        w.ue(self.pic_width_in_mbs_minus1);
        w.ue(self.pic_height_in_map_units_minus1);
        w.flag(self.frame_mbs_only_flag);
        if !self.frame_mbs_only_flag {
            w.flag(self.mb_adaptive_frame_field_flag);
        }
        w.flag(self.direct_8x8_inference_flag);
        w.flag(self.frame_crop.is_some());
        if let Some(c) = self.frame_crop {
            c.iter().for_each(|&v| w.ue(v));
        }
        w.flag(self.vui.is_some());
        if let Some(v) = &self.vui {
            v.write(w);
        }
        self.tail.write(w);
    }

    pub fn log2_max_frame_num(&self) -> u32 {
        self.log2_max_frame_num_minus4 + 4
    }

    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num()
    }

    pub fn log2_max_poc_lsb(&self) -> u32 {
        self.log2_max_pic_order_cnt_lsb_minus4 + 4
    }

    pub fn max_poc_lsb(&self) -> u32 {
        1 << self.log2_max_poc_lsb()
    }

    /// MaxDpbFrames of the level (A.3.1 h, Table A-1): frames the decoded picture buffer
    /// can hold at this picture size. Unknown levels get the largest buffer.
    pub fn max_dpb_frames(&self) -> u32 {
        let set3 = self.constraint_flags & 0x10 != 0;
        let mbs = match self.level_idc {
            9 | 10 => 396,
            11 if set3 && matches!(self.profile_idc, 66 | 77 | 88) => 396,
            11 => 900,
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110400,
            51 | 52 => 184320,
            _ => 696320,
        };
        (mbs / (self.pic_width_in_mbs() * self.frame_height_in_mbs()).max(1)).clamp(1, 16)
    }

    /// (max_num_reorder_frames, max_dec_frame_buffering), inferred when not signalled (E.2.1).
    pub fn reorder_limits(&self) -> (u32, u32) {
        if let Some(b) = self.vui.as_ref().and_then(|v| v.bitstream_restriction.as_ref()) {
            return (b.max_num_reorder_frames, b.max_dec_frame_buffering);
        }
        let intra_only = matches!(self.profile_idc, 44 | 86 | 100 | 110 | 122 | 244) && self.constraint_flags & 0x10 != 0;
        let n = if intra_only { 0 } else { self.max_dpb_frames() };
        (n, n)
    }

    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane_flag { 0 } else { self.chroma_format_idc }
    }

    pub fn pic_width_in_mbs(&self) -> u32 {
        self.pic_width_in_mbs_minus1 + 1
    }

    pub fn frame_height_in_mbs(&self) -> u32 {
        (2 - self.frame_mbs_only_flag as u32) * (self.pic_height_in_map_units_minus1 + 1)
    }

    /// Display size after cropping.
    pub fn size(&self) -> (u32, u32) {
        let (sub_w, sub_h) = match self.chroma_array_type() {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (crop_x, crop_y): (u32, u32) = if self.chroma_array_type() == 0 {
            (1, 2 - self.frame_mbs_only_flag as u32)
        } else {
            (sub_w, sub_h * (2 - self.frame_mbs_only_flag as u32))
        };
        let w = self.pic_width_in_mbs() * 16;
        let h = self.frame_height_in_mbs() * 16;
        match self.frame_crop {
            Some([l, r, t, b]) => {
                (w.saturating_sub(crop_x.saturating_mul(l.saturating_add(r))), h.saturating_sub(crop_y.saturating_mul(t.saturating_add(b))))
            }
            None => (w, h),
        }
    }

    /// Frame rate from VUI timing info as (num, den), assuming one frame per two ticks.
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        let t = self.vui.as_ref()?.timing.as_ref()?;
        if t.num_units_in_tick == 0 || t.time_scale == 0 {
            return None;
        }
        let (mut n, mut d) = (t.time_scale as u64, 2 * t.num_units_in_tick as u64);
        let g = gcd(n, d);
        n /= g;
        d /= g;
        Some((n as u32, d as u32))
    }

    /// Human-readable list of the fields that differ between two SPSs.
    pub fn differences(&self, other: &Sps) -> String {
        let mut d = Vec::new();
        let mut cmp = |name: &str, a: String, b: String| {
            if a != b {
                d.push(format!("{name} {a} -> {b}"));
            }
        };
        cmp("profile", self.profile_name().into(), other.profile_name().into());
        cmp("level_idc", self.level_idc.to_string(), other.level_idc.to_string());
        cmp("chroma_format_idc", self.chroma_format_idc.to_string(), other.chroma_format_idc.to_string());
        cmp("bit_depth", (self.bit_depth_luma_minus8 + 8).to_string(), (other.bit_depth_luma_minus8 + 8).to_string());
        let (s0, s1) = (self.size(), other.size());
        cmp("size", format!("{}x{}", s0.0, s0.1), format!("{}x{}", s1.0, s1.1));
        cmp("max_num_ref_frames", self.max_num_ref_frames.to_string(), other.max_num_ref_frames.to_string());
        cmp("log2_max_frame_num", self.log2_max_frame_num().to_string(), other.log2_max_frame_num().to_string());
        cmp("pic_order_cnt_type", self.pic_order_cnt_type.to_string(), other.pic_order_cnt_type.to_string());
        cmp("frame_mbs_only", self.frame_mbs_only_flag.to_string(), other.frame_mbs_only_flag.to_string());
        cmp("scaling lists", self.scaling_lists.is_some().to_string(), other.scaling_lists.is_some().to_string());
        cmp("VUI", format!("{:?}", self.vui), format!("{:?}", other.vui));
        if d.is_empty() { "other fields".into() } else { d.join(", ") }
    }

    pub fn profile_name(&self) -> &'static str {
        match self.profile_idc {
            66 if self.constraint_flags & 0x40 != 0 => "Constrained Baseline",
            66 => "Baseline",
            77 => "Main",
            88 => "Extended",
            100 => "High",
            110 => "High 10",
            122 => "High 4:2:2",
            244 => "High 4:4:4 Predictive",
            44 => "CAVLC 4:4:4 Intra",
            _ => "other",
        }
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Vui {
    fn parse(r: &mut BitReader) -> Result<Self> {
        let mut v = Vui::default();
        if r.flag()? {
            let idc = r.u(8)? as u8;
            let (sw, sh) = if idc == 255 { (r.u(16)? as u16, r.u(16)? as u16) } else { (0, 0) };
            v.aspect_ratio = Some((idc, sw, sh));
        }
        if r.flag()? {
            v.overscan_appropriate = Some(r.flag()?);
        }
        if r.flag()? {
            let video_format = r.u(3)? as u8;
            let video_full_range_flag = r.flag()?;
            let colour_description = if r.flag()? { Some((r.u(8)? as u8, r.u(8)? as u8, r.u(8)? as u8)) } else { None };
            v.video_signal_type = Some(VideoSignalType { video_format, video_full_range_flag, colour_description });
        }
        if r.flag()? {
            v.chroma_loc = Some((r.ue()?, r.ue()?));
        }
        if r.flag()? {
            v.timing = Some(Timing { num_units_in_tick: r.u(32)?, time_scale: r.u(32)?, fixed_frame_rate_flag: r.flag()? });
        }
        if r.flag()? {
            v.nal_hrd = Some(Hrd::parse(r)?);
        }
        if r.flag()? {
            v.vcl_hrd = Some(Hrd::parse(r)?);
        }
        if v.nal_hrd.is_some() || v.vcl_hrd.is_some() {
            v.low_delay_hrd_flag = r.flag()?;
        }
        v.pic_struct_present_flag = r.flag()?;
        if r.flag()? {
            v.bitstream_restriction = Some(BitstreamRestriction {
                motion_vectors_over_pic_boundaries_flag: r.flag()?,
                max_bytes_per_pic_denom: r.ue()?,
                max_bits_per_mb_denom: r.ue()?,
                log2_max_mv_length_horizontal: r.ue()?,
                log2_max_mv_length_vertical: r.ue()?,
                max_num_reorder_frames: r.ue()?,
                max_dec_frame_buffering: r.ue()?,
            });
        }
        Ok(v)
    }

    fn write(&self, w: &mut BitWriter) {
        w.flag(self.aspect_ratio.is_some());
        if let Some((idc, sw, sh)) = self.aspect_ratio {
            w.u(8, idc as u32);
            if idc == 255 {
                w.u(16, sw as u32);
                w.u(16, sh as u32);
            }
        }
        w.flag(self.overscan_appropriate.is_some());
        if let Some(o) = self.overscan_appropriate {
            w.flag(o);
        }
        w.flag(self.video_signal_type.is_some());
        if let Some(s) = &self.video_signal_type {
            w.u(3, s.video_format as u32);
            w.flag(s.video_full_range_flag);
            w.flag(s.colour_description.is_some());
            if let Some((p, t, m)) = s.colour_description {
                w.u(8, p as u32);
                w.u(8, t as u32);
                w.u(8, m as u32);
            }
        }
        w.flag(self.chroma_loc.is_some());
        if let Some((t, b)) = self.chroma_loc {
            w.ue(t);
            w.ue(b);
        }
        w.flag(self.timing.is_some());
        if let Some(t) = &self.timing {
            w.u(32, t.num_units_in_tick);
            w.u(32, t.time_scale);
            w.flag(t.fixed_frame_rate_flag);
        }
        w.flag(self.nal_hrd.is_some());
        if let Some(h) = &self.nal_hrd {
            h.write(w);
        }
        w.flag(self.vcl_hrd.is_some());
        if let Some(h) = &self.vcl_hrd {
            h.write(w);
        }
        if self.nal_hrd.is_some() || self.vcl_hrd.is_some() {
            w.flag(self.low_delay_hrd_flag);
        }
        w.flag(self.pic_struct_present_flag);
        w.flag(self.bitstream_restriction.is_some());
        if let Some(b) = &self.bitstream_restriction {
            w.flag(b.motion_vectors_over_pic_boundaries_flag);
            w.ue(b.max_bytes_per_pic_denom);
            w.ue(b.max_bits_per_mb_denom);
            w.ue(b.log2_max_mv_length_horizontal);
            w.ue(b.log2_max_mv_length_vertical);
            w.ue(b.max_num_reorder_frames);
            w.ue(b.max_dec_frame_buffering);
        }
    }
}

impl Hrd {
    fn parse(r: &mut BitReader) -> Result<Self> {
        let cpb_cnt_minus1 = r.ue()?;
        if cpb_cnt_minus1 > 31 {
            bail!("cpb_cnt_minus1 {cpb_cnt_minus1} out of range");
        }
        let bit_rate_scale = r.u(4)? as u8;
        let cpb_size_scale = r.u(4)? as u8;
        let mut cpbs = Vec::new();
        for _ in 0..=cpb_cnt_minus1 {
            cpbs.push((r.ue()?, r.ue()?, r.flag()?));
        }
        Ok(Self {
            bit_rate_scale,
            cpb_size_scale,
            cpbs,
            initial_cpb_removal_delay_length_minus1: r.u(5)? as u8,
            cpb_removal_delay_length_minus1: r.u(5)? as u8,
            dpb_output_delay_length_minus1: r.u(5)? as u8,
            time_offset_length: r.u(5)? as u8,
        })
    }

    fn write(&self, w: &mut BitWriter) {
        w.ue(self.cpbs.len() as u32 - 1);
        w.u(4, self.bit_rate_scale as u32);
        w.u(4, self.cpb_size_scale as u32);
        for &(br, cs, cbr) in &self.cpbs {
            w.ue(br);
            w.ue(cs);
            w.flag(cbr);
        }
        w.u(5, self.initial_cpb_removal_delay_length_minus1 as u32);
        w.u(5, self.cpb_removal_delay_length_minus1 as u32);
        w.u(5, self.dpb_output_delay_length_minus1 as u32);
        w.u(5, self.time_offset_length as u32);
    }
}
