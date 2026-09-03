#!/usr/bin/env bash
#
# pvpn installer.
#
#   ./setup.sh                 install deps + pvpn, then guided setup
#   ./setup.sh --uninstall     remove everything this script installed
#   ./setup.sh --no-wizard     install only (skip login / vpn-check prompts)
#   ./setup.sh --always-on     also recover the tunnel after suspend/link change
#   ./setup.sh --no-always-on  remove those recovery hooks again
#
# Auto-installs proton-vpn-cli via apt (Debian/Ubuntu) or dnf (Fedora).
# If repo.protonvpn.com is blocked, downloads and refreshes that repo
# through Tor (socks5h://127.0.0.1:9050) — same situation this project
# exists for.
#
# pvpn itself lands only under $HOME:
#   ~/.local/bin/pvpn          the CLI (one binary, nothing runs in the background)
#   ~/.local/bin/vpn-check
#   ~/.local/share/pvpn/       Python shims (sitecustomize, sign-in)
#
# --always-on is the one exception: it needs sudo and writes root-owned files
# under /etc and /usr/local/sbin. It is opt-in and never runs by default.
# See docs/always-on.md.
#
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HOME/.local/bin"
LIB="$HOME/.local/share/pvpn"
TMPDIR="${TMPDIR:-/tmp}"
WORKDIR="$(mktemp -d "$TMPDIR/pvpn-setup.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT

# Official Proton repo meta-packages (from protonvpn.com/support).
DEB_RELEASE_URL="https://repo.protonvpn.com/debian/dists/stable/main/binary-all/protonvpn-stable-release_1.0.8_all.deb"
DEB_RELEASE_SHA256="0b14e71586b22e498eb20926c48c7b434b751149b1f2af9902ef1cfe6b03e180"
RPM_RELEASE_VER="1.0.4-1"

NO_WIZARD=0
ALWAYS_ON=0
NO_ALWAYS_ON=0
LID_LOCK=ask
for arg in "$@"; do
    case "$arg" in
        --uninstall)   # handled below
            ;;
        --no-wizard)   NO_WIZARD=1 ;;
        --always-on)   ALWAYS_ON=1 ;;
        --no-always-on) NO_ALWAYS_ON=1 ;;
        --lid-lock)    LID_LOCK=yes ;;
        --no-lid-lock) LID_LOCK=no ;;
        -h|--help)
            sed -n '3,24p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            if [[ "$arg" != "--uninstall" ]]; then
                echo "Unknown option: $arg (try --help)" >&2
                exit 1
            fi
            ;;
    esac
done

c(){ printf '\033[%sm%s\033[0m\n' "$1" "$2"; }
ok(){ c '1;32' "  ok    $1"; }; bad(){ c '1;31' "  MISS  $1"; }
note(){ c '0;37' "        $1"; }; head_(){ c '1;36' "$1"; }
warn(){ c '1;33' "  warn  $1"; }

ask_yes() {
    local prompt="$1" reply
    if [[ ! -t 0 ]]; then return 1; fi
    read -r -p "$prompt [Y/n] " reply || true
    case "${reply:-Y}" in
        n|N|no|NO) return 1 ;;
        *) return 0 ;;
    esac
}

# --- distro detection -------------------------------------------------

detect_pm() {
    # Prefer os-release over which(1): some Ubuntu boxes also ship dnf.
    local id="" like=""
    if [[ -r /etc/os-release ]]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        id="${ID:-}"
        like="${ID_LIKE:-}"
    fi

    if [[ -e /run/ostree-booted ]]; then
        echo "ostree"
        return
    fi

    case "$id" in
        debian|ubuntu|linuxmint|pop|elementary|neon|zorin|kali)
            echo "apt"; return ;;
        fedora|rhel|centos|rocky|almalinux|nobara|bazzite)
            echo "dnf"; return ;;
    esac
    case " $like " in
        *" debian "*|*" ubuntu "*) echo "apt"; return ;;
        *" fedora "*|*" rhel "*|*" centos "*) echo "dnf"; return ;;
    esac

    if command -v apt-get >/dev/null 2>&1; then echo "apt"; return; fi
    if command -v dnf >/dev/null 2>&1; then echo "dnf"; return; fi
    echo "unknown"
}

PM="$(detect_pm)"

# --- Tor / download helpers -------------------------------------------

repo_reachable() {
    curl -fsI --max-time 6 "https://repo.protonvpn.com/" >/dev/null 2>&1
}

