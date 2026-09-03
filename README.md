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

pvpn servers   everything this network has taught us, in one table
pvpn history   every connect attempt here: what, how long, how it went
pvpn working   servers that actually carried traffic here
pvpn fast      servers that measured quick here (not the same thing)
pvpn blocked   what failed here, why, and when it will be tried again
pvpn forget    lift a block by hand
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
(`pvpn servers`, `pvpn history`) — without leaving anything running to do it.

**6. "No traffic" is not a diagnosis.** A tunnel that carries nothing looks
identical whether the server is dead, your wifi dropped, or your own client
certificate expired — and Proton's client will sit in `Connected` through
all three. Blaming the server for the other two retires healthy servers a
day at a time. `pvpn` reads Proton's own log for the verdict and checks that
your uplink is still there before it writes anything down. See
[Verifying a tunnel](#verifying-a-tunnel).

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
./setup.sh --no-wizard      # install only, skip login prompts
./setup.sh --uninstall      # remove the ~/.local pvpn files
./setup.sh --always-on      # also put the tunnel back after a suspend
./setup.sh --no-always-on   # remove those hooks again
```

`--always-on` is opt-in and needs sudo — it is the only part of this
project that writes outside `$HOME`. Without it, closing the lid suspends,
the tunnel dies, and DNS stops resolving until you run `pvpn up` again. See
[docs/always-on.md](docs/always-on.md).

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

pvpn hop                  # the next best server, then the one after
pvpn hop JP               # somewhere in Japan
pvpn hop SG-FREE#12       # that exact server
```

Bare `pvpn hop` does not hand the choice back to Proton. It works down the
list this network has already taught it — **servers proven to carry traffic
here first**, then the measured rank — and keeps going if one fails, up to
four servers. Naming one gets you that one, and one attempt.

## Verifying a tunnel

Every connect ends with the same question, and getting it wrong is
expensive in a way you only notice a week later.

`up` and `hop` watch a fresh tunnel until one of four things is true:

| verdict | what it means | what it costs the server |
|---|---|---|
| **carrying** | a probe reached the internet through it | clears any block, joins `pvpn working` |
| **session killed** | Proton logged the session as over | blocked as `session-killed` |
| **link down** | *your own* uplink went away, or you roamed | **nothing** |
| **quiet** | the window ran out with no traffic and no verdict | blocked as `no-traffic-after-settle` |

Two of those are new, and each fixes a specific failure:

**Reading Proton's log turns a fixed wait into an answer.** Measured on a
school network: the tunnel device came up at 22:09:22, Proton's local agent
gave up on the session at **22:09:46**, and `pvpn` — watching only for
traffic — kept polling until **22:11:36** before writing the server off. The
verdict had been sitting in the log for 110 seconds. It is now read every
second, so a killed session ends the attempt in about twenty rather than
ninety, and the next server is tried that much sooner.

**Checking your own uplink stops it blaming the wrong thing.** A tunnel
carries nothing when the wifi drops, when the laptop roams to another SSID,
or when a captive portal reasserts itself. Recorded as a server failure that
is a healthy server gone from tomorrow's list for 24 hours — 96 if it
happens twice. So before anything is written down, `pvpn` pings the physical
link's own gateway, which is reachable *outside* the tunnel, and compares
the network key to the one the server was chosen for. Positive evidence that
your network died means nothing is blamed and nothing is blocked.

That check is deliberately conservative. A gateway that ignores ICMP with no
fresh ARP entry is *unknown*, not *down*, and unknown changes no decision —
otherwise `pvpn up` would stop working down the list on exactly the networks
it exists for.

What has not changed: a tunnel that is merely **slow** is still never torn
down. Every early exit above is taken on evidence, never on a clock.

## What this network taught us

Two lists, filled two different ways, and the difference is the tool's
oldest lesson:

```bash
pvpn fast                 # measured quick — a TLS handshake time
pvpn working              # actually carried traffic — a real connect
pvpn blocked              # failed here, why, and when it lifts
pvpn servers              # all of it, one table per network
pvpn servers --all        # every network this laptop has learned
pvpn history              # every attempt: server, seconds, outcome
pvpn history --all        # across networks, newest first
pvpn forget SG-FREE#13    # try it again now
pvpn forget --all         # clear this network's blocks
```

A handshake time says how fast *something* answered. Where a transparent
proxy terminates TLS locally that something is the proxy, so `pvpn fast`
says so next to any server that has never carried a packet. `pvpn working`
is the list to trust, and the one `pvpn hop` reaches for first.

`pvpn history` is the copy of the narration that outlives the terminal. Each
line is one attempt — which server, how many seconds until the outcome was
known, and what it was. `link-down` and `cert-expired` are the two nobody is
blamed for; an evening of those is *why* nothing worked, and no server was
written off for any of them.

Nothing measures in the background. These lists are maintained by the
commands you already ran: every `pvpn best` pays for the latencies, every
`pvpn up` and `pvpn hop` pays for the verdicts.

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
  on the same server anyway. What *does* end an attempt early is evidence —
  see [Verifying a tunnel](#verifying-a-tunnel).
- **Telling "this server" from "this network" from "this laptop".** A dead
  server, a killed session, your wifi dropping and your own certificate
  expiring all look the same from the outside: no traffic. Only the first
  two are the server's fault, and only they get written down.
- **Nothing runs behind your back.** There used to be a daemon that
  rebuilt the tunnel whenever it decided one was missing. On a network
  that fails every connect it does not fail with you, it fails at you —
  and debugging a network while something keeps rewriting your routing
  table is not debugging. A tunnel now exists because you asked for one,
  and stays until you say otherwise.
- **Fast vs working vs blocked, as observed on this network.** A fast TLS
  handshake does not prove a server works ([docs/transparent-proxy.md](docs/transparent-proxy.md)).
  Latency comes from the measurement pass `best`/`up` already run; only a
  real connect outcome writes the *working* and *blocked* lists. Blocks
  expire after 24 hours, stretch to 4× that for a server that keeps
  failing, and `pvpn forget` lifts one by hand when you know something the
  record does not.
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
| `PVPN_NETWORK` | | pin the network key instead of deriving it from the SSID |

Persisted observations live in `~/.local/share/pvpn/state.json` — per
network: what each server measured, what it did when actually asked for a
tunnel, and the last 200 attempts. Keys from the old daemon are ignored
rather than rejected, and every field added since is optional, so an
existing config or state file still loads and keeps what it learned.

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

## Surviving suspend

Closing a laptop lid is usually a real suspend, and NetworkManager tears
the tunnel down whenever logind announces one. Your desktop's own
lid setting is often a dead key — logind's `HandleLidSwitch` is what
decides — and Caffeine-style extensions cannot prevent it, because they
take an *idle* inhibitor and logind's lid handling only respects a
`handle-lid-switch` **block** inhibitor.

Worse than the dropped tunnel is what it leaves behind: Proton's leak guard
claims every domain (`~.`) with the nameserver `::1`, systemd-resolved's own
stub, so DNS resolves nothing at all until you reconnect by hand.

```bash
./setup.sh --always-on
```

installs a resume hook that withdraws that stale DNS claim and rebuilds the
tunnel, plus an optional `logind` drop-in that makes a lid close lock
rather than suspend. It is event-driven and bounded — not the old `pvpnd`
supervisor.

A deliberate `pvpn down` is never undone, awake or locked: it holds until
you run `pvpn up`. `pvpn-autoconnect --off` stops the reconnect entirely.
[docs/always-on.md](docs/always-on.md) has the evidence and the reasoning.

## Limits — read before filing a bug

- **A network can pass every reachability check and still refuse the tunnel.**
  Some filters terminate TLS on a local proxy: the handshake completes, then
  the session is closed before any tunnel data moves. `pvpn` now names this
  outcome — `session-killed` in `pvpn blocked` and `pvpn history` — and
  reaches it in seconds rather than after the settle window, but it cannot
  make such a network work. See
  [docs/transparent-proxy.md](docs/transparent-proxy.md).
- **The uplink check needs a gateway that answers, or a kernel that has
  recently seen one.** On a network where the gateway drops ICMP and the ARP
  entry has gone stale, "did my wifi die?" is unanswerable, and `pvpn` falls
  back to the old behaviour of blaming the server. It will not guess.
- **If the network drops your VPN's packets, nothing here helps.** Run
  `vpn-check`; if it says VPNs are blocked by address, that's the answer.
- Free tier gives you ~100 servers to choose between, not the full list.
- UDP-blocking / DPI networks kill WireGuard, OpenVPN-UDP, and often
  OpenVPN-TCP. **Stealth (`protun-tls`) is the default.**
- `pvpn fix --hosts` edits `/etc/hosts`. Verify that file if the command
  is ever killed with -9.
- **A suspend always drops the tunnel.** `--always-on` shortens the outage
  to a reconnect; it cannot make one survive a suspend, and it will not
  rescue a network that refuses every connect anyway.

## License

MIT — see [LICENSE](LICENSE).
