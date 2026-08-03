# pvpn

A wrapper around Proton VPN's Linux CLI that makes it usable on networks
which filter it — school, corporate, or public wifi — without stranding you
without internet when a connect fails.

```
pvpn up        connect          pvpn status   where am I exiting?
pvpn best      rank servers     pvpn apps     what skips the tunnel?
pvpn down      disconnect       pvpn try      try every protocol
pvpn login     sign in          vpn-check     will a VPN work on this wifi?
```

## Why this exists

Proton's own client works fine on an open network. On a filtered one it has
three problems that make it feel broken rather than blocked:

**1. It hijacks your routing before the tunnel is up.** `protonvpn connect`
installs a full-tunnel route the moment it *starts* connecting. If the tunnel
never comes up, every packet blackholes until it gives up ~2 minutes later.
Your browser just hangs. `pvpn` wraps this so a failure, a timeout, or Ctrl-C
always restores your normal routing.

**2. It spends 30 seconds calling an API it cannot reach.** Where Proton's API
domains are DNS-blocked, `connect` still tries to refresh two of them first:

```
/vpn/v1/logicals      -> "No working transports found"   15.0s
/vpn/v2/clientconfig  -> "No working transports found"   15.0s
CONN.CONNECT:START -> Connected                           0.3s
```

The tunnel takes **0.3 seconds**. The other 30 are `AutoTransport.TRANSPORT_TIMEOUT
= 15`, hit twice. Both refreshes are optional — the client proceeds with cached
data — so `lib/sitecustomize.py` lowers that to 2s via `PYTHONPATH`. **Measured:
40s → 10s.** Nothing outside pvpn's own subprocesses is affected.

**3. Its health checks obey your proxy.** If you export `HTTPS_PROXY`/`ALL_PROXY`,
naive checks measure the proxy rather than the tunnel — and a proxy's circuits
break the instant the tunnel takes over routing, so a working VPN looks dead.
Every check here uses `curl --noproxy '*'`.

**4. Its "fastest server" is not measured.** Proton ranks servers by a
server-side `Score`. Measured from Sydney on a free account, its top pick is
Amsterdam at **260 ms**, while Singapore — which it never suggests — answers in
**99 ms**. `pvpn best` times the servers you can actually use and ranks those;
`pvpn up` connects to the winner. See [docs/best-server.md](docs/best-server.md).

## Install

```bash
git clone https://github.com/luohoa97/protun-unblocked.git && cd protun-unblocked && ./setup.sh
```

`setup.sh` detects **apt** (Debian/Ubuntu) or **dnf** (Fedora), installs
`proton-vpn-cli` + Tor/NetworkManager deps, then installs `pvpn` under `$HOME`
and walks you through `vpn-check` → `pvpn login` → `pvpn up`.

If `repo.protonvpn.com` is blocked, the Proton repo package and apt/dnf
refreshes for that host go through Tor automatically (`socks5h://127.0.0.1:9050`).

```bash
./setup.sh --no-wizard   # install only, skip login prompts
./setup.sh --uninstall   # remove ~/.local pvpn files
```

## Usage

```bash
vpn-check          # is a VPN even possible on this network?
pvpn login         # once
pvpn up            # measure servers, connect to the fastest
```

To see what it picked and why, or to choose differently:

```bash
pvpn best                 # rank the servers you can use
pvpn best --connect       # rank them, then connect to the best
pvpn best --country JP    # one country only
```

Every connect also checks that your Flatpak apps are on the tunnel and puts
back any that a proxy setting had taken off it. To look at that yourself:

```bash
pvpn apps                 # anything routed around the tunnel?
pvpn apps --fix           # put it back on
pvpn apps --verify        # start the apps and compare their real exit
```

`vpn-check` reports whether Proton's servers are reachable, whether UDP can
leave, and whether DNS is hijacked — so you know if the problem is the network
before you spend time on the client.

## What it handles

- **Slow settling — the big one.** A tunnel can take a long time to pass its
  first packet: 12s in one measurement, and **over 20s** on US-FREE#15, which
  a short window wrote off as dead. It then worked on the next attempt using
  *the same server*. So `PVPN_SETTLE` defaults to 45s. That costs nothing when
  things work — probes return in ~0.3s and the loop exits the instant traffic
  flows — and only a dead tunnel ever pays it. **Don't lower it to "make
  connects fast"; that just discards tunnels that were about to work.**
