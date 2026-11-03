# Using firegeowall

- [Commands](#commands)
- [Country codes](#country-codes)
- [Configuration](#configuration)
- [What runs when](#what-runs-when)
- [Troubleshooting](#troubleshooting)

All commands that change or read nftables need root (`CAP_NET_ADMIN`), so run
them with `sudo`. Every command accepts `--json` for machine-readable output and
`--config FILE` (default `/etc/firegeowall/config.yml`).

## Commands

| Command | What it does |
|---|---|
| `fgw add CC [CC ...] [--force]` | Add countries to the block list, fetch their lists and apply at once |
| `fgw remove CC [CC ...]` | Remove countries and apply at once |
| `fgw list` | Show the configured countries |
| `fgw status` | Interface, health, last update, next timer run, ranges and drops per country |
| `fgw show CC` | Print the cached ranges of one country |
| `fgw check [--print-nft]` | Validate config, hook chain, persistence, owned chain, sets and cache |
| `fgw update` | Fetch fresh lists, validate them, refill the sets (the weekly timer runs this) |

Examples:

```sh
sudo fgw add CN RU KP          # block three countries
sudo fgw remove KP             # unblock one
sudo fgw status                # overview with drop counters
sudo fgw show RU | head        # first ranges of a country
sudo fgw check                 # everything in order? exit code 0 = yes
sudo fgw check --print-nft     # nftables snippet that matches this config
```

`fgw status` looks like this:

```
Interface  ens6 (hook geo_prerouting, priority -150)    Health OK
Updated    2026-11-03 16:52 UTC via ipdeny    next Sun 2026-11-08 03:44:10 CET

Country    v4 ranges   v6 ranges   dropped v4   dropped v6
CN             5,517       2,036            0            0
RU             8,664       2,167            0            0
Total         14,181       4,203            0            0
```

In nftables every country is one set and one counted rule, so the usual tools
show the same picture:

```sh
sudo nft list chain inet filter geo_block
#   ip saddr @geo_cn counter packets 12 bytes 720 drop comment "CN"
```

### Safety rails

- **Self-protection:** `fgw add` refuses the country of the SSH session you are
  connected from. `--force` overrides it; keep a second way in (console, VPN on
  another interface) before you use it.
- **Fail open:** if a download fails, a list is suspiciously small or shrank by
  more than `max_shrink_percent`, the previous sets stay. An empty country list
  filters nothing. No error ever leaves the server closed.
- **Atomic:** the chain and its sets are replaced in a single nftables
  transaction, checked with `nft -c` first.

## Country codes

fgw uses **ISO 3166-1 alpha-2** codes: two letters, case does not matter.

- Full list: [ISO 3166-1 alpha-2 on Wikipedia](https://en.wikipedia.org/wiki/ISO_3166-1_alpha-2)
- Official source: [ISO Online Browsing Platform](https://www.iso.org/obp/ui/#search/code/)
- Lists the default source actually offers: [ipdeny.com IP blocks](https://www.ipdeny.com/ipblocks/)

Unknown codes are rejected, so typos fail loudly. The classic one: the United
Kingdom is **GB**, not UK.

| Code | Country | Code | Country |
|---|---|---|---|
| CN | China | KP | North Korea |
| RU | Russia | IR | Iran |
| BY | Belarus | VN | Vietnam |
| US | United States | DE | Germany |
| GB | United Kingdom | NL | Netherlands |

IP-to-country data is never perfect: providers move ranges, VPNs and clouds
blur borders. Treat country blocking as noise reduction in front of your real
protection (strong authentication, CrowdSec or fail2ban, updates), not as a
replacement for it.

## Configuration

`/etc/firegeowall/config.yml`. Only `countries` is required; everything else has
a built-in default. `fgw add` / `fgw remove` edit only the `countries:` block and
keep your comments.

```yaml
# Public interface (shown by status/check). Default: interface of the default route.
interface: ens6

# ISO 3166-1 alpha-2 codes. Empty list = nothing is blocked.
countries:
  - CN
  - RU
```

The full reference (sources with priorities, db-ip and local lists, size limits,
nftables names) is in [DESIGN.md, section 5](DESIGN.md#5-configuration).

## What runs when

| When | What | Network |
|---|---|---|
| Boot, after `nftables.service` | `fgw-restore.service` refills the sets from `/var/lib/firegeowall/zones` | no |
| `systemctl reload/restart nftables` | `fgw-restore.service` is re-run (the reload empties the sets) | no |
| Sunday 03:30 (+ up to 30 min) | `fgw-update.timer` fetches fresh lists | yes |
| `fgw add` / `fgw remove` | config edited, lists fetched for new countries, applied | yes (add) |

The units run as the unprivileged user `fgw` with only `CAP_NET_ADMIN`.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `fgw check`: hook chain not persistent | The hook chain is not in a file `/etc/nftables.conf` loads. Add the `include "/etc/nftables.d/*.conf"` line, see [INSTALL.md](INSTALL.md#step-1-hook-chain-in-nftables) |
| `table inet filter does not exist` | nftables has no such table. Install the hook chain snippet and reload nftables |
| Warning: no hook chain jumps to `geo_block` | Sets are loaded but filter nothing. Check the snippet and your interface name |
| `fgw-restore.service` failed after boot | `nftables.service` was not active (the unit requires it but never starts it). `systemctl enable --now nftables` |
| All drop counters stay 0 | Possible and fine if an upstream firewall (hosting provider) already drops most traffic. Test from outside, see INSTALL.md |
| `apt update`: repository is not signed | `/etc/apt/keyrings/firegeowall.gpg` is not mode 0644 |

Logs: `journalctl -u fgw-update.service -u fgw-restore.service`.