ensure_tor() {
    if ! command -v torsocks >/dev/null 2>&1 || ! command -v tor >/dev/null 2>&1; then
        head_ "Installing Tor (needed — Proton's repo/API is blocked here)"
        case "$PM" in
            apt)
                sudo apt-get update -y
                sudo DEBIAN_FRONTEND=noninteractive apt-get install -y tor torsocks
                ;;
            dnf)
                sudo dnf install -y tor torsocks
                ;;
            ostree)
                bad "rpm-ostree system: install tor manually, then re-run"
                note "  rpm-ostree install tor torsocks && sudo systemctl reboot"
                exit 1
                ;;
            *)
                bad "Cannot auto-install tor on this distro. Install tor + torsocks, re-run."
                exit 1
                ;;
        esac
    fi

    if ! systemctl is-active --quiet tor 2>/dev/null; then
        note "Starting tor..."
        sudo systemctl enable --now tor
    fi

    local i
    for i in 1 2 3 4 5 6 7 8 9 10; do
        if (exec 3<>/dev/tcp/127.0.0.1/9050) 2>/dev/null; then
            ok "tor listening on 127.0.0.1:9050"
            return 0
        fi
        sleep 1
    done
    bad "tor is installed but 127.0.0.1:9050 is not accepting connections"
    exit 1
}

# Download URL to file. Tries direct first; falls back to Tor.
# Sets USE_TOR=1 if Tor was required for this (or earlier) download.
USE_TOR=0
# Download, returning non-zero instead of aborting. For a file that may
# legitimately not exist - a package index for an architecture Proton does
# not publish, say - where the caller has something more useful to say than
# "download failed".
fetch_optional() {
    local url="$1" out="$2"
    if [[ "$USE_TOR" -eq 0 ]] && curl -fsSL --max-time 60 -o "$out" "$url"; then
        return 0
    fi
    ensure_tor
    USE_TOR=1
    note "Fetching via Tor: $url"
    # Discard torsocks multiarch warning on stderr so sha checks stay clean.
    torsocks curl -fsSL --max-time 180 -o "$out" "$url" 2>/dev/null
}

fetch() {
    if ! fetch_optional "$@"; then
        bad "Download failed even through Tor: $1"
        exit 1
    fi
}

# --- package installs -------------------------------------------------

base_deps_present() {
    command -v curl >/dev/null 2>&1 \
        && command -v tor >/dev/null 2>&1 \
        && command -v torsocks >/dev/null 2>&1 \
        && command -v nmcli >/dev/null 2>&1 \
        && [[ -x /usr/bin/python3 ]]
}

pvpn_installed() {
    [[ -x "$BIN/pvpn" && -x "$BIN/vpn-check" \
        && -f "$LIB/sitecustomize.py" && -f "$LIB/aiodns.py" ]]
}

everything_ready() {
    base_deps_present \
        && command -v protonvpn >/dev/null 2>&1 \
        && pvpn_installed
}

install_base_deps() {
    if base_deps_present; then
        ok "base dependencies already installed — skipping"
        return 0
    fi

    head_ "Installing base dependencies ($PM)"
    case "$PM" in
        apt)
            sudo apt-get update -y
            sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
                curl ca-certificates network-manager python3 \
                tor torsocks gnome-keyring
            ;;
        dnf)
            # Base deps come from Fedora mirrors — no Tor needed.
            sudo dnf install -y curl ca-certificates NetworkManager python3 \
                tor torsocks gnome-keyring
            ;;
        ostree)
            bad "Immutable ostree system detected."
            note "Layer packages, reboot, then re-run ./setup.sh:"
            note "  rpm-ostree install proton-vpn-cli tor torsocks"
            note "(add Proton's release RPM first — see README)"
            exit 1
            ;;
        *)
            bad "Unsupported package manager. Need apt (Debian/Ubuntu) or dnf (Fedora)."
            exit 1
            ;;
    esac
    ok "base dependencies"
}

install_proton_apt() {
    head_ "Installing proton-vpn-cli (apt)"
    if ! repo_reachable; then
        warn "repo.protonvpn.com blocked — using Tor for Proton packages"
        ensure_tor
        USE_TOR=1
    else
        ok "repo.protonvpn.com reachable directly"
    fi

    local deb="$WORKDIR/protonvpn-stable-release.deb"
    fetch "$DEB_RELEASE_URL" "$deb"
    echo "${DEB_RELEASE_SHA256}  ${deb}" | sha256sum --check - >/dev/null
    ok "release package checksum"

    sudo dpkg -i "$deb"

    # Refresh indexes; proxy only the Proton host when needed.
    local -a opts=()
    if [[ "$USE_TOR" -eq 1 ]]; then
        opts+=(
            -o "Acquire::https::Proxy::repo.protonvpn.com=socks5h://127.0.0.1:9050"
            -o "Acquire::http::Proxy::repo.protonvpn.com=socks5h://127.0.0.1:9050"
        )
    fi
    sudo apt-get "${opts[@]}" update -y
    sudo DEBIAN_FRONTEND=noninteractive apt-get "${opts[@]}" install -y \
        proton-vpn-cli \
        python3-proton-vpn-network-manager \
        network-manager-openvpn
    # Stealth = python3-proton-vpn-lib + proton-vpn-linux (NM protun plugin).
    # proton-vpn-linux may only be in Proton unstable — pull the .deb if apt misses it.
    ensure_stealth_backend_apt "${opts[@]}"
    ok "proton-vpn-cli + Stealth backends"
}

