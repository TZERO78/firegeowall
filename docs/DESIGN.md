# firegeowall design

Status: design phase, 2026-11-01. Nothing here is implemented yet.

## 0. Principles

Five sentences that decide every design question below. When in doubt, pick the
option that satisfies more of them.

1. **Simple.** One list, one switch-free mode, seven commands, one YAML file. If a
   feature needs a paragraph to explain, it is not in version 1.
2. **Fast to use.** `fgw add CN` takes effect before the prompt returns. Output fits
   on one screen. Shell completions ship with the binary.
3. **Seamless with nftables.** The tool speaks nftables' own language: it only fills
   sets the operator declared, in one atomic transaction, and never invents rules.
   `nft list ruleset -t` still shows the whole firewall on one page.
4. **Dynamic enough.** Changes are live immediately, sets survive reboots and
   reloads, lists refresh weekly. No daemon is needed for that; systemd already
   provides the clock and the ordering.
5. **Light on RAM.** No resident process. The binary runs, fills the sets, exits.
   Zone files are streamed line by line, never loaded whole. The kernel holds the
   ranges in interval sets, which is the most compact place for them.

## 1. Why this exists

The author ran a home-made Bash GeoIP blocker on a hardened Debian VPS for about a
year. It worked, but two things were wrong with it:

- **The ruleset was huge.** Tens of thousands of CIDR ranges lived inside the
  nftables configuration files. `nft list ruleset` became unreadable and the config
  was impossible to review.
- **A logic bug hid in the noise.** The country check chain ended in `accept` for the
  home country instead of `return`. An `accept` inside a jumped-to chain ends input
  evaluation, so every connection from the home country reached every listening port.
  The port restrictions only applied to the rest of the world. Nobody noticed for
  months because the config was too big to read.

The lesson: keep the data out of the rules, keep the rules small enough to read, and
never let the tool generate firewall logic.

Alternatives were considered and rejected for this use case:

- **CrowdSec** is reactive. It can tag alerts with a country and ban with
  `--scope country`, but only the Cloudflare bouncer enforces country decisions. It
  does not do pre-emptive blocking and its maintainers say so.
- **Cloudflare WAF** can block countries for free, but only for HTTP(S) behind its
  proxy. It does nothing for UDP (STUN, WireGuard) or SSH, and it adds a dependency
  and an origin-IP exposure problem. Rejected to keep the setup self-contained.
- **Traefik geoblock plugins** filter inside the reverse proxy, after the traffic has
  already reached the host. Too late and HTTP-only.
- **nftgeo** (GitHub, 2026) does roughly the right thing but is brand new, has no
  releases and generates rules itself.

## 2. Goals

1. Block traffic from a list of countries on the public interface, all ports.
2. Keep the nftables configuration small and human-readable.
3. Never lock the operator out, never make the host unreachable on error.
4. Be boring: a CLI, a config file, a timer. No daemon, no web UI, no database.
5. Be reviewable: fewer than ~1,500 lines of Rust, seven commands.
6. Target exactly one platform: Linux with nftables (kernel 5.x+, `nft` 1.0+).
7. IPv4 and IPv6 are treated identically: same country list, same sources, same
   rules, same thresholds logic, same status output. There is no IPv6 switch and
   no special case anywhere in the code; every code path is written once over an
   address family parameter.

## 3. Non-goals

- Anything but Linux with nftables. No iptables, no ipset, no nft-compat shims,
  no BSD/pf, no macOS, no Windows. One target keeps the code small and the
  transaction semantics exact. A host that still runs iptables migrates to nftables
  first; that is out of scope here.
- Allow-list mode (default-deny). It would lock you out on an empty or stale set.
  Could be added later as a second set without redesign.
- Generating or editing nftables rules. The operator owns the ruleset.
- Replacing CrowdSec. They complement each other: CrowdSec reacts to behaviour,
  firegeowall filters by origin.
- Per-port or per-service rules. One list, one interface.
- A dashboard. `fgw status --json` exists so someone else can build one.

## 4. How it fits into nftables

The operator's ruleset declares two empty sets and one small chain. firegeowall only
ever changes the *contents* of the sets.

