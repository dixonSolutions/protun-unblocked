#!/usr/bin/env bash
#
# Tests for the always-on suspend/resume recovery (docs/always-on.md).
#
# Nothing here touches the real system. The scripts under test shell out to
# ip / resolvectl / nmcli / systemctl, so each case puts fakes on PATH and
# asserts on what would have been called. No connect, no disconnect, no
# routing or DNS change, and nothing needs root or a tunnel.
#
# Run via tests/run-tests.sh, or directly:  tests/test-always-on.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
    G=$'\e[32m'; R=$'\e[31m'; X=$'\e[0m'
else
    G=''; R=''; X=''
fi

pass() { PASS=$((PASS + 1)); printf '  %sok%s   %s\n' "$G" "$X" "$1"; }
fail() {
    FAIL=$((FAIL + 1))
    printf '  %sFAIL%s %s\n' "$R" "$X" "$1"
    [[ -n "${2:-}" ]] && printf '       %s\n' "$2"
}

assert_eq() {
    local label="$1" want="$2" got="$3"
    if [[ "$want" == "$got" ]]; then pass "$label"
    else fail "$label" "want [$want], got [$got]"; fi
}

# --- fakes -------------------------------------------------------------
#
# TUNNEL=up|down decides what the fake `ip` reports, which is the single
# fact every one of these scripts branches on.

make_fakes() {
    local bin="$1"
    mkdir -p "$bin"

    cat > "$bin/ip" <<'EOF'
#!/bin/sh
case "$1 $2" in
  "route get")
      [ "${TUNNEL:-down}" = up ] && \
          echo "1.1.1.1 dev proton0 table 245447468 src 10.2.0.2" && exit 0
      echo "1.1.1.1 via 10.0.0.1 dev wlan0 src 10.0.0.5"; exit 0 ;;
  "route show")
      echo "default via 10.0.0.1 dev wlan0 proto dhcp metric 600"
      [ "${TUNNEL:-down}" = up ] && echo "default dev proton0 scope link"
      exit 0 ;;
  "link show")
      case "${LINKS:-}" in *"$3"*) exit 0 ;; *) exit 1 ;; esac ;;
esac
exit 0
EOF

    # Each fake appends its argv to a log the test then asserts on.
    for tool in resolvectl nmcli systemctl runuser loginctl flock pvpn; do
        cat > "$bin/$tool" <<EOF