# Install NM protun plugin (proton-vpn-linux) + lib if missing.
# Stable apt often lacks proton-vpn-linux; fetch latest from unstable via Tor/direct.
# The pool path of a package in an apt Packages index, relative to the repo
# root. Empty when the index does not carry that package.
deb_filename() {
    awk -v want="$1" '
        $1 == "Package:"  { match_ = ($2 == want) }
        match_ && $1 == "Filename:" { print $2; exit }
    ' "$2"
}

ensure_stealth_backend_apt() {
    local -a opts=("$@")
    if /usr/bin/python3 - <<'PY' 2>/dev/null
from proton.vpn.backend.networkmanager.protocol.protun.protun import ProtunTLS
ProtunTLS.plugin_exists = None
raise SystemExit(0 if ProtunTLS.validate() else 1)
PY
    then
        ok "Stealth (protun-tls) backend already available"
        return 0
    fi

    head_ "Installing Stealth backend (proton-vpn-linux)"
    if sudo DEBIAN_FRONTEND=noninteractive apt-get "${opts[@]}" install -y \
            python3-proton-vpn-lib proton-vpn-linux 2>/dev/null; then
        :
    else
        # Pull debs from unstable (where the NM protun plugin currently lives).
        #
        # Ask dpkg for the architecture rather than assuming amd64. Proton
        # does publish arm64 - VERIFIED: proton-vpn-linux_0.4.0_arm64.deb is
        # in the unstable index - so hardcoding amd64 was not merely
        # unportable, it denied those machines a backend that exists. The
        # only symptom would have been Stealth still validating as MISS
        # after a setup that reported success.
        ensure_tor
        local repo="https://repo.protonvpn.com/debian"
        local arch; arch="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
        local pkg_index="$WORKDIR/Packages.unstable"
        if ! fetch_optional "$repo/dists/unstable/main/binary-${arch}/Packages" \
                "$pkg_index" || [[ ! -s "$pkg_index" ]]; then
            bad "Proton publishes no package index for ${arch}."
            note "Stealth needs proton-vpn-linux, which is not built for it."
            note "Other protocols still work: pvpn up openvpn-tcp"
            return 1
        fi

        # Take the path the index gives rather than reassembling a filename
        # from the version. Architecture-independent packages are published
        # as _all.deb, so a guessed "_${arch}.deb" would 404 on the very
        # package most likely to be arch-independent.
        local lib_path linux_path
        lib_path="$(deb_filename python3-proton-vpn-lib "$pkg_index")"
        linux_path="$(deb_filename proton-vpn-linux "$pkg_index")"
        [[ -n "$linux_path" && -n "$lib_path" ]] || {
            bad "Could not find the Stealth packages in Proton unstable (${arch})"
            return 1
        }
        fetch "$repo/$lib_path"   "$WORKDIR/python3-proton-vpn-lib.deb"
        fetch "$repo/$linux_path" "$WORKDIR/proton-vpn-linux.deb"
        sudo dpkg -i "$WORKDIR/python3-proton-vpn-lib.deb" "$WORKDIR/proton-vpn-linux.deb" \
            || sudo apt-get "${opts[@]}" install -f -y
    fi
    sudo systemctl restart NetworkManager >/dev/null 2>&1 || true
    sleep 1
    if /usr/bin/python3 - <<'PY' 2>/dev/null
from proton.vpn.backend.networkmanager.protocol.protun.protun import ProtunTLS
ProtunTLS.plugin_exists = None
raise SystemExit(0 if ProtunTLS.validate() else 1)
PY
    then
        ok "Stealth (protun-tls) ready"
    else
        warn "Stealth packages installed but protun-tls still validates as MISS"
        note "Reboot or: sudo systemctl restart NetworkManager"
        return 1
    fi
}

