#!/usr/bin/env bash
#
# Tests for the `pvpn best` command line.
#
# These run pvpn as a real subprocess, so they cover argument parsing and
# dispatch the way a user meets them. Nothing here connects, disconnects,
# or touches routing: every case either asks for help, feeds pvpn a fixture
# server list, or is expected to be rejected.
#
# Run via tests/run-tests.sh, or directly:  tests/test-pvpn-cli.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
PVPN="$REPO/bin/pvpn"

FIXTURE_DIR="$(mktemp -d)"
FIXTURE="$FIXTURE_DIR/serverlist.json"
trap 'rm -rf "$FIXTURE_DIR"' EXIT

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

# Run pvpn against the fixture list so no test depends on the real cache,
# on being signed in, or on having network.
run_pvpn() {
    PVPN_SERVERLIST="$FIXTURE" "$PVPN" "$@" 2>&1
}

# Same, but keeps stderr out of the way, so a test can assert on exactly
# what a pipe would receive.
run_pvpn_stdout() {
    PVPN_SERVERLIST="$FIXTURE" "$PVPN" "$@" 2>/dev/null
}

assert_contains() {
    local label="$1" needle="$2" haystack="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        pass "$label"
    else
        fail "$label" "expected to find: $needle"
    fi
}

assert_not_contains() {
    local label="$1" needle="$2" haystack="$3"
    if [[ "$haystack" != *"$needle"* ]]; then
        pass "$label"
    else
        fail "$label" "did not expect to find: $needle"
    fi
}

assert_status() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        pass "$label"
    else
        fail "$label" "expected exit $expected, got $actual"
    fi
}

# A stand-in for flatpak, put on PATH so the Flatpak tests exercise the real
# audit-and-fix loop without touching the machine's own apps.
#
# It is backed by a state file of "APP VAR" lines, so an unset actually
# sticks and a second scan sees the difference. Unset variables are reported
# as VAR= rather than dropped, which is what flatpak itself does and the
# case most likely to be mistaken for a proxy that is still set.
FAKE_BIN="$FIXTURE_DIR/bin"
FAKE_STATE="$FIXTURE_DIR/flatpak-state"

write_fake_flatpak() {
    mkdir -p "$FAKE_BIN"
    cat > "$FAKE_BIN/flatpak" <<'STUB'
#!/usr/bin/env bash
state="${FAKE_FLATPAK_STATE:?}"

# Report VAR as set, unless a previous `override --unset-env` cleared it.
#
# FAKE_FLATPAK_STUBBORN names an app whose proxy survives the unset, the way
# one written into the app's own manifest does.
env_line() {
    local app="$1" var="$2" value="$3"
    if [[ "$app" == "${FAKE_FLATPAK_STUBBORN:-}" ]]; then
        printf '%s=%s\n' "$var" "$value"
    elif grep -qxF "$app $var" "$state" 2>/dev/null; then
        printf '%s=\n' "$var"
    else
        printf '%s=%s\n' "$var" "$value"
    fi
}

case "${1:-}" in
    list)
        printf '%s\n' org.example.Clean org.example.Proxied com.example.AlsoProxied
        ;;
    info)
        app="${3:-}"
        echo "[Context]"
        echo "shared=network;ipc;"
        echo "[Environment]"
        echo "SSL_CERT_DIR=/etc/ssl/certs"
        echo "no_proxy=localhost"
        case "$app" in
            org.example.Proxied)
                env_line "$app" http_proxy socks5://127.0.0.1:9050
                env_line "$app" ALL_PROXY  socks5://127.0.0.1:9050
                ;;
            com.example.AlsoProxied)
                env_line "$app" HTTPS_PROXY http://corp.example:3128
                ;;
        esac
        ;;
    override)
        app=""; var=""
        for arg in "$@"; do
            case "$arg" in
                --unset-env=*) var="${arg#--unset-env=}" ;;
                override|--*)  ;;
                *)             app="$arg" ;;
            esac
        done
        [[ -n "$app" && -n "$var" ]] && printf '%s %s\n' "$app" "$var" >> "$state"
        ;;
esac
exit 0
STUB
    chmod +x "$FAKE_BIN/flatpak"
    : > "$FAKE_STATE"
}

# pvpn, with the stub flatpak in front of any real one.
run_pvpn_flatpak() {
    PATH="$FAKE_BIN:$PATH" FAKE_FLATPAK_STATE="$FAKE_STATE" \
        PVPN_SERVERLIST="$FIXTURE" "$PVPN" "$@" 2>&1
}

