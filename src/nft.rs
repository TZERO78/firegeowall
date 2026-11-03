//! Talking to nftables: building the one-transaction script, running `nft`,
//! and reading the live state back via `nft -j`.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{Config, NftNames};
use crate::zones::{Family, Zone};

const CHUNK: usize = 1000;

fn nft_bin() -> String {
    std::env::var("FGW_NFT").unwrap_or_else(|_| "nft".into())
}

/// Where the distribution's nftables service loads its ruleset from.
/// `FGW_NFT_CONF` overrides it (tests).
pub fn conf_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FGW_NFT_CONF") {
        return Some(p.into());
    }
    [
        "/etc/nftables.conf",           // Debian, Ubuntu, Arch
        "/etc/sysconfig/nftables.conf", // Fedora, RHEL
        "/etc/nftables.nft",            // Alpine
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

/// The file, among `conf` and everything it includes, that defines `chain`.
/// `None` means the chain only lives in the kernel: the next `flush ruleset`
/// of an nftables reload drops it for good.
pub fn chain_persisted(conf: &Path, chain: &str) -> std::io::Result<Option<PathBuf>> {
    let mut todo = vec![conf.to_path_buf()];
    let mut seen = Vec::new();
    while let Some(f) = todo.pop() {
        if seen.contains(&f) || seen.len() > 256 {
            continue;
        }
        let text = std::fs::read_to_string(&f)?;
        seen.push(f.clone());
        for line in text.lines() {
            let l = line.split('#').next().unwrap_or("").trim();
            if let Some(rest) = l.strip_prefix("chain ")
                && rest.split(|c: char| c.is_whitespace() || c == '{').next() == Some(chain)
            {
                return Ok(Some(f));
            }
            if let Some(inc) = l.strip_prefix("include ") {
                let inc = inc.trim().trim_end_matches(';').trim_matches('"');
                let p = Path::new(inc);
                let p = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    f.parent().unwrap_or(Path::new("/etc")).join(p)
                };
                todo.extend(glob(&p));
            }
        }
    }
    Ok(None)
}

/// Expand `*` and `?` in the last path component, sorted like nft does.
fn glob(p: &Path) -> Vec<PathBuf> {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !name.contains(['*', '?']) {
        return vec![p.to_path_buf()];
    }
    let dir = p.parent().unwrap_or(Path::new("/"));
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| !n.starts_with('.') && wildcard(name.as_bytes(), n.as_bytes()))
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

fn wildcard(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => wildcard(&pat[1..], s) || (!s.is_empty() && wildcard(pat, &s[1..])),
        (Some(b'?'), Some(_)) => wildcard(&pat[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => wildcard(&pat[1..], &s[1..]),
        _ => false,
    }
}

/// Run a script with `nft -f -` (or `nft -c -f -` for a dry run).
pub fn run(script: &str, check_only: bool) -> Result<()> {
    let mut cmd = Command::new(nft_bin());
    if check_only {
        cmd.arg("-c");
    }
    let mut child = cmd
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run nft (is nftables installed?)")?;
    child.stdin.take().unwrap().write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first: Vec<&str> = err.lines().take(6).collect();
        bail!(
            "nft {}rejected the change, nothing was applied:\n{}",
            if check_only { "-c " } else { "" },
            first.join("\n")
        );
    }
    Ok(())
}

/// `nft -j <args>`. `Ok(None)` when the object does not exist.
fn json(args: &[&str]) -> Result<Option<Value>> {
    let out = Command::new(nft_bin())
        .arg("-j")
        .args(args)
        .output()
        .context("cannot run nft (is nftables installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("No such file or directory") {
            return Ok(None);
        }
        if err.contains("Operation not permitted") || err.contains("Permission denied") {
            bail!("reading nftables needs CAP_NET_ADMIN (run with sudo)");
        }
        bail!("nft -j {}: {}", args.join(" "), err.trim());
    }
    Ok(Some(
        serde_json::from_slice(&out.stdout).context("nft returned invalid JSON")?,
    ))
}

fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.get("nftables")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

