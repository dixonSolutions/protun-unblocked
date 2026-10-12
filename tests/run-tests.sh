#!/usr/bin/env bash
#
# Run the whole suite.
#
#   tests/run-tests.sh
#
# Everything here is offline and read-only: no connect, no disconnect, no
# routing changes, and no requirement to be signed in. Safe to run at any
# time, including while the VPN is up.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
    G=$'\e[32m'; R=$'\e[31m'; B=$'\e[1m'; X=$'\e[0m'
else
    G=''; R=''; B=''; X=''
fi

head_() { printf '\n%s== %s ==%s\n' "$B" "$1" "$X"; }

FAILED=()

head_ "Bash syntax"
for script in "$REPO"/legacy/pvpn.sh "$REPO"/bin/pvpn "$REPO"/bin/vpn-check \
              "$REPO"/bin/pvpn-autoconnect "$REPO"/setup.sh "$HERE"/*.sh; do
    if bash -n "$script"; then
        printf '  ok   %s\n' "${script#"$REPO"/}"
    else
        FAILED+=("syntax: ${script#"$REPO"/}")
    fi
done

head_ "POSIX sh syntax (system/)"
for script in "$REPO"/system/pvpn-dns-unsnap "$REPO"/system/pvpn-kick-user \
              "$REPO"/system/90-pvpn-autoconnect; do
    if sh -n "$script"; then
        printf '  ok   %s\n' "${script#"$REPO"/}"
    else
        FAILED+=("syntax: ${script#"$REPO"/}")
    fi
done

head_ "Python syntax"
if /usr/bin/python3 -m py_compile "$REPO"/lib/*.py "$HERE"/*.py; then
    printf '  ok   lib/*.py tests/*.py\n'
else
    FAILED+=("python syntax")
fi

# Optional linters. Absent tools are reported, not failed, so the suite
# stays runnable on a bare machine.
head_ "Linters"
if command -v shellcheck >/dev/null 2>&1; then
    if shellcheck "$REPO/legacy/pvpn.sh" "$REPO/bin/pvpn" \
            "$REPO/bin/pvpn-autoconnect" "$REPO"/system/pvpn-dns-unsnap \
            "$REPO"/system/pvpn-kick-user "$REPO"/system/90-pvpn-autoconnect \
            "$HERE"/*.sh; then
        printf '  ok   shellcheck\n'
    else
        FAILED+=("shellcheck")
    fi
else
    printf '  --   shellcheck not installed, skipping\n'
fi

if command -v ruff >/dev/null 2>&1; then
    if ruff check "$REPO/lib/best-server.py" "$HERE"/*.py \
            --select=E,F,W,B,SIM,UP --line-length=100 --quiet; then
        printf '  ok   ruff\n'
    else
        FAILED+=("ruff")
    fi
else
    printf '  --   ruff not installed, skipping\n'
fi

head_ "Unit tests: server ranking (Python, reference)"
if /usr/bin/python3 -m unittest discover -s "$HERE" -q; then
    printf '  ok   tests/test_best_server.py\n'
else
    FAILED+=("test_best_server.py")
fi

head_ "Unit tests: Rust workspace"
if command -v cargo >/dev/null 2>&1; then
    # The window needs GTK's development files; the CLI must not.
    scope=(--workspace)
    if ! pkg-config --exists gtk4 libadwaita-1 2>/dev/null; then
        scope+=(--exclude pvpn-gui)
        printf '  --   GTK4/libadwaita dev files missing, skipping pvpn-gui\n'
    fi
    if (cd "$REPO" && cargo test "${scope[@]}" --offline --quiet); then
        printf '  ok   cargo test\n'
    else
        FAILED+=("cargo test")
    fi

    # Cargo unifies features across a workspace build. If anything turns on
    # zbus's `tokio` feature, pvpn's blocking D-Bus calls panic inside its
    # own runtime ("Cannot start a runtime from within a runtime") — on
    # every command that reads the network. The tray library did exactly
    # that once; this keeps it from coming back.
    if (cd "$REPO" && cargo tree --workspace --offline -e features -i zbus 2>/dev/null) \
            | grep -q 'zbus feature "tokio"'; then
        printf '  FAIL zbus built with its tokio feature — pvpn would panic\n'
        FAILED+=("zbus tokio feature")
    else
        printf '  ok   zbus stays on async-io\n'
    fi
else
    printf '  --   cargo not installed, skipping Rust tests\n'
fi

head_ "Always-on recovery"
if "$HERE/test-always-on.sh"; then
    printf '  ok   test-always-on.sh\n'
else
    FAILED+=("test-always-on.sh")
fi

head_ "CLI tests: legacy bash pvpn"
if "$HERE/test-pvpn-cli.sh"; then
    :
else
    FAILED+=("test-pvpn-cli.sh")
fi

head_ "CLI tests: Rust pvpn"
if [[ -x "$HERE/test-pvpn-rust.sh" ]]; then
    if "$HERE/test-pvpn-rust.sh"; then
        :
    else
        FAILED+=("test-pvpn-rust.sh")
    fi
fi

echo
if (( ${#FAILED[@]} == 0 )); then
    printf '%sAll checks passed.%s\n' "$G" "$X"
    exit 0
fi
printf '%sFailed:%s %s\n' "$R" "$X" "${FAILED[*]}"
exit 1
