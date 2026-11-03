# firegeowall design

Status: implemented in v0.1.0 (2026-11-03). Section 14 lists where the code deviates from the first draft.

## 0. Principles

Five sentences that decide every design question below. When in doubt, pick the
option that satisfies more of them.

1. **Simple.** One list, one switch-free mode, seven commands, one YAML file. If a
   feature needs a paragraph to explain, it is not in version 1.
2. **Fast to use.** `fgw add CN` takes effect before the prompt returns. Output fits
   on one screen. Shell completions ship with the binary.
3. **Seamless with nftables.** The tool speaks nftables' own language: it owns one
   chain and the sets in it, rebuilds them in one atomic transaction, and touches
   nothing else. `nft list ruleset -t` shows `drop comment "RU"` per country with its
   own counter, and the whole firewall still fits on one page.
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
keep what the tool may touch to a sharply bounded part of the ruleset.

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
- Touching anything outside its own chain and sets. The hook chain, the policy and
  the rest of the ruleset belong to the operator.
- Replacing CrowdSec. They complement each other: CrowdSec reacts to behaviour,
  firegeowall filters by origin.
- Per-port or per-service rules. One list, one interface.
- A dashboard. `fgw status --json` exists so someone else can build one.

## 4. How it fits into nftables

The operator's ruleset declares one small hook chain that jumps into a chain owned by
firegeowall. The tool owns exactly that one chain and the sets it references, and
nothing else.

```nft
table inet filter {
    # Owned by the operator. Never touched by fgw.
    chain geo_prerouting {
        type filter hook prerouting priority -150; policy accept;
        iifname != "ens6" return
        ct state established,related return
        jump geo_block
    }

    # Owned by fgw. Rebuilt atomically on add / remove / update / restore.
    # Shown here as it looks with the default country list loaded.
    chain geo_block {
        ip  saddr @geo_cn  counter drop comment "CN"
        ip  saddr @geo_ru  counter drop comment "RU"
        ip  saddr @geo_kp  counter drop comment "KP"
        ip  saddr @geo_ir  counter drop comment "IR"
        ip  saddr @geo_by  counter drop comment "BY"
        ip  saddr @geo_vn  counter drop comment "VN"
        ip6 saddr @geo6_cn counter drop comment "CN"
        ip6 saddr @geo6_ru counter drop comment "RU"
        ...
    }

    set geo_cn  { type ipv4_addr; flags interval; auto-merge; }
    set geo6_cn { type ipv6_addr; flags interval; auto-merge; }
    ...
}
```

Design notes:

- **One set and one rule per country.** `nft list ruleset -t` shows `drop comment
  "RU"` with its own packet counter. You can read which country is blocked and how
  often it hit, straight from the firewall, without the tool. Twelve lines for six
  countries, still one screen.
- **Ownership is a hard line.** The hook chain `geo_prerouting`, its policy and its
  priority belong to the operator and are only *checked* by `fgw check`. The chain
  `geo_block` and every set named `geo_<cc>` / `geo6_<cc>` belong to fgw and are
  rebuilt as a whole. fgw never adds, edits or removes anything outside of those.
- **Atomic rebuild.** Every change (add, remove, update, restore) is one nftables
  transaction: create missing sets, fill them, `flush chain geo_block`, add the rules
  in the configured order, delete sets of countries no longer configured. Either the
  whole transaction applies or nothing does. There is no moment with a half-built
  chain or an empty set that a rule still points to.
- **prerouting, not input.** Traffic that is DNATed to a container never passes the
  input hook. A prerouting chain sees host traffic and forwarded traffic alike. This
  closes the gap the old blocker had.
- **Priority -150** sits after conntrack (-200) so `ct state` works, and before
  NAT prerouting (-100) so blocked traffic is dropped before DNAT.
- **`return` and `jump`, never `accept`.** The hook chain only returns or jumps, the
  owned chain only drops. Everything else continues to the normal input/forward
  chains untouched. This is the fix for the old bug, baked into the design.
- **Interface match first.** Loopback, WireGuard/NetBird (`wt0`), Tailscale and
  container bridges are never evaluated.
- **Established connections survive.** Adding a country while connected from it does
  not cut the session. The next connection is refused.
