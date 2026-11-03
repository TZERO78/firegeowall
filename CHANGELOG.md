# Changelog

All notable changes to firegeowall (`fgw`). Format:
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[Semantic Versioning](https://semver.org/). The section of a release is also
its GitHub release notes.

## [Unreleased]

## [0.1.2] - 2026-11-03

### Added

- Ansible role `contrib/ansible/roles/firegeowall`: installs the package from
  the signed APT repository (or a local `.deb`), writes the config and the hook
  chain (from `fgw check --print-nft`), adds the `/etc/nftables.d` include,
  fills the sets, enables boot restore and the weekly timer, ends with
  `fgw check`. Idempotent; tested on a fresh Debian 13 VM.
- Signed APT repository on GitHub Pages, `scripts/install.sh` (pins the
  repository key by SHA-256), `docs/INSTALL.md`, `docs/USAGE.md`,
  `docs/RELEASING.md`.
- This changelog, also shipped in the package under `/usr/share/doc/firegeowall/`.

### Fixed

- After `systemctl stop nftables` and a later `start`, the sets stayed empty
  until the next reboot or timer run: `PartOf=` only passes on restarts.
  `fgw-restore.service` is now also `WantedBy=nftables.service`. The package
  upgrade re-creates the unit links if the unit was enabled; nothing to do by
  hand.
- `fgw-restore.service` no longer has `Requisite=nftables.service`. Together
  with the new `WantedBy=` it made any service start bring a stopped nftables
  back up, and its `flush ruleset` wipes rules other tools loaded. Without the
  table fgw fails and changes nothing, so `After=` is enough.

## [0.1.1] - 2026-11-03

### Added

- Debian package (cargo-deb): `/usr/sbin/fgw`, units in
  `/usr/lib/systemd/system`, the `fgw` user via sysusers.d. The package
  enables nothing by itself.
- `fgw check` and `fgw status` fail when the hook chain is not defined in the
  ruleset file nftables.service loads (`/etc/nftables.conf`, the Fedora and
  Alpine paths, following includes and globs). Without it the next nftables
  reload drops the chain and nothing is filtered. `FGW_NFT_CONF` overrides
  the path.

### Changed

- The units call `/usr/sbin/fgw` instead of `/usr/local/sbin/fgw`.
- The shortest accepted IPv4 prefix is /4 instead of /8. The aggregated US
  list has `16.0.0.0/6` and `6.0.0.0/7`, so US could neither be added nor
  updated. /0 to /3 are still rejected.

### Fixed

- The units no longer start `nftables.service`. With `Requires=`, a weekly
  timer run on a host with nftables inactive started it, and its
  `flush ruleset` wiped every other rule (Docker, ufw, ...).
- `fgw add` with a db-ip source always reported "no list": the CSV filter
  only kept countries already in the config.
- A first `sudo fgw add` left a root-owned lock file, and the timer, running
  as `fgw`, then failed with "Permission denied". The lock file now follows
  the state directory owner like every other file.
- Install instructions add the `include "/etc/nftables.d/*.conf"` line that
  Debian's `/etc/nftables.conf` lacks.

## [0.1.0] - 2026-11-03

First working version.

- Seven commands: `add`, `remove`, `list`, `status`, `show`, `check`,
  `update`; `apply --from-cache` for the restore unit.
- One nftables set and one counted drop rule per country and family, in a
  chain fgw owns; your hook chain jumps into it. IPv4 and IPv6 alike.
- Atomic rebuilds, validated with `nft -c` first. Failed downloads, lists
  that shrink by more than `max_shrink_percent` and implausibly large ranges
  keep the previous sets. Fail open, never closed.
- Boot restore from a local cache, weekly refresh by a systemd timer; both
  units run as the unprivileged user `fgw` with only `CAP_NET_ADMIN`.
- `fgw add` refuses the country of the current SSH session without `--force`.
- Config edits keep comments; ISO 3166 check, so `UK` fails (it is `GB`).

[Unreleased]: https://github.com/TZERO78/firegeowall/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/TZERO78/firegeowall/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/TZERO78/firegeowall/releases/tag/v0.1.1
[0.1.0]: https://github.com/TZERO78/firegeowall/releases/tag/v0.1.0
