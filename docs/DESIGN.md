# firegeowall design

Status: design phase, 2026-11-01. Nothing here is implemented yet.

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

## 3. Non-goals

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

`/etc/firegeowall/config.toml`, owned by root, mode 0644:

```toml
# Public interface the rule applies to. Only used by `fgw check` and `fgw status`.
interface = "ens6"

# ISO 3166-1 alpha-2 codes. Empty list = nothing is blocked.
countries = ["CN", "RU", "KP", "IR", "BY", "VN"]

# Where zone files come from. First that works wins.
sources = ["ipdeny", "dbip"]

# Refuse to apply a set smaller than this many ranges (per family).
# Protects against truncated downloads and empty lists.
min_ranges_v4 = 1000
min_ranges_v6 = 100

# Names of the nftables objects. Change only if your ruleset differs.
[nft]
table   = "inet filter"
set_v4  = "geo_block"
set_v6  = "geo_block6"
```

`fgw add` and `fgw remove` rewrite the `countries` line and nothing else, preserving
comments. The same file is deployed by Ansible, so CLI and configuration management
never disagree.

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

1. For each configured country, download `CC.zone` and `CC.zone6` from the first
   working source over HTTPS. Verify each line parses as a CIDR of the right family.
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

## 8. Boot behaviour

After a reboot the sets are empty until something fills them. Because this is a
block list, empty means *unfiltered*, not *unreachable*. That is the intended failure
direction. Still, the gap should be seconds, not until the next timer run:

- `fgw-restore.service`: `After=nftables.service`, `Before=network-online.target`,
  runs `fgw apply --from-cache` (internal subcommand, not in the public seven). Needs
  no network. Fills the sets from `/var/lib/firegeowall/zones/`.
- `fgw-update.timer`: weekly, `OnCalendar=Sun 03:30`, `Persistent=true`,
  `RandomizedDelaySec=30m`.

If `nftables.service` is reloaded by hand (`flush ruleset`), the sets are recreated
empty by the ruleset and `fgw-restore.service` does not re-run. `fgw check` detects
"sets exist but are empty while the cache is not" and tells the operator to run
`fgw update`. A `PartOf=nftables.service` dependency on the restore unit is an option
to evaluate.

## 9. Safety summary

| Risk | Mitigation |
|---|---|
| Lock yourself out | Block-list only; `add` refuses own country without `--force`; established connections are never cut; mesh interfaces are excluded |
| Empty or truncated download | Per-line CIDR validation; `min_ranges_*`; abort before touching sets |
| Set empty during update | Flush and add in one nftables transaction |
| Bad nftables syntax | `nft -c` before `nft -f` |
| Host unreachable after reboot | Fail-open by design; restore from cache without network |
| Tool rewrites firewall logic | It cannot: it only runs `flush set` / `add element` |
| Running as root all the time | systemd units with `CapabilityBoundingSet=CAP_NET_ADMIN`, `ProtectSystem=strict`, `ReadWritePaths=/var/lib/firegeowall`, `NoNewPrivileges=yes` |
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
- Crates: `clap` (derive), `serde` + `toml` (config, preserving comments via
  `toml_edit`), `ipnet` (CIDR parsing and aggregation), `ureq` with `rustls` (no
  OpenSSL dependency, static build), `serde_json`, `anyhow`, `tracing` +
  `tracing-journald`.
- nftables is driven by spawning `nft -f -` with the script on stdin. No libnftables
  binding, no unsafe code. If the JSON API is ever wanted, `nft -j` is a drop-in.
- Target: `x86_64-unknown-linux-musl`, fully static binary, < 5 MB. Deployment is
  one file plus two unit files and one config.
- Development happens on a workstation. The VPS gets the binary only. No toolchain
  on the server.
- Tests: unit tests for CIDR validation and script generation; an integration test
  that runs against a throwaway `nft` table in a network namespace (`ip netns`),
  skipped when not root.

## 12. Deployment layout

```
/usr/local/sbin/fgw
/etc/firegeowall/config.toml
/etc/nftables.d/50-firegeowall.conf        (from contrib/, operator-owned)
/etc/systemd/system/fgw-update.service
/etc/systemd/system/fgw-update.timer
/etc/systemd/system/fgw-restore.service
/var/lib/firegeowall/zones/CC.zone, CC.zone6
/var/lib/firegeowall/state.json
```

An Ansible role will ship these; the CLI edits only `config.toml`.

## 13. Open questions

- Final country list for the first deployment. Proposed: CN, RU, KP, IR, BY, VN.
  Must not include the country of any legitimate remote peer.
- Should `fgw update` also refresh countries that were removed, to keep `fgw show`
  useful for them? Probably yes, cache everything ever requested, cap at 30 days.
- IPv6: block by default with the same list, or make it opt-in? Proposed: same list,
  same behaviour, no special casing.
- Overlap with CrowdSec: a CrowdSec-banned IP inside a blocked country is dropped
  twice. Harmless, but `fgw status` could note how many CrowdSec decisions are
  already covered. Nice-to-have, not v1.

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
