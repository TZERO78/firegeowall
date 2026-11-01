# firegeowall (`fgw`)

Country-based blocking for nftables, done the simple way.

`fgw` is a small Rust command-line tool that keeps an nftables set filled with the
IP ranges of the countries you want to keep out. nftables does the filtering; `fgw`
only manages the set contents. No daemon, no web UI, no rule generation.

**Status: design phase.** No code yet. The summary below is what will be built;
the full design with rationale, nftables integration, config format, safety
analysis and decision log is in [docs/DESIGN.md](docs/DESIGN.md).

## Design

- **Block list only, everything else allowed.** An empty list means no filtering.
  You cannot lock yourself out with an empty configuration.
- **One set, one rule per address family.** The nftables ruleset defines the empty
  sets `geo_block` / `geo_block6`; `fgw` fills them. The drop rules live in your own
  nftables config and are never touched by the tool.
- **Whole public interface, all ports.** The rule is meant to sit in a prerouting
  chain, so it catches traffic to the host and traffic forwarded to containers alike.
  Loopback and mesh/VPN interfaces are unaffected.
- **Atomic updates.** Flush and refill happen in one nftables transaction, validated
  with `nft -c` first. If a download fails or a list is suspiciously small, the
  previous set is kept.
- **Boot restore.** The last good zone files are cached locally and loaded by a
  systemd unit right after nftables, with no network needed.
- **Fail open, never closed.** Any error leaves the server reachable and unfiltered,
  never unreachable.
- **Self-protection.** Adding the country of your current SSH session is refused
  unless you pass `--force`.
- **Data source:** public per-country zone files (ipdeny.com / db-ip), refreshed
  weekly by a systemd timer.

## Commands (seven, no more)

```
fgw add CN        add a country to the block list and apply immediately
fgw remove CN     remove a country and apply immediately
fgw list          show configured countries
fgw status        mode, countries, interface, last update, drop counters
fgw show CN       print the ranges of a country
fgw check         validate config and sets against nft -c
fgw update        refresh zone files and refill the sets (run by the timer)
```

Configuration lives in `/etc/firegeowall/config.toml`, one line per country, so
Ansible and the CLI edit the same file.

## Non-goals

- No allow-list mode (default-deny would lock you out; may come later as a second set)
- No web dashboard
- No rule generation, no replacement for your firewall config

## License

Dual-licensed under MIT or Apache-2.0, at your option.