write_fixture() {
    /usr/bin/python3 - "$FIXTURE" <<'PY'
import json, sys

def logical(name, country, city, lat, long, tier=0, load=50, score=5.0):
    return {
        "Name": name, "ExitCountry": country, "EntryCountry": country,
        "Tier": tier, "Load": load, "Score": score, "City": city,
        "Status": 1, "Features": 0,
        "Location": {"Lat": lat, "Long": long},
        "Servers": [{"EntryIP": "203.0.113.1", "ExitIP": "203.0.113.1",
                     "Status": 1, "ServicesDown": 0}],
    }

# 203.0.113.0/24 is TEST-NET-3 and is never routed, so if a test ever does
# reach the probing path it fails fast instead of generating real traffic.
json.dump({
    "MaxTier": 0,
    "LogicalServers": [
        logical("SG-FREE#2", "SG", "Singapore", 1.29, 103.85, load=77),
        logical("JP-FREE#9", "JP", "Tokyo", 35.68, 139.69, load=81),
        logical("NL-FREE#15", "NL", "Amsterdam", 52.37, 4.89, load=55, score=4.93),
        logical("NL-PLUS#1", "NL", "Amsterdam", 52.37, 4.89, tier=2),
    ],
}, open(sys.argv[1], "w"))
PY
}

echo "pvpn CLI tests"
write_fixture

# --- help and discoverability -----------------------------------------

out="$(run_pvpn help)"
assert_contains "help lists 'pvpn best'" "pvpn best" "$out"
assert_contains "help explains --connect" "pvpn best --connect" "$out"

out="$(run_pvpn best --help)"; status=$?
assert_status  "best --help exits cleanly" 0 "$status"
assert_contains "best --help documents --connect" "--connect" "$out"
assert_contains "best --help documents --country" "--country" "$out"
assert_contains "best --help documents --quick" "--quick" "$out"

# --- argument handling -------------------------------------------------

out="$(run_pvpn best --nonsense)"; status=$?
assert_status   "unknown option is rejected" 1 "$status"
assert_contains "unknown option explains itself" "Unknown option" "$out"

out="$(run_pvpn definitelynotacommand)"; status=$?
assert_status   "unknown command is rejected" 1 "$status"
assert_contains "unknown command explains itself" "Unknown command" "$out"

# --- ranking through the CLI -------------------------------------------
#
# --quick skips probing, so these assert the ordering logic without any
# network involvement.

out="$(run_pvpn best --quick --limit 5)"; status=$?
assert_status   "best --quick exits cleanly" 0 "$status"
assert_contains "best --quick renders a table" "RATING" "$out"
assert_contains "best --quick lists a free server" "SG-FREE#2" "$out"
assert_not_contains "best --quick hides out-of-tier servers" "NL-PLUS#1" "$out"

out="$(run_pvpn best --quick --country JP)"
assert_contains "country filter keeps the match" "JP-FREE#9" "$out"
assert_not_contains "country filter drops the rest" "SG-FREE#2" "$out"

# --json has to be pipeable, so status chatter must go to stderr and leave
# stdout holding nothing but the document.
out="$(run_pvpn_stdout best --quick --limit 1 --json)"
if printf '%s' "$out" | /usr/bin/python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
    pass "best --json emits valid JSON on stdout"
else
    fail "best --json emits valid JSON on stdout" "output was not parseable: $out"
fi

assert_not_contains "best --json keeps chatter off stdout" "no refresh needed" "$out"

# --- pvpn apps ---------------------------------------------------------

out="$(run_pvpn help)"
assert_contains "help lists 'pvpn apps'" "pvpn apps" "$out"

out="$(run_pvpn apps --help)"; status=$?
assert_status   "apps --help exits cleanly" 0 "$status"
assert_contains "apps --help documents --fix" "--fix" "$out"
assert_contains "apps --help documents --verify" "--verify" "$out"

out="$(run_pvpn apps --nonsense)"; status=$?
assert_status   "apps rejects unknown options" 1 "$status"

# The detection regex decides whether an app is reported as bypassing the
# tunnel, and one case is easy to get wrong: `flatpak override --unset-env`
# leaves the variable name in place with an empty value. Treating that as a
# proxy would make a fixed app look permanently broken.
PROXY_RE="$(sed -n "s/^PROXY_VARS_RE='\(.*\)'$/\1/p" "$PVPN")"

if [[ -n "$PROXY_RE" ]]; then
    pass "proxy detection regex is readable from pvpn"
else
    fail "proxy detection regex is readable from pvpn" "PROXY_VARS_RE not found"
fi

matches() { printf '%s\n' "$1" | grep -qE "$PROXY_RE"; }

for setting in \
    'http_proxy=socks5://127.0.0.1:9050' \
    'https_proxy=socks5://127.0.0.1:9050' \
    'ALL_PROXY=socks5://127.0.0.1:9050' \
    'HTTP_PROXY=http://corp:3128' \
    'ftp_proxy=http://x'
do
    if matches "$setting"; then
        pass "detects proxy: ${setting%%=*}"
    else
        fail "detects proxy: ${setting%%=*}" "should have matched: $setting"
    fi
done

for setting in 'http_proxy=' 'https_proxy=' 'ALL_PROXY=' 'no_proxy=localhost' 'SSL_CERT_DIR=/x'; do
    if matches "$setting"; then
        fail "ignores harmless: $setting" "should not have matched"
    else
        pass "ignores harmless: $setting"
    fi