```nft
table inet filter {
    set geo_block {
        type ipv4_addr
        flags interval
        auto-merge
    }
    set geo_block6 {
        type ipv6_addr
        flags interval
        auto-merge
    }

    chain geo_prerouting {
        type filter hook prerouting priority -150; policy accept;
        iifname != "ens6" return
        ct state established,related return
        ip  saddr @geo_block  counter drop
        ip6 saddr @geo_block6 counter drop
    }
}
```

Design notes:

- **prerouting, not input.** Traffic that is DNATed to a container never passes the
  input hook. A prerouting chain sees host traffic and forwarded traffic alike. This
  closes the gap the old blocker had.
- **Priority -150** sits after conntrack (-200) so `ct state` works, and before
  NAT prerouting (-100) so blocked traffic is dropped before DNAT.
- **`return`, never `accept`.** The chain only drops. Everything else continues to the
  normal input/forward chains untouched. This is the fix for the old bug, baked into
  the design.
- **Interface match first.** Loopback, WireGuard/NetBird (`wt0`), Tailscale and
  container bridges are never evaluated.
- **Established connections survive.** Adding a country while connected from it does
  not cut the session. The next connection is refused.
- **`counter`** gives `fgw status` per-rule drop counts for free.
- **Reading the ruleset:** `nft list ruleset -t` prints rules without set contents.
  The ruleset stays one screen long.

The snippet above is shipped as `contrib/nftables/50-firegeowall.conf` for operators
who use an `include "/etc/nftables.d/*.conf"` layout. `fgw check` verifies that the
sets exist and warns if the chain is missing, but never creates either.

## 5. Configuration

`/etc/firegeowall/config.yml`, owned by root, mode 0644. YAML, because everything
else on the host (CrowdSec, Ansible, NetBird) is YAML too.

```yaml
# Public interface the rule applies to. Used by `fgw check` and `fgw status`.
interface: ens6

# ISO 3166-1 alpha-2 codes. Empty list = nothing is blocked.
countries:
  - CN
  - RU
  - KP
  - IR
  - BY
  - VN

# Where the per-country range lists come from.
# Tried in order of priority (lowest number first). The first source that
# delivers a valid file for a country wins; the others are only fallbacks.
# {cc} is replaced by the lower-case country code.
sources:
  - name: ipdeny
    priority: 10
    ipv4: https://www.ipdeny.com/ipblocks/data/aggregated/{cc}-aggregated.zone
    ipv6: https://www.ipdeny.com/ipv6/ipaddresses/aggregated/{cc}-aggregated.zone
    format: zone            # one CIDR per line

  - name: dbip               # optional fallback, not in the defaults
    priority: 20
    url: https://download.db-ip.com/free/dbip-country-lite-{yyyy}-{mm}.csv.gz
    format: dbip-csv        # start,end,cc rows; converted to CIDRs locally

  - name: local
    priority: 90
    path: /etc/firegeowall/custom/{cc}.zone
    format: zone            # hand-maintained ranges, e.g. a hosting provider

# Refuse to apply a set smaller than this many ranges (per family).
# Protects against truncated downloads and empty lists.
min_ranges:
  ipv4: 1000
  ipv6: 100

# Names of the nftables objects. Change only if your ruleset differs.
nft:
  table: inet filter
  set_v4: geo_block
  set_v6: geo_block6
  chain: geo_prerouting
  chain_priority: -150      # fgw check warns if the live chain differs

# Cache and state. Normally left alone.
state_dir: /var/lib/firegeowall
```

Rules for the file:

- **Sources are ordered by `priority`.** Lower number = tried first. For each
  country, the first source that returns a file passing validation is used, and
  `fgw status` shows which one. A source with `path:` instead of a URL is a local
  file, which makes hand-maintained lists and air-gapped hosts possible.
- **Missing source for a country is not an error** as long as at least one source
  delivers. Only when all sources fail for a country does `fgw update` abort.
- **`fgw add` / `fgw remove` edit only the `countries:` block**, line by line, and
  leave every other line and comment untouched. The tool never re-serialises the
  whole file, so operator comments survive. If the block cannot be found, the
  command refuses rather than guess.
- **Ansible deploys the same file.** CLI and configuration management never
  disagree because there is only one file.
- **`fgw check` validates the schema** and reports unknown keys, so a typo like
  `prioroty:` fails loudly instead of being silently ignored.

### Built-in defaults