- **Free-tier server roulette — fixed.** Proton's CLI refuses to let a free
  account name a server, so a failed connect used to retry by asking for "the
  fastest" again and landing on the same one: two consecutive attempts were
  observed both picking `US-FREE#15`. `pvpn up` now measures first and holds a
  *ranked* list, so attempt 2 goes to the second-best server rather than
  repeating the first. The shim narrows Proton's blanket refusal to the tier
  check the client already uses, so a free account can use its own free
  servers — nothing above your tier is reachable, then or now.
  See [docs/best-server.md](docs/best-server.md).
- **Apps that quietly skip the tunnel.** A proxy setting takes an app off the
  VPN without anything in `pvpn status` showing it. Found here: ZapZap carried
  a leftover `ALL_PROXY=socks5://127.0.0.1:9050`, so the host exited in Mexico
  City while that one app exited through a Tor relay in Vienna. Nothing about
  that decays on its own, so `pvpn up` clears it on every connect (0.34s
  across 57 apps) and says what it removed; `pvpn apps` is there to check.
  Flatpak itself is not the problem — sandboxes share the host's network, and
  Xonotic was verified on the tunnel over both TCP and UDP.
  See [docs/flatpak.md](docs/flatpak.md).
- **Measuring through a tunnel.** Probing servers while connected times the
  tunnel, not the path to each server: connected via Mexico City, the ranking
  picked Mexico City. `pvpn best --connect` disconnects before measuring, and
  `pvpn best` warns when a tunnel is up.
- **Self-resurrecting tunnels.** Proton's NM profile is created with
  autoconnect on, so `pvpn down` was observed being undone by NetworkManager
  30 seconds later. `pvpn down` now clears that flag. Proton also runs a
  reconnection daemon as root, which can beat you to a server; `pvpn up` moves
  off it when you asked for a specific one.
- **Stray kill-switch.** Proton creates `pvpnksintrf0` while connecting even
  with the kill switch off. An interrupted connect can leave it behind,
  blackholing everything. `pvpn down` removes it explicitly.
- **Blocked API for login.** `pvpn login` routes account traffic through Tor,
  with a shim that forces aiohttp onto its threaded resolver — torsocks cannot
  proxy the UDP that `aiodns` uses, so lookups fail without it.

## Tuning

| variable | default | meaning |
|---|---|---|
| `PVPN_TIMEOUT` | 30 | seconds to wait for a tunnel |
| `PVPN_SETTLE` | 45 | grace period for traffic to start (cheap: exits as soon as traffic flows) |
| `PVPN_ATTEMPTS` | 3 | connect attempts (new server each time) |
| `PVPN_API_TIMEOUT` | 2 | Proton API transport timeout |
| `PVPN_FAST` | 0 | blackhole the API in `/etc/hosts` for the connect (needs sudo) |
| `PVPN_BEST` | 1 | measure and pick the server; 0 leaves the choice to Proton |
| `PVPN_BEST_TIMEOUT` | 90 | seconds allowed for a measurement run |
| `PVPN_FIX_APPS` | 1 | put Flatpak apps back on the tunnel; 0 only reports |

## Tests

```bash
tests/run-tests.sh
```

Offline and read-only — no connect, no disconnect, no routing changes, and no
need to be signed in. Safe to run with the VPN up.

## Limits — read before filing a bug

- **If the network drops your VPN's packets, nothing here helps.** Run
  `vpn-check`; if it says VPNs are blocked by address, that's the answer, on
  any OS.
- Free tier gives you ~100 servers to choose between, not the full list. `pvpn
  best` finds the best of those; it cannot conjure a closer one.
- UDP-blocking / DPI networks kill WireGuard, OpenVPN-UDP, and often OpenVPN-TCP
  (TCP connects, TLS handshake dies). **Stealth (`protun-tls`) is the default**
  and the protocol that survives — needs `python3-proton-vpn-lib` +
  `proton-vpn-linux` (NM protun plugin; currently in Proton unstable).
- `PVPN_FAST=1` is the least-tested path — it edits `/etc/hosts` and removes
  its own block on exit, but verify `/etc/hosts` if it's ever killed with -9.

## License

MIT — see [LICENSE](LICENSE).
