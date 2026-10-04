//! Syntax pieces shared by SPS/PPS: scaling lists and RBSP trailing data.

use crate::bits::{BitReader, BitWriter};
use anyhow::Result;

/// scaling_list() (7.3.2.1.1.1). The raw delta_scale values are kept so that
/// re-serialization is bit exact (the same list can be coded in several ways).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScalingList {
    pub deltas: Vec<i32>,
}

impl ScalingList {
    pub fn parse(r: &mut BitReader, size: usize) -> Result<Self> {
        let mut deltas = Vec::new();
        let mut last = 8i32;
        let mut next = 8i32;
        for _ in 0..size {
            if next != 0 {
                let d = r.se()?;
                deltas.push(d);
                next = (last + d + 256).rem_euclid(256);
            }
            if next != 0 {
                last = next;
            }
        }
        Ok(Self { deltas })
    }

    pub fn write(&self, w: &mut BitWriter) {
        for &d in &self.deltas {
            w.se(d);
        }
    }
}

/// Optional list of scaling lists, one presence flag per entry.
pub fn parse_scaling_lists(r: &mut BitReader, count: usize) -> Result<Vec<Option<ScalingList>>> {
    (0..count).map(|i| if r.flag()? { Ok(Some(ScalingList::parse(r, if i < 6 { 16 } else { 64 })?)) } else { Ok(None) }).collect()
}

pub fn write_scaling_lists(w: &mut BitWriter, lists: &[Option<ScalingList>]) {
    for l in lists {
        w.flag(l.is_some());
        if let Some(l) = l {
            l.write(w);
        }
    }
}

/// Whatever follows the parsed syntax in an SPS/PPS. Normally this is exactly
/// rbsp_trailing_bits(), which is regenerated on write; anything else (extensions we
/// do not parse) is kept verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RbspTail {
    Trailing,
    Raw { bytes: Vec<u8>, bits: usize },
}

impl RbspTail {
    pub fn read(r: &BitReader) -> Self {
        let data = r.data();
        let pos = r.pos();
        let total = data.len() * 8;
        let canonical = pos < total && {
            let mut rr = BitReader::at(data, pos);
            rr.flag().unwrap_or(false) && (pos + 1..total).all(|_| !rr.flag().unwrap_or(true)) && (pos + 1).div_ceil(8) * 8 == total
        };
        if canonical {
            RbspTail::Trailing
        } else {
            let mut w = BitWriter::new();
            w.append_bits(data, pos, total);
            RbspTail::Raw { bits: total - pos, bytes: w.into_bytes() }
        }
    }

    pub fn write(&self, w: &mut BitWriter) {
        match self {
            RbspTail::Trailing => w.trailing_bits(),
            RbspTail::Raw { bytes, bits } => w.append_bits(bytes, 0, *bits),
        }
    }
}