install_proton_dnf() {
    head_ "Installing proton-vpn-cli (dnf)"
    local fedora_ver
    if [[ -r /etc/fedora-release ]]; then
        fedora_ver="$(awk '{print $3}' /etc/fedora-release)"
    else
        fedora_ver="$(rpm -E %fedora 2>/dev/null || true)"
    fi
    if [[ -z "${fedora_ver:-}" || "$fedora_ver" == "%fedora" ]]; then
        bad "Could not detect Fedora version"
        exit 1
    fi

    if ! repo_reachable; then
        warn "repo.protonvpn.com blocked — using Tor for Proton packages"
        ensure_tor
        USE_TOR=1
    else
        ok "repo.protonvpn.com reachable directly"
    fi

    local rpm_url="https://repo.protonvpn.com/fedora-${fedora_ver}-stable/protonvpn-stable-release/protonvpn-stable-release-${RPM_RELEASE_VER}.noarch.rpm"
    local rpm="$WORKDIR/protonvpn-stable-release.rpm"
    fetch "$rpm_url" "$rpm"

    local -a proxy=()
    if [[ "$USE_TOR" -eq 1 ]]; then
        proxy=(--setopt=proxy=socks5h://127.0.0.1:9050)
    fi

    sudo dnf install -y "$rpm"
    # Accept Proton's repo key non-interactively when possible; fall back to prompt.
    sudo dnf "${proxy[@]}" check-update --refresh || true
    if ! sudo dnf "${proxy[@]}" install -y proton-vpn-cli python3-proton-vpn-lib \
            python3-proton-vpn-network-manager; then
        warn "Retrying proton-vpn-cli install (accept the OpenPGP key if prompted)..."
        sudo dnf "${proxy[@]}" install proton-vpn-cli python3-proton-vpn-lib
    fi
    ok "proton-vpn-cli + protocol backends"
}

install_proton() {
    if command -v protonvpn >/dev/null 2>&1; then
        ok "proton-vpn-cli already installed — skipping CLI install"
    else
        case "$PM" in
            apt) install_proton_apt ;;
            dnf) install_proton_dnf ;;
            *)
                bad "Cannot install proton-vpn-cli automatically on this system"
                exit 1
                ;;
        esac
        if ! command -v protonvpn >/dev/null 2>&1; then
            bad "protonvpn still not on PATH after install"
            exit 1
        fi
    fi
    # Always ensure Stealth — required primary protocol on filtered networks.
    case "$PM" in
        apt)
            local -a opts=()
            if [[ "${USE_TOR:-0}" -eq 1 ]] || ! repo_reachable; then
                ensure_tor
                USE_TOR=1
                opts+=(
                    -o "Acquire::https::Proxy::repo.protonvpn.com=socks5h://127.0.0.1:9050"
                    -o "Acquire::http::Proxy::repo.protonvpn.com=socks5h://127.0.0.1:9050"
                )
            fi
            ensure_stealth_backend_apt "${opts[@]}" || true
            ;;
        dnf)
            # Best-effort on Fedora; package name may lag Debian unstable.
            sudo dnf install -y python3-proton-vpn-lib proton-vpn-linux 2>/dev/null || true
            ;;
    esac
}

# --- pvpn files -------------------------------------------------------

find_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        command -v cargo
        return 0
    fi
    if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
        export PATH="$HOME/.cargo/bin:$PATH"
        echo "$HOME/.cargo/bin/cargo"
        return 0
    fi
    return 1
}

install_rust_toolchain() {
    if find_cargo >/dev/null; then
        return 0
    fi
    head_ "Rust (cargo) is required to build pvpn"
    if [[ ! -t 0 ]]; then
        bad "cargo not found. Install rustup: https://rustup.rs/"
        return 1
    fi
    if ask_yes "Install a user-local Rust toolchain via rustup?"; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
        export PATH="$HOME/.cargo/bin:$PATH"
        find_cargo >/dev/null
        return $?
    fi
    bad "Skipped. Install cargo and re-run ./setup.sh"
    return 1
}

# There used to be a pvpnd user service that reconnected on its own. It
# is gone, and leaving a copy of it running would fight every command this
# script is about to install — so take it out before installing anything.
remove_old_daemon() {
    local unit="$HOME/.config/systemd/user/pvpnd.service"
    [[ -f "$unit" || -x "$BIN/pvpnd" ]] || return 0
    head_ "Removing the old pvpnd daemon"
    if systemctl --user is-active --quiet pvpnd 2>/dev/null; then
        note "stopping pvpnd (this leaves any tunnel it built alone)"
    fi
    systemctl --user disable --now pvpnd 2>/dev/null || true
    rm -f "$unit"
    systemctl --user daemon-reload 2>/dev/null || true
    rm -f "$BIN/pvpnd"
    ok "pvpnd removed — nothing runs in the background any more"
    note "Your fast/blocked server lists are untouched (~/.local/share/pvpn/state.json)"
}

install_pvpn() {
    head_ "Installing pvpn"
    mkdir -p "$BIN" "$LIB"
    remove_old_daemon

    install_rust_toolchain || return 1
    local cargo
    cargo="$(find_cargo)"
    note "Building release binaries (first time can take a minute)..."
    (cd "$SRC" && "$cargo" build --release --locked)
    install -m755 "$SRC/target/release/pvpn"  "$BIN/pvpn";  ok "$BIN/pvpn"
    install -m755 "$SRC/bin/vpn-check" "$BIN/vpn-check"; ok "$BIN/vpn-check"
    install -m644 "$SRC/lib/sitecustomize.py" "$LIB/";   ok "$LIB/sitecustomize.py"
    install -m644 "$SRC/lib/aiodns.py"        "$LIB/";   ok "$LIB/aiodns.py"
    [[ -f "$SRC/lib/debug-signin.py" ]] && \
        install -m755 "$SRC/lib/debug-signin.py" "$LIB/" && ok "$LIB/debug-signin.py"
    [[ -f "$SRC/lib/signin-bridge.py" ]] && \
        install -m755 "$SRC/lib/signin-bridge.py" "$LIB/" && ok "$LIB/signin-bridge.py"
    # lib/best-server.py stays in the checkout for comparison; ranking now
    # lives in pvpn-core and is not installed into the shim path.
    case ":$PATH:" in
        *":$BIN:"*) ok "$BIN already on PATH" ;;
        *)
            for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
                [[ -f $rc ]] && ! grep -q '\.local/bin' "$rc" \
                    && echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$rc"
            done
            warn "Added $BIN to PATH — open a new shell or: source ~/.bashrc"
            export PATH="$BIN:$PATH"
            ;;
    esac
}

