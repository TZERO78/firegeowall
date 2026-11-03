#!/bin/sh
# Integration test against real nftables, inside a throwaway container:
#   podman run --rm --cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN -v .:/src:Z fgw-build sh tests/netns.sh
# Builds a veth pair to a second network namespace (the peer) that plays the
# outside world, and checks that packets from blocked ranges are dropped.
set -eu
FGW=${FGW:-/src/target/debug/fgw}
T=$(mktemp -d)
pass=0
ok()   { pass=$((pass+1)); echo "ok   $*"; }
fail() { echo "FAIL $*"; exit 1; }

# --- network: host side veth0 10.99.0.1, peer side 1.0.1.5 (listed) + 9.9.9.9 (not)
unshare --net sleep 600 &
PEER=$!
sleep 0.3
trap 'kill $PEER 2>/dev/null; rm -rf $T' EXIT
pe() { nsenter -t "$PEER" -n "$@"; }
ip link add veth0 type veth peer name veth1
ip link set veth1 netns "$PEER"
ip addr add 10.99.0.1/24 dev veth0
ip addr add 1.0.1.1/24 dev veth0
ip link set veth0 up
pe ip link set lo up
pe ip addr add 10.99.0.2/24 dev veth1
pe ip addr add 1.0.1.5/24 dev veth1
pe ip link set veth1 up
sleep 1

# --- ruleset: base filter table + the contrib hook chain, interface veth0
nft add table inet filter
sed 's/"ens6"/"veth0"/' /src/contrib/nftables/50-firegeowall.conf > $T/50.conf
nft -f $T/50.conf

# --- config with a local source only (no network needed)
mkdir -p $T/custom
printf '1.0.1.0/24\n240e::/18\n' > $T/custom/cn.zone
printf '5.5.0.0/16\n' > $T/custom/ru.zone
printf '175.45.176.0/22\n' > $T/custom/kp.zone
cat > $T/config.yml <<YML
# test config
interface: veth0
countries:
  - KP   # keep me
sources:
  - name: local
    priority: 1
    path: $T/custom/{cc}.zone
    format: zone
state_dir: $T/state
YML
F="$FGW --config $T/config.yml"

reach() { pe ping -c1 -W1 -I "$1" 10.99.0.1 >/dev/null 2>&1; }

reach 1.0.1.5 || fail "baseline: peer cannot reach host"
ok "baseline reachable"

$F apply --from-cache 2>&1 | grep -q "no cache for KP" && ok "apply without cache reports missing KP" || fail "apply without cache"
$F add cn ru >/dev/null || fail "add"
grep -q "^  - KP   # keep me$" $T/config.yml && grep -q "^  - CN$" $T/config.yml && grep -q "^  - RU$" $T/config.yml \
  && ok "add edited only the countries block, comment kept" || { cat $T/config.yml; fail "config edit"; }
nft list chain inet filter geo_block | grep -q 'ip saddr @geo_cn counter packets 0 bytes 0 drop comment "CN"' \
  && ok "rule per country with counter" || { nft list chain inet filter geo_block; fail "rules"; }
nft list chain inet filter geo_block | grep -q 'ip6 saddr @geo6_cn' && ok "IPv6 rule for CN" || fail "ip6 rule"
nft list chain inet filter geo_block | grep -q 'geo6_kp' && fail "empty v6 list must not get a set" || ok "no set for empty KP v6"

reach 1.0.1.5 && fail "blocked source still reaches host" || ok "CN source dropped"
reach 10.99.0.2 && ok "unlisted source still reaches host" || fail "unlisted source blocked"
nft list chain inet filter geo_block | grep 'comment "CN"' | grep -q 'packets [1-9]' && ok "CN counter counts" || fail "counter"

$F check >/dev/null && ok "check passes" || { $F check; fail "check"; }
$F status | grep -q "Health OK" && ok "status health OK" || { $F status; fail "status"; }
$F list | grep -q "^CN" && ok "list" || fail "list"
[ "$($F show CN | head -1)" = "1.0.1.0/24" ] && ok "show" || fail "show"
$F --json status | grep -q '"health": "OK"' && ok "json status" || fail "json"

# self-protection
SSH_CONNECTION="5.5.1.1 5000 10.99.0.1 22" $F remove RU >/dev/null
SSH_CONNECTION="5.5.1.1 5000 10.99.0.1 22" $F add RU 2>&1 | grep -q "refusing to block RU" && ok "own country refused" || fail "guard"
grep -q "RU" $T/config.yml && fail "refused add changed config" || ok "refused add left config alone"
SSH_CONNECTION="5.5.1.1 5000 10.99.0.1 22" $F add RU --force >/dev/null && ok "--force overrides" || fail "force"

# typos fail loudly
$F add UK 2>&1 | grep -q "not an ISO" && ok "UK rejected" || fail "iso"

# remove: set deleted in the same transaction
$F remove CN >/dev/null
nft list sets | grep -q geo_cn && fail "geo_cn still there" || ok "remove deletes set"
reach 1.0.1.5 && ok "CN source reachable again" || fail "unblock"

# counters survive a rebuild via state.json
$F add CN >/dev/null; reach 1.0.1.5 || true; $F remove KP >/dev/null
$F --json status | grep -A3 '"country": "CN"' | grep -q '"dropped_v4": 2' && ok "counters banked across rebuild (1 before remove + 1 after re-add)" || { $F --json status; fail "bank"; }

# the classic: flush ruleset (nftables reload) empties everything, restore refills
nft flush ruleset; nft add table inet filter; nft -f $T/50.conf
reach 1.0.1.5 && ok "after reload: open (fail-open)" || fail "fail-open"
$F check >/dev/null && fail "check must fail after reload" || ok "check detects empty chain"
$F apply --from-cache >/dev/null && ok "restore from cache" || fail "restore"
reach 1.0.1.5 && fail "still open after restore" || ok "blocked again after restore"

# a broken list never reaches nftables
cp $T/custom/cn.zone $T/cn.bak; echo "0.0.0.0/0" >> $T/custom/cn.zone
$F update 2>&1 | grep -q "implausibly large" && ok "update rejects /0" || fail "update /0"
reach 1.0.1.5 && fail "live sets changed after failed update" || ok "live sets kept after failed update"
$F status | grep -q "WARN" && ok "status shows degraded update" || fail "degraded"
cp $T/cn.bak $T/custom/cn.zone; $F update >/dev/null && ok "good update clears it" || fail "update"
$F check | grep -q WARN && fail "warning not cleared" || ok "no warnings left"

# real lists go below /8: the aggregated US list has 16.0.0.0/6 and 6.0.0.0/7
printf '16.0.0.0/6\n6.0.0.0/7\n' > $T/custom/us.zone
$F add US >/dev/null && ok "large real ranges (US /6, /7) accepted" || fail "add US"
nft list set inet filter geo_us | grep -q '16.0.0.0/6' && ok "/6 in the set" || fail "geo_us"
$F remove US >/dev/null

# empty list = nothing blocked
$F remove CN RU >/dev/null
grep -q "^countries: \[\]$" $T/config.yml && ok "empty list written as []" || fail "empty list"
[ -z "$(nft list chain inet filter geo_block | grep drop)" ] && ok "chain empty" || fail "chain not empty"

# real network: ipdeny, if reachable
if [ "${NET:-0}" = 1 ]; then
  sed -i '/^sources:/,/format: zone/d' $T/config.yml
  $F add KP BY >/dev/null && ok "ipdeny fetch (KP BY)" || fail "ipdeny"
  $F list
fi
echo "$pass checks passed"
$F check --print-nft | head -3
