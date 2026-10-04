//! Slice header (7.3.3) parsing/writing and slice data carry-over.
//!
//! The slice data itself is not parsed: on write, the (possibly modified) header is
//! re-serialized and the original slice data bits are appended. With CABAC the data
//! starts byte aligned (cabac_alignment_one_bit), so it is copied byte-wise; with
//! CAVLC it is bit-shifted to follow the new header.

use crate::bits::{BitReader, BitWriter, last_one_bit};
use crate::nal::{NAL_IDR, NalHeader};
use crate::pps::Pps;
use crate::sps::Sps;
use anyhow::{Result, bail};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    SP,
    SI,
}

impl SliceType {
    pub fn from_raw(t: u32) -> Self {
        match t % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::SP,
            _ => SliceType::SI,
        }
    }

    pub fn letter(self) -> char {
        match self {
            SliceType::P => 'P',
            SliceType::B => 'B',
            SliceType::I => 'I',
            SliceType::SP => 'p',
            SliceType::SI => 'i',
        }
    }

    pub fn is_intra(self) -> bool {
        matches!(self, SliceType::I | SliceType::SI)
    }
}

/// ref_pic_list_modification entry: (modification_of_pic_nums_idc, value)
pub type RplmOp = (u32, u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeightEntry {
    pub luma: Option<(i32, i32)>,
    pub chroma: Option<[(i32, i32); 2]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u32,
    pub chroma_log2_weight_denom: u32,
    pub l0: Vec<WeightEntry>,
    pub l1: Vec<WeightEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mmco {
    /// 1: difference_of_pic_nums_minus1
    ForgetShort(u32),
    /// 2: long_term_pic_num
    ForgetLong(u32),
    /// 3: difference_of_pic_nums_minus1, long_term_frame_idx
    ShortToLong(u32, u32),
    /// 4: max_long_term_frame_idx_plus1
    MaxLongTermIdx(u32),
    /// 5
    Reset,
    /// 6: long_term_frame_idx
    CurrentToLong(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecRefPicMarking {
    Idr {
        no_output_of_prior_pics_flag: bool,
        long_term_reference_flag: bool,
    },
    /// None: sliding window; Some(ops): adaptive_ref_pic_marking_mode_flag = 1
    NonIdr(Option<Vec<Mmco>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    /// raw slice_type (0..9)
    pub slice_type_raw: u32,
    pub pps_id: u32,
    pub colour_plane_id: u32,
    pub frame_num: u32,
    pub field_pic_flag: bool,
    pub bottom_field_flag: bool,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred_flag: bool,
    pub num_ref_idx_active_override_flag: bool,
    /// effective values (from the PPS defaults unless overridden)
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub rplm_l0: Option<Vec<RplmOp>>,
    pub rplm_l1: Option<Vec<RplmOp>>,
    pub pred_weight_table: Option<PredWeightTable>,
    pub dec_ref_pic_marking: Option<DecRefPicMarking>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub sp_for_switch_flag: bool,
    pub slice_qs_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    pub slice_group_change_cycle: u32,
}

/// Fields of the slice header that can be read before the PPS is known.
pub fn parse_pps_id(rbsp: &[u8]) -> Result<u32> {
    let mut r = BitReader::new(rbsp);
    r.ue()?;
    r.ue()?;
    r.ue()
}

fn slice_group_change_cycle_bits(sps: &Sps, pps: &Pps) -> u32 {
    let pic_size_in_map_units = sps.pic_width_in_mbs() * (sps.pic_height_in_map_units_minus1 + 1);
    let x = pic_size_in_map_units.div_ceil(pps.slice_group_change_rate()) + 1;
    // Ceil(Log2(x))
    if x <= 1 { 0 } else { 32 - (x - 1).leading_zeros() }
}

impl SliceHeader {
    pub fn slice_type(&self) -> SliceType {
        SliceType::from_raw(self.slice_type_raw)
    }

    /// Parse the header from `r` (positioned just after the NAL header byte).
    pub fn parse(r: &mut BitReader, nal: NalHeader, sps: &Sps, pps: &Pps) -> Result<Self> {
        let idr = nal.nal_unit_type == NAL_IDR;
        let first_mb_in_slice = r.ue()?;
        let slice_type_raw = r.ue()?;
        if slice_type_raw > 9 {
            bail!("slice_type {slice_type_raw} out of range");
        }
        let st = SliceType::from_raw(slice_type_raw);
        let pps_id = r.ue()?;
        let colour_plane_id = if sps.separate_colour_plane_flag { r.u(2)? } else { 0 };
        let frame_num = r.u(sps.log2_max_frame_num())?;
        let (mut field_pic_flag, mut bottom_field_flag) = (false, false);
        if !sps.frame_mbs_only_flag {
            field_pic_flag = r.flag()?;
            if field_pic_flag {
                bottom_field_flag = r.flag()?;
            }
        }
        let idr_pic_id = if idr { r.ue()? } else { 0 };
        let (mut pic_order_cnt_lsb, mut delta_pic_order_cnt_bottom, mut delta_pic_order_cnt) = (0, 0, [0, 0]);
        if sps.pic_order_cnt_type == 0 {
            pic_order_cnt_lsb = r.u(sps.log2_max_poc_lsb())?;
            if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
                delta_pic_order_cnt_bottom = r.se()?;
            }
        }
        if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
            delta_pic_order_cnt[0] = r.se()?;
            if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
                delta_pic_order_cnt[1] = r.se()?;
            }
        }
        let redundant_pic_cnt = if pps.redundant_pic_cnt_present_flag { r.ue()? } else { 0 };
        let direct_spatial_mv_pred_flag = if st == SliceType::B { r.flag()? } else { false };
        let mut num_ref_idx_active_override_flag = false;
        let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        let mut num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        if matches!(st, SliceType::P | SliceType::SP | SliceType::B) {
            num_ref_idx_active_override_flag = r.flag()?;
            if num_ref_idx_active_override_flag {
                num_ref_idx_l0_active_minus1 = r.ue()?;
                if st == SliceType::B {
                    num_ref_idx_l1_active_minus1 = r.ue()?;
                }
            }
        }
        if num_ref_idx_l0_active_minus1 > 31 || num_ref_idx_l1_active_minus1 > 31 {
            bail!("num_ref_idx_active_minus1 out of range");
        }
        let mut rplm_l0 = None;
        let mut rplm_l1 = None;
        if !st.is_intra() {
            rplm_l0 = parse_rplm(r)?;
            if st == SliceType::B {
                rplm_l1 = parse_rplm(r)?;
            }
        }
        let pred_weight_table = if (pps.weighted_pred_flag && matches!(st, SliceType::P | SliceType::SP))
            || (pps.weighted_bipred_idc == 1 && st == SliceType::B)
        {
            Some(parse_pred_weight_table(r, sps, st, num_ref_idx_l0_active_minus1, num_ref_idx_l1_active_minus1)?)
        } else {
            None
        };
        let dec_ref_pic_marking = if nal.nal_ref_idc != 0 { Some(parse_dec_ref_pic_marking(r, idr)?) } else { None };
        let cabac_init_idc = if pps.entropy_coding_mode_flag && !st.is_intra() { r.ue()? } else { 0 };
        let slice_qp_delta = r.se()?;
        let (mut sp_for_switch_flag, mut slice_qs_delta) = (false, 0);
        if matches!(st, SliceType::SP | SliceType::SI) {
            if st == SliceType::SP {
                sp_for_switch_flag = r.flag()?;
            }
            slice_qs_delta = r.se()?;
        }
        let (mut disable_deblocking_filter_idc, mut slice_alpha_c0_offset_div2, mut slice_beta_offset_div2) = (0, 0, 0);
        if pps.deblocking_filter_control_present_flag {
            disable_deblocking_filter_idc = r.ue()?;
            if disable_deblocking_filter_idc != 1 {
                slice_alpha_c0_offset_div2 = r.se()?;
                slice_beta_offset_div2 = r.se()?;
            }
        }
        let slice_group_change_cycle = if pps.num_slice_groups_minus1 > 0 && matches!(pps.slice_group_map_type(), Some(3..=5)) {
            r.u(slice_group_change_cycle_bits(sps, pps))?
        } else {
            0
        };
        Ok(Self {
            first_mb_in_slice,
            slice_type_raw,
            pps_id,
            colour_plane_id,
            frame_num,
            field_pic_flag,
            bottom_field_flag,
            idr_pic_id,
            pic_order_cnt_lsb,
            delta_pic_order_cnt_bottom,
            delta_pic_order_cnt,
            redundant_pic_cnt,
            direct_spatial_mv_pred_flag,
            num_ref_idx_active_override_flag,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            rplm_l0,
            rplm_l1,
            pred_weight_table,
            dec_ref_pic_marking,
            cabac_init_idc,
            slice_qp_delta,
            sp_for_switch_flag,
            slice_qs_delta,
            disable_deblocking_filter_idc,
            slice_alpha_c0_offset_div2,
            slice_beta_offset_div2,
            slice_group_change_cycle,
        })
    }

    pub fn write(&self, w: &mut BitWriter, nal: NalHeader, sps: &Sps, pps: &Pps) {
        let idr = nal.nal_unit_type == NAL_IDR;
        let st = self.slice_type();
        w.ue(self.first_mb_in_slice);
        w.ue(self.slice_type_raw);
        w.ue(self.pps_id);
        if sps.separate_colour_plane_flag {
            w.u(2, self.colour_plane_id);
        }
        w.u(sps.log2_max_frame_num(), self.frame_num);
        if !sps.frame_mbs_only_flag {
            w.flag(self.field_pic_flag);
            if self.field_pic_flag {
                w.flag(self.bottom_field_flag);
            }
        }
        if idr {
            w.ue(self.idr_pic_id);
        }
        if sps.pic_order_cnt_type == 0 {
            w.u(sps.log2_max_poc_lsb(), self.pic_order_cnt_lsb);
            if pps.bottom_field_pic_order_in_frame_present_flag && !self.field_pic_flag {
                w.se(self.delta_pic_order_cnt_bottom);
            }
        }
        if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
            w.se(self.delta_pic_order_cnt[0]);
            if pps.bottom_field_pic_order_in_frame_present_flag && !self.field_pic_flag {
                w.se(self.delta_pic_order_cnt[1]);
            }
        }
        if pps.redundant_pic_cnt_present_flag {
            w.ue(self.redundant_pic_cnt);
        }
        if st == SliceType::B {
            w.flag(self.direct_spatial_mv_pred_flag);
        }
        if matches!(st, SliceType::P | SliceType::SP | SliceType::B) {
            w.flag(self.num_ref_idx_active_override_flag);
            if self.num_ref_idx_active_override_flag {
                w.ue(self.num_ref_idx_l0_active_minus1);
                if st == SliceType::B {
                    w.ue(self.num_ref_idx_l1_active_minus1);
                }
            }
        }
        if !st.is_intra() {
            write_rplm(w, &self.rplm_l0);
            if st == SliceType::B {
                write_rplm(w, &self.rplm_l1);
            }
        }
        if let Some(t) = &self.pred_weight_table {
            write_pred_weight_table(w, t, sps, st);
        }
        if let Some(m) = &self.dec_ref_pic_marking {
            write_dec_ref_pic_marking(w, m);
        }
        if pps.entropy_coding_mode_flag && !st.is_intra() {
            w.ue(self.cabac_init_idc);
        }
        w.se(self.slice_qp_delta);
        if matches!(st, SliceType::SP | SliceType::SI) {
            if st == SliceType::SP {
                w.flag(self.sp_for_switch_flag);
            }
            w.se(self.slice_qs_delta);
        }
        if pps.deblocking_filter_control_present_flag {
            w.ue(self.disable_deblocking_filter_idc);
            if self.disable_deblocking_filter_idc != 1 {
                w.se(self.slice_alpha_c0_offset_div2);
                w.se(self.slice_beta_offset_div2);
            }
        }
        if pps.num_slice_groups_minus1 > 0 && matches!(pps.slice_group_map_type(), Some(3..=5)) {
            w.u(slice_group_change_cycle_bits(sps, pps), self.slice_group_change_cycle);
        }
    }

    pub fn has_mmco5(&self) -> bool {
        matches!(&self.dec_ref_pic_marking, Some(DecRefPicMarking::NonIdr(Some(ops))) if ops.contains(&Mmco::Reset))
    }
}

fn parse_rplm(r: &mut BitReader) -> Result<Option<Vec<RplmOp>>> {
    if !r.flag()? {
        return Ok(None);
    }
    let mut ops = Vec::new();
    loop {
        let idc = r.ue()?;
        match idc {
            0..=2 => ops.push((idc, r.ue()?)),
            3 => return Ok(Some(ops)),
            _ => bail!("modification_of_pic_nums_idc {idc} out of range"),
        }
        if ops.len() > 64 {
            bail!("too many ref_pic_list_modification entries");
        }
    }
}

fn write_rplm(w: &mut BitWriter, ops: &Option<Vec<RplmOp>>) {
    w.flag(ops.is_some());
    if let Some(ops) = ops {
        for &(idc, v) in ops {
            w.ue(idc);
            w.ue(v);
        }
        w.ue(3);
    }
}

fn parse_weights(r: &mut BitReader, n: u32, chroma: bool) -> Result<Vec<WeightEntry>> {
    let mut v = Vec::new();
    for _ in 0..=n {
        let luma = if r.flag()? { Some((r.se()?, r.se()?)) } else { None };
        let chroma = if chroma && r.flag()? { Some([(r.se()?, r.se()?), (r.se()?, r.se()?)]) } else { None };
        v.push(WeightEntry { luma, chroma });
    }
    Ok(v)
}

fn parse_pred_weight_table(r: &mut BitReader, sps: &Sps, st: SliceType, l0: u32, l1: u32) -> Result<PredWeightTable> {
    let chroma = sps.chroma_array_type() != 0;
    let luma_log2_weight_denom = r.ue()?;
    let chroma_log2_weight_denom = if chroma { r.ue()? } else { 0 };
    let l0 = parse_weights(r, l0, chroma)?;
    let l1 = if st == SliceType::B { parse_weights(r, l1, chroma)? } else { Vec::new() };
    Ok(PredWeightTable { luma_log2_weight_denom, chroma_log2_weight_denom, l0, l1 })
}

fn write_weights(w: &mut BitWriter, entries: &[WeightEntry], chroma: bool) {
    for e in entries {
        w.flag(e.luma.is_some());
        if let Some((wt, off)) = e.luma {
            w.se(wt);
            w.se(off);
        }
        if chroma {
            w.flag(e.chroma.is_some());
            if let Some(c) = e.chroma {
                for (wt, off) in c {
                    w.se(wt);
                    w.se(off);
                }
            }
        }
    }
}

fn write_pred_weight_table(w: &mut BitWriter, t: &PredWeightTable, sps: &Sps, st: SliceType) {
    let chroma = sps.chroma_array_type() != 0;
    w.ue(t.luma_log2_weight_denom);
    if chroma {
        w.ue(t.chroma_log2_weight_denom);
    }
    write_weights(w, &t.l0, chroma);
    if st == SliceType::B {
        write_weights(w, &t.l1, chroma);
    }
}

fn parse_dec_ref_pic_marking(r: &mut BitReader, idr: bool) -> Result<DecRefPicMarking> {
    if idr {
        return Ok(DecRefPicMarking::Idr { no_output_of_prior_pics_flag: r.flag()?, long_term_reference_flag: r.flag()? });
    }
    if !r.flag()? {
        return Ok(DecRefPicMarking::NonIdr(None));
    }
    let mut ops = Vec::new();
    loop {
        let op = match r.ue()? {
            0 => return Ok(DecRefPicMarking::NonIdr(Some(ops))),
            1 => Mmco::ForgetShort(r.ue()?),
            2 => Mmco::ForgetLong(r.ue()?),
            3 => Mmco::ShortToLong(r.ue()?, r.ue()?),
            4 => Mmco::MaxLongTermIdx(r.ue()?),
            5 => Mmco::Reset,
            6 => Mmco::CurrentToLong(r.ue()?),
            x => bail!("memory_management_control_operation {x} out of range"),
        };
        ops.push(op);
        if ops.len() > 66 {
            bail!("too many memory_management_control_operations");
        }
    }
}

fn write_dec_ref_pic_marking(w: &mut BitWriter, m: &DecRefPicMarking) {
    match m {
        DecRefPicMarking::Idr { no_output_of_prior_pics_flag, long_term_reference_flag } => {
            w.flag(*no_output_of_prior_pics_flag);
            w.flag(*long_term_reference_flag);
        }
        DecRefPicMarking::NonIdr(None) => w.flag(false),
        DecRefPicMarking::NonIdr(Some(ops)) => {
            w.flag(true);
            for op in ops {
                match *op {
                    Mmco::ForgetShort(d) => {
                        w.ue(1);
                        w.ue(d);
                    }
                    Mmco::ForgetLong(n) => {
                        w.ue(2);
                        w.ue(n);
                    }
                    Mmco::ShortToLong(d, i) => {
                        w.ue(3);
                        w.ue(d);
                        w.ue(i);
                    }
                    Mmco::MaxLongTermIdx(m) => {
                        w.ue(4);
                        w.ue(m);
                    }
                    Mmco::Reset => w.ue(5),
                    Mmco::CurrentToLong(i) => {
                        w.ue(6);
                        w.ue(i);
                    }
                }
            }
            w.ue(0);
        }
    }
}

/// A coded slice: parsed header plus the untouched original RBSP, which is shared
/// between duplicates (cloning a Slice is cheap).
#[derive(Clone, Debug)]
pub struct Slice {
    pub header: SliceHeader,
    pub sps: Arc<Sps>,
    pub pps: Arc<Pps>,
    /// original RBSP without the NAL header byte
    rbsp: Arc<Vec<u8>>,
    /// bit position in `rbsp` where the slice header ends
    header_bits: usize,
    /// bit position in `rbsp` where slice_data() starts (after any cabac_alignment_one_bit)
    data_bit: usize,
    /// replacement slice data (CAVLC only): bytes and length in bits, without trailing bits
    new_data: Option<(Arc<Vec<u8>>, usize)>,
}

impl Slice {
    pub fn parse(nal: NalHeader, rbsp: Vec<u8>, sps: Arc<Sps>, pps: Arc<Pps>) -> Result<Self> {
        let mut r = BitReader::new(&rbsp);
        let header = SliceHeader::parse(&mut r, nal, &sps, &pps)?;
        let header_bits = r.pos();
        if pps.entropy_coding_mode_flag {
            while !r.byte_aligned() {
                if !r.flag()? {
                    bail!("cabac_alignment_one_bit is zero");
                }
            }
        }
        let data_bit = r.pos();
        if data_bit >= rbsp.len() * 8 {
            bail!("slice has no data");
        }
        Ok(Self { header, sps, pps, rbsp: Arc::new(rbsp), header_bits, data_bit, new_data: None })
    }

    /// Length of the parsed slice header in bits (without NAL header byte).
    pub fn header_bits(&self) -> usize {
        self.header_bits
    }

    pub fn slice_data_bits(&self) -> usize {
        match &self.new_data {
            Some((_, bits)) => *bits,
            None => self.rbsp.len() * 8 - self.data_bit,
        }
    }

    /// The slice data as (buffer, start bit, end bit). For CAVLC the end is the
    /// rbsp_stop_one_bit; for CABAC it is the end of the RBSP.
    pub fn slice_data(&self) -> (&[u8], usize, usize) {
        if let Some((bytes, bits)) = &self.new_data {
            return (bytes, 0, *bits);
        }
        let end = if self.pps.entropy_coding_mode_flag {
            self.rbsp.len() * 8
        } else {
            last_one_bit(&self.rbsp).unwrap_or(self.data_bit).max(self.data_bit)
        };
        (&self.rbsp, self.data_bit, end)
    }

    /// Replace the slice data of a CAVLC slice (`bits` long, without trailing bits).
    pub fn set_slice_data(&mut self, bytes: Vec<u8>, bits: usize) -> Result<()> {
        if self.pps.entropy_coding_mode_flag {
            bail!("replacing CABAC slice data is not supported");
        }
        self.new_data = Some((Arc::new(bytes), bits));
        Ok(())
    }

    /// Serialize the RBSP (without NAL header byte) with the current header.
    pub fn write_rbsp(&self, nal: NalHeader) -> Result<Vec<u8>> {
        let mut w = BitWriter::new();
        self.header.write(&mut w, nal, &self.sps, &self.pps);
        if self.pps.entropy_coding_mode_flag {
            while !w.byte_aligned() {
                w.flag(true);
            }
            // slice data, rbsp_stop_one_bit and any cabac_zero_words, all byte aligned
            debug_assert!(self.data_bit.is_multiple_of(8));
            w.append_bits(&self.rbsp, self.data_bit, self.rbsp.len() * 8);
        } else if let Some((bytes, bits)) = &self.new_data {
            w.append_bits(bytes, 0, *bits);
            w.trailing_bits();
        } else {
            let Some(stop) = last_one_bit(&self.rbsp) else { bail!("slice without rbsp_stop_one_bit") };
            if stop < self.data_bit {
                bail!("slice header runs past rbsp_stop_one_bit");
            }
            w.append_bits(&self.rbsp, self.data_bit, stop);
            w.trailing_bits();
        }
        Ok(w.into_bytes())
    }
}
