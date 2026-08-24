#!/usr/bin/env bash
#
# Tests for the Rust `pvpn` CLI.
#
# Offline: ranking uses a fixture server list (`--quick` / `--offline`),
# and apps uses a stub flatpak. Nothing here connects, disconnects, or
# touches routing.
#
# Run via tests/run-tests.sh, or directly:  tests/test-pvpn-rust.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not installed — skipping Rust CLI tests"
    exit 0
fi

if ! (cd "$REPO" && cargo build -p pvpn --quiet); then
    echo "cargo build -p pvpn failed"
    exit 1
fi

PVPN="$REPO/target/debug/pvpn"

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

run_pvpn() {
    PVPN_SERVERLIST="$FIXTURE" "$PVPN" "$@" 2>&1
}

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

FAKE_BIN="$FIXTURE_DIR/bin"
FAKE_STATE="$FIXTURE_DIR/flatpak-state"

write_fake_flatpak() {
    mkdir -p "$FAKE_BIN"
    cat > "$FAKE_BIN/flatpak" <<'STUB'
#!/usr/bin/env bash
state="${FAKE_FLATPAK_STATE:?}"
env_line() {
    local app="$1" var="$2" value="$3"
    if grep -qxF "$app $var" "$state" 2>/dev/null; then
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

run_pvpn_flatpak() {
    PATH="$FAKE_BIN:$PATH" FAKE_FLATPAK_STATE="$FAKE_STATE" \
        PVPN_SERVERLIST="$FIXTURE" "$PVPN" "$@" 2>&1
}

echo "Rust pvpn CLI tests"
write_fixture

out="$(run_pvpn help)"
assert_contains "help lists 'pvpn best'" "pvpn best" "$out"
assert_contains "help explains --connect" "pvpn best --connect" "$out"
assert_contains "help lists 'pvpn apps'" "pvpn apps" "$out"

out="$(run_pvpn best --help)"; status=$?
assert_status  "best --help exits cleanly" 0 "$status"
assert_contains "best --help documents --connect" "--connect" "$out"
assert_contains "best --help documents --country" "--country" "$out"
assert_contains "best --help documents --quick" "--quick" "$out"

out="$(run_pvpn best --nonsense)"; status=$?
assert_status   "unknown option is rejected" 1 "$status"
assert_contains "unknown option explains itself" "Unknown option" "$out"

out="$(run_pvpn definitelynotacommand)"; status=$?
assert_status   "unknown command is rejected" 1 "$status"
assert_contains "unknown command explains itself" "Unknown command" "$out"

out="$(run_pvpn best --quick --limit 5)"; status=$?
assert_status   "best --quick exits cleanly" 0 "$status"
assert_contains "best --quick renders a table" "RATING" "$out"
assert_contains "best --quick lists a free server" "SG-FREE#2" "$out"
assert_not_contains "best --quick hides out-of-tier servers" "NL-PLUS#1" "$out"

out="$(run_pvpn best --quick --country JP)"
assert_contains "country filter keeps the match" "JP-FREE#9" "$out"
assert_not_contains "country filter drops the rest" "SG-FREE#2" "$out"

out="$(run_pvpn_stdout best --quick --limit 1 --json)"
if printf '%s' "$out" | /usr/bin/python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
    pass "best --json emits valid JSON on stdout"
else
    fail "best --json emits valid JSON on stdout" "output was not parseable: $out"
fi
assert_not_contains "best --json keeps chatter off stdout" "Ranking locally" "$out"

# Cross-check: Rust --quick order matches Python --no-probe names.
py_names="$(/usr/bin/python3 "$REPO/lib/best-server.py" \
    --serverlist "$FIXTURE" --no-probe --format names --limit 5 2>/dev/null)"
rs_names="$(printf '%s\n' "$out" | /usr/bin/python3 -c '
import json,sys
d=json.load(sys.stdin)
print("\n".join(r["name"] for r in d.get("results",[])[:5]))
' 2>/dev/null || true)"
# Re-run json at limit 5 for a fair compare.
rs_json="$(run_pvpn_stdout best --quick --limit 5 --json)"
rs_names="$(printf '%s\n' "$rs_json" | /usr/bin/python3 -c '
import json,sys
d=json.load(sys.stdin)
print("\n".join(r["name"] for r in d.get("results",[])))
')"
if [[ "$py_names" == "$rs_names" ]]; then
    pass "best --quick order matches lib/best-server.py --no-probe"
else
    fail "best --quick order matches lib/best-server.py --no-probe" \
        "python: [$py_names] rust: [$rs_names]"
fi

out="$(run_pvpn apps --help)"; status=$?
assert_status   "apps --help exits cleanly" 0 "$status"
assert_contains "apps --help documents --fix" "--fix" "$out"
assert_contains "apps --help documents --verify" "--verify" "$out"

out="$(run_pvpn apps --nonsense)"; status=$?
assert_status   "apps rejects unknown options" 1 "$status"

write_fake_flatpak
out="$(run_pvpn_flatpak apps)"; status=$?
assert_status   "apps reports a bypass as failure" 1 "$status"
assert_contains "apps names the proxied app" "org.example.Proxied" "$out"
assert_contains "apps names every proxied app" "com.example.AlsoProxied" "$out"
assert_not_contains "apps leaves clean apps alone" "org.example.Clean is routed" "$out"

out="$(run_pvpn_flatpak apps --fix)"; status=$?
assert_status   "apps --fix exits cleanly" 0 "$status"
assert_contains "apps --fix unsets the lowercase name" "unset http_proxy" "$out"
assert_contains "apps --fix unsets the uppercase name" "unset ALL_PROXY" "$out"
assert_contains "apps --fix confirms the app is back" "now uses the tunnel" "$out"

out="$(run_pvpn_flatpak apps)"; status=$?
assert_status   "a fixed machine passes the audit" 0 "$status"
assert_contains "audit reports everything on the tunnel" "no proxy overrides found" "$out"

# Nothing is left that needs a daemon, a socket, or a service to be
# running. These commands answer in this process or not at all.
out="$(run_pvpn help)"
assert_not_contains "help does not mention a daemon" "daemon" "$out"
assert_not_contains "help does not mention pvpnd" "pvpnd" "$out"
assert_contains "help says nothing runs in the background" \
    "Nothing runs in the background" "$out"

out="$(run_pvpn logs --help)"; status=$?
assert_status   "logs --help exits cleanly" 0 "$status"
assert_not_contains "logs no longer reads a daemon journal" "journalctl" "$out"

# `fast` and `blocked` are the point of keeping any state at all: they must
# answer from disk, and must say which network they are talking about.
STATE_HOME="$FIXTURE_DIR/state-home"
mkdir -p "$STATE_HOME/pvpn"
run_pvpn_isolated() {
    XDG_DATA_HOME="$STATE_HOME" XDG_CONFIG_HOME="$FIXTURE_DIR/config-home" \
        PVPN_SERVERLIST="$FIXTURE" "$PVPN" "$@" 2>&1
}

out="$(run_pvpn_isolated fast)"; status=$?
assert_status   "fast answers without a daemon" 0 "$status"
assert_contains "an empty fast list says how it gets filled" "pvpn best" "$out"

out="$(run_pvpn_isolated blocked)"; status=$?
assert_status   "blocked answers without a daemon" 0 "$status"
assert_contains "an empty blocked list names the network" "No blocked servers on" "$out"

# A state file written by the daemon must still load: `want_up` is gone
# from the struct but is still in everyone's state.json.
/usr/bin/python3 - "$STATE_HOME/pvpn/state.json" <<'STATE'
import json, sys
json.dump({
    "networks": {"wifi:test": {
        "servers": {"SG-FREE#9": {
            "ema_latency_ms": 180.0, "samples": 4, "last_probe_ok": None,
            "status": "blocked", "blocked_reason": "refused",
            "blocked_since": "2999-01-01T00:00:00Z",
            "consecutive_connect_failures": 2}},
        "last_full_rank": {"computed_at": None, "servers": []}}},
    "want_up": True,
}, open(sys.argv[1], "w"))
STATE
out="$(run_pvpn_isolated blocked)"; status=$?
assert_status   "a daemon-era state file still loads" 0 "$status"

echo
if (( FAIL == 0 )); then
    printf '%s%d passed%s\n' "$G" "$PASS" "$X"
    exit 0
fi
printf '%s%d failed%s, %d passed\n' "$R" "$FAIL" "$X" "$PASS"
exit 1