# --- Flatpak routing --------------------------------------------------

# Flatpak apps ride the host's tunnel, unless one of them was given a proxy
# — then it exits wherever that proxy exits, on every tunnel from then on,
# and no VPN status will ever mention it. `pvpn up` clears these on each
# connect; this does it once at install, so an existing bypass is gone
# before the first connect rather than after it.
fix_flatpak_routing() {
    command -v flatpak >/dev/null 2>&1 || return 0
    head_ "Checking Flatpak apps"
    if "$BIN/pvpn" apps --fix; then
        ok "no Flatpak app is routed around the VPN"
    else
        warn "some apps could not be fixed — see: pvpn apps"
    fi
}

# --- always-on: surviving suspend and link changes ---------------------

# Opt-in, and deliberately not the default. These are the only files this
# installer puts outside $HOME, and the lid drop-in changes what the machine
# does when you close it.
#
# This is not pvpnd coming back. Nothing polls, nothing holds a `want_up`,
# and nothing is running between events: a resume or a physical link coming
# up starts one bounded attempt, which gives up rather than fighting a
# network that refuses. See docs/always-on.md.

always_on_installed() {
    [[ -f /etc/systemd/system/pvpn-recover.service ]]
}

install_lid_lock() {
    local want="$LID_LOCK" sdver
    if [[ "$want" == ask ]]; then
        if ask_yes "Also stop the lid from suspending (it will lock instead)?"; then
            want=yes
        else
            want=no
        fi
    fi
    if [[ "$want" != yes ]]; then
        note "lid left alone — closing it still suspends and still drops the"
        note "tunnel, but the resume hook now puts both back."
        return 0
    fi
    sudo install -d -m755 /etc/systemd/logind.conf.d
    sudo install -m644 -o root -g root "$SRC/system/10-pvpn-lid-lock.conf" \
        /etc/systemd/logind.conf.d/10-pvpn-lid-lock.conf
    ok "/etc/systemd/logind.conf.d/10-pvpn-lid-lock.conf"
    # Restarting logind keeps sessions alive from systemd 246 on; older
    # versions could drop the graphical session, so those are told to reboot.
    sdver="$(systemctl --version | awk 'NR==1{print $2}')"
    if [[ "$sdver" =~ ^[0-9]+$ ]] && (( sdver >= 246 )); then
        sudo systemctl restart systemd-logind
        ok "logind restarted — the lid policy is live now"
    else
        warn "reboot to apply the lid policy (systemd $sdver is too old to"
        note "restart logind without risking your session)"
    fi
    warn "a closed lid now stays awake — heat in a bag, and battery drain"
}

install_always_on() {
    head_ "Always-on: recovering after suspend and link changes"

    if ! sudo -v; then
        bad "needs sudo — skipped. Re-run: ./setup.sh --always-on"
        return 1
    fi

    # The reconnect runs as you: pvpn reads Proton's session from your
    # keyring, so it needs your session bus and cannot work as root.
    install -m755 "$SRC/bin/pvpn-autoconnect" "$BIN/pvpn-autoconnect"
    ok "$BIN/pvpn-autoconnect"
    mkdir -p "$HOME/.config/systemd/user"
    install -m644 "$SRC/system/pvpn-autoconnect.user.service" \
        "$HOME/.config/systemd/user/pvpn-autoconnect.service"
    ok "$HOME/.config/systemd/user/pvpn-autoconnect.service"
    systemctl --user daemon-reload 2>/dev/null || true

    # Root half: the DNS repair, the resume hook, and the link-up trigger.
    sudo install -m755 -o root -g root "$SRC/system/pvpn-dns-unsnap" \
        /usr/local/sbin/pvpn-dns-unsnap
    ok "/usr/local/sbin/pvpn-dns-unsnap"
    sudo install -m755 -o root -g root "$SRC/system/pvpn-kick-user" \
        /usr/local/sbin/pvpn-kick-user
    ok "/usr/local/sbin/pvpn-kick-user"
    sudo install -m644 -o root -g root "$SRC/system/pvpn-recover.service" \
        /etc/systemd/system/pvpn-recover.service
    ok "/etc/systemd/system/pvpn-recover.service"
    # NetworkManager refuses dispatcher scripts that are not root-owned or are
    # group/world writable, so these modes are load-bearing.
    sudo install -d -m755 /etc/NetworkManager/dispatcher.d
    sudo install -m755 -o root -g root "$SRC/system/90-pvpn-autoconnect" \
        /etc/NetworkManager/dispatcher.d/90-pvpn-autoconnect
    ok "/etc/NetworkManager/dispatcher.d/90-pvpn-autoconnect"

    sudo systemctl daemon-reload
    if sudo systemctl enable pvpn-recover.service >/dev/null 2>&1; then
        ok "pvpn-recover.service enabled on every resume path"
    else
        warn "could not enable pvpn-recover.service"
    fi

    install_lid_lock
    echo
    note "Stop it reconnecting for you at any time:  pvpn-autoconnect --off"
    note "What it does and why:                      docs/always-on.md"
}

