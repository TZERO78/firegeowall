#!/bin/sh
# firegeowall installer for Debian and Ubuntu (amd64).
#
# Adds the signed firegeowall APT repository and installs (or upgrades) the
# package. Afterwards new releases arrive with `apt update && apt upgrade`.
#
#   curl -fsSLO https://raw.githubusercontent.com/TZERO78/firegeowall/main/scripts/install.sh
#   less install.sh            # read it first
#   sudo sh install.sh         # install or upgrade
#   sudo sh install.sh --uninstall   # remove package, repository and key (config kept)
#
# What it touches:
#   /etc/apt/keyrings/firegeowall.gpg               repository key (pinned by SHA-256 below)
#   /etc/apt/sources.list.d/firegeowall.sources     repository entry
#   the package firegeowall                         enables nothing on its own
#
# It does not touch nftables, does not enable any unit and does not choose
# countries. Those steps are yours: docs/INSTALL.md, "First setup".
set -eu

REPO_URL=${FGW_REPO_URL:-https://tzero78.github.io/firegeowall}
KEY_SHA256=f890ad1b747e99b9731484bef3b6e285e0a4acf571eaf614226fc3c80fd098e2
KEY_FPR="CF41 7369 2FFE BB3F 8BFA  9BD8 41E5 BAAD 4FCB A55B"
KEYRING=/etc/apt/keyrings/firegeowall.gpg
SOURCES=/etc/apt/sources.list.d/firegeowall.sources

say() { printf 'firegeowall: %s\n' "$*"; }
die() { printf 'firegeowall: ERROR: %s\n' "$*" >&2; exit 1; }

fetch() { # fetch URL FILE
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https,file' --tlsv1.2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "need curl or wget"
    fi
}

[ "$(id -u)" = 0 ] || die "run as root (sudo sh $0)"
command -v apt-get >/dev/null 2>&1 && command -v dpkg >/dev/null 2>&1 \
    || die "this installer is for Debian/Ubuntu (apt + dpkg)"

if [ "${1:-}" = "--uninstall" ]; then
    if dpkg -s firegeowall >/dev/null 2>&1; then
        apt-get remove -y firegeowall
    fi
    rm -f "$SOURCES" "$KEYRING"
    apt-get update -qq || true
    say "removed package, repository and key."
    say "kept: /etc/firegeowall, /var/lib/firegeowall, user fgw (apt purge firegeowall removes the first two)."
    say "the sets stay in the running ruleset until nftables is reloaded."
    exit 0
elif [ -n "${1:-}" ]; then
    die "unknown option: $1 (only --uninstall)"
fi

ARCH=$(dpkg --print-architecture)
[ "$ARCH" = amd64 ] || die "only amd64 packages are published (this is $ARCH); build from source, see docs/INSTALL.md"
command -v systemctl >/dev/null 2>&1 || die "systemd is required"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "fetching repository key"
fetch "$REPO_URL/firegeowall-archive-keyring.gpg" "$TMP/key.gpg"
GOT=$(sha256sum "$TMP/key.gpg" | cut -d' ' -f1)
[ "$GOT" = "$KEY_SHA256" ] || die "repository key does not match the pinned one (got $GOT). Not installing."
say "key ok: $KEY_FPR"

install -d -m 0755 /etc/apt/keyrings
# 0644: apt verifies signatures as the unprivileged user _apt.
install -m 0644 "$TMP/key.gpg" "$KEYRING"
cat > "$TMP/sources" <<EOF
# firegeowall APT repository, written by scripts/install.sh
# Key fingerprint: $KEY_FPR
Types: deb
URIs: $REPO_URL
Suites: stable
Components: main
Architectures: amd64
Signed-By: $KEYRING
EOF
install -m 0644 "$TMP/sources" "$SOURCES"

say "apt update"
apt-get update -qq

if dpkg -s firegeowall >/dev/null 2>&1; then
    OLD=$(dpkg-query -W -f='${Version}' firegeowall)
    DEBIAN_FRONTEND=noninteractive apt-get install -y --only-upgrade firegeowall
    NEW=$(dpkg-query -W -f='${Version}' firegeowall)
    say "firegeowall $OLD -> $NEW (configuration and enabled units unchanged)"
else
    DEBIAN_FRONTEND=noninteractive apt-get install -y firegeowall
    say "installed $(dpkg-query -W -f='${Version}' firegeowall). Nothing is enabled yet."
    say "next: hook chain, countries, units -> /usr/share/doc/firegeowall/README.Debian"
fi
say "future releases: apt update && apt upgrade"