- **Reading the ruleset:** `nft list ruleset -t` prints rules without set contents.

The hook chain is shipped as `contrib/nftables/50-firegeowall.conf` for operators
who use an `include "/etc/nftables.d/*.conf"` layout. Debian's stock
`/etc/nftables.conf` has no such include; without it, the next reload drops the
hook chain and fgw filters nothing (the README install adds the line). It also declares an empty
`chain geo_block` so the `jump` is valid before fgw has ever run. `fgw check`
verifies that the hook chain exists and jumps to `geo_block`, and warns if the
priority or interface differ from the configuration.

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

# Refuse a list with fewer ranges than this (per family). Small countries are
# small: KP has one IPv4 range and no IPv6 list at all, so the floor is low.
min_ranges:
  ipv4: 1
  ipv6: 0

# Refuse a list that shrank by more than this many percent against the cached
# one. This is the real protection against truncated downloads.
max_shrink_percent: 50

# Names of the nftables objects. Change only if your ruleset differs.
nft:
  table: inet filter
  chain: geo_block          # owned by fgw: rebuilt as a whole
  set_prefix_v4: geo_       # sets become geo_cn, geo_ru, ...
  set_prefix_v6: geo6_      # and geo6_cn, geo6_ru, ...
  hook_chain: geo_prerouting   # owned by the operator: only checked
  hook_priority: -150          # fgw check warns if the live chain differs

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
| `min_ranges.ipv4` / `.ipv6` | 1 / 0 | a missing IPv6 list (404) means "no IPv6 ranges" |
| `max_shrink_percent` | 50 | against the cached list |
| `nft.table` | `inet filter` | |
| `nft.chain` | `geo_block` | owned and rebuilt by fgw |
| `nft.set_prefix_v4` / `nft.set_prefix_v6` | `geo_` / `geo6_` | one set per country and family |
| `nft.hook_chain` | `geo_prerouting` | only checked, never created |
| `nft.hook_priority` | `-150` | only checked |
| `state_dir` | `/var/lib/firegeowall` | matches `StateDirectory=` in the units |

`fgw check --print-nft` prints the ruleset snippet the current configuration
expects (names and priority substituted), so an operator with different names can
paste it into their own `/etc/nftables.d/`. It prints, it never writes. This is a
flag on `check`, not an eighth command.

## 6. Commands

Seven commands. If an eighth is proposed it needs a very good reason.

| Command | Does | Touches system |
|---|---|---|
| `fgw add CC [CC…]` | Add countries to config, rebuild chain and sets immediately | config, owned chain + sets |
| `fgw remove CC [CC…]` | Remove countries, rebuild chain and sets immediately | config, owned chain + sets |
| `fgw list` | Print configured countries with range counts | nothing |
| `fgw status` | Interface, countries, set sizes, last update, drop counters, timer state | nothing |
| `fgw show CC` | Print the ranges of one country from the cache | nothing |
| `fgw check` | Validate config, hook chain, owned chain, sets and cache; exit non-zero on problems | nothing |
| `fgw update` | Fetch zone files, validate, rebuild chain and sets atomically. Run by the timer | cache, owned chain + sets |

Common behaviour:

- Every command accepts `--json` for machine-readable output.
- Commands that change state print what they did and exit 0; on any failure they
  change nothing and exit non-zero with one clear sentence.
- `add` refuses to add the country of the caller's current SSH session
  (`SSH_CONNECTION` → GeoIP lookup against the cache) unless `--force` is given.
- `add`/`remove`/`update` need `CAP_NET_ADMIN`; everything else runs as any user.

### Example `fgw status` output

```
Interface  ens6 (hook geo_prerouting, priority -150)      Health OK
Updated    2026-11-01 03:30 via ipdeny                    next Sun 03:30

Country   v4 ranges   v6 ranges   dropped v4   dropped v6
CN           8,412       3,120        1,311           31
RU           9,870       2,044          402            0
KP              12           1            0            0
IR           3,105         610           68            0
BY             801         140           12            0
VN           2,230         410           49            0
Total       24,430       6,325        1,842           31
```

## 7. Update and apply

`fgw update` does, in order:

1. For each configured country, walk the `sources` list by priority and fetch the
   IPv4 and IPv6 lists from the first source that delivers over HTTPS (or from a local
   path). Verify each line parses as a CIDR of the right family. Record which source
   was used.