remove_always_on() {
    head_ "Removing the always-on hooks"
    systemctl --user stop pvpn-autoconnect.service 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/pvpn-autoconnect.service" \
          "$BIN/pvpn-autoconnect"
    systemctl --user daemon-reload 2>/dev/null || true
    ok "user reconnect removed"

    if sudo -v 2>/dev/null; then
        sudo systemctl disable --now pvpn-recover.service 2>/dev/null || true
        sudo rm -f /etc/systemd/system/pvpn-recover.service \
                   /usr/local/sbin/pvpn-dns-unsnap \
                   /usr/local/sbin/pvpn-kick-user \
                   /etc/NetworkManager/dispatcher.d/90-pvpn-autoconnect \
                   /etc/systemd/logind.conf.d/10-pvpn-lid-lock.conf
        sudo systemctl daemon-reload
        ok "recovery hooks and lid policy removed"
        note "closing the lid suspends again — logind's default is back"
    else
        warn "no sudo — these root-owned files were left in place:"
        note "/etc/systemd/system/pvpn-recover.service"
        note "/usr/local/sbin/pvpn-dns-unsnap"
        note "/usr/local/sbin/pvpn-kick-user"
        note "/etc/NetworkManager/dispatcher.d/90-pvpn-autoconnect"
        note "/etc/systemd/logind.conf.d/10-pvpn-lid-lock.conf"
    fi
}

# --- guided next steps ------------------------------------------------

already_signed_in() {
    # `protonvpn status` exits 0 even when logged out ("Disconnected").
    # `protonvpn info` prints Account: 'None' until sign-in succeeds.
    local acct
    acct="$(protonvpn info 2>/dev/null | awk -F"'" '/^Account:/{print $2; exit}')"
    [[ -n "$acct" && "$acct" != "None" ]]
}

# True when this laptop is already allowing outbound VPN traffic — no changes needed.
# Readable without sudo via /etc/default/ufw + /etc/ufw/ufw.conf.
local_firewall_already_allows() {
    if ! command -v ufw >/dev/null 2>&1 && ! command -v firewall-cmd >/dev/null 2>&1; then
        return 0   # no host firewall tool → nothing blocking locally
    fi

    if command -v ufw >/dev/null 2>&1; then
        local enabled out_pol
        enabled="$(grep -E '^ENABLED=' /etc/ufw/ufw.conf 2>/dev/null | cut -d= -f2 || true)"
        out_pol="$(grep -E '^DEFAULT_OUTPUT_POLICY=' /etc/default/ufw 2>/dev/null \
            | cut -d= -f2 | tr -d '"' || true)"
        # Inactive, or active with ACCEPT outgoing → laptop is not filtering egress.
        if [[ "${enabled:-no}" != "yes" ]]; then
            return 0
        fi
        if [[ "${out_pol:-ACCEPT}" == "ACCEPT" ]]; then
            return 0
        fi
        # Restrictive outgoing — only skip if we already installed our allow rules.
        sudo -n ufw status 2>/dev/null | grep -q 'pvpn:' && return 0
        return 1
    fi

    # firewalld: default is allow outbound; treat as already ok unless we know otherwise.
    return 0
}

# Make sure THIS machine is not the thing dropping outbound UDP / 443.
# School/corp wifi filters are separate — those cannot be fixed locally.
ensure_local_udp_allow() {
    head_ "Local firewall — allow VPN egress from this laptop"

    if local_firewall_already_allows; then
        ok "already allowing outbound — skipping"
        return 0
    fi

    if command -v ufw >/dev/null 2>&1; then
        # Outgoing is DROP/REJECT — add explicit allow-out rules for VPN.
        sudo ufw allow out to any port 443 proto tcp comment 'pvpn: Stealth/HTTPS' >/dev/null
        sudo ufw allow out to any port 1194 proto udp comment 'pvpn: OpenVPN-UDP' >/dev/null
        sudo ufw allow out to any port 51820 proto udp comment 'pvpn: WireGuard' >/dev/null
        sudo ufw allow out to any port 3478 proto udp comment 'pvpn: STUN check' >/dev/null
        sudo ufw allow out to any proto udp comment 'pvpn: general outbound UDP' >/dev/null
        ok "ufw allow-out rules for UDP + TCP/443"
        return 0
    fi

    if command -v firewall-cmd >/dev/null 2>&1 \
        && sudo firewall-cmd --state >/dev/null 2>&1; then
        sudo firewall-cmd --permanent --add-service=https >/dev/null 2>&1 || true
        sudo firewall-cmd --permanent --add-port=1194/udp >/dev/null 2>&1 || true
        sudo firewall-cmd --permanent --add-port=51820/udp >/dev/null 2>&1 || true
        sudo firewall-cmd --permanent --add-port=3478/udp >/dev/null 2>&1 || true
        sudo firewall-cmd --reload >/dev/null 2>&1 || true
        ok "firewalld: opened https + common VPN UDP ports"
        return 0
    fi

    note "No active ufw/firewalld — nothing to allowlist on the laptop"
}