Everything except `countries` has a built-in default, so a minimal `config.yml` is
just the country list. The defaults match `contrib/nftables/50-firegeowall.conf`
exactly, so a fresh install of the three files (binary, contrib ruleset, config
with countries) works without touching the `nft:` block.

| Key | Default | Note |
|---|---|---|
| `interface` | interface of the default IPv4 route | detected at runtime; shown by `fgw status` |
| `sources` | ipdeny only | db-ip and local sources are opt-in; one source keeps the default path simple |
| `min_ranges.ipv4` / `.ipv6` | 1000 / 100 | |
| `nft.table` | `inet filter` | |
| `nft.set_v4` / `nft.set_v6` | `geo_block` / `geo_block6` | |
| `nft.chain` | `geo_prerouting` | only checked, never created |
| `nft.chain_priority` | `-150` | only checked |
| `state_dir` | `/var/lib/firegeowall` | matches `StateDirectory=` in the units |

`fgw check --print-nft` prints the ruleset snippet the current configuration
expects (names and priority substituted), so an operator with different names can
paste it into their own `/etc/nftables.d/`. It prints, it never writes. This is a
flag on `check`, not an eighth command.

## 6. Commands

Seven commands. If an eighth is proposed it needs a very good reason.

| Command | Does | Touches system |
|---|---|---|
| `fgw add CC [CC…]` | Add countries to config, refill sets immediately | config, sets |
| `fgw remove CC [CC…]` | Remove countries, refill sets immediately | config, sets |
| `fgw list` | Print configured countries with range counts | nothing |
| `fgw status` | Interface, countries, set sizes, last update, drop counters, timer state | nothing |
| `fgw show CC` | Print the ranges of one country from the cache | nothing |
| `fgw check` | Validate config, sets, chain and cache; exit non-zero on problems | nothing |
| `fgw update` | Fetch zone files, validate, refill sets atomically. Run by the timer | cache, sets |

Common behaviour:

- Every command accepts `--json` for machine-readable output.
- Commands that change state print what they did and exit 0; on any failure they
  change nothing and exit non-zero with one clear sentence.
- `add` refuses to add the country of the caller's current SSH session
  (`SSH_CONNECTION` → GeoIP lookup against the cache) unless `--force` is given.
- `add`/`remove`/`update` need `CAP_NET_ADMIN`; everything else runs as any user.

### Example `fgw status` output

```
Interface  ens6
Countries  CN RU KP IR BY VN            6 countries
Sets       geo_block 41,230 ranges      geo_block6 9,874 ranges
Updated    2026-11-01 03:30 (ipdeny)    next: Sun 03:30 via fgw-update.timer
Dropped    1,842 packets since boot     v4 1,811 / v6 31
Health     OK
```

## 7. Update and apply

`fgw update` does, in order:

1. For each configured country, walk the `sources` list by priority and fetch the
   IPv4 and IPv6 lists from the first source that delivers over HTTPS (or from a local
   path). Verify each line parses as a CIDR of the right family. Record which source
   was used.
2. Write the files to `/var/lib/firegeowall/zones/` atomically (temp file + rename).
   Keep the previous version until the new one is complete.
3. Build one nftables script:
   ```
   flush set inet filter geo_block
   add element inet filter geo_block { 1.2.3.0/24, ... }
   flush set inet filter geo_block6
   add element inet filter geo_block6 { ... }
   ```
   Run `nft -c -f` on it. Then `nft -f`. Both flush and add happen in one
   transaction, so the set is never empty for even a millisecond.
4. Record the timestamp and source in `/var/lib/firegeowall/state.json`.

If any download fails, any file fails validation, or a set would be smaller than
`min_ranges_*`, the run aborts before step 3 and the live sets stay as they are. The
failure is logged to the journal and `fgw status` shows `Health DEGRADED` with the
reason.

`fgw add`/`remove` do the same without the download step, using the cache. If a newly
added country is not in the cache yet, it is fetched first.

## 8. Running as a service

Nothing in firegeowall is a daemon. Two oneshot units and a timer do the work, all
shipped in `contrib/systemd/`:

| Unit | When | What |
|---|---|---|
| `fgw-restore.service` | after `nftables.service` at boot, and again whenever nftables is restarted or reloaded (`PartOf=nftables.service`) | `fgw apply --from-cache`: fill the sets from `/var/lib/firegeowall/zones/`, no network |
| `fgw-update.timer` | `Sun 03:30`, randomised by up to 30 min, `Persistent=true` | starts `fgw-update.service` |
| `fgw-update.service` | by the timer, or `systemctl start fgw-update` by hand | `fgw update`: fetch, validate, refill |

