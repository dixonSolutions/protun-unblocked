#!/usr/bin/env bash
#
# Tests for the Tailscale dispatcher hook (docs/tailscale.md).
#
# Nothing here touches the real system. A fake `ip` on PATH answers route
# queries from TUNNEL=up|down and keeps "installed" rules in a file, so each
# case can assert on the rule set the hook leaves behind.
#
# Run via tests/run-tests.sh, or directly:  tests/test-tailscale.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
HOOK="$REPO/system/80-pvpn-tailscale"

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

# --- fake ip -----------------------------------------------------------
#
# Route output is copied from a real machine with protun-tls up: Proton's
# table carries the tunnel default and, behind it, the leak guard's
# unreachable default.

BIN="$WORK/bin"
mkdir -p "$BIN"
cat > "$BIN/ip" <<'EOF'
#!/bin/sh
fam="$1"; shift
case "$1 $2" in
  "route show")
      if [ "${TUNNEL:-down}" = up ]; then
          echo "default dev proton0 table ${TABLE:-245447468} proto static metric 50 "
          echo "unreachable default table ${TABLE:-245447468} proto static metric 100 "
      fi
      echo "default via 10.177.200.1 dev wlp0s20f3 proto dhcp src 10.177.200.58 metric 600 "
      echo "default dev tailscale0 table 52 "
      exit 0 ;;
  "rule del")
      # Remove one matching rule, fail when none is left (like the real one).
      line="$(grep -n "^$fam pref $4 " "$RULES" | head -1 | cut -d: -f1)"
      [ -n "$line" ] || exit 2
      sed -i "${line}d" "$RULES"; exit 0 ;;
  "rule add")
      shift 2; echo "$fam $*" >> "$RULES"; exit 0 ;;
esac
exit 0
EOF
chmod +x "$BIN/ip"

RULES="$WORK/rules"; export RULES

run_hook() {
    PATH="$BIN:$PATH" timeout 5 sh "$HOOK" "$1" "$2" >/dev/null 2>&1
    echo "exit=$?"
}

WANT_UP="-4 pref 5200 fwmark 0x80000/0xff0000 lookup 245447468
-6 pref 5200 fwmark 0x80000/0xff0000 lookup 245447468"

# --- cases -------------------------------------------------------------

: > "$RULES"
assert_eq "tunnel up: exits 0" "exit=0" "$(TUNNEL=up run_hook proton0 up)"
assert_eq "tunnel up: Tailscale's mark goes to Proton's table, v4 and v6" \
    "$WANT_UP" "$(cat "$RULES")"

TUNNEL=up run_hook wlp0s20f3 dhcp4-change >/dev/null
assert_eq "repeated events do not stack rules" "$WANT_UP" "$(cat "$RULES")"

TUNNEL=down run_hook proton0 down >/dev/null
assert_eq "tunnel down: rules removed (no pointing at the leak guard)" \
    "" "$(cat "$RULES")"

# A missed `down` must heal on whatever event comes next.
printf '%s\n' "$WANT_UP" > "$RULES"
TUNNEL=down run_hook wlp0s20f3 up >/dev/null
assert_eq "stale rule from a missed down is cleared by the next event" \
    "" "$(cat "$RULES")"

# Proton renumbers its table: the rule follows it, the old one goes.
printf '%s\n' "$WANT_UP" > "$RULES"
TUNNEL=up TABLE=777 run_hook proton0 up >/dev/null
assert_eq "follows the table Proton actually uses" \
    "-4 pref 5200 fwmark 0x80000/0xff0000 lookup 777
-6 pref 5200 fwmark 0x80000/0xff0000 lookup 777" "$(cat "$RULES")"

for act in pre-up pre-down hostname reapply; do
    : > "$RULES"
    TUNNEL=up run_hook proton0 "$act" >/dev/null
    assert_eq "ignores action '$act'" "" "$(cat "$RULES")"
done

# Tailscale installs its bypass at 5210; ours has to be read first.
pref="$(sed -n 's/^PREF=//p' "$HOOK")"
if (( pref < 5210 )); then
    pass "rule sits ahead of tailscaled's 5210"
else
    fail "rule sits ahead of tailscaled's 5210" "PREF=$pref"
fi

printf '\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
