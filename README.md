# firegeowall (`fgw`)

Country-based blocking for nftables, done the simple way.

`fgw` is a small Rust command-line tool that keeps an nftables set filled with the
IP ranges of the countries you want to keep out. nftables does the filtering; `fgw`
owns one chain and the sets in it, and touches nothing else. No daemon, no web UI.

Principles: simple, fast to use, seamless with nftables, dynamic enough, light on
RAM. No resident process; the kernel holds the ranges, the binary runs and exits.

**Status: v0.1.1, first working version.** The full design with
rationale, nftables integration, config format, safety analysis and decision log
is in [docs/DESIGN.md](docs/DESIGN.md).

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

Configuration lives in `/etc/firegeowall/config.yml`: the country list, and the
sources the range lists are fetched from, in priority order. Ansible and the CLI
edit the same file.

## Build and test

No toolchain on the server: everything builds in a throwaway container.

```
podman build -t fgw-build contrib/build
podman run --rm -v .:/src:Z fgw-build cargo build --release      # target/release/fgw, static musl, ~3 MB
podman run --rm -v .:/src:Z fgw-build cargo test
podman run --rm --cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN \
    -v .:/src:Z fgw-build sh -c 'cargo build && sh tests/netns.sh'  # real nftables, real packets
```

The integration test runs in the container's own network namespace and never
touches the host firewall.

## Install

### Debian / Ubuntu package

Each release has a `firegeowall_<version>_amd64.deb` attached (static binary, runs
on any amd64 Debian or Ubuntu with systemd and nftables):

```
apt install ./firegeowall_0.1.1-1_amd64.deb
```

The package installs `/usr/sbin/fgw`, the systemd units, the service user `fgw` and
an empty `/etc/firegeowall/config.yml`, and **enables nothing**: you first add the
hook chain to nftables. The steps are in `/usr/share/doc/firegeowall/README.Debian`.
Upgrades keep your config and your enabled/disabled units; `apt purge` removes
config, cache and state.

Build the package yourself: `cargo deb --no-build` in the build container (after the
release build above); the result lands in `target/debian/`.

### By hand

```
install -m 0755 target/release/fgw /usr/sbin/fgw
install -d /etc/firegeowall && install -m 0644 contrib/config.yml /etc/firegeowall/
install -m 0644 contrib/nftables/50-firegeowall.conf /etc/nftables.d/   # adjust the interface
install -m 0644 contrib/systemd/fgw-* /etc/systemd/system/
install -m 0644 contrib/systemd/sysusers-fgw.conf /etc/sysusers.d/fgw.conf
systemd-sysusers && systemctl daemon-reload
nft -f /etc/nftables.d/50-firegeowall.conf                             # or reload nftables
systemctl start fgw-update && systemctl enable --now fgw-restore fgw-update.timer
fgw check && fgw status
```

The units call `/usr/sbin/fgw`. Note that `contrib/config.yml` ships an example
list (CN RU KP IR BY VN); the Debian package starts with an empty one.

## Non-goals

- Linux with nftables only. No iptables, no ipset, no other operating systems
- No allow-list mode (default-deny would lock you out; may come later as a second set)
- No web dashboard
- Nothing outside its own chain and sets; your hook chain and policy stay yours

## License

Dual-licensed under MIT or Apache-2.0, at your option.
