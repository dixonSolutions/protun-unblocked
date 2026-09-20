# How pvpn is put together

`pvpn` is one binary. You type a command, it does the work, it exits.
Nothing is left running, nothing reconnects on its own, and nothing has an
opinion about what should be up while you are not looking.

This does **not** reimplement Proton's VPN protocols, account auth, or
CAPTCHA. Those stay `protonvpn` plus the Python shims in `lib/`, invoked as
subprocesses. What is Rust here is ranking, connect orchestration, and the
persisted per-network knowledge about servers.

## Layout

```
crates/pvpn-core/   rank, probe, geo, serverlist, state, config, proc, link, intent, lock
crates/pvpn/        the CLI: connect, verify, watch, blocklist, session, narration
lib/                Python shims loaded into protonvpn via PYTHONPATH
system/             opt-in suspend/resume recovery (see always-on.md)
legacy/             the previous bash tool, and the removed daemon
```

Three of those exist only to answer one question — *when a tunnel carries no
traffic, whose fault is it?* — because getting that wrong is silent and
compounding:

- **`pvpn-core::link`** asks whether the network *under* the tunnel is
  still there, by pinging the physical uplink's own gateway. That gateway
  is reachable outside the tunnel, so it answers while everything else on
  the machine is routed through a tunnel that may be dead. It returns
  `Up`/`Down`/**`Unknown`**, and only `Down` — positive evidence — changes
  any decision.
- **`pvpn::verify`** watches a fresh tunnel and returns one of four
  verdicts: carrying, session died, link down, quiet. It ends the attempt
  the moment any of them is true, so a killed session costs about twenty
  seconds instead of the full ninety-second settle window — but it never
  ends one on a clock alone, because every tunnel this tool ever wrote off
  on a timeout turned out to be merely slow.
- **`pvpn::watch`** asks the same question about a tunnel that is already
  established, which `verify` structurally cannot: `verify` returns the
  moment a fresh tunnel proves itself, and then the process exits. A tunnel
  killed an hour later left no trace anywhere — measured on `wifi:detnsw`,
  a protun-tcp carrier died three minutes in and `proton0` kept its routes,
  so every route-based check on the machine went on reporting a healthy
  tunnel while DNS hung. `watch` reuses `verify`'s definition of "carrying"
  (`carrying_now`) so the two cannot drift, confirms a bad answer three
  times before acting, and consults `link` before blaming a server for what
  was really the wifi.

Immediately before each `up` attempt, the CLI separately proves that the
configured local DNS resolver answers and that ordinary HTTPS works without
the tunnel. If either baseline is already broken, no server is attempted or
blocked. This prevents a dead local filtering resolver from turning every
hostname-based tunnel probe into false evidence against the server.

Carrying traffic records success; a dead Proton session or a quiet tunnel
records a server failure. A confirmed physical-link outage records only the
attempt. Otherwise, walking out of wifi range could remove a healthy server
from tomorrow's ranked list for a day, and four days the second time.
Pre-tunnel failures default to `client-error`, which records the attempt but
does not block the requested server or remove its saved profile. Only explicit
refusal, TLS-handshake, or session-death evidence can attribute such a failure
to the server; account restrictions, authentication, certificate, unknown
client errors, and local-link failures cannot.

One authentication message is a known false alarm. When Secret Service is
locked, Proton's CLI prints `Authentication required` / `Please sign in`
without ever starting a tunnel. The SSO session is still in the keyring —
`protonvpn signin` then refuses with "Already signed in". `pvpn` reads
`KeyringLocked` from Proton's own log, classifies the attempt as
`keyring-locked`, tells you to unlock the keyring rather than sign out, and
on a named hop tries the locally saved NetworkManager profile (which already
holds the credentials).

An explicit `pvpn hop <server>` prefers Proton's current inventory. If Proton
marks that exact server unavailable but still supplies an endpoint, `pvpn`
warns, temporarily enables only that endpoint in the cache, and verifies the
result rather than assuming the flag means the server is dead. If that current
endpoint cannot carry traffic—or Proton no longer supplies one—the maintained
local NetworkManager profile is the fallback. Only failure of both paths is
recorded. If Proton starts a different server, `pvpn` disconnects it instead of
verifying or blocking it as though it were the requested target. Inventory
freshness comes from Proton's embedded expiration time; load-only updates also
rewrite the file, so its modification time is not evidence of freshness.

### Saved system VPNs

Proton's Linux backend creates a temporary NetworkManager profile for each
connection and removes it on disconnect. While connected, that live
`ProtonVPN <server>` profile is the only entry shown. Before `pvpn down` or
`pvpn hop` tears down a tunnel that carried verified traffic, `pvpn` preserves
the profile under that same plain name with autoconnect disabled. This avoids
both a status suffix and a duplicate active/saved pair.

That list follows the same evidence policy as the blocklist. A confirmed
server failure removes its saved profile; local DNS, certificate, or physical
network failures do not. A later successful connection refreshes the profile
with the current Proton credentials while retaining only one entry.
`pvpn` also recognizes a profile activated from desktop Network Settings,
even though Proton's own CLI state machine reports that manual activation as
disconnected. Running `pvpn up` or `pvpn hop` is itself an explicit request
for a usable session. `pvpn up` first verifies an existing tunnel and returns
immediately when it is already carrying traffic. Otherwise it preserves a
proven profile, disconnects the stale tunnel, and establishes and verifies a
new one. Hop and
teardown preserve only a profile NetworkManager currently reports as active;
a stale `protonvpn status` value cannot trigger profile preservation after the
real tunnel has already disappeared.
Teardown also remembers the active profile UUID and removes that exact
transient copy if Proton leaves it beside an existing same-name saved profile;
the backend can otherwise display both as disconnected for tens of seconds.

### Connection hot path

A server that has already carried traffic on the current network has earned a
fast path. `pvpn up` orders those servers by observed activation-to-verified
traffic time and reliability, then activates the best saved NetworkManager
profile directly over D-Bus. This avoids inventory refresh, server probing,
Python client startup, and repeated protocol discovery. If D-Bus or the saved
profile is unavailable, the existing `nmcli` and Proton client path remains the
fallback.

The tunnel itself is not where the time goes. Measured on `wifi:detnsw`,
2026-09-21: NetworkManager's `connection-activate` to the protun plugin's
`tunnel established` is 0.70s — the plugin's first handshake probe is lost
behind the transparent proxy and its retry fires at 0.5s — and a `pvpn up`
from a clean disconnect now takes 1.05s end to end: 0.06s of preflight,
0.10s for NetworkManager to start the plugin, the plugin's 0.70s, one round
trip for the first probe to answer, and 0.07s to record the result. Before
this work the same connect took between 4.7s and 29s, and everything in the
difference was this tool's own overhead. Each piece was removed for a reason
it is worth keeping (`RUST_LOG=pvpn=debug pvpn up` prints the `t+…s` split):

- **Only a profile that runs a transport this network passes is a
  candidate.** The proven list is per server; the saved profile's backend
  is whatever it was when the profile was saved. On 2026-09-21 the best
  proven server's profile was OpenVPN over UDP, on a network that drops VPN
  UDP, and the fast path waited thirty-five seconds for a handshake that was
  never coming before starting Proton's client. A protun profile says which
  transport it is — its `settings` JSON lists the ports per peer — and
  `State::profile_protocol_is_plausible` requires the family (`protun`,
  `openvpn`, `wireguard`) to appear among this network's recent successes.
  Up to three qualifying profiles are tried before Proton's client is.
- **The certificate's expiry is remembered, not re-read.** Reading it means
  starting Python and importing Proton's loader — 320ms — and the two
  timestamps change only when a certificate is issued. They are kept in
  `~/.local/share/pvpn/cert.json`, trusted until Proton's own renewal point,
  and cleared by a sign-in or sign-out. Past that point the keyring is asked
  again and the file replaced, so a stale copy costs one Python start and
  then stops being stale.
- **The probe endpoints are looked up before the routes change.** Activating
  a protun profile points the resolver through the tunnel before the
  WireGuard handshake has finished, so the first hostname lookups sat in
  the tunnel for systemd-resolved's timeout: the tunnel was established at
  0.75s and verified at 4.7s. `net::ProbeSet` resolves the three probe hosts
  on the ordinary network first, and each probe is then one TCP connect via
  curl's `--resolve`. Rounds are fired every 100ms without waiting for the
  previous one, and the first to answer ends the verification.
- **A proven server's patience is its own record.** The ninety-second settle
  window is right for a server that has never been timed here. For one whose
  history says it carries in a second, ten seconds of silence is a verdict,
  not slowness: the window is four times the server's recorded time, never
  under ten seconds and never over the configured settle, so a dead proven
  server costs seconds before the next one is tried.
- **Proton's client is asked nothing on the way in.** `protonvpn status` is
  3.7s of Python, and `up` ran it on every connect from a clean disconnect
  to learn whether the client still believed it had a tunnel. What it
  reports comes from `~/.cache/Proton/VPN/connection/connection_persistence.json`,
  written when the client starts a connection and removed when it tears one
  down; that file is read directly (`proc::proton_client_persisted`), and
  `protonvpn disconnect` — another Python start — is only run when the file
  says there is something for the client to disconnect. A tunnel this tool
  activated over D-Bus is never in that file and is deactivated over D-Bus.
  `pvpn status` reads the same file and dropped from 4.5s to 0.6s for it.
- **Nothing that does not need the terminal waits at it.** The Flatpak audit
  (one `flatpak info` per installed app — three seconds of CPU on the test
  machine) and the certificate renewal (a Python start, two API probes, and
  the request itself, over Tor where the API is blocked) run as `pvpn
  after-connect` in a process of their own, detached from the terminal's
  process group so Ctrl-C cannot cut them off, narrating into
  `~/.local/share/pvpn/after-connect.log`. It does its two jobs and exits;
  nothing polls, nothing holds state, nothing reconnects. `pvpn watch` starts
  the renewal half of it (`pvpn cert --renew --if-due`, log at
  `cert-renew.log`) whenever it finds the tunnel carrying, so the renewal
  window is never missed for want of a connect falling inside it; a renewal
  that fails is not retried for fifteen minutes.

Success is never inferred from activation alone. NetworkManager must report an
active Proton tunnel and one of several independent internet probes must pass.
Those probes race rather than queue, so a filtered endpoint cannot delay a
healthy endpoint. DNS and ordinary-internet preflight checks likewise run
concurrently before routes change — and concurrently with the probe lookups
and the certificate check, since none of the three needs the others. Fixed
post-disconnect sleeps were replaced with bounded readiness polling; the
command continues as soon as the old tunnel is actually gone.

The saved profile the hot path activates exists because a teardown preserved
it, and that preservation used to trust `protonvpn disconnect`'s exit code.
On 2026-09-21 the client removed the profile, logged `Disconnected`, then
exited non-zero because its follow-up location request hit the `/etc/hosts`
blackhole — and the clone that had just been made was deleted for it, so the
next connect took the slow path. Preservation now waits for NetworkManager to
release the profile, and deactivates it directly if the client did not.

The latency stored as `ready_ms` starts before activation and ends only when
traffic is verified. It includes VPN-plugin retries and therefore predicts the
wait a user experiences better than TLS handshake latency. Older state and
history files load with this field absent and learn it on their next successful
connection.

### One connect at a time

NetworkManager's `protun` plugin allows exactly one active connection, so two
overlapping connects do not double the odds of a tunnel — they guarantee a
refusal: `The 'protun' plugin only supports a single active connection`, which
the CLI surfaces as a bare `SystemExit: 1`. Measured on 2026-09-11: a manual
`pvpn hop` overlapped `pvpn-autoconnect`'s `pvpn up`, the hop's connect was
refused, and the hop's failure cleanup then tore down the working tunnel the
autoconnect had just built.

`up`, `hop`, `try`, `best --connect` and `down` now hold an flock on
`$XDG_RUNTIME_DIR/pvpn-connect.lock` for their whole duration
(`pvpn-core::lock`). A second command waits, saying who it is waiting for —
the holder writes its pid and command line into the file — and Ctrl-C during
the wait exits outright, because a waiter has touched nothing and holds
nothing. The kernel releases the lock when the holder dies for any reason, so
a stale lock cannot exist. `down` alone gives up waiting after 30 seconds and
tears down regardless: it is the off switch, and an off switch that waits out
someone else's whole connect is broken in the other direction.

The same incident drives two smaller rules. Disconnecting waits until
NetworkManager has actually let go of the old profile — Proton's state machine
saying `Disconnected` is not that, and connecting on top of a half-torn-down
profile is the refusal above — with a direct `nmcli` deactivation as the
fallback the narration always claimed was happening. And `pvpn hop` naming the
server you are already on is a no-op: it verifies the tunnel is carrying and
keeps it, rather than tearing a working tunnel down to rebuild it.

## There was a daemon; it is gone

`pvpnd` was a long-running user service with a supervisor loop that polled
`protonvpn status` every five seconds and rebuilt the tunnel whenever it
decided one was missing. It solved a real problem — a one-shot script
forgets everything — and it created a worse one.

On a network that fails every connect, a supervisor does not fail with
you. It fails *at* you, forever, and the only way to make it stop is to
find and disable a service you did not know you had. `want_up` was
persisted to disk, so a reboot resumed the fight. `pvpn down` cleared it,
but only for as long as nothing else set it again. Diagnosing a broken
network while something keeps rewriting your routing table underneath you
is not debuggable.

The state was worth keeping. The process was not. So the state moved into
`state.json`, the connect logic moved into the CLI, and the supervisor and
background prober were deleted:

| daemon did | now |
|---|---|
| supervisor rebuilt the tunnel on a drop | nothing does; run `pvpn up` |
| background sweep maintained the fast list | the measurements `best`/`up` already run are recorded |
| socket RPC narrated over `$XDG_RUNTIME_DIR/pvpn.sock` | the work is in your terminal, so it prints there |
| `want_up` on disk resumed a tunnel after a reboot | a tunnel exists because you asked, and until you say otherwise |
| `journalctl --user -u pvpnd` | `pvpn logs` reads Proton's own log |

The behaviour worth having survived the move. `pvpn up` still walks a
freshly measured ranked list rather than accepting Proton's pick, still
refuses to tear down a merely slow tunnel, still tells our own expired
certificate apart from a dead server, and still writes down what it
learns.

The removed source is kept at `legacy/pvpnd/` for reference.

### What came back, and what did not

One real problem outlived the daemon: a suspend takes the tunnel with it,
and Proton's leak guard leaves DNS pointed at `::1` afterwards, so the
machine cannot resolve anything until you notice and run `pvpn` yourself.
That is not a supervisor's problem to solve — it is a single event with a
single response.

`setup.sh --always-on` installs that response, and it is shaped to keep
every objection above satisfied: nothing polls, nothing runs between
events, retries are capped at three, and a resume or a link coming up is the
only thing that starts it. It is opt-in and it names itself
(`pvpn-autoconnect --status`).

It does persist one thing, and the direction is the argument. `pvpn down`
writes a `down-by-user` marker that `pvpn up` and `pvpn hop` clear, so a
suspend cannot put back a tunnel you just turned off. `want_up` fought you;
`want_down` can only ever cause less to happen. See
[always-on.md](always-on.md).

One thing did come back on a clock, and it is worth naming the concession.
`--always-on` installs `pvpn-watch.timer`, which runs `pvpn watch` every two
minutes. The daemon's polling is the single practice this document argues
hardest against, so the difference has to carry weight:

| `pvpnd`'s supervisor | `pvpn-watch.timer` |
|---|---|
| resident process, five-second loop | a process that starts, asks, and exits |
| asked `protonvpn status` — the client's opinion | sends a packet and waits for an answer |
| rebuilt a tunnel that was never there | exits immediately when no tunnel is up |
| held `want_up` across reboots | holds nothing between firings |
| silent about what it decided | records every verdict in `pvpn history` |

The last two rows are the point. A supervisor rebuilds tunnels because it
believes one *should* exist; this only ever examines one that already does,
and what it mostly produces is not a reconnect but a line in the history
saying that a server which connects here does not necessarily stay connected
here. Nothing else on the machine was writing that down.

## State — `~/.local/share/pvpn/state.json`

The observations are filed **per network** and populated in different ways. A
fast TLS handshake does not prove a server works — see
[transparent-proxy.md](transparent-proxy.md).

- **Fast list** — TLS-handshake latency, written from the measurement pass
  that `pvpn best` and `pvpn up` already run before choosing. Answers "is
  it quick from here?" Skipped entirely when the latencies came back too
  fast to be real, because then they are a local middlebox's numbers and
  not the servers'.
- **Blocked list** — written only from the outcome of a real connect.
  Answers "does traffic actually flow?" Entries expire after
  `blocked_retry_after_hours`, and the hold is stretched up to 4× for a
  server that keeps failing.
- **Verified-ready time** — activation through the first proven traffic,
  recorded in milliseconds only on success. This drives the repeated-connect
  hot path, with connect success rate preventing a flaky server from winning
  on one unusually quick attempt.

```bash
pvpn fast       # what this network measured as quick
pvpn blocked    # what this network refused, and why
```

Filed per network because which servers work is a property of the network,
not of the laptop. Moving between a filtered school wifi and a phone
hotspot moves the whole set of measurements and blocks with it rather than
blending them.

`last_full_rank` is the exact server order used for a connect: computed
fresh (sweep + refine) by `pvpn up`, with blocked servers excluded and
recently fast ones folded into the shortlist.

## Config — `~/.config/pvpn/config.toml`

Persistent equivalents of the `PVPN_*` env vars. Env vars still override.

```toml
auto_best_server = true         # connect down a measured rank, not Proton's pick
connect_timeout_secs = 30
settle_secs = 90                # grace period for traffic to start
blocked_retry_after_hours = 24
country = ""
free_only = false
fix_apps = true
autoconnect_networks = "started"  # or "all", or ["ssid", "wired:eth0"]; see always-on.md
```

Keys left over from the daemon (`auto_reconnect`, `reconnect_attempts`,
`reconnect_backoff_secs`, `probe_interval_secs`) are ignored rather than
rejected, so an old file still loads.

## Privileged operations

`pvpn` runs as you, the same trust level as `protonvpn`. Two operations
need interactive `sudo` and are skipped with a warning:

- blackholing Proton's API in `/etc/hosts`
- force-deleting a stray `pvpnksintrf0` kill-switch interface

`setup.sh --always-on` is the one thing that installs root-owned files
rather than asking for sudo at use time — four under `/etc` and
`/usr/local/sbin`, listed in [always-on.md](always-on.md), removable with
`setup.sh --no-always-on`.

Use `pvpn fix` (and `pvpn fix --unhosts`) for those. `pvpn fix --hosts` still
exists but is no longer suggested anywhere: the shim already fails blocked API
calls in two seconds, and the blackhole also blocks the API *through* the
tunnel, where it is reachable — which sends every certificate renewal over Tor
and, measured 2026-09-21, makes `protonvpn disconnect` exit non-zero after it
has succeeded.

## Logs

There is no pvpn log to read back — the narration goes to the terminal
that asked for the work, as it happens. `pvpn logs` reads *Proton's* log,
which is where `ExpiredCertificate` and a session the far end tore down
actually surface.

```bash
pvpn logs -f
```