Design notes:

- **Boot gap.** After a reboot the sets are empty until `fgw-restore` runs. Because
  this is a block list, empty means *unfiltered*, not *unreachable*: the intended
  failure direction. The restore unit is ordered `Before=network-online.target`, so
  the gap is a few hundred milliseconds, not until the next timer run.
- **`flush ruleset` is handled.** `/etc/nftables.conf` begins with `flush ruleset`,
  so a `systemctl reload nftables` recreates the sets empty. `PartOf=` makes the
  restore unit re-run on every restart or reload of `nftables.service`. `fgw check`
  additionally detects "sets exist but are empty while the cache is not".
- **Unprivileged account.** Both services run as user `fgw` (created via
  `sysusers.d`) with only `CAP_NET_ADMIN` as an ambient capability. nftables is
  driven over netlink, which needs exactly that capability and nothing else. No
  root process, no setuid binary.
- **Hardening.** `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
  `NoNewPrivileges`, `MemoryDenyWriteExecute`, `SystemCallFilter=@system-service`,
  `RestrictAddressFamilies` (the restore unit does not even get `AF_INET`). The only
  writable path is the state directory. `/etc/firegeowall` is read-only to the
  services; only the interactive CLI run by an admin edits it.
- **Interactive use.** `fgw add`, `fgw remove` and `fgw update` run by hand need
  `CAP_NET_ADMIN` and write access to the config, so they run via `sudo`. `fgw
  status`, `list`, `show` and `check` work as any user; `status` reads set sizes via
  `nft -j list set`, which needs `CAP_NET_ADMIN` too, so it falls back to the cached
  counts and says so when run unprivileged.
- **`apply --from-cache`** is an internal subcommand used only by the restore unit.
  It is not one of the seven public commands and is hidden from `--help`.

## 9. Safety summary

| Risk | Mitigation |
|---|---|
| Lock yourself out | Block-list only; `add` refuses own country without `--force`; established connections are never cut; mesh interfaces are excluded |
| Empty or truncated download | Per-line CIDR validation; `min_ranges_*`; abort before touching sets |
| Set empty during update | Flush and add in one nftables transaction |
| Bad nftables syntax | `nft -c` before `nft -f` |
| Host unreachable after reboot | Fail-open by design; restore from cache without network |
| Tool rewrites firewall logic | It cannot: it only runs `flush set` / `add element` |
| Running as root all the time | services run as user `fgw` with `AmbientCapabilities=CAP_NET_ADMIN` only; `ProtectSystem=strict`, `StateDirectory=`, `NoNewPrivileges`, syscall filter (see §8) |
| Plain-text secrets | None exist. Sources are public files over HTTPS |

## 10. Data sources

| Source | Format | Notes |
|---|---|---|
| ipdeny.com | `https://www.ipdeny.com/ipblocks/data/aggregated/CC-aggregated.zone`, `.../ipv6/data/aggregated/CC-aggregated.zone` | Free, no key, aggregated, updated daily |
| db-ip.com (lite) | monthly CSV `dbip-country-lite-YYYY-MM.csv.gz` | Fallback; CC BY 4.0, attribution in README |

MaxMind GeoLite2 is deliberately not used: it needs an account key and its license
is awkward for a redistributed tool. CrowdSec already keeps a GeoLite2 copy on the
host for enrichment; firegeowall does not touch it.

## 11. Implementation notes (Rust)

- Edition 2024, MSRV = current stable at first release.
- Crates: `clap` (derive), `serde` + a maintained YAML crate (`serde_yaml_ng` or
  `serde-saphyr`; the original `serde_yaml` is archived) for reading the config,
  plain line editing for the `countries:` block so comments survive, `ipnet` (CIDR parsing and aggregation), `ureq` with `rustls` (no
  OpenSSL dependency, static build), `serde_json`, `anyhow`, `tracing` +
  `tracing-journald`.
- nftables is driven by spawning `nft -f -` with the script on stdin. No libnftables
  binding, no unsafe code. If the JSON API is ever wanted, `nft -j` is a drop-in.