#[derive(Debug, Default, Clone)]
pub struct Hook {
    pub kind: Option<String>,
    pub hook: Option<String>,
    pub prio: Option<i64>,
    pub policy: Option<String>,
    pub jumps_to: Vec<String>,
    /// Interfaces from `iifname != "<x>" return` rules.
    pub iif_only: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub comment: Option<String>,
    pub family: Option<Family>,
    pub set: Option<String>,
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct Live {
    pub table: bool,
    pub hook: Option<Hook>,
    pub owned_chain: bool,
    pub rules: Vec<Rule>,
    /// Sets in the table this tool owns (`geo_xx` / `geo6_xx`).
    pub owned_sets: Vec<String>,
}

impl Live {
    pub fn dropped(&self, cc: &str, f: Family) -> u64 {
        self.rules
            .iter()
            .filter(|r| r.comment.as_deref() == Some(cc) && r.family == Some(f))
            .map(|r| r.packets)
            .sum()
    }
}

/// Everything about our table in one terse (no set elements) call.
pub fn read_live(n: &NftNames) -> Result<Live> {
    let Some(v) = json(&["-t", "list", "table", "inet", n.table_name()])? else {
        return Ok(Live::default());
    };
    let mut live = Live {
        table: true,
        ..Live::default()
    };
    let mut hook_rules = Vec::new();
    for it in items(&v) {
        if let Some(c) = it.get("chain") {
            let name = c["name"].as_str().unwrap_or_default();
            if name == n.chain {
                live.owned_chain = true;
            } else if name == n.hook_chain {
                live.hook = Some(Hook {
                    kind: c["type"].as_str().map(String::from),
                    hook: c["hook"].as_str().map(String::from),
                    prio: c["prio"].as_i64(),
                    policy: c["policy"].as_str().map(String::from),
                    ..Hook::default()
                });
            }
        } else if let Some(s) = it.get("set") {
            let name = s["name"].as_str().unwrap_or_default();
            if n.is_owned_set(name) {
                live.owned_sets.push(name.to_string());
            }
        } else if let Some(r) = it.get("rule") {
            let chain = r["chain"].as_str().unwrap_or_default();
            if chain == n.chain {
                live.rules.push(parse_rule(r));
            } else if chain == n.hook_chain {
                hook_rules.push(r.clone());
            }
        }
    }
    if let Some(h) = live.hook.as_mut() {
        for r in &hook_rules {
            let expr = r["expr"].as_array().cloned().unwrap_or_default();
            for e in &expr {
                if let Some(t) = e.pointer("/jump/target").and_then(Value::as_str) {
                    h.jumps_to.push(t.to_string());
                }
            }
            let is_return = expr.iter().any(|e| e.get("return").is_some());
            for e in &expr {
                let m = &e["match"];
                if is_return
                    && m["op"] == "!="
                    && m.pointer("/left/meta/key").and_then(Value::as_str) == Some("iifname")
                    && let Some(i) = m["right"].as_str()
                {
                    h.iif_only.push(i.to_string());
                }
            }
        }
    }
    Ok(live)
}

fn parse_rule(r: &Value) -> Rule {
    let mut rule = Rule {
        comment: r["comment"].as_str().map(String::from),
        family: None,
        set: None,
        packets: 0,
        bytes: 0,
    };
    for e in r["expr"].as_array().into_iter().flatten() {
        if let Some(m) = e.get("match") {
            rule.family = match m.pointer("/left/payload/protocol").and_then(Value::as_str) {
                Some("ip") => Some(Family::V4),
                Some("ip6") => Some(Family::V6),
                _ => rule.family,
            };
            if let Some(s) = m["right"].as_str().and_then(|s| s.strip_prefix('@')) {
                rule.set = Some(s.to_string());
            }
        }
        if let Some(c) = e.get("counter") {
            rule.packets = c["packets"].as_u64().unwrap_or(0);
            rule.bytes = c["bytes"].as_u64().unwrap_or(0);
        }
    }
    rule
}

/// Number of elements in a set, `None` if the set does not exist.
pub fn set_len(n: &NftNames, set: &str) -> Result<Option<usize>> {
    let Some(v) = json(&["list", "set", "inet", n.table_name(), set])? else {
        return Ok(None);
    };
    Ok(items(&v)
        .find_map(|it| it.get("set"))
        .map(|s| s["elem"].as_array().map_or(0, Vec::len)))
}

/// The whole rebuild as one nftables transaction: create and fill the sets,
/// replace the rules of the owned chain, delete sets no longer configured.
pub fn build_script(n: &NftNames, zones: &[(String, Zone)], existing: &[String]) -> String {
    let t = &n.table;
    let mut s = String::with_capacity(
        zones
            .iter()
            .map(|(_, z)| z.v4.len() * 20 + z.v6.len() * 30)
            .sum::<usize>()
            + 4096,
    );
    let _ = writeln!(s, "# generated by fgw, applied as one transaction");
    let _ = writeln!(s, "add chain {t} {}", n.chain);

    let mut wanted = Vec::new();
    for (cc, z) in zones {
        for (f, set, len) in [
            (Family::V4, n.set_v4(cc), z.v4.len()),
            (Family::V6, n.set_v6(cc), z.v6.len()),
        ] {
            if len == 0 {
                continue; // e.g. no IPv6 allocations: no set, no rule
            }
            let ty = if f == Family::V4 {
                "ipv4_addr"
            } else {
                "ipv6_addr"
            };
            let _ = writeln!(
                s,
                "add set {t} {set} {{ type {ty}; flags interval; auto-merge; }}"
            );
            let _ = writeln!(s, "flush set {t} {set}");
            let elems: Vec<String> = match f {
                Family::V4 => z.v4.iter().map(|x| x.to_string()).collect(),
                Family::V6 => z.v6.iter().map(|x| x.to_string()).collect(),
            };
            for chunk in elems.chunks(CHUNK) {
                let _ = writeln!(s, "add element {t} {set} {{ {} }}", chunk.join(", "));
            }
            wanted.push((cc.clone(), f, set));
        }
    }

    let _ = writeln!(s, "flush chain {t} {}", n.chain);
    // Rule order: config order, IPv4 first, then IPv6.
    for fam in [Family::V4, Family::V6] {
        for (cc, f, set) in wanted.iter().filter(|w| w.1 == fam) {
            let m = if *f == Family::V4 { "ip" } else { "ip6" };
            let _ = writeln!(
                s,
                "add rule {t} {} {m} saddr @{set} counter drop comment \"{cc}\"",
                n.chain
            );
        }
    }
    for old in existing {
        if !wanted.iter().any(|w| &w.2 == old) {
            let _ = writeln!(s, "delete set {t} {old}");
        }
    }
    s
}

/// The ruleset snippet the configuration expects (`fgw check --print-nft`).
pub fn expected_snippet(cfg: &Config, iface: &str) -> String {
    let n = &cfg.nft;
    format!(
        "# firegeowall: hook chain (yours) and the chain fgw owns (its).\n\
         table {t} {{\n    \
             chain {h} {{\n        \
                 type filter hook prerouting priority {p}; policy accept;\n        \
                 iifname != \"{iface}\" return\n        \
                 ct state established,related return\n        \
                 jump {c}\n    \
             }}\n\n    \
             chain {c} {{\n    \
             }}\n\
         }}\n",
        t = n.table,
        h = n.hook_chain,
        p = n.hook_priority,
        c = n.chain,
    )
}

pub fn version() -> Result<String> {
    let out = Command::new(nft_bin())
        .arg("--version")
        .output()
        .map_err(|e| anyhow!("{e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_shape() {
        let n = NftNames::default();
        let mut cn = Zone::default();
        cn.push_line("1.0.1.0/24", None).unwrap();
        cn.push_line("240e::/18", None).unwrap();
        let mut kp = Zone::default();
        kp.push_line("175.45.176.0/22", None).unwrap();
        let s = build_script(
            &n,
            &[("CN".into(), cn), ("KP".into(), kp)],
            &["geo_cn".into(), "geo_ru".into(), "geo6_ru".into()],
        );
        assert!(s.contains("add element inet filter geo_cn { 1.0.1.0/24 }"));
        assert!(s.contains(
            "add rule inet filter geo_block ip saddr @geo_cn counter drop comment \"CN\""
        ));
        assert!(s.contains(
            "add rule inet filter geo_block ip6 saddr @geo6_cn counter drop comment \"CN\""
        ));
        assert!(!s.contains("geo6_kp"));
        assert!(s.contains("delete set inet filter geo_ru\n"));
        assert!(s.contains("delete set inet filter geo6_ru\n"));
        assert!(!s.contains("delete set inet filter geo_cn"));
        // the chain is flushed after the sets are filled and before rules are added
        let f = s.find("flush chain").unwrap();
        assert!(s.find("add element").unwrap() < f && f < s.find("add rule").unwrap());
        // deletions come last, after the rules that used them are gone
        assert!(s.rfind("add rule").unwrap() < s.find("delete set").unwrap());
    }

    #[test]
    fn finds_chain_through_includes() {
        let d = std::env::temp_dir().join(format!("fgw-conf-{}", std::process::id()));
        std::fs::create_dir_all(d.join("nftables.d")).unwrap();
        let conf = d.join("nftables.conf");
        let snippet = d.join("nftables.d/50-firegeowall.conf");
        std::fs::write(
            &snippet,
            "table inet filter {\n  chain geo_prerouting {\n  }\n}\n",
        )
        .unwrap();
        std::fs::write(d.join("nftables.d/README"), "chain geo_prerouting\n").unwrap();

        // stock Debian file: no include, the chain is not persisted
        std::fs::write(
            &conf,
            "flush ruleset\ntable inet filter {\n  chain input {\n  }\n}\n",
        )
        .unwrap();
        assert_eq!(chain_persisted(&conf, "geo_prerouting").unwrap(), None);

        std::fs::write(
            &conf,
            "flush ruleset\n# include \"x\"\ninclude \"nftables.d/*.conf\"\n",
        )
        .unwrap();
        assert_eq!(
            chain_persisted(&conf, "geo_prerouting").unwrap(),
            Some(snippet)
        );
        assert_eq!(chain_persisted(&conf, "geo_pre").unwrap(), None);
        assert!(chain_persisted(&d.join("missing"), "x").is_err());
        std::fs::remove_dir_all(&d).unwrap();

        assert!(wildcard(b"*.conf", b"50-firegeowall.conf"));
        assert!(!wildcard(b"*.conf", b"README"));
    }
}
