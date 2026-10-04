//! Frame selections like "0-10,25,40-" (inclusive, 0-based decode order).

use anyhow::{Context, Result, bail};
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeSet(Vec<(usize, Option<usize>)>);

impl RangeSet {
    pub fn contains(&self, i: usize) -> bool {
        self.0.iter().any(|&(a, b)| i >= a && b.is_none_or(|b| i <= b))
    }
}

impl FromStr for RangeSet {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let num = |t: &str| t.trim().parse::<usize>().with_context(|| format!("bad frame number '{t}' in '{s}'"));
        let mut v = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let r = match part.split_once('-') {
                None => {
                    let n = num(part)?;
                    (n, Some(n))
                }
                Some((a, b)) => {
                    let a = if a.trim().is_empty() { 0 } else { num(a)? };
                    let b = if b.trim().is_empty() { None } else { Some(num(b)?) };
                    if b.is_some_and(|b| b < a) {
                        bail!("empty range '{part}'");
                    }
                    (a, b)
                }
            };
            v.push(r);
        }
        if v.is_empty() {
            bail!("empty frame selection");
        }
        Ok(RangeSet(v))
    }
}

/// "RANGES:COUNT", e.g. "30:40" or "100-110,200:3"
#[derive(Clone, Debug)]
pub struct DupSpec {
    pub frames: RangeSet,
    pub count: usize,
}

impl FromStr for DupSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (r, c) = s.rsplit_once(':').with_context(|| format!("expected FRAMES:COUNT, got '{s}'"))?;
        let count = c.trim().parse().with_context(|| format!("bad repeat count in '{s}'"))?;
        Ok(DupSpec { frames: r.parse()?, count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ranges() {
        let r: RangeSet = "1-3, 7, 10-".parse().unwrap();
        let hits: Vec<usize> = (0..13).filter(|&i| r.contains(i)).collect();
        assert_eq!(hits, vec![1, 2, 3, 7, 10, 11, 12]);
        let r: RangeSet = "-2".parse().unwrap();
        assert!(r.contains(0) && r.contains(2) && !r.contains(3));
        assert!("5-3".parse::<RangeSet>().is_err());
        assert!("x".parse::<RangeSet>().is_err());
        let d: DupSpec = "30-31:4".parse().unwrap();
        assert_eq!(d.count, 4);
        assert!(d.frames.contains(31));
    }
}