2. Write the files to `/var/lib/firegeowall/zones/` atomically (temp file + rename).
   Keep the previous version until the new one is complete.
3. Build one nftables script that rebuilds the owned chain and its sets:
   ```
   add set inet filter geo_cn  { type ipv4_addr; flags interval; auto-merge; }
   add set inet filter geo6_cn { type ipv6_addr; flags interval; auto-merge; }
   flush set inet filter geo_cn
   add element inet filter geo_cn { 1.2.3.0/24, ... }
   ...                                    (one block per configured country)
   flush chain inet filter geo_block
   add rule inet filter geo_block ip  saddr @geo_cn  counter drop comment "CN"
   add rule inet filter geo_block ip6 saddr @geo6_cn counter drop comment "CN"
   ...                                    (two rules per country, config order)
   delete set inet filter geo_xx         (sets of countries no longer configured)
   ```
   Run `nft -c -f` on it. Then `nft -f`. Everything is one transaction: sets are
   never empty while a rule points at them, and the chain is never half built.
   Counters of unchanged countries are preserved by re-adding the rule with the
   same handle semantics where nftables allows it; where it does not, `fgw status`
   also keeps a running total in `state.json`.
4. Record the timestamp and source in `/var/lib/firegeowall/state.json`.

If any download fails, any file fails validation, or a list is below `min_ranges` or shrank by
more than `max_shrink_percent`, the run aborts before step 3 and the live sets stay as they are. The
failure is logged to the journal and `fgw status` shows `Health DEGRADED` with the
reason.

`fgw add`/`remove` do the same without the download step, using the cache. If a newly
added country is not in the cache yet, it is fetched first.

## 8. Running as a service

Nothing in firegeowall is a daemon. Two oneshot units and a timer do the work, all
shipped in `contrib/systemd/`:

| Unit | When | What |
|---|---|---|
| `fgw-restore.service` | after `nftables.service` at boot, and again whenever nftables is restarted (`PartOf=`) or reloaded (`ReloadPropagatedFrom=` + `ExecReload=`) | `fgw apply --from-cache`: fill the sets from `/var/lib/firegeowall/zones/`, no network |
| `fgw-update.timer` | `Sun 03:30`, randomised by up to 30 min, `Persistent=true` | starts `fgw-update.service` |
| `fgw-update.service` | by the timer, or `systemctl start fgw-update` by hand | `fgw update`: fetch, validate, refill |

Design notes:

- **Boot gap.** After a reboot the sets are empty until `fgw-restore` runs. Because
  this is a block list, empty means *unfiltered*, not *unreachable*: the intended
  failure direction. The restore unit is ordered `Before=network-online.target`, so
  the gap is a few hundred milliseconds, not until the next timer run.
- **`flush ruleset` is handled.** `/etc/nftables.conf` begins with `flush ruleset`,
  so a `systemctl reload nftables` leaves `geo_block` empty and the country sets gone. `PartOf=` makes the
  restore unit re-run on every restart or reload of `nftables.service`. `fgw check`
  additionally detects "chain geo_block is empty while countries are configured".
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
| Empty or truncated download | Per-line CIDR validation, prefixes shorter than /8 (v4) or /12 (v6) rejected, `min_ranges`, `max_shrink_percent`; abort before touching sets |
| Set empty or chain half built during update | Sets, elements, chain rules and deletions in one nftables transaction |
| Bad nftables syntax | `nft -c` before `nft -f` |
| Host unreachable after reboot | Fail-open by design; restore from cache without network |
| Tool rewrites firewall logic | It touches only chain `geo_block` and sets named `geo_*`/`geo6_*`; the hook chain and everything else are read-only to it |
| Running as root all the time | services run as user `fgw` with `AmbientCapabilities=CAP_NET_ADMIN` only; `ProtectSystem=strict`, `StateDirectory=`, `NoNewPrivileges`, syscall filter (see §8) |
| Plain-text secrets | None exist. Sources are public files over HTTPS |

## 10. Data sources

