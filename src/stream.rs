//! Whole-stream model: parsed NAL units, access unit grouping and serialization.

use crate::bits::BitWriter;
use crate::nal::{self, NalHeader, RawNal};
use crate::pps::Pps;
use crate::slice::{self, Slice, SliceType};
use crate::sps::Sps;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum NalKind {
    Sps(Arc<Sps>),
    Pps(Arc<Pps>),
    Slice(Box<Slice>),
    /// Anything we pass through untouched (SEI, AUD, unsupported or unparseable units).
    Other,
}

#[derive(Clone, Debug)]
pub struct Nal {
    /// bytes preceding the NAL unit in the byte stream (start code etc.)
    pub prefix: Vec<u8>,
    pub header: NalHeader,
    /// original NAL unit bytes (EBSP, including the header byte)
    pub raw: Arc<Vec<u8>>,
    pub kind: NalKind,
    /// set when the parsed content was modified and must be re-serialized
    pub dirty: bool,
}

impl Nal {
    pub fn nal_type(&self) -> u8 {
        self.header.nal_unit_type
    }

    pub fn slice(&self) -> Option<&Slice> {
        match &self.kind {
            NalKind::Slice(s) => Some(s),
            _ => None,
        }
    }

    pub fn slice_mut(&mut self) -> Option<&mut Slice> {
        match &mut self.kind {
            NalKind::Slice(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_parameter_set(&self) -> bool {
        matches!(self.kind, NalKind::Sps(_) | NalKind::Pps(_))
    }

    /// RBSP (without header byte) regenerated from the parsed syntax, or None for
    /// units we do not parse.
    pub fn reserialize_rbsp(&self) -> Result<Option<Vec<u8>>> {
        Ok(match &self.kind {
            NalKind::Sps(s) => {
                let mut w = BitWriter::new();
                s.write(&mut w);
                Some(w.into_bytes())
            }
            NalKind::Pps(p) => {
                let mut w = BitWriter::new();
                p.write(&mut w);
                Some(w.into_bytes())
            }
            NalKind::Slice(s) => Some(s.write_rbsp(self.header)?),
            NalKind::Other => None,
        })
    }

    /// NAL unit bytes (EBSP incl. header). `force` re-serializes even unmodified units.
    pub fn ebsp(&self, force: bool) -> Result<Cow<'_, [u8]>> {
        if (self.dirty || force)
            && let Some(rbsp) = self.reserialize_rbsp()?
        {
            let mut out = vec![self.header.to_byte()];
            nal::rbsp_to_ebsp(&rbsp, &mut out);
            return Ok(Cow::Owned(out));
        }
        Ok(Cow::Borrowed(&self.raw))
    }

    pub fn write_to(&self, out: &mut Vec<u8>, force: bool) -> Result<()> {
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(&self.ebsp(force)?);
        Ok(())
    }
}

pub struct Stream {
    pub nals: Vec<Nal>,
    /// bytes after the last NAL unit
    pub tail: Vec<u8>,
    pub warnings: Vec<String>,
}

impl Stream {
    /// Parse a byte stream. Parameter sets are tracked in order; slices are then
    /// parsed in parallel.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let (raws, tail) = nal::split_annexb(buf)?;
        let mut sps_map: HashMap<u32, Arc<Sps>> = HashMap::new();
        let mut pps_map: HashMap<u32, Arc<Pps>> = HashMap::new();
        let headers: Vec<NalHeader> = raws.iter().map(|r| NalHeader::parse(r.data.first().copied().unwrap_or(0))).collect();
        let pending: Vec<Pending> = raws
            .iter()
            .zip(&headers)
            .map(|(r, &header)| {
                if r.data.is_empty() || header.has_extended_header() {
                    Pending::Done(NalKind::Other)
                } else {
                    plan_kind(header, &r.data, &mut sps_map, &mut pps_map).unwrap_or_else(|e| Pending::Failed(format!("{e:#}")))
                }
            })
            .collect();
        let parsed: Vec<(NalKind, Option<String>)> = raws
            .par_iter()
            .zip(headers.par_iter())
            .zip(pending.into_par_iter())
            .map(|((r, &header), p)| match p {
                Pending::Done(kind) => (kind, None),
                Pending::Failed(e) => (NalKind::Other, Some(e)),
                Pending::Slice(sps, pps) => match Slice::parse(header, nal::ebsp_to_rbsp(&r.data[1..]), sps, pps) {
                    Ok(s) => (NalKind::Slice(Box::new(s)), None),
                    Err(e) => (NalKind::Other, Some(format!("{e:#}"))),
                },
            })
            .collect();
        let mut warnings = Vec::new();
        let mut nals = Vec::with_capacity(raws.len());
        for (i, ((RawNal { prefix, data }, header), (kind, warning))) in raws.into_iter().zip(headers).zip(parsed).enumerate() {
            if let Some(w) = warning {
                warnings.push(format!("NAL #{i} ({}): {w}", nal::nal_type_name(header.nal_unit_type)));
            }
            nals.push(Nal { prefix, header, raw: Arc::new(data), kind, dirty: false });
        }
        Ok(Self { nals, tail, warnings })
    }

    pub fn write(&self, force: bool) -> Result<Vec<u8>> {
        Ok(self.write_with_offsets(force)?.0)
    }

    /// Like `write`, plus the byte offset at which each NAL unit (with its start code)
    /// begins. Modified units are serialized in parallel.
    pub fn write_with_offsets(&self, force: bool) -> Result<(Vec<u8>, Vec<usize>)> {
        let parts: Vec<Cow<[u8]>> = self.nals.par_iter().map(|n| n.ebsp(force)).collect::<Result<_>>()?;
        let size = self.nals.iter().zip(&parts).map(|(n, p)| n.prefix.len() + p.len()).sum::<usize>() + self.tail.len();
        let mut out = Vec::with_capacity(size);
        let mut offsets = Vec::with_capacity(self.nals.len());
        for (n, p) in self.nals.iter().zip(&parts) {
            offsets.push(out.len());
            out.extend_from_slice(&n.prefix);
            out.extend_from_slice(p);
        }
        out.extend_from_slice(&self.tail);
        Ok((out, offsets))
    }
}

/// What the sequential pass of `Stream::parse` decided about one NAL unit.
enum Pending {
    Done(NalKind),
    Failed(String),
    /// a slice to parse with these parameter sets
    Slice(Arc<Sps>, Arc<Pps>),
}

fn plan_kind(
    header: NalHeader,
    data: &[u8],
    sps_map: &mut HashMap<u32, Arc<Sps>>,
    pps_map: &mut HashMap<u32, Arc<Pps>>,
) -> Result<Pending> {
    Ok(match header.nal_unit_type {
        nal::NAL_SPS => {
            let sps = Arc::new(Sps::parse(&nal::ebsp_to_rbsp(&data[1..]))?);
            sps_map.insert(sps.sps_id, sps.clone());
            Pending::Done(NalKind::Sps(sps))
        }
        nal::NAL_PPS => {
            let rbsp = nal::ebsp_to_rbsp(&data[1..]);
            let (_, sps_id) = Pps::parse_ids(&rbsp)?;
            let pps = Arc::new(Pps::parse(&rbsp, sps_map.get(&sps_id).map(|s| s.as_ref()))?);
            pps_map.insert(pps.pps_id, pps.clone());
            Pending::Done(NalKind::Pps(pps))
        }
        nal::NAL_SLICE | nal::NAL_IDR => {
            // the PPS id sits in the first few bytes of the header
            let pps_id = slice::parse_pps_id(&nal::ebsp_to_rbsp(&data[1..data.len().min(64)]))?;
            let pps = pps_map.get(&pps_id).with_context(|| format!("slice refers to unknown PPS {pps_id}"))?.clone();
            let sps = sps_map.get(&pps.sps_id).with_context(|| format!("PPS {pps_id} refers to unknown SPS {}", pps.sps_id))?.clone();
            Pending::Slice(sps, pps)
        }
        _ => Pending::Done(NalKind::Other),
    })
}

/// One coded picture with its non-VCL units (7.4.1.2.3).
#[derive(Clone, Debug, Default)]
pub struct AccessUnit {
    pub nals: Vec<Nal>,
}

impl AccessUnit {
    pub fn slices(&self) -> impl Iterator<Item = &Slice> {
        self.nals.iter().filter_map(|n| n.slice())
    }