#!/bin/sh
echo "$tool \$*" >> "\$CALLS"
exit 0
EOF
    done
    chmod +x "$bin"/*
}

BIN="$WORK/bin"
make_fakes "$BIN"

# --- pvpn-dns-unsnap ---------------------------------------------------

printf '\n  -- pvpn-dns-unsnap --\n'

run_unsnap() {
    CALLS="$WORK/calls.$$"; export CALLS; : > "$CALLS"
    TUNNEL="$1" LINKS="${2:-}" PATH="$BIN:$PATH" \
        sh "$REPO/system/pvpn-dns-unsnap" >/dev/null 2>&1
    echo "exit=$?"
    cat "$CALLS"
}

out="$(run_unsnap up "ipv6leakintrf0 proton0")"
assert_eq "tunnel up: exits 0" "exit=0" "$(echo "$out" | head -1)"
if [[ "$out" == *"resolvectl"* ]]; then
    fail "tunnel up: reverts nothing" "it called resolvectl: $out"
else
    pass "tunnel up: reverts nothing"
fi

out="$(run_unsnap down "ipv6leakintrf0 proton0")"
if [[ "$out" == *"resolvectl revert ipv6leakintrf0"* ]]; then
    pass "tunnel down: withdraws the leak guard's ~. claim"
else
    fail "tunnel down: withdraws the leak guard's ~. claim" "$out"
fi
if [[ "$out" == *"nmcli general reload dns-full"* ]]; then
    pass "tunnel down: re-publishes the physical link's DNS"
else
    fail "tunnel down: re-publishes the physical link's DNS" "$out"
fi

# A machine where the leak interface never existed must not be nudged.
out="$(run_unsnap down "")"
if [[ "$out" == *"nmcli"* ]]; then
    fail "no leak interface: no NM reload" "$out"
else
    pass "no leak interface: no NM reload"
fi

# --- the dispatcher ----------------------------------------------------

printf '\n  -- 90-pvpn-autoconnect (NetworkManager dispatcher) --\n'

run_dispatch() {
    CALLS="$WORK/calls.d"; export CALLS; : > "$CALLS"
    PATH="$BIN:$PATH" sh "$REPO/system/90-pvpn-autoconnect" "$1" "$2" >/dev/null 2>&1
    cat "$CALLS"
}

# It must never call pvpn: NetworkManager kills long dispatcher scripts, and
# a SIGKILL mid-connect strands the kill switch.
if grep -qE '(^|[^-])\bpvpn\b *(up|down|hop)' "$REPO/system/90-pvpn-autoconnect"; then
    fail "dispatcher never invokes pvpn directly" "found a pvpn call"
else
    pass "dispatcher never invokes pvpn directly"
fi

out="$(run_dispatch wlan0 up)"
assert_eq "physical link up: starts the unit" \
    "systemctl start --no-block pvpn-recover.service" "$out"

for ifc in proton0 ipv6leakintrf0 pvpnksintrf0 tailscale0 docker0 veth123 lo wg0; do
    out="$(run_dispatch "$ifc" up)"
    assert_eq "ignores $ifc (would retrigger itself)" "" "$out"
done

for act in down vpn-up vpn-down connectivity-change pre-up; do
    out="$(run_dispatch wlan0 "$act")"
    assert_eq "ignores action '$act'" "" "$out"
done

# --- pvpn-autoconnect --------------------------------------------------

printf '\n  -- pvpn-autoconnect --\n'

AC="$REPO/bin/pvpn-autoconnect"
CFG="$WORK/config"

DATA="$WORK/data"
mkdir -p "$DATA/pvpn"
run_ac() {
    XDG_CONFIG_HOME="$CFG" XDG_DATA_HOME="$DATA" PATH="$BIN:$PATH" "$AC" "$@" 2>&1
}
# One attempt, no backoff: these cases assert on *whether* pvpn ran, and the
# real 20s/40s sleeps between retries would make the suite unusable.
run_ac_connect() {
    CALLS="$1"; export CALLS; : > "$CALLS"
    XDG_CONFIG_HOME="$CFG" XDG_DATA_HOME="$DATA" XDG_RUNTIME_DIR="$WORK" \
        TUNNEL="${2:-down}" PVPN_AUTOCONNECT_ATTEMPTS=1 PATH="$BIN:$PATH" \
        "$AC" >/dev/null 2>&1
}

assert_eq "default is on" "on" "$(run_ac --status | awk '{print $1}')"
run_ac --off >/dev/null
assert_eq "--off is remembered" "off" "$(run_ac --status | awk '{print $1}')"
if [[ -e "$CFG/pvpn/autoconnect-off" ]]; then
    pass "--off leaves an inspectable marker"
else
    fail "--off leaves an inspectable marker" "no $CFG/pvpn/autoconnect-off"
fi

# The point of the marker: a deliberate `pvpn down` is not undone.
run_ac_connect "$WORK/calls.ac" down
assert_eq "while off: attempts nothing" "" "$(cat "$WORK/calls.ac")"

run_ac --on >/dev/null
assert_eq "--on restores it" "on" "$(run_ac --status | awk '{print $1}')"

# The positive case, which also proves the fake pvpn above is the one being
# reached -- otherwise the "attempts nothing" test could pass vacuously.
run_ac_connect "$WORK/calls.ac2" down
if grep -q '^pvpn up$' "$WORK/calls.ac2"; then
    pass "while on and no tunnel: runs pvpn up"
else
    fail "while on and no tunnel: runs pvpn up" "calls: $(cat "$WORK/calls.ac2")"
fi

# And the guard that matters most: a live tunnel is never disturbed.
run_ac_connect "$WORK/calls.ac3" up
# Taking the lock is expected; touching the tunnel is not.
if grep -q '^pvpn ' "$WORK/calls.ac3"; then
    fail "while a tunnel is up: never runs pvpn" "calls: $(cat "$WORK/calls.ac3")"
else
    pass "while a tunnel is up: never runs pvpn"
fi

out="$(run_ac --nonsense 2>&1)"; rc=$?
assert_eq "rejects unknown options" "1" "$rc"

printf '\n  -- honouring a deliberate pvpn down --\n'

# The marker pvpn down writes. Lock the laptop after turning the VPN off and
# the resume must not turn it back on.
touch "$DATA/pvpn/down-by-user"
run_ac_connect "$WORK/calls.down" down
assert_eq "after pvpn down: a resume leaves it down" "" "$(cat "$WORK/calls.down")"
if [[ "$(run_ac --status)" == *"pvpn down"* ]]; then
    pass "--status explains the hold"
else
    fail "--status explains the hold" "$(run_ac --status)"
fi

# ...and pvpn up releases it. (pvpn clears the marker itself; this asserts the
# script reconnects again once it is gone.)
rm -f "$DATA/pvpn/down-by-user"
run_ac_connect "$WORK/calls.up" down
if grep -q '^pvpn up$' "$WORK/calls.up"; then
    pass "after pvpn up: resume reconnects again"
else
    fail "after pvpn up: resume reconnects again" "calls: $(cat "$WORK/calls.up")"
fi

# The two markers are separate facts: releasing the down-hold must not
# silently re-enable autoconnect for someone who ran --off.
run_ac --off >/dev/null
run_ac_connect "$WORK/calls.both" down
assert_eq "--off still wins with no down-marker" "" "$(cat "$WORK/calls.both")"
run_ac --on >/dev/null

# --- shipped unit files ------------------------------------------------

printf '\n  -- unit files --\n'

for unit in system/pvpn-recover.service system/pvpn-autoconnect.user.service; do
    if grep -q '^\[Service\]' "$REPO/$unit" && grep -q '^Type=oneshot' "$REPO/$unit"; then
        pass "$unit is a oneshot service"
    else
        fail "$unit is a oneshot service"
    fi
done

# A Restart= here would relaunch pvpn while a previous connect was still
# finishing, and overlapping connects strand the kill switch.
if grep -q '^Restart=' "$REPO/system/pvpn-autoconnect.user.service"; then
    fail "user unit has no Restart=" "found one"
else
    pass "user unit has no Restart="
fi

# It must not be enableable on its own — only pvpn-recover.service starts it.
if grep -q '^\[Install\]' "$REPO/system/pvpn-autoconnect.user.service"; then
    fail "user unit has no [Install]" "found one; it could self-start"
else
    pass "user unit has no [Install]"
fi

if grep -q 'WantedBy=.*suspend.target' "$REPO/system/pvpn-recover.service"; then
    pass "recover unit hooks the resume path"
else
    fail "recover unit hooks the resume path"
fi

# DNS must be repaired before the reconnect: the connect needs to resolve
# Proton's API.
order="$(grep -n '^ExecStart=' "$REPO/system/pvpn-recover.service" | head -2)"
if [[ "$(echo "$order" | head -1)" == *dns-unsnap* \
   && "$(echo "$order" | tail -1)" == *kick-user* ]]; then
    pass "recover unit repairs DNS before reconnecting"
else
    fail "recover unit repairs DNS before reconnecting" "$order"
fi

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
