# pvpn

A wrapper around Proton VPN's Linux CLI that makes it usable on networks
which filter it — school, corporate, or public wifi — without stranding you
without internet when a connect fails.

One binary. You type a command, it does the work, it exits — nothing runs
in the background and nothing reconnects on its own. What it *remembers*
is what this network taught it: a per-network fast/blocked list of servers
and the ranked order to try them in, so the two minutes you spend finding
a server that works are not spent again tomorrow.

```
pvpn up        connect          pvpn status   where am I exiting?
pvpn best      rank servers     pvpn apps     what skips the tunnel?
pvpn hop       change server    pvpn try      try every protocol
pvpn down      disconnect       vpn-check     will a VPN work on this wifi?
pvpn login     sign in          pvpn fix      privileged cleanup (sudo)
pvpn fast      what this network measured as quick
pvpn blocked   what this network refused, and why
```

See [docs/architecture.md](docs/architecture.md) for how it fits together,
including why the daemon this used to have was removed.

## Why this exists

Proton's own client works fine on an open network. On a filtered one it has
problems that make it feel broken rather than blocked:

**1. It hijacks your routing before the tunnel is up.** `protonvpn connect`
installs a full-tunnel route the moment it *starts* connecting. If the tunnel
never comes up, every packet blackholes until it gives up. `pvpn` wraps this
so a failure, a timeout, or Ctrl-C always restores your normal routing.

**2. It spends 30 seconds calling an API it cannot reach.** Where Proton's API
domains are DNS-blocked, `connect` still tries to refresh them first. The
tunnel takes **0.3 seconds**; the other 30 are doomed API calls.
`lib/sitecustomize.py` lowers that timeout via `PYTHONPATH`. **Measured:
40s → 10s.**

**3. Its health checks obey your proxy.** Every check here uses
`curl --noproxy '*'`.

**4. Its "fastest server" is not measured.** Proton ranks by a server-side
`Score`. From Sydney, its top pick is Amsterdam at **260 ms**, while
Singapore — which it never suggests — answers in **99 ms**. `pvpn best`
times the servers you can actually use; `pvpn up` connects down that list.
See [docs/best-server.md](docs/best-server.md).

**5. A one-shot script forgets everything.** The bash tool (`legacy/pvpn.sh`)
remeasured on every `-c` and could not remember which servers this network
actually let through. `pvpn` writes that down instead
(`pvpn fast`, `pvpn blocked`) — without leaving anything running to do it.

## Install

```bash
git clone https://github.com/luohoa97/protun-unblocked.git && cd protun-unblocked && ./setup.sh
```

`setup.sh` detects **apt** (Debian/Ubuntu) or **dnf** (Fedora), installs
`proton-vpn-cli` + Tor/NetworkManager deps, builds `pvpn`, installs it
under `$HOME`, and walks you through `vpn-check` → `pvpn login` →
`pvpn up`. If you have an older install with the `pvpnd` daemon, setup
stops and removes it; your fast/blocked lists are left alone.

Needs a Rust toolchain (`cargo`). If it is missing, setup offers to
install rustup for your user.

If `repo.protonvpn.com` is blocked, the Proton repo package and apt/dnf
refreshes for that host go through Tor automatically (`socks5h://127.0.0.1:9050`).

```bash
./setup.sh --no-wizard   # install only, skip login prompts
./setup.sh --uninstall   # remove the ~/.local pvpn files
```

## Usage

```bash
vpn-check          # is a VPN even possible on this network?
pvpn login         # once
pvpn up            # measure servers, connect to the fastest
```

`up`, `down`, `hop` and `best` narrate as they go — which servers were
measured, what was picked and why, when the tunnel is up and traffic is
being checked. A connect on a hostile network can take two minutes, and
watching it work is most of how you tell "slow" from "blocked".

`pvpn status` checks the routing table, not just what Proton believes. A
client that lost its session keeps reporting the server it lost while
every packet leaves in the clear; status says so, and exits non-zero.

To see what it picked and why, or to choose differently:

```bash
pvpn best                 # rank the servers you can use
pvpn best --connect       # rank them, then connect to the best
pvpn best --country JP    # one country only

pvpn hop                  # anywhere but the server you are on
pvpn hop JP               # somewhere in Japan
pvpn hop SG-FREE#12       # that exact server
```

Every connect also checks that your Flatpak apps are on the tunnel and puts
back any that a proxy setting had taken off it:

```bash
pvpn apps                 # anything routed around the tunnel?
pvpn apps --fix           # put it back on
pvpn apps --verify        # start the apps and compare their real exit
```

`pvpn fix` is the interactive, sudo-needed half of cleanup: deleting a
stray `pvpnksintrf0`, and optionally blackholing Proton's API in
`/etc/hosts`.

## What it handles

- **Slow settling — the big one.** Time until a tunnel passes its first
  packet has been measured at **12s**, **>20s**, **>45s**. Every tunnel
  written off as dead turned out to be alive moments later. So
  `settle_secs` is 90, and running out of it **does not tear the tunnel
  down.** Discarding a slow tunnel costs a full reconnect and often lands
  on the same server anyway.
- **Nothing runs behind your back.** There used to be a daemon that
  rebuilt the tunnel whenever it decided one was missing. On a network
  that fails every connect it does not fail with you, it fails at you —
  and debugging a network while something keeps rewriting your routing
  table is not debugging. A tunnel now exists because you asked for one,
  and stays until you say otherwise.
- **Fast vs blocked, as observed on this network.** A fast TLS handshake
  does not prove a server works ([docs/transparent-proxy.md](docs/transparent-proxy.md)).
  Latency comes from the measurement pass `best`/`up` already run; only a
  real connect outcome writes the *blocked* list. Blocks expire after 24
  hours, and stretch to 4× that for a server that keeps failing.
- **Nobody chooses the server, so nothing measures it.** `pvpn up` connects
  down a freshly measured rank. Free accounts can name in-tier servers
  because `lib/sitecustomize.py` narrows Proton's blanket refusal to the
  tier check the client already uses. `pvpn hop` steers the local cache
  for "anywhere but here" or a whole country.
- **Apps that quietly skip the tunnel.** A leftover proxy override takes
  an app off the VPN without anything in `pvpn status` showing it.
  See [docs/flatpak.md](docs/flatpak.md).
- **Self-resurrecting tunnels.** Proton's NM profile is created with
  autoconnect on. `pvpn down` clears that flag.
- **Blocked API for login.** `pvpn login` routes account traffic through Tor
  when needed, with a shim that forces aiohttp onto its threaded resolver.

## Config

`~/.config/pvpn/config.toml` — env vars still override for one-off tuning:

| file / env | default | meaning |
|---|---|---|
| `auto_best_server` / `PVPN_BEST` | true | connect down a measured rank, not Proton's pick |
| `connect_timeout_secs` / `PVPN_TIMEOUT` | 30 | seconds to wait for a tunnel |
| `settle_secs` / `PVPN_SETTLE` | 90 | grace period for traffic to start |
| `blocked_retry_after_hours` | 24 | re-try a blocked server after this long |
| `country` / `PVPN_BEST_COUNTRY` | | optional country filter |
| `fix_apps` / `PVPN_FIX_APPS` | true | put Flatpak apps back on the tunnel |

Persisted observations live in `~/.local/share/pvpn/state.json`. Keys from
the old daemon are ignored rather than rejected, so an existing config
file still loads.

## Tests

```bash
tests/run-tests.sh
```

Offline and read-only for the suite — no connect, no disconnect, no routing
changes, and no need to be signed in. Safe to run with the VPN up.
Includes `cargo test` (ranking/geo/state/cache match the Python fixtures)
and a Rust CLI check of `pvpn best --quick` against
`lib/best-server.py --no-probe` on the same fixture.

The deprecated bash script is still exercised as `legacy/pvpn.sh`.

## Limits — read before filing a bug

- **A network can pass every reachability check and still refuse the tunnel.**
  Some filters terminate TLS on a local proxy: the handshake completes, then
  the session is closed before any tunnel data moves. See
  [docs/transparent-proxy.md](docs/transparent-proxy.md).
- **If the network drops your VPN's packets, nothing here helps.** Run
  `vpn-check`; if it says VPNs are blocked by address, that's the answer.
- Free tier gives you ~100 servers to choose between, not the full list.
- UDP-blocking / DPI networks kill WireGuard, OpenVPN-UDP, and often
  OpenVPN-TCP. **Stealth (`protun-tls`) is the default.**
- `pvpn fix --hosts` edits `/etc/hosts`. Verify that file if the command
  is ever killed with -9.

## License

MIT — see [LICENSE](LICENSE).
