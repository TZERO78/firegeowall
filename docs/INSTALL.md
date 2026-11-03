# Installing firegeowall

Four ways in, from easiest to most manual. All of them end at the same
[first setup](#first-setup), because the package deliberately enables nothing:
the nftables hook chain belongs to you, not to a package script.

- [1. Install script (recommended)](#1-install-script-recommended)
- [2. APT repository by hand](#2-apt-repository-by-hand)
- [3. Single .deb file](#3-single-deb-file)
- [4. From source](#4-from-source)
- [5. Ansible](#5-ansible)
- [First setup](#first-setup)
- [Updates](#updates)
- [Uninstall](#uninstall)

Requirements: Linux with nftables and systemd. Packages are published for
**amd64** (static binary, any Debian 12+/Ubuntu 22.04+ should work; tested on
Debian 13). Other architectures: build from source.

## 1. Install script (recommended)

[`scripts/install.sh`](../scripts/install.sh) adds the signed APT repository and
installs the package. Run it again any time to upgrade.

```sh
curl -fsSLO https://raw.githubusercontent.com/TZERO78/firegeowall/main/scripts/install.sh
less install.sh        # read before running anything as root
sudo sh install.sh
```

What it does, and nothing more:

1. Downloads the repository key and compares its SHA-256 with the value pinned
   in the script. A different key aborts the install before anything is written.
2. Writes `/etc/apt/keyrings/firegeowall.gpg` and
   `/etc/apt/sources.list.d/firegeowall.sources` (both mode 0644).
3. `apt-get update`, then installs `firegeowall`, or upgrades it if it is
   already installed.

It does not touch nftables, enable units or choose countries.

## 2. APT repository by hand

The same steps as the script, if you prefer to type them:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://tzero78.github.io/firegeowall/firegeowall-archive-keyring.gpg -o /tmp/fgw.gpg
gpg --show-keys /tmp/fgw.gpg
#   must show  CF41 7369 2FFE BB3F 8BFA  9BD8 41E5 BAAD 4FCB A55B
sudo install -m 0644 /tmp/fgw.gpg /etc/apt/keyrings/firegeowall.gpg
sudo curl -fsSL https://tzero78.github.io/firegeowall/firegeowall.sources \
    -o /etc/apt/sources.list.d/firegeowall.sources
sudo apt update && sudo apt install firegeowall
```

The keyring must be readable by everyone (0644): apt verifies signatures as the
unprivileged user `_apt`, and a 0600 or 0640 file makes `apt update` fail with
"repository is not signed".

The key is also in this repository: [`contrib/apt/`](../contrib/apt/). It is an
Ed25519 key used only for this repository, valid until 2029-11-02.

## 3. Single .deb file

Every [release](https://github.com/TZERO78/firegeowall/releases) has the package
and its checksum attached:

```sh
sha256sum -c firegeowall_0.1.2-1_amd64.deb.sha256
sudo apt install ./firegeowall_0.1.2-1_amd64.deb
```

Without the APT repository you get no updates through `apt upgrade`; install
the next .deb the same way.

What the package installs:

| Path | |
|---|---|
| `/usr/sbin/fgw` | static binary |
| `/usr/lib/systemd/system/fgw-restore.service` | refill sets from cache after boot / nftables reload |
| `/usr/lib/systemd/system/fgw-update.service`, `.timer` | weekly refresh (Sun 03:30 + up to 30 min) |
| `/usr/lib/sysusers.d/fgw.conf` | service user `fgw` |
| `/etc/firegeowall/config.yml` | created once from an empty template, never overwritten |
| `/usr/share/doc/firegeowall/` | README.Debian, examples (config, nftables snippet) |

## 4. From source

Everything builds in a throwaway container; no Rust toolchain on the host.

```sh
git clone https://github.com/TZERO78/firegeowall && cd firegeowall
podman build -t fgw-build contrib/build
podman run --rm -v .:/src:Z fgw-build cargo build --release --locked   # target/release/fgw
podman run --rm -v .:/src:Z fgw-build cargo deb --no-build --no-strip   # optional: target/debian/*.deb
```

Install by hand (without a package):

```sh
sudo install -m 0755 target/release/fgw /usr/sbin/fgw
sudo install -d /etc/firegeowall
sudo install -m 0644 contrib/deb/config.yml /etc/firegeowall/config.yml   # empty list
sudo install -m 0644 contrib/systemd/fgw-* /etc/systemd/system/
sudo install -m 0644 contrib/systemd/sysusers-fgw.conf /etc/sysusers.d/fgw.conf
sudo systemd-sysusers && sudo systemctl daemon-reload
```

Then continue with the first setup.

## 5. Ansible

The role in [`contrib/ansible/roles/firegeowall`](../contrib/ansible/roles/firegeowall/)
does the install and the whole first setup below in one run: APT repository
(key checked against the fingerprint above), package, config, hook chain from
`fgw check --print-nft`, the `/etc/nftables.d` include, first fill, units, and a
final `fgw check`. A second run changes nothing.

```yaml
- hosts: firewalls
  become: true
  roles:
    - role: firegeowall
      fgw_countries: [CN, RU, KP]   # Ansible owns the list
      # fgw_interface: ens6         # default: interface of the default route
      # fgw_install: deb            # a local package instead of the repository
      # fgw_deb_src: target/debian/firegeowall_0.1.2-1_amd64.deb
```

Ansible owns the country list: `fgw add` / `fgw remove` on the host are
reverted by the next run. Change `fgw_countries` instead.

## First setup

### Step 1: hook chain in nftables

fgw fills its own chain `geo_block`. Something has to jump into it: a small
prerouting chain that you own. The package ships an example:

```sh
sudo install -d /etc/nftables.d
sudo cp /usr/share/doc/firegeowall/examples/50-firegeowall.conf /etc/nftables.d/
sudo editor /etc/nftables.d/50-firegeowall.conf     # replace ens6 with your public interface
```

Find your public interface with `ip route show default` (the word after `dev`).
Only that interface is filtered; loopback, VPN and mesh interfaces stay untouched.

Debian's default `/etc/nftables.conf` does **not** read `/etc/nftables.d/`. Add
the include once, otherwise the next nftables reload drops the hook chain and
nothing is filtered (`fgw check` reports this as FAIL):

```sh
grep -q '^include "/etc/nftables.d/\*.conf"' /etc/nftables.conf \
    || echo 'include "/etc/nftables.d/*.conf"' | sudo tee -a /etc/nftables.conf
sudo nft -c -f /etc/nftables.conf && sudo systemctl enable --now nftables && sudo systemctl reload nftables
```

If your table or chain names differ, `fgw check --print-nft` prints the snippet
that matches your `config.yml`.

### Step 2: choose countries

```sh
sudo fgw add CN RU
```

Codes are ISO 3166-1 alpha-2, see [USAGE.md, country codes](USAGE.md#country-codes).
`fgw add` refuses the country your current SSH session comes from unless you pass
`--force`. Never block the country of anyone who must reach the server directly
(you while travelling, a friend's VPN client, monitoring).

### Step 3: enable the units

```sh
sudo systemctl enable --now fgw-restore.service fgw-update.timer
sudo fgw check
sudo fgw status
```

`fgw check` must end without FAIL. From now on the sets are restored at boot
from the local cache (no network needed) and refreshed weekly.

### Test from outside

Use an online port checker with probes in several countries (for example
check-host.net, "TCP port"). Probes from a blocked country time out, others
connect. `sudo fgw status` then shows the drops per country.

## Updates

With the APT repository (methods 1 and 2):

```sh
sudo apt update && sudo apt upgrade
```

Upgrades keep `config.yml`, the cache and your enabled/disabled units; the
running filter is not interrupted.

Automatic updates are off by default on purpose: fgw is part of your firewall,
so you may want to see each new version first. To let `unattended-upgrades`
install it anyway:

```
// /etc/apt/apt.conf.d/52unattended-upgrades-firegeowall
Unattended-Upgrade::Origins-Pattern { "origin=firegeowall,codename=stable"; };
```

## Uninstall

```sh
sudo sh install.sh --uninstall   # package, repository and key; config kept
sudo apt purge firegeowall       # also /etc/firegeowall and /var/lib/firegeowall
```

The user `fgw` is kept (Debian convention). The sets and the `geo_block` rules
stay in the running ruleset until nftables is reloaded; remove your hook chain
file from `/etc/nftables.d/` and reload to clean up completely.
