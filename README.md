# firegeowall (`fgw`)

Country-based blocking for nftables, done the simple way.

`fgw` is a small Rust command-line tool that keeps an nftables set filled with the
IP ranges of the countries you want to keep out. nftables does the filtering; `fgw`
owns one chain and the sets in it, and touches nothing else. No daemon, no web UI.

Principles: simple, fast to use, seamless with nftables, dynamic enough, light on
RAM. No resident process; the kernel holds the ranges, the binary runs and exits.

**Status: v0.1.1.** Running in production on a Debian 13 server. Packages for
Debian/Ubuntu (amd64) from a signed APT repository.

## Quick start

```sh
curl -fsSLO https://raw.githubusercontent.com/TZERO78/firegeowall/main/scripts/install.sh
less install.sh && sudo sh install.sh      # adds the APT repository, installs firegeowall
```

The package enables nothing by itself. Three steps follow: add the hook chain to
nftables, choose countries, enable the units. All in
[docs/INSTALL.md, First setup](docs/INSTALL.md#first-setup).

```sh
sudo fgw add CN RU                                   # block, applied at once
sudo systemctl enable --now fgw-restore.service fgw-update.timer
sudo fgw status                                      # drops per country
sudo apt update && sudo apt upgrade                  # later: new releases
```

Country codes are ISO 3166-1 alpha-2 ([list](https://en.wikipedia.org/wiki/ISO_3166-1_alpha-2);
the United Kingdom is `GB`).

## Documentation

| | |
|---|---|
| [docs/INSTALL.md](docs/INSTALL.md) | Install script, APT repository, .deb, from source, first setup, updates, uninstall |
| [docs/USAGE.md](docs/USAGE.md) | Commands, country codes, configuration, timers, troubleshooting |
| [docs/DESIGN.md](docs/DESIGN.md) | Why it exists, nftables integration, full config reference, safety analysis, decision log |
| [docs/RELEASING.md](docs/RELEASING.md) | Building releases, the APT repository workflow, the signing key |

## Design

- **Block list only, everything else allowed.** An empty list means no filtering.
  You cannot lock yourself out with an empty configuration.
- **IPv4 and IPv6 alike.** Same list, same behaviour, no switch, no special cases.
- **One set and one rule per country.** `nft list ruleset -t` shows
  `ip saddr @geo_ru counter drop comment "RU"` with its own counter. Your hook chain
  jumps into `geo_block`, the one chain `fgw` owns and rebuilds atomically.
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

## Commands

```
fgw add CN        add countries to the block list and apply immediately
fgw remove CN     remove countries and apply immediately
fgw list          show configured countries
fgw status        interface, health, last update, ranges and drops per country
fgw show CN       print the cached ranges of a country
fgw check         validate config, hook chain, persistence, sets and cache
fgw update        refresh the lists and refill the sets (run by the weekly timer)
```

Details and examples: [docs/USAGE.md](docs/USAGE.md).

## Build and test

No toolchain on the server: everything builds in a throwaway container.

```
podman build -t fgw-build contrib/build
podman run --rm -v .:/src:Z fgw-build cargo build --release      # target/release/fgw, static musl, ~3 MB
podman run --rm -v .:/src:Z fgw-build cargo test
podman run --rm --cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN \
    -v .:/src:Z fgw-build sh -c 'cargo build && sh tests/netns.sh'  # real nftables, real packets
podman run --rm -v .:/src:Z fgw-build cargo deb --no-build --no-strip   # target/debian/*.deb
```

The integration test runs in the container's own network namespace and never
touches the host firewall.

## Non-goals

- Linux with nftables only. No iptables, no ipset, no other operating systems
- No allow-list mode (default-deny would lock you out; may come later as a second set)
- No web dashboard
- Nothing outside its own chain and sets; your hook chain and policy stay yours

## License

Dual-licensed under MIT or Apache-2.0, at your option.
