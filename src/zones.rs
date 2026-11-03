//! Range lists: parsing and validation of zone files, aggregation, and the
//! on-disk cache in `<state_dir>/zones/`.

use anyhow::{Context, Result, bail};
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::V4 => "IPv4",
            Family::V6 => "IPv6",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Zone {
    pub v4: Vec<Ipv4Net>,
    pub v6: Vec<Ipv6Net>,
}

/// Shortest prefix accepted. Nothing a single country owns is bigger; anything
/// shorter is a broken list (or a /0) and would block a large part of the world.
const MIN_PREFIX_V4: u8 = 8;
const MIN_PREFIX_V6: u8 = 12;

impl Zone {
    /// Parse one line. Empty lines and `#` comments are skipped. A bare address
    /// counts as a host route. `only` rejects ranges of the other family.
    pub fn push_line(&mut self, line: &str, only: Option<Family>) -> Result<()> {
        let s = line.split('#').next().unwrap_or("").trim();
        if s.is_empty() {
            return Ok(());
        }
        let net: IpNet = match s.parse::<IpNet>() {
            Ok(n) => n,
            Err(_) => match s.parse::<IpAddr>() {
                Ok(a) => IpNet::from(a),
                Err(_) => bail!("'{s}' is not an IP range"),
            },
        };
        match net.trunc() {
            IpNet::V4(n) => {
                if only == Some(Family::V6) {
                    bail!("IPv4 range {n} in an IPv6 list");
                }
                if n.prefix_len() < MIN_PREFIX_V4 {
                    bail!("range {n} is implausibly large (shorter than /{MIN_PREFIX_V4})");
                }
                self.v4.push(n);
            }
            IpNet::V6(n) => {
                if only == Some(Family::V4) {
                    bail!("IPv6 range {n} in an IPv4 list");
                }
                if n.prefix_len() < MIN_PREFIX_V6 {
                    bail!("range {n} is implausibly large (shorter than /{MIN_PREFIX_V6})");
                }
                self.v6.push(n);
            }
        }
        Ok(())
    }

    /// Read a whole list, line by line. Errors carry the line number.
    pub fn read_into(&mut self, r: impl BufRead, only: Option<Family>, label: &str) -> Result<()> {
        for (n, line) in r.lines().enumerate() {
            let line = line.with_context(|| format!("{label}: read error"))?;
            self.push_line(&line, only)
                .with_context(|| format!("{label}, line {}", n + 1))?;
        }
        Ok(())
    }

    /// Merge overlapping and adjacent ranges. Also sorts.
    pub fn aggregate(&mut self) {
        self.v4 = Ipv4Net::aggregate(&self.v4);
        self.v6 = Ipv6Net::aggregate(&self.v6);
    }

    pub fn len(&self, f: Family) -> usize {
        match f {
            Family::V4 => self.v4.len(),
            Family::V6 => self.v6.len(),
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(a) => self.v4.iter().any(|n| n.contains(&a)),
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(m) => self.v4.iter().any(|n| n.contains(&m)),
                None => self.v6.iter().any(|n| n.contains(&a)),
            },
        }
    }
}

fn zones_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("zones")
}

pub fn cache_paths(state_dir: &Path, cc: &str) -> (PathBuf, PathBuf) {
    let d = zones_dir(state_dir);
    let cc = cc.to_ascii_lowercase();
    (d.join(format!("{cc}.zone")), d.join(format!("{cc}.zone6")))
}

/// The cached zone of a country, or `None` if it has never been fetched.
pub fn read_cache(state_dir: &Path, cc: &str) -> Result<Option<Zone>> {
    let (p4, p6) = cache_paths(state_dir, cc);
    let mut z = Zone::default();
    match fs::File::open(&p4) {
        Ok(f) => z.read_into(
            BufReader::new(f),
            Some(Family::V4),
            &p4.display().to_string(),
        )?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", p4.display())),
    }
    match fs::File::open(&p6) {
        Ok(f) => z.read_into(
            BufReader::new(f),
            Some(Family::V6),
            &p6.display().to_string(),
        )?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", p6.display())),
    }
    Ok(Some(z))
}

pub fn write_cache(state_dir: &Path, cc: &str, z: &Zone) -> Result<()> {
    let dir = zones_dir(state_dir);
    util::ensure_dir(state_dir)?;
    util::ensure_dir(&dir)?;
    let (p4, p6) = cache_paths(state_dir, cc);
    let mut b4 = Vec::with_capacity(z.v4.len() * 18);
    for n in &z.v4 {
        writeln!(b4, "{n}")?;
    }
    let mut b6 = Vec::with_capacity(z.v6.len() * 24);
    for n in &z.v6 {
        writeln!(b6, "{n}")?;
    }
    util::write_atomic(&p4, &b4, 0o644)?;
    util::write_atomic(&p6, &b6, 0o644)?;
    Ok(())
}

pub fn remove_cache(state_dir: &Path, cc: &str) -> Result<()> {
    let (p4, p6) = cache_paths(state_dir, cc);
    for p in [p4, p6] {
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot remove {}", p.display())),
        }
    }
    Ok(())
}

/// Country codes that have a cache file.
pub fn cached_countries(state_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(zones_dir(state_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let cc = n.strip_suffix(".zone")?;
            (cc.len() == 2).then(|| cc.to_ascii_uppercase())
        })
        .collect();
    v.sort();
    v
}

/// Modification time of a country's IPv4 cache file (unix seconds).
pub fn cache_mtime(state_dir: &Path, cc: &str) -> Option<u64> {
    let (p4, _) = cache_paths(state_dir, cc);
    let m = fs::metadata(p4).ok()?.modified().ok()?;
    m.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rejects() {
        let mut z = Zone::default();
        let data = "# comment\n1.2.3.0/24\n\n5.6.7.8\n2001:db8::/32 # x\n";
        z.read_into(data.as_bytes(), None, "t").unwrap();
        assert_eq!(z.v4.len(), 2);
        assert_eq!(z.v6.len(), 1);
        assert_eq!(z.v4[1].to_string(), "5.6.7.8/32");

        let mut z = Zone::default();
        assert!(z.read_into("0.0.0.0/0\n".as_bytes(), None, "t").is_err());
        assert!(
            z.read_into("1.2.3.0/24\n".as_bytes(), Some(Family::V6), "t")
                .is_err()
        );
        assert!(z.read_into("not-an-ip\n".as_bytes(), None, "t").is_err());
    }

    #[test]
    fn host_bits_and_aggregation() {
        let mut z = Zone::default();
        z.read_into(
            "10.0.0.7/24\n10.0.1.0/24\n10.0.0.0/25\n".as_bytes(),
            None,
            "t",
        )
        .unwrap();
        z.aggregate();
        assert_eq!(
            z.v4.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            ["10.0.0.0/23"]
        );
        assert!(z.contains("10.0.1.9".parse().unwrap()));
        assert!(!z.contains("10.0.2.1".parse().unwrap()));
    }
}