# Prefer Stealth when the network kills general UDP (e.g. detnsw).
prefer_stealth_protocol() {
    local settings="$HOME/.config/Proton/VPN/settings.json"
    head_ "Prefer Stealth (protun-tls) — network blocks general UDP"

    if [[ ! -f "$settings" ]]; then
        # Touch settings via a harmless config call if signed in.
        protonvpn config set ipv6 on >/dev/null 2>&1 || true
    fi
    if [[ ! -f "$settings" ]]; then
        warn "No Proton settings yet — sign in first, then: pvpn up protun-tls"
        return 1
    fi

    /usr/bin/python3 - "$settings" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
d = json.loads(p.read_text())
d["protocol"] = "protun-tls"
p.write_text(json.dumps(d, indent=2) + "\n")
print(d["protocol"])
PY
    ok "default protocol set to protun-tls (TCP/443 Stealth)"
    note "WireGuard/OpenVPN-UDP will keep failing on this wifi — that is expected"
}

run_wizard() {
    [[ "$NO_WIZARD" -eq 1 ]] && return 0
    [[ -t 0 ]] || {
        head_ "Next steps (non-interactive shell — run these yourself)"
        cat <<EOF
  1. sudo systemctl enable --now tor   # if API/repo stay blocked
  2. vpn-check                         # will a VPN work on this wifi?
  3. pvpn login                        # sign in (uses Tor when API is blocked)
  4. pvpn up protun-tls                # Stealth if UDP is filtered
EOF
        return 0
    }

    echo
    head_ "Guided setup"

    if ! systemctl is-active --quiet tor 2>/dev/null; then
        if ask_yes "Start Tor now? (needed when Proton's API is blocked)"; then
            ensure_tor
        else
            note "Skipping Tor. Start later with: sudo systemctl enable --now tor"
        fi
    else
        ok "tor already running"
    fi

    # Laptop firewall — skip entirely when outbound is already allowed.
    if local_firewall_already_allows; then
        ok "local firewall already allows outbound — skipping"
    elif ask_yes "Configure local firewall to allow VPN UDP/TCP egress from this laptop?"; then
        ensure_local_udp_allow
    else
        note "Skipped local firewall. Re-run setup or allow outbound UDP yourself."
    fi

    local check_env="$WORKDIR/vpn-check.env"
    local udp=1 proton_tcp=1 verdict=ok
    if ask_yes "Run vpn-check to see if a VPN can work on this network?"; then
        echo
        "$BIN/vpn-check" --machine "$check_env" || true
        echo
        # shellcheck disable=SC1090
        [[ -f "$check_env" ]] && . "$check_env"
    fi

    local want_stealth=0
    if [[ "${udp:-1}" == "0" ]]; then
        warn "Network UDP egress failed — that is almost always the wifi filter."
        note "Your laptop allowlist cannot unblock school/corp UDP drops."
        want_stealth=1
    fi

    export PATH="$BIN:$PATH"
    if already_signed_in; then
        ok "Proton CLI already has a session"
    else
        note "Not signed in yet. Login goes through Tor (can take a minute)."
        if ask_yes "Sign in to Proton VPN now?"; then
            ensure_tor
            if ! "$BIN/pvpn" login; then
                warn "Login failed. Fix with:  pvpn login"
                note "Common cause: Tor too slow — retry, or: PVPN_LOGIN_API_TIMEOUT=120 pvpn login"
                return 1
            fi
            if ! already_signed_in; then
                warn "Login finished but still no session — try:  pvpn login"
                return 1
            fi
            ok "Signed in"
        else
            note "Sign in later with:  pvpn login"
            note "Connect needs a session — skipping connect step."
            return 0
        fi
    fi

    # Stealth is the primary default whenever the backend is present.
    local connect_proto="protun-tls"
    if "$BIN/pvpn" protocols 2>/dev/null | grep -q '\[OK  \] protun-tls'; then
        prefer_stealth_protocol || true
        if (( want_stealth )); then
            note "UDP is filtered here — Stealth (protun-tls) is the right protocol."
        fi
    else
        warn "Stealth (protun-tls) backend missing — cannot use primary protocol."
        note "Install: python3-proton-vpn-lib + proton-vpn-linux, then re-run setup."
        connect_proto="openvpn-tcp"
        /usr/bin/python3 - <<'PY' 2>/dev/null || true
import json, pathlib
p = pathlib.Path.home()/".config/Proton/VPN/settings.json"
if p.exists():
    d=json.loads(p.read_text()); d["protocol"]="openvpn-tcp"
    p.write_text(json.dumps(d, indent=2)+"\n")
PY
    fi

    if ask_yes "Connect now (pvpn up ${connect_proto})?"; then
        "$BIN/pvpn" up "$connect_proto" || {
            warn "Connect failed. Try:  pvpn try   (walks every protocol)"
        }
    else
        note "Connect later with:  pvpn up   (defaults to Stealth / protun-tls)"
    fi
}

