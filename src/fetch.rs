//! Getting range lists from the configured sources, in priority order.

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::net::IpAddr;
use std::time::Duration;

use crate::config::{Config, Format, Source};
use crate::util;
use crate::zones::{Family, Zone};

const MAX_BODY: u64 = 64 * 1024 * 1024;

pub struct Fetcher {
    agent: ureq::Agent,
    /// db-ip covers the whole world in one file: downloaded once per run.
    dbip: HashMap<String, std::result::Result<HashMap<String, Zone>, String>>,
}

impl Fetcher {
    pub fn new() -> Fetcher {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(120)))
            .user_agent(concat!("firegeowall/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Fetcher {
            agent,
            dbip: HashMap::new(),
        }
    }

    /// GET a URL. `Ok(None)` means 404 (the source has no list for this).
    fn get(&self, url: &str) -> Result<Option<Box<dyn Read + Send>>> {
        match self.agent.get(url).call() {
            Ok(resp) => {
                let r = resp.into_body().into_with_config().limit(MAX_BODY).reader();
                Ok(Some(Box::new(r)))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(anyhow!("{url}: {e}")),
        }
    }

    /// Walk the sources by priority and return the first list for `cc` that
    /// passes validation, with the name of the source that delivered it.
    pub fn country(
        &mut self,
        cfg: &Config,
        cc: &str,
        prev: Option<&Zone>,
    ) -> Result<(String, Zone)> {
        let mut why = Vec::new();
        for src in &cfg.sources {
            let res = self
                .load_source(cfg, src, cc)
                .and_then(|z| z.ok_or_else(|| anyhow!("no list for {cc}")))
                .and_then(|z| validate(cfg, z, prev));
            match res {
                Ok(z) => return Ok((src.name.clone(), z)),
                Err(e) => why.push(format!("{}: {e:#}", src.name)),
            }
        }
        bail!(
            "no source delivered a usable list for {cc} ({})",
            why.join("; ")
        )
    }

    fn load_source(&mut self, cfg: &Config, src: &Source, cc: &str) -> Result<Option<Zone>> {
        let lc = cc.to_ascii_lowercase();
        let mut z = Zone::default();
        match src.format {
            Format::Zone => {
                if let Some(p) = &src.path {
                    let p = p.replace("{cc}", &lc);
                    let f = match std::fs::File::open(&p) {
                        Ok(f) => f,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                        Err(e) => return Err(e).with_context(|| format!("cannot read {p}")),
                    };
                    z.read_into(BufReader::new(f), None, &p)?;
                } else {
                    let u4 = src.ipv4.as_deref().unwrap_or_default().replace("{cc}", &lc);
                    let Some(r) = self.get(&u4)? else {
                        return Ok(None);
                    };
                    z.read_into(BufReader::new(r), Some(Family::V4), &u4)?;
                    if let Some(u6) = &src.ipv6 {
                        let u6 = u6.replace("{cc}", &lc);
                        // 404 for IPv6 is normal: some countries have no IPv6 allocations.
                        if let Some(r) = self.get(&u6)? {
                            z.read_into(BufReader::new(r), Some(Family::V6), &u6)?;
                        }
                    }
                }
            }
            Format::DbipCsv => {
                let url = src.url.clone().unwrap_or_default();
                if !self.dbip.contains_key(&url) {
                    let res = self
                        .load_dbip(&url, cfg.countries())
                        .map_err(|e| format!("{e:#}"));
                    self.dbip.insert(url.clone(), res);
                }
                match &self.dbip[&url] {
                    Ok(m) => match m.get(cc) {
                        Some(found) => z = found.clone(),
                        None => return Ok(None),
                    },
                    Err(e) => bail!("{e}"),
                }
            }
        }
        z.aggregate();
        Ok(Some(z))
    }

    /// Stream the gzipped db-ip CSV and keep only the configured countries.
    fn load_dbip(&self, template: &str, wanted: &[String]) -> Result<HashMap<String, Zone>> {
        // The file for the current month appears on the 1st; fall back one month.
        let (y, m, _) = util::civil((util::now() / 86_400) as i64);
        let prev = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
        for (y, m) in [(y, m), prev] {
            let url = template
                .replace("{yyyy}", &format!("{y:04}"))
                .replace("{mm}", &format!("{m:02}"));
            let Some(r) = self.get(&url)? else { continue };
            let mut out: HashMap<String, Zone> = HashMap::new();
            let rd = BufReader::new(GzDecoder::new(r));
            for (n, line) in rd.lines().enumerate() {
                let line = line.with_context(|| format!("{url}: read error"))?;
                let mut it = line.split(',');
                let (Some(a), Some(b), Some(c)) = (it.next(), it.next(), it.next()) else {
                    bail!("{url}, line {}: expected start,end,country", n + 1);
                };
                let c = c.trim().trim_matches('"');
                if !wanted.iter().any(|w| w == c) {
                    continue;
                }
                let (a, b): (IpAddr, IpAddr) = (
                    a.trim_matches('"')
                        .parse()
                        .with_context(|| format!("{url}, line {}", n + 1))?,
                    b.trim_matches('"')
                        .parse()
                        .with_context(|| format!("{url}, line {}", n + 1))?,
                );
                let z = out.entry(c.to_string()).or_default();
                match (a, b) {
                    (IpAddr::V4(a), IpAddr::V4(b)) => z.v4.extend(ipnet::Ipv4Subnets::new(a, b, 0)),
                    (IpAddr::V6(a), IpAddr::V6(b)) => z.v6.extend(ipnet::Ipv6Subnets::new(a, b, 0)),
                    _ => bail!("{url}, line {}: mixed address families", n + 1),
                }
            }
            return Ok(out);
        }
        bail!("no db-ip file for this or last month")
    }
}

/// The guard rails from the config: minimum size and maximum shrink against
/// the previously cached list.
fn validate(cfg: &Config, z: Zone, prev: Option<&Zone>) -> Result<Zone> {
    for (f, min) in [
        (Family::V4, cfg.min_ranges.ipv4),
        (Family::V6, cfg.min_ranges.ipv6),
    ] {
        let n = z.len(f);
        if n < min {
            bail!(
                "{} list has {n} ranges, fewer than min_ranges ({min})",
                f.name()
            );
        }
        if let Some(p) = prev {
            let old = p.len(f);
            let floor = old * (100 - cfg.max_shrink_percent as usize) / 100;
            if old > 0 && n < floor {
                bail!(
                    "{} list shrank from {old} to {n} ranges (more than {}%); keeping the old one",
                    f.name(),
                    cfg.max_shrink_percent
                );
            }
        }
    }
    Ok(z)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrink_guard() {
        let cfg = Config::parse("countries: [CN]\n").unwrap();
        let mut big = Zone::default();
        for i in 0..100u8 {
            big.push_line(&format!("10.{i}.0.0/16"), None).unwrap();
        }
        let mut small = Zone::default();
        small.push_line("10.0.0.0/16", None).unwrap();
        assert!(validate(&cfg, small.clone(), Some(&big)).is_err());
        assert!(validate(&cfg, big.clone(), Some(&small)).is_ok());
        assert!(validate(&cfg, Zone::default(), None).is_err()); // ipv4 min 1
    }
}
