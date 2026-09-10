//! Checked-in type profiles for `pyrs profile` / `pyrs compile --profile`.
//!
//! A stale or missing profile is ignored: the compile degrades to today's
//! unprofiled emit rather than trusting the wrong types.

use crate::hash::Sha256;
use std::collections::BTreeMap;

pub const FORMAT: &str = "pyrs-profile 1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub compiler: String,
    pub source: String,
    /// site id → up to four (print_tag, hit count) pairs
    pub sites: BTreeMap<i32, Vec<(i32, u64)>>,
}

pub fn source_hash(sources: &[(String, String)]) -> String {
    let mut h = Sha256::new();
    for (name, source) in sources {
        h.field(name.as_bytes());
        h.field(source.as_bytes());
    }
    h.hex()
}

pub fn parse(text: &str) -> Result<Profile, String> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let header = lines.next().ok_or_else(|| "empty profile".to_string())?;
    if header != FORMAT {
        return Err(format!("unknown profile header {header:?}"));
    }
    let mut compiler = None;
    let mut source = None;
    let mut sites = BTreeMap::new();
    let mut cur: Option<i32> = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix("compiler ") {
            compiler = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("source ") {
            source = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("site ") {
            let id: i32 = rest
                .trim()
                .parse()
                .map_err(|_| format!("bad site id {rest:?}"))?;
            sites.entry(id).or_insert_with(Vec::new);
            cur = Some(id);
            continue;
        }
        let Some(id) = cur else {
            return Err(format!("unexpected profile line {line:?}"));
        };
        let mut bits = line.split_whitespace();
        let tag: i32 = bits
            .next()
            .ok_or_else(|| format!("missing tag on {line:?}"))?
            .parse()
            .map_err(|_| format!("bad tag on {line:?}"))?;
        let count: u64 = bits
            .next()
            .ok_or_else(|| format!("missing count on {line:?}"))?
            .parse()
            .map_err(|_| format!("bad count on {line:?}"))?;
        sites.get_mut(&id).unwrap().push((tag, count));
    }
    Ok(Profile {
        compiler: compiler.ok_or_else(|| "profile is missing compiler".to_string())?,
        source: source.ok_or_else(|| "profile is missing source".to_string())?,
        sites,
    })
}

pub fn matches(profile: &Profile, compiler: &str, source: &str) -> bool {
    profile.compiler == compiler && profile.source == source
}
