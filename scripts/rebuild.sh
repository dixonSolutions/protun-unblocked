#!/usr/bin/env bash
#
# Rebuild pvpn from this checkout and reinstall it over an existing install.
#
#   scripts/rebuild.sh               build + reinstall pvpn, the window and shims
#   scripts/rebuild.sh --pull        git pull --ff-only first
#   scripts/rebuild.sh --no-gui      skip the window (pvpn-gui)
#   scripts/rebuild.sh --no-install  build only, leave ~/.local alone
#
# The window is rebuilt only when the GTK4/libadwaita development files are
# present; without them it is skipped with a warning and pvpn still installs.
#
# This is the quick path after a code change or a pull. It does not touch
# system packages, Tor, sudo-owned files or the login wizard — for a first
# install, or to change --always-on, use ./setup.sh.
#
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HOME/.local/bin"
LIB="$HOME/.local/share/pvpn"
GUI_ID="io.github.dixonsolutions.ProtunUnblocked"
APPS_DIR="$HOME/.local/share/applications"
ICONS_DIR="$HOME/.local/share/icons/hicolor"

PULL=0
INSTALL=1
GUI=1
for arg in "$@"; do
    case "$arg" in
        --pull)       PULL=1 ;;
        --no-install) INSTALL=0 ;;
        --no-gui)     GUI=0 ;;
        -h|--help)
            sed -n '3,15p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "Unknown option: $arg (try --help)" >&2
            exit 1
            ;;
    esac
done

c(){ printf '\033[%sm%s\033[0m\n' "$1" "$2"; }
ok(){ c '1;32' "  ok    $1"; }; bad(){ c '1;31' "  MISS  $1"; }
note(){ c '0;37' "        $1"; }; head_(){ c '1;36' "$1"; }
warn(){ c '1;33' "  warn  $1"; }

find_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        command -v cargo
        return 0
    fi
    if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
        echo "$HOME/.cargo/bin/cargo"
        return 0
    fi
    return 1
}

gui_build_deps_present() {
    command -v pkg-config >/dev/null 2>&1 \
        && pkg-config --exists 'gtk4 >= 4.16' 'libadwaita-1 >= 1.7'
}

if (( PULL )); then
    head_ "Pulling latest changes"
    git -C "$SRC" pull --ff-only
fi

head_ "Building pvpn"
if ! cargo="$(find_cargo)"; then
    bad "cargo not found — run ./setup.sh to install a Rust toolchain"
    exit 1
fi
(cd "$SRC" && "$cargo" build --release --locked -p pvpn)
ok "$SRC/target/release/pvpn"

GUI_BUILT=0
if (( GUI )); then
    head_ "Building the window (pvpn-gui)"
    if ! gui_build_deps_present; then
        warn "GTK4 >= 4.16 / libadwaita >= 1.7 development files not found — skipping the window"
        note "  sudo dnf install gtk4-devel libadwaita-devel   (apt: libgtk-4-dev libadwaita-1-dev)"
    elif (cd "$SRC" && "$cargo" build --release --locked -p pvpn-gui); then
        GUI_BUILT=1
        ok "$SRC/target/release/pvpn-gui"
    else
        warn "pvpn-gui did not build — pvpn is still reinstalled"
    fi
fi

(( INSTALL )) || exit 0

# Reinstalling over nothing would leave a pvpn without its dependencies
# (proton-vpn-cli, NetworkManager, the Python API) — that is setup's job.
if [[ ! -x "$BIN/pvpn" ]]; then
    warn "pvpn is not installed yet — run ./setup.sh for the first install"
    exit 1
fi

head_ "Reinstalling pvpn"
mkdir -p "$BIN" "$LIB"
install -m755 "$SRC/target/release/pvpn"  "$BIN/pvpn";  ok "$BIN/pvpn"
install -m755 "$SRC/bin/vpn-check" "$BIN/vpn-check"; ok "$BIN/vpn-check"
install -m644 "$SRC/lib/sitecustomize.py" "$LIB/";   ok "$LIB/sitecustomize.py"
install -m644 "$SRC/lib/aiodns.py"        "$LIB/";   ok "$LIB/aiodns.py"
[[ -f "$SRC/lib/debug-signin.py" ]] && \
    install -m755 "$SRC/lib/debug-signin.py" "$LIB/" && ok "$LIB/debug-signin.py"
[[ -f "$SRC/lib/signin-bridge.py" ]] && \
    install -m755 "$SRC/lib/signin-bridge.py" "$LIB/" && ok "$LIB/signin-bridge.py"

# Only refresh the always-on helper if --always-on already put it there; the
# root-owned half of always-on stays with ./setup.sh --always-on.
if [[ -x "$BIN/pvpn-autoconnect" ]]; then
    install -m755 "$SRC/bin/pvpn-autoconnect" "$BIN/pvpn-autoconnect"
    ok "$BIN/pvpn-autoconnect"
fi

if (( GUI_BUILT )); then
    head_ "Reinstalling the window"
    install -m755 "$SRC/target/release/pvpn-gui" "$BIN/pvpn-gui"; ok "$BIN/pvpn-gui"
    data="$SRC/crates/pvpn-gui/data"
    install -Dm644 "$data/$GUI_ID.desktop" "$APPS_DIR/$GUI_ID.desktop"
    sed -i "s|^Exec=pvpn-gui|Exec=$BIN/pvpn-gui|" "$APPS_DIR/$GUI_ID.desktop"
    ok "$APPS_DIR/$GUI_ID.desktop"
    install -Dm644 "$data/icons/$GUI_ID.svg" "$ICONS_DIR/scalable/apps/$GUI_ID.svg"
    install -Dm644 "$data/icons/$GUI_ID-symbolic.svg" "$ICONS_DIR/symbolic/apps/$GUI_ID-symbolic.svg"
    ok "icons"
    # Lets `pvpn up` from a systemd unit start the tray in a unit of its own.
    dbus_dir="$HOME/.local/share/dbus-1/services"
    mkdir -p "$dbus_dir"
    printf '[D-BUS Service]\nName=%s\nExec=%s --gapplication-service\n' \
        "$GUI_ID" "$BIN/pvpn-gui" > "$dbus_dir/$GUI_ID.service"
    ok "$dbus_dir/$GUI_ID.service"
    # Refresh the desktop shortcut only if setup already put one there.
    desktop_dir="$(xdg-user-dir DESKTOP 2>/dev/null || echo "$HOME/Desktop")"
    if [[ -f "$desktop_dir/$GUI_ID.desktop" && "$desktop_dir" != "$HOME" ]]; then
        install -m755 "$APPS_DIR/$GUI_ID.desktop" "$desktop_dir/$GUI_ID.desktop"
        command -v gio >/dev/null 2>&1 \
            && gio set "$desktop_dir/$GUI_ID.desktop" metadata::trusted true 2>/dev/null || true
        ok "$desktop_dir/$GUI_ID.desktop"
    fi
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && gtk-update-icon-cache -q -t "$ICONS_DIR" 2>/dev/null || true
    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database -q "$APPS_DIR" 2>/dev/null || true
fi

note "$("$BIN/pvpn" --version 2>/dev/null || echo "installed from $(git -C "$SRC" rev-parse --short HEAD)")"