# --- uninstall --------------------------------------------------------

if [[ "${1:-}" == "--uninstall" ]]; then
    # Hooks outside $HOME go first — an uninstall that left a dispatcher
    # script calling a deleted binary would be worse than not uninstalling.
    always_on_installed && remove_always_on
    # The unit and the daemon binary are from older installs; remove them
    # here too so an uninstall does not leave one behind.
    systemctl --user disable --now pvpnd 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/pvpnd.service"
    systemctl --user daemon-reload 2>/dev/null || true
    rm -f "$BIN/pvpn" "$BIN/pvpnd" "$BIN/vpn-check"
    rm -rf "$LIB"
    c '1;32' "Removed pvpn. Your Proton config in ~/.config/Proton was left alone."
    echo "Settings and server lists left at ~/.config/pvpn and ~/.local/share/pvpn/state.json"
    echo "To remove that too:  rm -rf ~/.config/pvpn ~/.local/share/pvpn"
    echo "To remove Proton too:  rm -rf ~/.config/Proton ~/.cache/Proton"
    echo "To remove the Proton CLI package:"
    case "$(detect_pm)" in
        apt) echo "  sudo apt autoremove --purge proton-vpn-cli protonvpn-stable-release" ;;
        dnf) echo "  sudo dnf remove proton-vpn-cli protonvpn-stable-release" ;;
    esac
    exit 0
fi

if (( NO_ALWAYS_ON )) && (( ! ALWAYS_ON )); then
    remove_always_on
    exit 0
fi

# --- main -------------------------------------------------------------

head_ "Detected package manager: $PM"
if [[ "$PM" == "unknown" ]]; then
    bad "Need apt (Debian/Ubuntu) or dnf (Fedora)."
    exit 1
fi

if everything_ready; then
    ok "already installed — skipping package installs"
    # Still run this. "Already installed" was answered by everything_ready,
    # which does not look for the Stealth backend - so an install that
    # predates Stealth support would be told it was up to date and left
    # without the one protocol that works on a filtered network. Both halves
    # of install_proton return immediately when satisfied, so on a machine
    # that really is current this costs a version check and two lines of
    # output.
    install_proton
    install_pvpn >/dev/null   # refresh scripts from this checkout
    fix_flatpak_routing
    if (( ALWAYS_ON )) || always_on_installed; then
        install_always_on || true
    fi
    run_wizard
    exit 0
fi

install_base_deps
install_proton

# Re-check soft deps after installs
head_ "Checking dependencies"
command -v protonvpn >/dev/null && ok "proton-vpn-cli" || { bad "proton-vpn-cli"; exit 1; }
command -v torsocks  >/dev/null && ok "torsocks"       || warn "torsocks missing"
command -v curl      >/dev/null && ok "curl"           || { bad "curl"; exit 1; }
command -v nmcli     >/dev/null && ok "nmcli"          || { bad "nmcli (NetworkManager)"; exit 1; }
[[ -x /usr/bin/python3 ]]      && ok "system python3"  || { bad "/usr/bin/python3"; exit 1; }

if [[ "$(command -v python3)" != "/usr/bin/python3" ]]; then
    note "note: 'python3' on your PATH is $(command -v python3)"
    note "      pvpn always calls /usr/bin/python3 explicitly, so this is fine."
fi

install_pvpn
fix_flatpak_routing
if (( ALWAYS_ON )) || always_on_installed; then
    install_always_on || true
fi

echo
head_ "Done installing"
cat <<'EOF'
  vpn-check          will a VPN work on this wifi?
  pvpn login         sign in to Proton (once)
  pvpn up            connect to the fastest measured server
  pvpn best          rank the fastest servers you can use
  pvpn down          disconnect
  pvpn status        where am I exiting?
  pvpn apps          are my Flatpak apps really on the tunnel?
  pvpn blocked       which servers this network has refused
EOF
if always_on_installed; then
    echo "  pvpn-autoconnect --status   is the tunnel rebuilt for you after a resume?"
fi

run_wizard
