//! fgw: block countries on the public interface with nftables sets.
//! See docs/DESIGN.md for the why and the how.

#[cfg(not(target_os = "linux"))]
compile_error!("firegeowall supports Linux with nftables only");

mod config;
mod fetch;
mod guard;
mod iso;
mod nft;
mod state;
mod util;
mod zones;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use config::Config;
use fetch::Fetcher;
use state::State;
use util::{fmt_num, fmt_time};
use zones::{Family, Zone};

#[derive(Parser)]
#[command(
    name = "fgw",
    version,
    about = "Block countries on the public interface with nftables sets"
)]
struct Cli {
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    /// Configuration file.
    #[arg(long, global = true, value_name = "FILE", default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add countries to the block list and apply immediately.
    Add {
        #[arg(required = true, value_name = "CC")]
        countries: Vec<String>,
        /// Also block the country your SSH session comes from.
        #[arg(long)]
        force: bool,
    },
    /// Remove countries from the block list and apply immediately.
    Remove {
        #[arg(required = true, value_name = "CC")]
        countries: Vec<String>,
    },
    /// Show the configured countries.
    List,
    /// Interface, countries, set sizes, last update, drop counters, timer.
    Status,
    /// Print the cached ranges of one country.
    Show {
        #[arg(value_name = "CC")]
        country: String,
    },
    /// Validate config, hook chain, owned chain, sets and cache.
    Check {
        /// Print the nftables snippet this configuration expects, then exit.
        #[arg(long)]
        print_nft: bool,
    },
    /// Fetch fresh lists, validate them and refill the sets (run by the timer).
    Update,
    /// Fill the sets from the local cache, no network (used by fgw-restore).
    #[command(hide = true)]
    Apply {
        #[arg(long, required = true)]
        from_cache: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let res = match &cli.cmd {
        Cmd::Add { countries, force } => cmd_add(&cli, countries, *force),
        Cmd::Remove { countries } => cmd_remove(&cli, countries),
        Cmd::List => cmd_list(&cli),
        Cmd::Status => cmd_status(&cli),
        Cmd::Show { country } => cmd_show(&cli, country),
        Cmd::Check { print_nft } => cmd_check(&cli, *print_nft),
        Cmd::Update => cmd_update(&cli),
        Cmd::Apply { .. } => cmd_apply(&cli),
    };
    match res {
        Ok(code) => code,
        Err(e) => {
            if cli.json {
                println!("{}", json!({ "ok": false, "error": format!("{e:#}") }));
            } else {
                eprintln!("fgw: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn print_json(v: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

// ---------------------------------------------------------------- core

/// A country's zone from the cache, fetched first if it was never cached.
fn ensure_zone(cfg: &Config, f: &mut Option<Fetcher>, st: &mut State, cc: &str) -> Result<Zone> {
    if let Some(z) = zones::read_cache(&cfg.state_dir, cc)? {
        return Ok(z);
    }
    let (src, z) = f.get_or_insert_with(Fetcher::new).country(cfg, cc, None)?;
    zones::write_cache(&cfg.state_dir, cc, &z)?;
    st.record_fetch(cc, &src, &z);
    Ok(z)
}

/// Rebuild the owned chain and sets for exactly `zones`, in one transaction.
fn rebuild(cfg: &Config, zones: &[(String, Zone)], st: &mut State) -> Result<()> {
    let live = nft::read_live(&cfg.nft)?;
    if !live.table {
        bail!(
            "table {} does not exist. Install contrib/nftables/50-firegeowall.conf \
             (or the output of `fgw check --print-nft`) and reload nftables",
            cfg.nft.table
        );
    }
    let script = nft::build_script(&cfg.nft, zones, &live.owned_sets);
    nft::run(&script, true)?;
    nft::run(&script, false)?;
    st.bank_counters(&live);
    st.last_apply = Some(util::now());
    if live
        .hook
        .as_ref()
        .is_none_or(|h| !h.jumps_to.contains(&cfg.nft.chain))
    {
        eprintln!(
            "fgw: warning: no hook chain '{}' jumps to '{}'. The sets are loaded but filter nothing. Run `fgw check`.",
            cfg.nft.hook_chain, cfg.nft.chain
        );
    }
    Ok(())
}

fn normalize_list(input: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for c in input {
        let cc = config::normalize_cc(c)?;
        if !out.contains(&cc) {
            out.push(cc);
        }
    }
    Ok(out)
}

/// Apply `desired`, then write it to the config. If writing the config fails,
/// put the old list back so file and firewall never disagree.
fn change_list(
    cli: &Cli,
    cfg: &Config,
    desired: &[String],
    st: &mut State,
    f: &mut Option<Fetcher>,
) -> Result<()> {
    let text = std::fs::read_to_string(&cli.config)?;
    let new_text = config::edit_countries(&text, desired)
        .with_context(|| format!("cannot edit {}", cli.config.display()))?;
    let zones: Vec<(String, Zone)> = desired
        .iter()
        .map(|cc| ensure_zone(cfg, f, st, cc).map(|z| (cc.clone(), z)))
        .collect::<Result<_>>()?;
    rebuild(cfg, &zones, st)?;
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&cli.config)?.permissions());
    if let Err(e) = util::write_atomic(&cli.config, new_text.as_bytes(), mode & 0o7777) {
        let old: Vec<(String, Zone)> = cfg
            .countries()
            .iter()
            .filter_map(|cc| {
                zones::read_cache(&cfg.state_dir, cc)
                    .ok()
                    .flatten()
                    .map(|z| (cc.clone(), z))
            })
            .collect();
        let _ = rebuild(cfg, &old, st);
        return Err(e).context("config not written, previous block list restored");
    }
    Ok(())
}

// ---------------------------------------------------------------- add / remove

fn cmd_add(cli: &Cli, input: &[String], force: bool) -> Result<ExitCode> {
    let req = normalize_list(input)?;
    let cfg = Config::load(&cli.config)?;
    let _lock = util::lock(&cfg.state_dir)?;
    let mut st = State::load(&cfg.state_dir);
    let cur = cfg.countries().to_vec();
    let new: Vec<String> = req.iter().filter(|c| !cur.contains(c)).cloned().collect();
    let already: Vec<String> = req.iter().filter(|c| cur.contains(c)).cloned().collect();

    if !new.is_empty() {
        // The config does not list the new countries yet; a world-wide source
        // like db-ip must still keep them.
        let mut f = Some(Fetcher::with_extra(&new));
        // Fetch new countries first: the self-check needs their ranges.
        let mut fresh = Vec::new();
        for cc in &new {
            fresh.push((cc.clone(), ensure_zone(&cfg, &mut f, &mut st, cc)?));
        }
        if let Some(ip) = guard::ssh_client() {
            for (cc, z) in &fresh {
                if z.contains(ip) && !force {
                    st.save(&cfg.state_dir)?;
                    bail!(
                        "refusing to block {cc}: your SSH session comes from {ip}, which is in {cc}. \
                         Add --force if you really mean it"
                    );
                }
            }
        }
        let desired: Vec<String> = cur.iter().chain(new.iter()).cloned().collect();
        change_list(cli, &cfg, &desired, &mut st, &mut f)?;
        st.save(&cfg.state_dir)?;
    }
    report_change(cli, &cfg, &st, "added", &new, &already, "already blocked")
}

fn cmd_remove(cli: &Cli, input: &[String]) -> Result<ExitCode> {
    let req = normalize_list(input)?;
    let cfg = Config::load(&cli.config)?;
    let _lock = util::lock(&cfg.state_dir)?;
    let mut st = State::load(&cfg.state_dir);
    let cur = cfg.countries().to_vec();
    let gone: Vec<String> = req.iter().filter(|c| cur.contains(c)).cloned().collect();
    let absent: Vec<String> = req.iter().filter(|c| !cur.contains(c)).cloned().collect();
    if !gone.is_empty() {
        let desired: Vec<String> = cur.iter().filter(|c| !gone.contains(c)).cloned().collect();
        change_list(cli, &cfg, &desired, &mut st, &mut None)?;
        st.save(&cfg.state_dir)?;
    }
    report_change(cli, &cfg, &st, "removed", &gone, &absent, "not blocked")
}

fn report_change(
    cli: &Cli,
    cfg: &Config,
    st: &State,
    verb: &str,
    changed: &[String],
    skipped: &[String],
    skipped_why: &str,
) -> Result<ExitCode> {
    let now_list = Config::load(&cli.config)?.countries().to_vec();
    if cli.json {
        print_json(
            json!({ "ok": true, verb: changed, skipped_why.replace(' ', "_"): skipped, "countries": now_list }),
        );
        return Ok(ExitCode::SUCCESS);
    }
    for cc in changed {
        match st.countries.get(cc) {
            Some(c) if verb == "added" => println!(
                "Blocked {cc}: {} IPv4 / {} IPv6 ranges ({}).",
                fmt_num(c.v4 as u64),
                fmt_num(c.v6 as u64),
                c.source
            ),
            _ if verb == "added" => println!("Blocked {cc}."),
            _ => println!("Unblocked {cc}."),
        }
    }
    if !skipped.is_empty() {
        println!("{}: {}", capitalize(skipped_why), skipped.join(" "));
    }
    if now_list.is_empty() {
        println!("No countries are blocked.");
    } else {
        println!(
            "Now blocking {} {}: {}",
            now_list.len(),
            plural(now_list.len()),
            now_list.join(" ")
        );
    }
    let _ = cfg;
    Ok(ExitCode::SUCCESS)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "country" } else { "countries" }
}

// ---------------------------------------------------------------- update / apply

fn cmd_update(cli: &Cli) -> Result<ExitCode> {
    let cfg = Config::load(&cli.config)?;
    let _lock = util::lock(&cfg.state_dir)?;
    let mut st = State::load(&cfg.state_dir);
    let mut f = Fetcher::new();
    let mut got = Vec::new();
    let mut failed = Vec::new();
    for cc in cfg.countries() {
        let prev = zones::read_cache(&cfg.state_dir, cc).ok().flatten();
        match f.country(&cfg, cc, prev.as_ref()) {
            Ok((src, z)) => got.push((cc.clone(), src, z)),
            Err(e) => failed.push(format!("{e:#}")),
        }
    }
    if !failed.is_empty() {
        st.degraded = Some(format!(
            "update {}: {}",
            fmt_time(util::now()),
            failed.join("; ")
        ));
        st.save(&cfg.state_dir)?;
        bail!(
            "update aborted, live sets unchanged:\n  {}",
            failed.join("\n  ")
        );
    }

    let zones: Vec<(String, Zone)> = got
        .iter()
        .map(|(cc, _, z)| (cc.clone(), z.clone()))
        .collect();
    rebuild(&cfg, &zones, &mut st)?;
    for (cc, src, z) in &got {
        zones::write_cache(&cfg.state_dir, cc, z)?;
        st.record_fetch(cc, src, z);
    }
    for cc in zones::cached_countries(&cfg.state_dir) {
        if !cfg.countries().contains(&cc) {
            zones::remove_cache(&cfg.state_dir, &cc)?;
        }
    }
    st.countries.retain(|cc, _| cfg.countries().contains(cc));
    st.last_update = Some(util::now());
    st.degraded = None;
    st.save(&cfg.state_dir)?;

    if cli.json {
        let c: Vec<_> = got
            .iter()
            .map(|(cc, src, z)| json!({ "country": cc, "source": src, "ipv4": z.v4.len(), "ipv6": z.v6.len() }))
            .collect();
        print_json(json!({ "ok": true, "countries": c }));
    } else {
        let (v4, v6): (usize, usize) = got
            .iter()
            .fold((0, 0), |a, (_, _, z)| (a.0 + z.v4.len(), a.1 + z.v6.len()));
        println!(
            "Updated {} {}: {} IPv4 / {} IPv6 ranges.",
            got.len(),
            plural(got.len()),
            fmt_num(v4 as u64),
            fmt_num(v6 as u64)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_apply(cli: &Cli) -> Result<ExitCode> {
    let cfg = Config::load(&cli.config)?;
    let _lock = util::lock(&cfg.state_dir)?;
    let mut st = State::load(&cfg.state_dir);
    let mut zones = Vec::new();
    let mut missing = Vec::new();
    for cc in cfg.countries() {
        match zones::read_cache(&cfg.state_dir, cc)? {
            Some(z) => zones.push((cc.clone(), z)),
            None => missing.push(cc.clone()),
        }
    }
    rebuild(&cfg, &zones, &mut st)?;
    st.save(&cfg.state_dir)?;
    if !missing.is_empty() {
        bail!(
            "restored {} {} from cache; no cache for {} (run `fgw update`)",
            zones.len(),
            plural(zones.len()),
            missing.join(" ")
        );
    }
    if !cli.json {
        println!(
            "Restored {} {} from cache.",
            zones.len(),
            plural(zones.len())
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- list / show

fn counts(cfg: &Config, st: &State, cc: &str) -> Option<(usize, usize, String, u64)> {
    if let Some(c) = st.countries.get(cc) {
        return Some((c.v4, c.v6, c.source.clone(), c.fetched));
    }
    let z = zones::read_cache(&cfg.state_dir, cc).ok().flatten()?;
    Some((
        z.v4.len(),
        z.v6.len(),
        "cache".into(),
        zones::cache_mtime(&cfg.state_dir, cc).unwrap_or(0),
    ))
}

fn cmd_list(cli: &Cli) -> Result<ExitCode> {
    let cfg = Config::load(&cli.config)?;
    let st = State::load(&cfg.state_dir);
    if cli.json {
        let v: Vec<_> = cfg
            .countries()
            .iter()
            .map(|cc| match counts(&cfg, &st, cc) {
                Some((a, b, s, t)) => {
                    json!({ "country": cc, "ipv4": a, "ipv6": b, "source": s, "fetched": t })
                }
                None => json!({ "country": cc, "cached": false }),
            })
            .collect();
        print_json(json!({ "countries": v }));
        return Ok(ExitCode::SUCCESS);
    }
    if cfg.countries().is_empty() {
        println!("No countries configured: nothing is blocked.");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<8}{:>12}{:>12}  {:<8}  Fetched",
        "Country", "IPv4", "IPv6", "Source"
    );
    for cc in cfg.countries() {
        match counts(&cfg, &st, cc) {
            Some((a, b, s, t)) => println!(
                "{cc:<8}{:>12}{:>12}  {s:<8}  {}",
                fmt_num(a as u64),
                fmt_num(b as u64),
                fmt_time(t)
            ),
            None => println!(
                "{cc:<8}{:>12}{:>12}  {:<8}  never (run fgw update)",
                "-", "-", "-"
            ),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_show(cli: &Cli, cc: &str) -> Result<ExitCode> {
    let cc = config::normalize_cc(cc)?;
    let cfg = Config::load(&cli.config)?;
    let Some(z) = zones::read_cache(&cfg.state_dir, &cc)? else {
        bail!("{cc} is not in the cache (only configured countries are; run `fgw update`)");
    };
    if cli.json {
        let v4: Vec<String> = z.v4.iter().map(|n| n.to_string()).collect();
        let v6: Vec<String> = z.v6.iter().map(|n| n.to_string()).collect();
        print_json(json!({ "country": cc, "ipv4": v4, "ipv6": v6 }));
    } else {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for n in &z.v4 {
            let _ = writeln!(out, "{n}");
        }
        for n in &z.v6 {
            let _ = writeln!(out, "{n}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- check / status

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Check {
    level: Level,
    what: String,
    msg: String,
}

fn ck(v: &mut Vec<Check>, level: Level, what: &str, msg: impl Into<String>) {
    v.push(Check {
        level,
        what: what.into(),
        msg: msg.into(),
    });
}

/// Interface of the IPv4 default route, from /proc/net/route.
fn default_iface() -> Option<String> {
    let s = std::fs::read_to_string("/proc/net/route").ok()?;
    s.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 2 && f[1] == "00000000").then(|| f[0].to_string())
    })
}

fn iface(cfg: &Config) -> String {
    cfg.interface
        .clone()
        .or_else(default_iface)
        .unwrap_or_else(|| "eth0".into())
}

const MAX_AGE: u64 = 14 * 86_400;

fn run_checks(path: &Path) -> (Option<Config>, Option<nft::Live>, Vec<Check>) {
    let mut v = Vec::new();
    let cfg = match Config::load(path) {
        Ok(c) => {
            let n = c.countries().len();
            ck(
                &mut v,
                Level::Ok,
                "config",
                format!("{}, {n} {}", path.display(), plural(n)),
            );
            c
        }
        Err(e) => {
            ck(&mut v, Level::Fail, "config", format!("{e:#}"));
            return (None, None, v);
        }
    };
    match nft::version() {
        Ok(s) => ck(&mut v, Level::Ok, "nft", s),
        Err(e) => {
            ck(&mut v, Level::Fail, "nft", format!("cannot run nft: {e}"));
            return (Some(cfg), None, v);
        }
    }
    let st = State::load(&cfg.state_dir);
    let n = &cfg.nft;
    let ifc = iface(&cfg);

    let live = match nft::read_live(n) {
        Ok(l) => Some(l),
        Err(e) => {
            ck(
                &mut v,
                Level::Warn,
                "live",
                format!("{e:#}; live checks skipped"),
            );
            None
        }
    };
    if let Some(l) = &live {
        if !l.table {
            ck(
                &mut v,
                Level::Fail,
                "table",
                format!("table {} does not exist", n.table),
            );
        } else {
            match &l.hook {
                None => ck(
                    &mut v,
                    Level::Fail,
                    "hook chain",
                    format!("chain {} missing (see fgw check --print-nft)", n.hook_chain),
                ),
                Some(h) => {
                    if h.kind.as_deref() != Some("filter")
                        || h.hook.as_deref() != Some("prerouting")
                    {
                        ck(
                            &mut v,
                            Level::Fail,
                            "hook chain",
                            format!(
                                "{} is not a 'type filter hook prerouting' chain",
                                n.hook_chain
                            ),
                        );
                    } else if h.policy.as_deref() != Some("accept") {
                        ck(
                            &mut v,
                            Level::Fail,
                            "hook chain",
                            format!(
                                "{} has policy {}; it must be accept",
                                n.hook_chain,
                                h.policy.as_deref().unwrap_or("?")
                            ),
                        );
                    } else if !h.jumps_to.contains(&n.chain) {
                        ck(
                            &mut v,
                            Level::Fail,
                            "hook chain",
                            format!("{} does not jump to {}", n.hook_chain, n.chain),
                        );
                    } else {
                        ck(
                            &mut v,
                            Level::Ok,
                            "hook chain",
                            format!("{} jumps to {}", n.hook_chain, n.chain),
                        );
                    }
                    if h.prio != Some(n.hook_priority) {
                        ck(
                            &mut v,
                            Level::Warn,
                            "priority",
                            format!(
                                "{} has priority {:?}, config expects {}",
                                n.hook_chain, h.prio, n.hook_priority
                            ),
                        );
                    }
                    if h.iif_only.is_empty() {
                        ck(
                            &mut v,
                            Level::Warn,
                            "interface",
                            format!(
                                "no 'iifname != \"{ifc}\" return' rule: every interface is filtered, mesh and loopback included"
                            ),
                        );
                    } else if !h.iif_only.contains(&ifc) {
                        ck(
                            &mut v,
                            Level::Warn,
                            "interface",
                            format!(
                                "hook chain filters {} but the public interface is {ifc}",
                                h.iif_only.join(",")
                            ),
                        );
                    } else {
                        ck(&mut v, Level::Ok, "interface", ifc.clone());
                    }
                }
            }
            if !l.owned_chain {
                ck(
                    &mut v,
                    Level::Fail,
                    "owned chain",
                    format!("chain {} missing", n.chain),
                );
            }
        }
    }

    let mut expected_sets = Vec::new();
    for cc in cfg.countries() {
        let z = match zones::read_cache(&cfg.state_dir, cc) {
            Ok(Some(z)) => z,
            Ok(None) => {
                ck(&mut v, Level::Fail, cc, "no cached list (run fgw update)");
                continue;
            }
            Err(e) => {
                ck(&mut v, Level::Fail, cc, format!("cache unreadable: {e:#}"));
                continue;
            }
        };
        if zones::cache_mtime(&cfg.state_dir, cc)
            .is_some_and(|t| util::now().saturating_sub(t) > MAX_AGE)
        {
            ck(&mut v, Level::Warn, cc, "cached list is older than 14 days");
        }
        let mut problems = Vec::new();
        for (f, set) in [(Family::V4, n.set_v4(cc)), (Family::V6, n.set_v6(cc))] {
            if z.len(f) == 0 {
                continue;
            }
            expected_sets.push(set.clone());
            let Some(l) = live.as_ref().filter(|l| l.table) else {
                continue;
            };
            if !l
                .rules
                .iter()
                .any(|r| r.set.as_deref() == Some(set.as_str()))
            {
                problems.push(format!("no rule for {set}"));
            }
            match nft::set_len(n, &set) {
                Ok(Some(0)) => problems.push(format!("{set} is empty")),
                Ok(Some(_)) => {}
                Ok(None) => problems.push(format!("{set} missing")),
                Err(e) => problems.push(format!("{set}: {e:#}")),
            }
        }
        if problems.is_empty() {
            ck(
                &mut v,
                Level::Ok,
                cc,
                format!(
                    "{} IPv4 / {} IPv6 ranges",
                    fmt_num(z.v4.len() as u64),
                    fmt_num(z.v6.len() as u64)
                ),
            );
        } else {
            ck(
                &mut v,
                Level::Fail,
                cc,
                format!(
                    "{} (nftables reloaded? run: systemctl restart fgw-restore)",
                    problems.join(", ")
                ),
            );
        }
    }
    if let Some(l) = &live {
        for r in &l.rules {
            if r.set.as_ref().is_none_or(|s| !expected_sets.contains(s)) {
                ck(
                    &mut v,
                    Level::Warn,
                    "owned chain",
                    format!(
                        "unexpected rule {:?}; the next fgw run removes it",
                        r.comment.as_deref().unwrap_or("?")
                    ),
                );
            }
        }
    }
    if let Some(d) = &st.degraded {
        ck(&mut v, Level::Warn, "update", d.clone());
    }
    (Some(cfg), live, v)
}

fn cmd_check(cli: &Cli, print_nft: bool) -> Result<ExitCode> {
    if print_nft {
        let cfg = Config::load(&cli.config)?;
        print!("{}", nft::expected_snippet(&cfg, &iface(&cfg)));
        return Ok(ExitCode::SUCCESS);
    }
    let (_, _, checks) = run_checks(&cli.config);
    let worst = checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok);
    if cli.json {
        let v: Vec<_> = checks
            .iter()
            .map(|c| json!({ "level": level_name(c.level), "check": c.what, "message": c.msg }))
            .collect();
        print_json(json!({ "ok": worst != Level::Fail, "checks": v }));
    } else {
        for c in &checks {
            println!("{:<5} {:<12} {}", level_name(c.level), c.what, c.msg);
        }
    }
    Ok(if worst == Level::Fail {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn level_name(l: Level) -> &'static str {
    match l {
        Level::Ok => "OK",
        Level::Warn => "WARN",
        Level::Fail => "FAIL",
    }
}

/// (ActiveState, next run) of fgw-update.timer, if systemd knows it.
fn timer() -> Option<(String, String)> {
    let out = Command::new("systemctl")
        .args([
            "show",
            "fgw-update.timer",
            "-p",
            "LoadState",
            "-p",
            "ActiveState",
            "-p",
            "NextElapseUSecRealtime",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(k))
            .unwrap_or("")
            .to_string()
    };
    if get("LoadState=") != "loaded" {
        return None;
    }
    Some((get("ActiveState="), get("NextElapseUSecRealtime=")))
}

fn cmd_status(cli: &Cli) -> Result<ExitCode> {
    let (cfg, live, checks) = run_checks(&cli.config);
    let Some(cfg) = cfg else {
        bail!(
            "{}",
            checks
                .first()
                .map(|c| c.msg.as_str())
                .unwrap_or("invalid config")
        );
    };
    let st = State::load(&cfg.state_dir);
    let worst = checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok);
    let problems: Vec<&Check> = checks.iter().filter(|c| c.level != Level::Ok).collect();
    let health = match worst {
        Level::Ok => "OK",
        Level::Warn => "WARN",
        Level::Fail => "DEGRADED",
    };
    let tm = timer();

    let rows: Vec<(String, usize, usize, u64, u64)> = cfg
        .countries()
        .iter()
        .map(|cc| {
            let (a, b, _, _) = counts(&cfg, &st, cc).unwrap_or((0, 0, String::new(), 0));
            let banked = st.countries.get(cc).cloned().unwrap_or_default();
            let (d4, d6) = live
                .as_ref()
                .map(|l| (l.dropped(cc, Family::V4), l.dropped(cc, Family::V6)))
                .unwrap_or((0, 0));
            (
                cc.clone(),
                a,
                b,
                banked.dropped_v4 + d4,
                banked.dropped_v6 + d6,
            )
        })
        .collect();

    if cli.json {
        let c: Vec<_> = rows
            .iter()
            .map(|(cc, a, b, d4, d6)| json!({ "country": cc, "ipv4": a, "ipv6": b, "dropped_v4": d4, "dropped_v6": d6 }))
            .collect();
        let p: Vec<_> = problems
            .iter()
            .map(|c| json!({ "level": level_name(c.level), "check": c.what, "message": c.msg }))
            .collect();
        print_json(json!({
            "health": health,
            "interface": iface(&cfg),
            "hook_chain": cfg.nft.hook_chain,
            "hook_priority": cfg.nft.hook_priority,
            "last_update": st.last_update,
            "last_apply": st.last_apply,
            "live_counters": live.is_some(),
            "timer": tm.as_ref().map(|(a, n)| json!({ "state": a, "next": n })),
            "countries": c,
            "problems": p,
        }));
        return Ok(ExitCode::SUCCESS);
    }

    let srcs: Vec<String> = {
        let mut s: Vec<String> = st.countries.values().map(|c| c.source.clone()).collect();
        s.sort();
        s.dedup();
        s
    };
    println!(
        "Interface  {} (hook {}, priority {})    Health {health}",
        iface(&cfg),
        cfg.nft.hook_chain,
        cfg.nft.hook_priority
    );
    let upd = st
        .last_update
        .map(fmt_time)
        .unwrap_or_else(|| "never".into());
    let via = if srcs.is_empty() {
        String::new()
    } else {
        format!(" via {}", srcs.join(", "))
    };
    let next = match &tm {
        Some((a, n)) if a == "active" && !n.is_empty() => format!("next {n}"),
        Some((a, _)) => format!("timer {a}"),
        None => "no timer".into(),
    };
    println!("Updated    {upd}{via}    {next}");
    println!();
    if rows.is_empty() {
        println!("No countries configured: nothing is blocked.");
    } else {
        println!(
            "{:<8}{:>12}{:>12}{:>13}{:>13}",
            "Country", "v4 ranges", "v6 ranges", "dropped v4", "dropped v6"
        );
        let mut t = (0, 0, 0, 0);
        for (cc, a, b, d4, d6) in &rows {
            println!(
                "{cc:<8}{:>12}{:>12}{:>13}{:>13}",
                fmt_num(*a as u64),
                fmt_num(*b as u64),
                fmt_num(*d4),
                fmt_num(*d6)
            );
            t = (t.0 + a, t.1 + b, t.2 + d4, t.3 + d6);
        }
        println!(
            "{:<8}{:>12}{:>12}{:>13}{:>13}",
            "Total",
            fmt_num(t.0 as u64),
            fmt_num(t.1 as u64),
            fmt_num(t.2),
            fmt_num(t.3)
        );
    }
    if live.is_none() {
        println!(
            "\n(drop counters since the last rebuild need sudo; shown totals are from state.json)"
        );
    }
    if !problems.is_empty() {
        println!();
        for c in problems {
            println!("{:<5} {:<12} {}", level_name(c.level), c.what, c.msg);
        }
    }
    Ok(ExitCode::SUCCESS)
}
