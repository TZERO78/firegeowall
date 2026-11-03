//! `/etc/firegeowall/config.yml`: parsing, defaults, validation, and the
//! line-based editor for the `countries:` block (so operator comments survive).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::iso;

pub const DEFAULT_PATH: &str = "/etc/firegeowall/config.yml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub interface: Option<String>,
    #[serde(default)]
    countries: Option<Vec<String>>,
    #[serde(default = "default_sources")]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub min_ranges: MinRanges,
    #[serde(default = "default_max_shrink")]
    pub max_shrink_percent: u8,
    #[serde(default)]
    pub nft: NftNames,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub priority: i32,
    #[serde(default)]
    pub ipv4: Option<String>,
    #[serde(default)]
    pub ipv6: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    pub format: Format,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Zone,
    DbipCsv,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MinRanges {
    pub ipv4: usize,
    pub ipv6: usize,
}

impl Default for MinRanges {
    fn default() -> Self {
        // Small countries are small: KP has a single IPv4 range and no IPv6 list.
        // The real protection against truncated downloads is max_shrink_percent.
        MinRanges { ipv4: 1, ipv6: 0 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NftNames {
    pub table: String,
    pub chain: String,
    pub set_prefix_v4: String,
    pub set_prefix_v6: String,
    pub hook_chain: String,
    pub hook_priority: i64,
}

impl Default for NftNames {
    fn default() -> Self {
        NftNames {
            table: "inet filter".into(),
            chain: "geo_block".into(),
            set_prefix_v4: "geo_".into(),
            set_prefix_v6: "geo6_".into(),
            hook_chain: "geo_prerouting".into(),
            hook_priority: -150,
        }
    }
}

impl NftNames {
    /// The table name without the family ("filter" for "inet filter").
    pub fn table_name(&self) -> &str {
        self.table.split_whitespace().nth(1).unwrap_or("")
    }
    pub fn set_v4(&self, cc: &str) -> String {
        format!("{}{}", self.set_prefix_v4, cc.to_ascii_lowercase())
    }
    pub fn set_v6(&self, cc: &str) -> String {
        format!("{}{}", self.set_prefix_v6, cc.to_ascii_lowercase())
    }
    /// True if `name` is a set this tool owns: prefix + two lower-case letters.
    pub fn is_owned_set(&self, name: &str) -> bool {
        let tail = |p: &str| {
            name.strip_prefix(p)
                .is_some_and(|t| t.len() == 2 && t.bytes().all(|b| b.is_ascii_lowercase()))
        };
        tail(&self.set_prefix_v4) || tail(&self.set_prefix_v6)
    }
}

fn default_sources() -> Vec<Source> {
    vec![Source {
        name: "ipdeny".into(),
        priority: 10,
        ipv4: Some("https://www.ipdeny.com/ipblocks/data/aggregated/{cc}-aggregated.zone".into()),
        ipv6: Some(
            "https://www.ipdeny.com/ipv6/ipaddresses/aggregated/{cc}-aggregated.zone".into(),
        ),
        url: None,
        path: None,
        format: Format::Zone,
    }]
}
fn default_max_shrink() -> u8 {
    50
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/firegeowall")
}

fn is_ident(s: &str) -> bool {
    let mut b = s.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn valid_interface(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 15
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._@-".contains(&c))
}

/// Upper-case a country code and check it against ISO 3166-1 alpha-2.
pub fn normalize_cc(s: &str) -> Result<String> {
    let cc = s.trim().to_ascii_uppercase();
    if !iso::is_country(&cc) {
        bail!("'{s}' is not an ISO 3166-1 alpha-2 country code");
    }
    Ok(cc)
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        let mut cfg: Config = if text.trim().is_empty() {
            serde_yaml_ng::from_str("{}")?
        } else {
            serde_yaml_ng::from_str(text)?
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Configured countries, upper-case, validated, in file order.
    pub fn countries(&self) -> &[String] {
        self.countries.as_deref().unwrap_or(&[])
    }

    fn validate(&mut self) -> Result<()> {
        let mut seen = Vec::new();
        for c in self.countries.take().unwrap_or_default() {
            let cc = normalize_cc(&c)?;
            if seen.contains(&cc) {
                bail!("country {cc} is listed twice");
            }
            seen.push(cc);
        }
        self.countries = Some(seen);

        if let Some(i) = &self.interface
            && !valid_interface(i)
        {
            bail!("interface '{i}' is not a valid interface name");
        }
        if self.max_shrink_percent > 100 {
            bail!("max_shrink_percent must be between 0 and 100");
        }

        let n = &self.nft;
        let parts: Vec<&str> = n.table.split_whitespace().collect();
        if parts.len() != 2 || parts[0] != "inet" || !is_ident(parts[1]) {
            bail!("nft.table must be 'inet <name>' (IPv4 and IPv6 share one chain)");
        }
        for (k, v) in [
            ("nft.chain", &n.chain),
            ("nft.set_prefix_v4", &n.set_prefix_v4),
            ("nft.set_prefix_v6", &n.set_prefix_v6),
            ("nft.hook_chain", &n.hook_chain),
        ] {
            if !is_ident(v) {
                bail!("{k} '{v}' is not a valid nftables name");
            }
        }
        if n.set_prefix_v4 == n.set_prefix_v6
            || n.set_prefix_v4.starts_with(&n.set_prefix_v6)
            || n.set_prefix_v6.starts_with(&n.set_prefix_v4)
        {
            bail!("nft.set_prefix_v4 and nft.set_prefix_v6 must not be prefixes of each other");
        }
        if n.chain == n.hook_chain {
            bail!("nft.chain and nft.hook_chain must differ");
        }

        if self.sources.is_empty() {
            bail!("at least one source is needed");
        }
        for s in &self.sources {
            let ctx = format!("source '{}'", s.name);
            if s.name.trim().is_empty() {
                bail!("every source needs a name");
            }
            let https = |u: &Option<String>| u.as_ref().is_none_or(|u| u.starts_with("https://"));
            if !https(&s.ipv4) || !https(&s.ipv6) || !https(&s.url) {
                bail!("{ctx}: URLs must use https://");
            }
            match s.format {
                Format::Zone => match (&s.ipv4, &s.ipv6, &s.url, &s.path) {
                    (Some(v4), v6, None, None) => {
                        if !v4.contains("{cc}") || v6.as_ref().is_some_and(|u| !u.contains("{cc}"))
                        {
                            bail!("{ctx}: ipv4/ipv6 URLs must contain {{cc}}");
                        }
                    }
                    (None, None, None, Some(p)) => {
                        if !p.contains("{cc}") {
                            bail!("{ctx}: path must contain {{cc}}");
                        }
                    }
                    _ => bail!("{ctx}: format zone needs either ipv4 (+ optional ipv6) or path"),
                },
                Format::DbipCsv => {
                    if s.url.is_none() || s.ipv4.is_some() || s.ipv6.is_some() || s.path.is_some() {
                        bail!("{ctx}: format dbip-csv needs exactly one url");
                    }
                }
            }
        }
        self.sources.sort_by_key(|s| s.priority);
        Ok(())
    }
}

/// Rewrite only the `countries:` block of `text` so it lists exactly `new`, in
/// that order. Existing item lines (and their comments) are kept in place, new
/// ones are appended. Refuses rather than guesses on any unusual layout.
pub fn edit_countries(text: &str, new: &[String]) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let header = lines
        .iter()
        .position(|l| l.starts_with("countries:"))
        .ok_or_else(|| anyhow::anyhow!("no top-level 'countries:' line found"))?;

    let head_rest = lines[header]["countries:".len()..].trim();
    let (head_inline_empty, head_comment) = match head_rest.split_once('#') {
        Some((v, c)) => (v.trim(), Some(c)),
        None => (head_rest, None),
    };
    let was_inline = match head_inline_empty {
        "" => false,
        "[]" => true,
        _ => bail!("'countries:' uses an inline list; write it as a block list to use add/remove"),
    };

    // Find the block: item lines, with indented comments / blank lines in between.
    let mut items: Vec<(usize, String)> = Vec::new();
    let mut indent = String::from("  ");
    if !was_inline {
        for (i, l) in lines.iter().enumerate().skip(header + 1) {
            let t = l.trim_start();
            if t.is_empty() || (t.starts_with('#') && l.len() != t.len()) {
                continue;
            }
            if l.len() == t.len() {
                break; // next top-level key or column-0 comment
            }
            let Some(item) = t.strip_prefix('-') else {
                bail!("line {}: unexpected content in the countries block", i + 1);
            };
            let code = item.split('#').next().unwrap_or("").trim();
            if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
                bail!("line {}: '{}' is not a country code", i + 1, code);
            }
            if items.is_empty() {
                indent = l[..l.len() - t.len()].to_string();
            }
            items.push((i, code.to_ascii_uppercase()));
        }
    }

    let keep: Vec<usize> = items
        .iter()
        .filter(|(_, c)| new.contains(c))
        .map(|(i, _)| *i)
        .collect();
    let drop: Vec<usize> = items
        .iter()
        .filter(|(_, c)| !new.contains(c))
        .map(|(i, _)| *i)
        .collect();
    let have: Vec<&String> = items.iter().map(|(_, c)| c).collect();
    let added: Vec<&String> = new.iter().filter(|c| !have.contains(c)).collect();
    let insert_after = keep.last().copied().unwrap_or(header);

    let comment = head_comment.map(|c| format!(" #{c}")).unwrap_or_default();
    let mut out = String::with_capacity(text.len() + 32);
    for (i, l) in lines.iter().enumerate() {
        if i == header {
            if new.is_empty() {
                out.push_str(&format!("countries: []{comment}\n"));
            } else if was_inline {
                out.push_str(&format!("countries:{comment}\n"));
            } else {
                out.push_str(l);
                out.push('\n');
            }
        } else if !drop.contains(&i) {
            out.push_str(l);
            out.push('\n');
        }
        if i == insert_after {
            for c in &added {
                out.push_str(&format!("{indent}- {c}\n"));
            }
        }
    }
    if !text.ends_with('\n') {
        out.pop();
    }

    // Never write something that does not read back as intended.
    let check = Config::parse(&out).context("edited config does not parse")?;
    if check.countries() != new {
        bail!("edited config does not list the intended countries; refusing to write");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: &[&str]) -> Vec<String> {
        x.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_from_minimal_file() {
        let c = Config::parse("countries:\n  - cn\n  - RU\n").unwrap();
        assert_eq!(c.countries(), &v(&["CN", "RU"]));
        assert_eq!(c.nft.set_v4("CN"), "geo_cn");
        assert_eq!(c.nft.set_v6("CN"), "geo6_cn");
        assert_eq!(c.sources[0].name, "ipdeny");
        assert!(c.nft.is_owned_set("geo6_ru"));
        assert!(!c.nft.is_owned_set("geo_prerouting"));
    }

    #[test]
    fn rejects_typos_and_bad_codes() {
        assert!(Config::parse("countries: [CN]\nprioroty: 3\n").is_err());
        assert!(Config::parse("countries: [UK]\n").is_err());
        assert!(Config::parse("countries: [CN, cn]\n").is_err());
        assert!(Config::parse("countries: []\nnft:\n  table: ip filter\n").is_err());
    }

    #[test]
    fn edit_keeps_comments_and_order() {
        let t = "# top\ninterface: ens6\ncountries:\n  - CN # big\n  # note\n  - RU\n\n# tail\nstate_dir: /x\n";
        let out = edit_countries(t, &v(&["CN", "RU", "KP"])).unwrap();
        assert_eq!(
            out,
            "# top\ninterface: ens6\ncountries:\n  - CN # big\n  # note\n  - RU\n  - KP\n\n# tail\nstate_dir: /x\n"
        );
        let out = edit_countries(&out, &v(&["RU", "KP"])).unwrap();
        assert!(!out.contains("CN"));
        assert!(out.contains("# note"));
        let out = edit_countries(&out, &v(&[])).unwrap();
        assert!(out.contains("countries: []\n"));
        let out = edit_countries(&out, &v(&["VN"])).unwrap();
        assert!(out.contains("countries:\n  - VN\n"));
    }

    #[test]
    fn edit_refuses_inline_lists() {
        assert!(edit_countries("countries: [CN]\n", &v(&["RU"])).is_err());
    }
}