done

# --- finding and fixing a bypass, end to end ---------------------------
#
# Against a stub flatpak, so the loop that actually removes a proxy is
# exercised rather than described.

write_fake_flatpak

out="$(run_pvpn_flatpak apps)"; status=$?
assert_status   "apps reports a bypass as failure" 1 "$status"
assert_contains "apps names the proxied app" "org.example.Proxied" "$out"
assert_contains "apps names every proxied app" "com.example.AlsoProxied" "$out"
assert_contains "apps shows the offending setting" "socks5://127.0.0.1:9050" "$out"
assert_not_contains "apps leaves clean apps alone" "org.example.Clean is routed" "$out"

out="$(run_pvpn_flatpak apps --fix)"; status=$?
assert_status   "apps --fix exits cleanly" 0 "$status"
assert_contains "apps --fix unsets the lowercase name" "unset http_proxy" "$out"
assert_contains "apps --fix unsets the uppercase name" "unset ALL_PROXY" "$out"
assert_contains "apps --fix confirms the app is back" "now uses the tunnel" "$out"
assert_contains "apps --fix warns about running apps" "until restarted" "$out"

# The fix has to survive a fresh scan, and an unset variable left behind as
# "VAR=" must not be mistaken for a proxy that is still set.
out="$(run_pvpn_flatpak apps)"; status=$?
assert_status   "a fixed machine passes the audit" 0 "$status"
assert_contains "audit reports everything on the tunnel" "no proxy overrides found" "$out"

# --- the connect path enforces this by itself --------------------------
#
# enforce_app_routing runs from cmd_up and has no command of its own, so it
# is reached by sourcing pvpn rather than invoking it.

call_enforce() {
    PATH="$FAKE_BIN:$PATH" FAKE_FLATPAK_STATE="$FAKE_STATE" \
        FAKE_FLATPAK_STUBBORN="${FAKE_FLATPAK_STUBBORN:-}" \
        bash -c 'source "$1"; enforce_app_routing' _ "$PVPN" 2>&1
}

write_fake_flatpak
out="$(call_enforce)"
assert_contains "connecting fixes a bypass without being asked" "was routed around the VPN" "$out"
assert_contains "connecting says which setting it removed" "unset http_proxy" "$out"

out="$(call_enforce)"
assert_not_contains "a clean machine gets no noise on connect" "routed around the VPN" "$out"

# PVPN_FIX_APPS=0 is the opt-out: still told, nothing changed.
write_fake_flatpak
out="$(PVPN_FIX_APPS=0 call_enforce)"
assert_contains "PVPN_FIX_APPS=0 still reports" "org.example.Proxied is routed around the VPN" "$out"
assert_contains "PVPN_FIX_APPS=0 says why nothing changed" "PVPN_FIX_APPS=0" "$out"
if [[ -s "$FAKE_STATE" ]]; then
    fail "PVPN_FIX_APPS=0 changes nothing" "it unset: $(cat "$FAKE_STATE")"
else
    pass "PVPN_FIX_APPS=0 changes nothing"
fi

# A proxy baked into an app's own manifest survives `--unset-env`. Saying
# "fixing" and then falling silent is the one outcome that would stop
# someone looking, so the connect path has to re-read and admit it.
write_fake_flatpak
out="$(FAKE_FLATPAK_STUBBORN=org.example.Proxied call_enforce)"
assert_contains "a fix that did not take is reported" "STILL routed around the VPN" "$out"

if grep -q 'enforce_app_routing' <(sed -n '/^cmd_up()/,/^}/p' "$PVPN"); then
    pass "cmd_up enforces app routing on connect"
else
    fail "cmd_up enforces app routing on connect" "a bypass would survive every connect"
fi

if grep -q 'fix_flatpak_routing' "$REPO/setup.sh"; then
    pass "setup.sh clears bypasses at install time"
else
    fail "setup.sh clears bypasses at install time" "an existing bypass would outlive the install"
fi

# --- the helper the command depends on ---------------------------------

if [[ -x "$REPO/lib/best-server.py" ]]; then
    pass "best-server.py is executable"
else
    fail "best-server.py is executable" "chmod +x lib/best-server.py"
fi

if grep -q 'best-server.py' "$REPO/setup.sh"; then
    pass "setup.sh installs best-server.py"
else
    fail "setup.sh installs best-server.py" "the helper would be missing after install"
fi

if grep -q '_patched_find_logical_server' "$REPO/lib/sitecustomize.py"; then
    pass "shim allows naming an in-tier server"
else
    fail "shim allows naming an in-tier server" "free accounts could not act on the ranking"
fi

# --- summary -----------------------------------------------------------

echo
if (( FAIL == 0 )); then
    printf '%s%d passed%s\n' "$G" "$PASS" "$X"
    exit 0
fi
printf '%s%d failed%s, %d passed\n' "$R" "$FAIL" "$X" "$PASS"
exit 1