- Target: `x86_64-unknown-linux-musl` (and `aarch64-unknown-linux-musl`), fully
  static binary, < 5 MB. Linux only; the crate uses `#[cfg(target_os = "linux")]`
  and refuses to build elsewhere rather than carrying dead abstractions. Deployment is
  one file plus two unit files and one config.
- Development happens on a workstation. The VPS gets the binary only. No toolchain
  on the server.

### Memory budget

| Where | What | Rough size |
|---|---|---|
| Kernel, nftables interval set | one element per aggregated range | about 50–100 bytes each; 50,000 ranges ≈ 3–5 MB |
| Disk cache `/var/lib/firegeowall/zones/` | plain text, one CIDR per line | 1–2 MB for six countries, both families |
| `fgw` while running | streamed parsing, one country in memory at a time | a few MB peak, 0 when idle |
| `fgw` binary | static musl build | < 5 MB on disk |

Rules that keep it that way:

- Zone files are read with a buffered line reader and written to the nft script as
  they are validated. No `Vec` of the whole world.
- Aggregation (merging adjacent ranges) is done per country, on sorted input, in a
  single pass. `auto-merge` on the set lets the kernel merge across countries.
- The db-ip whole-world CSV is streamed through a gzip decoder and filtered to the
  configured countries on the fly. It is never unpacked to disk.
- Nothing is cached in RAM between runs because there is no process between runs.

- Tests: unit tests for CIDR validation and script generation; an integration test
  that runs against a throwaway `nft` table in a network namespace (`ip netns`),
  skipped when not root.

## 12. Deployment layout

```
/usr/local/sbin/fgw
/etc/firegeowall/config.yml
/etc/firegeowall/custom/CC.zone               (optional local lists)
/etc/nftables.d/50-firegeowall.conf        (from contrib/, operator-owned)
/etc/systemd/system/fgw-update.service     (from contrib/systemd/)
/etc/systemd/system/fgw-update.timer
/etc/systemd/system/fgw-restore.service
/usr/lib/sysusers.d/fgw.conf                 (from contrib/systemd/sysusers-fgw.conf)
/var/lib/firegeowall/zones/CC.zone, CC.zone6
/var/lib/firegeowall/state.json
```

An Ansible role will ship these; the CLI edits only the `countries:` block of `config.yml`.

## 13. Open questions

- Final country list for the first deployment. Proposed: CN, RU, KP, IR, BY, VN.
  Must not include the country of any legitimate remote peer.

Resolved by the principles (section 0):

- Removed countries are dropped from the cache on the next `fgw update`. Keeping
  stale files for `fgw show` would be a second lifecycle to explain. If you want to
  look at a country, add it or fetch the zone file yourself.
- No CrowdSec overlap statistics in `fgw status`. Two tools, two outputs.
- IPv6: identical to IPv4, see goal 7.

## 14. Decision log

| Date | Decision |
|---|---|
| 2026-11-01 | Old Bash GeoIP blocker removed from the VPS after the accept/return bug was found |
| 2026-11-01 | Country filtering via Cloudflare WAF considered, then rejected: HTTP-only, external dependency |
| 2026-11-01 | Write a small Rust tool instead. CLI only, no dashboard |
| 2026-11-01 | Not port-specific: whole public interface, first rule, prerouting hook |
| 2026-11-01 | Block-list only, allow-all default. Allow mode dropped as a non-goal |
| 2026-11-01 | Seven commands, hard cap of ten |
| 2026-11-01 | Name: firegeowall, binary `fgw`. Dual MIT / Apache-2.0. Private until first working release |
| 2026-11-01 | Config is YAML (`config.yml`), sources listed with explicit priority, local file source supported |
| 2026-11-01 | Linux + nftables only. No iptables/ipset, no other OS |
| 2026-11-01 | IPv6 handled exactly like IPv4: same list, same behaviour, no opt-in, no special cases |
| 2026-11-01 | Principles fixed: simple, fast to use, seamless with nft, dynamic enough, light on RAM. Default source ipdeny only; removed countries leave the cache; no CrowdSec statistics |
| 2026-11-01 | Built-in defaults for the nft names and priority; `fgw check --print-nft` prints the expected snippet. Services run as unprivileged user `fgw` with `CAP_NET_ADMIN`; restore unit is `PartOf=nftables.service` |
