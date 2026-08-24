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
crates/pvpn-core/   rank, probe, geo, serverlist, state, config, proc
crates/pvpn/        the CLI: connect, blocklist, session, narration
lib/                Python shims loaded into protonvpn via PYTHONPATH
legacy/             the previous bash tool, and the removed daemon
```

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

## State — `~/.local/share/pvpn/state.json`

Two lists, filed **per network**, populated two different ways. A fast TLS
handshake does not prove a server works — see
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
```

Keys left over from the daemon (`auto_reconnect`, `reconnect_attempts`,
`reconnect_backoff_secs`, `probe_interval_secs`) are ignored rather than
rejected, so an old file still loads.

## Privileged operations

`pvpn` runs as you, the same trust level as `protonvpn`. Two operations
need interactive `sudo` and are skipped with a warning:

- blackholing Proton's API in `/etc/hosts`
- force-deleting a stray `pvpnksintrf0` kill-switch interface

Use `pvpn fix` (and `pvpn fix --hosts` / `pvpn fix --unhosts`) for those.

## Logs

There is no pvpn log to read back — the narration goes to the terminal
that asked for the work, as it happens. `pvpn logs` reads *Proton's* log,
which is where `ExpiredCertificate` and a session the far end tore down
actually surface.

```bash
pvpn logs -f
```