    pub fn first_vcl(&self) -> Option<&Nal> {
        self.nals.iter().find(|n| n.header.is_vcl())
    }

    pub fn first_slice(&self) -> Option<&Slice> {
        self.slices().next()
    }

    pub fn has_vcl(&self) -> bool {
        self.first_vcl().is_some()
    }

    /// True if some VCL unit could not be parsed.
    pub fn has_unparsed_vcl(&self) -> bool {
        self.nals.iter().any(|n| n.header.is_vcl() && n.slice().is_none())
    }

    pub fn is_idr(&self) -> bool {
        self.first_vcl().is_some_and(|n| n.nal_type() == nal::NAL_IDR)
    }

    pub fn nal_ref_idc(&self) -> u8 {
        self.first_vcl().map_or(0, |n| n.header.nal_ref_idc)
    }

    pub fn is_reference(&self) -> bool {
        self.nal_ref_idc() != 0
    }

    /// 'I' if every slice is intra, 'B' if any slice is B, otherwise 'P'; '?' if unknown.
    pub fn picture_type(&self) -> char {
        let types: Vec<SliceType> = self.slices().map(|s| s.header.slice_type()).collect();
        if types.is_empty() {
            '?'
        } else if types.iter().all(|t| t.is_intra()) {
            'I'
        } else if types.contains(&SliceType::B) {
            'B'
        } else {
            'P'
        }
    }

