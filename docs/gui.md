# Protun Unblocked — the window

`pvpn-gui` is a GTK4 + libadwaita app that wraps Proton VPN: everything
Proton's own app offers — quick connect, countries and cities, P2P, Secure
Core, Tor and streaming servers, NetShield, kill switch, port forwarding,
custom DNS, VPN Accelerator, moderate NAT, IPv6, account and plan — plus
what `pvpn` adds for networks that filter Proton: measured and proven
servers per network, blocks, history, a connect that cannot strand you, and
the always-on machinery. It runs the installed `pvpn` for everything that
changes the network, Proton's `protonvpn config` for Proton's settings, and
reads what both have written down for everything it shows.

```bash
pvpn                # open the window (in a desktop session; `pvpn help` for commands)
pvpn gui            # the same, explicitly
pvpn tray           # just the tray icon
pvpn-gui --tray     # the same, directly (what "Start with the session" runs)
```

It is also in the app grid and on the desktop. One instance per session: a
second launch hands over to the first. Bare `pvpn` prints the usage text
instead where there is no display, or with `PVPN_NO_GUI=1`.

## The tray comes with pvpn

`pvpn up`, `hop`, `try` and `best --connect` — and `pvpn watch` while a
tunnel is up — ask for the tray icon, whether or not the window was ever
opened. The request goes over the session bus (`app.tray`); if the app is
not running, the bus starts it from
`~/.local/share/dbus-1/services/io.github.dixonsolutions.ProtunUnblocked.service`,
in a cgroup of its own, so a request from inside the autoconnect or watch
unit survives that unit finishing. The icon comes up alone; the window does
not open. Preferences → General → "Tray icon whenever pvpn connects" turns
this off, as does `PVPN_NO_GUI=1` for one command.

## It watches; it does not reconnect

The daemon this project removed was removed because a supervisor on a
network that refuses every connect fights you instead of helping. The window
does not bring it back. Nothing in it decides to connect: the orb, the tray
menu and the buttons run a `pvpn` command because you pressed them.

Rebuilding a tunnel for you is still the job of the two things that always
did it — `pvpn-autoconnect` after a resume or link change, and the
`pvpn-watch.timer` health check — and both still ask `autoconnect_networks`
and `autoconnect_never` first. The window notices them working (they hold
pvpn's connect lock while they do) and tells you.

## Pages

| page | what it is |
|---|---|
| **Countries** | Proton's whole catalogue by country and city, filtered to all / P2P / Secure Core / Tor / streaming, with load, features and what your plan allows (countries you can use sort first). Fastest or random — anywhere, in a country, or in a city — picks by Proton's score and connects through `pvpn hop`, so the connect is guarded and what fails is remembered. Rows say which servers work or are blocked on this network. |
| **Connect** | The orb: grey off, amber while working (click to cancel — SIGINT, so pvpn restores routing), green when the tunnel carries, red when something claims a tunnel that is not there. Protocol choice, Switch server, Measure & connect, Check. A globe with every server this network knows, the ranked ones numbered; drag to turn, scroll to zoom, click one to connect or lift its block. The fastest few as one-click chips. |
| **Servers** | `pvpn servers`, `working`, `fast`, `blocked` and the ranking, per network, with search. Measure & rank (`pvpn best`), quick rank, measure-and-connect, lift one block or all. |
| **History** | `pvpn history`: every attempt, how long, what happened; per network or all. |
| **Logs** | This window's own activity, Proton's client log, the autoconnect / watch / recover journals, certificate renewals, after-connect chores, Tor, NetworkManager and resolved. Follow, filter, copy. |
| **System** | Account (view, sign in — in a terminal, for the prompts — browser bridge, sign out), certificate (status, renew), status / IP / protocols / check / check-and-repair / try every protocol, Flatpak audit / verify / fix, `pvpn fix` and the hosts blackhole (in a terminal, for sudo), and services. |

### Services

Start, stop, restart, enable and disable, with each unit's state and recent
journal: `pvpn-watch.timer`, `pvpn-watch.service`, `pvpn-autoconnect.service`
(user units), and `pvpn-recover.service`, `tor.service`,
`NetworkManager.service`, `systemd-resolved.service`, `tailscaled.service`
(system units, through `pkexec`). `systemd-logind` is deliberately not on the
list — restarting it ends the graphical session.

## Preferences

Kept separate from the connect page, and meant to cover everything:

- **General** — tray icon, close to tray, start with the session, start
  hidden, how often to read the state and to check traffic, the protocol
  for Connect, which `pvpn` to run, ranking options, globe labels.
- **Notifications** — a master switch and one each for *connected*,
  *disconnected*, *reconnecting*, *reconnected* and *failure*; a test button.
- **Proton VPN** — Proton's own client settings: NetShield, kill switch,
  port forwarding, VPN Accelerator, moderate NAT, IPv6, custom DNS,
  anonymous crash reports. Applied with `protonvpn config set`, read back
  from `protonvpn config list`; settings your plan does not include are
  shown locked. Account and plan (`protonvpn info`).
- **Connection** — every key in `~/.config/pvpn/config.toml`. Edited in
  place: comments and unknown keys survive, and nothing is written that
  `pvpn` would refuse to load.
- **Autoconnect** — "never reconnect automatically here" for the network
  you are on, autoconnect on/off, the hold after `pvpn down`, the health
  timer, and the allowed / never network lists.
- **Files** — direct editors for `config.toml`, the window's `gui.toml`,
  Proton's `settings.json`, `/etc/tor/torrc` (checked with
  `tor --verify-config` before saving) and `/etc/tor/torsocks.conf`. Every
  save keeps the previous version in `~/.cache/pvpn-gui/backups/`.

The window's own settings are in `~/.config/pvpn/gui.toml`.

## Notifications

What the window reads every few seconds becomes one of: disconnected,
connecting, reconnecting (the lock is held by the autoconnect hook or the
watch timer), connected, or broken (routes into a tunnel that answers
nothing, or a client that believes in a tunnel the kernel does not route
through). The changes between those are the five notifications. The
connect lock is checked every second on its own, so even a one-second
fast-path reconnect is seen as a reconnect. Clicking a notification opens
the window.

## Tray

A StatusNotifierItem, drawn in the orb's colour. On GNOME it needs an
AppIndicator host — the *Ubuntu AppIndicators* or *AppIndicator and
KStatusNotifierItem Support* extension. Without one the window works as
before and the icon appears as soon as a host does.

## Building

`setup.sh` builds and installs it when GTK4 ≥ 4.16 and libadwaita ≥ 1.7
development files are present (`./setup.sh --gui` installs them). By hand:

```bash
cargo build --release -p pvpn-gui
install -m755 target/release/pvpn-gui ~/.local/bin/
```

`PVPN_GUI_SNAPSHOT_DIR=/tmp/shots pvpn-gui` renders every page to PNG and
quits — under `xvfb-run` with `GDK_BACKEND=x11 GSK_RENDERER=cairo` it needs
no screen. `PVPN_GUI_DEBUG=1` prints every phase change and notification.

Map data: Natural Earth 1:110m land (public domain).