| Source | Format | Notes |
|---|---|---|
| ipdeny.com | `https://www.ipdeny.com/ipblocks/data/aggregated/CC-aggregated.zone`, `https://www.ipdeny.com/ipv6/ipaddresses/aggregated/CC-aggregated.zone` (404 for countries without IPv6) | Free, no key, aggregated, updated daily |
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

Debian package (`cargo deb`, see README):

```
/usr/sbin/fgw
/usr/lib/systemd/system/fgw-update.service
/usr/lib/systemd/system/fgw-update.timer
/usr/lib/systemd/system/fgw-restore.service
/usr/lib/sysusers.d/fgw.conf
/usr/share/firegeowall/config.yml             (template, copied once if absent)
/usr/share/doc/firegeowall/examples/          (config.yml, 50-firegeowall.conf)
/etc/firegeowall/config.yml                   (created by postinst, never overwritten)
/etc/firegeowall/custom/CC.zone               (optional local lists)
/etc/nftables.d/50-firegeowall.conf           (operator-owned, not shipped)
/var/lib/firegeowall/zones/CC.zone, CC.zone6
/var/lib/firegeowall/state.json
```

A manual install puts the units in `/etc/systemd/system/` and the sysusers file in
`/etc/sysusers.d/`; the binary path `/usr/sbin/fgw` is the same.

The CLI edits only the `countries:` block of `config.yml`.

## 13. Open questions

None at the moment. Everything below was decided on 2026-11-01; see the decision log.

- Initial country list: CN, RU, KP, IR, BY, VN. This is the list the first
  deployment on the author's VPS starts with and the one `contrib/config.yml`
  ships. Operators change it with `fgw add` / `fgw remove`. The country of any
  legitimate remote peer must never be on it.
- Removed countries are dropped from the cache on the next `fgw update`.
- No CrowdSec overlap statistics in `fgw status`.
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
| 2026-11-01 | One set and one rule per country in a tool-owned chain `geo_block`, so `nft list ruleset -t` shows `drop comment "RU"` with per-country counters. Hook chain stays operator-owned |
| 2026-11-01 | Initial country list fixed: CN, RU, KP, IR, BY, VN; shipped in `contrib/config.yml` |
| 2026-11-01 | Principles fixed: simple, fast to use, seamless with nft, dynamic enough, light on RAM. Default source ipdeny only; removed countries leave the cache; no CrowdSec statistics |
| 2026-11-01 | Built-in defaults for the nft names and priority; `fgw check --print-nft` prints the expected snippet. Services run as unprivileged user `fgw` with `CAP_NET_ADMIN`; restore unit is `PartOf=nftables.service` |
| 2026-11-03 | v0.1.0 implemented. Deviations from the draft: `min_ranges` default 1/0 instead of 1000/100 (BY has 105 IPv4 ranges, KP one and no IPv6) plus `max_shrink_percent: 50`; IPv6 404 = no IPv6 ranges, no set and no rule for that family; prefixes shorter than /8 or /12 rejected; reload of nftables propagated via `ReloadPropagatedFrom=` (PartOf covers only stop/restart); no `tracing` crate, stderr goes to the journal; ISO 3166 code check so `UK` fails; config lock via `flock` on `<state_dir>/lock` |
| 2026-11-03 | Builds in a rootless Podman container (`contrib/build/Containerfile`), integration test `tests/netns.sh` drops real packets from a peer namespace |
| 2026-11-03 | v0.1.1: Debian package via `cargo-deb`. Binary moves to `/usr/sbin/fgw`. The package enables nothing (the hook chain is operator-owned), creates `config.yml` once from an empty template instead of shipping a conffile (no dpkg prompts when `fgw add` edits it), keeps the user `fgw` on purge. Goal: a signed APT repository so `apt upgrade` delivers new releases |
| 2026-11-03 | Units no longer pull in `nftables.service` (`Requisite=` for restore, nothing for update): starting it from fgw would `flush ruleset`. `fgw add` keeps new countries when filtering db-ip. Shortest accepted IPv4 prefix lowered from /8 to /4 (the US list has a /6). README install adds the `/etc/nftables.d` include |
| 2026-11-03 | `fgw check` fails when the hook chain is not in the ruleset file nftables.service loads (`/etc/nftables.conf` and its includes): it would vanish on the next reload. The lock file follows the state directory owner like every other file, so a first `sudo fgw add` no longer locks the timer out |