    pub fn is_keyframe(&self) -> bool {
        self.picture_type() == 'I'
    }

    pub fn byte_size(&self) -> usize {
        self.nals.iter().map(|n| n.prefix.len() + n.raw.len()).sum()
    }
}

/// 7.4.1.2.4: does `cur` start a new primary coded picture after `prev`?
fn first_vcl_of_new_picture(prev: &Nal, cur: &Nal) -> bool {
    let (Some(a), Some(b)) = (prev.slice(), cur.slice()) else { return false };
    let (ha, hb) = (&a.header, &b.header);
    if hb.redundant_pic_cnt > 0 {
        return false;
    }
    let (na, nb) = (prev.header, cur.header);
    let idr_a = na.nal_unit_type == nal::NAL_IDR;
    let idr_b = nb.nal_unit_type == nal::NAL_IDR;
    ha.frame_num != hb.frame_num
        || ha.pps_id != hb.pps_id
        || ha.field_pic_flag != hb.field_pic_flag
        || ha.bottom_field_flag != hb.bottom_field_flag
        || ((na.nal_ref_idc == 0) != (nb.nal_ref_idc == 0))
        || (b.sps.pic_order_cnt_type == 0
            && (ha.pic_order_cnt_lsb != hb.pic_order_cnt_lsb || ha.delta_pic_order_cnt_bottom != hb.delta_pic_order_cnt_bottom))
        || (b.sps.pic_order_cnt_type == 1 && ha.delta_pic_order_cnt != hb.delta_pic_order_cnt)
        || idr_a != idr_b
        || (idr_a && idr_b && ha.idr_pic_id != hb.idr_pic_id)
}

pub fn group_access_units(nals: Vec<Nal>) -> Vec<AccessUnit> {
    let mut aus = Vec::new();
    let mut cur = AccessUnit::default();
    let mut last_vcl: Option<Nal> = None;
    for n in nals {
        let starts_new = if n.header.is_vcl() {
            cur.has_vcl() && last_vcl.as_ref().is_some_and(|p| first_vcl_of_new_picture(p, &n))
        } else {
            // AUD, SPS, PPS, SEI and 14..18 after the last VCL unit start the next access unit
            cur.has_vcl() && matches!(n.nal_type(), 6..=9 | 14..=18)
        };
        if starts_new && !cur.nals.is_empty() {
            aus.push(std::mem::take(&mut cur));
        }
        if n.header.is_vcl() {
            last_vcl = Some(n.clone());
        }
        cur.nals.push(n);
    }
    if !cur.nals.is_empty() {
        aus.push(cur);
    }
    aus
}
