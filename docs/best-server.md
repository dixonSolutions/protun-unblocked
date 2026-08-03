# Picking a server

`pvpn best` measures the servers your account can use and ranks them.
`pvpn up` applies that ranking automatically.

```bash
pvpn best             # measure and rank
pvpn best --connect   # measure, then connect to the winner
pvpn up               # same choice, applied automatically
```

## Why not just use Proton's "fastest"?

Proton ships a `Score` with every server and its client connects to the
lowest one. That score is computed server-side and is not a measurement of
your path to the server. Measured from Sydney on a free account:

| server | Proton's rank | measured RTT | load |
|---|---|---|---|
| `NL-FREE#15` (Amsterdam) | **1st** | 260 ms | 55% |
| `JP-FREE#9` (Tokyo) | unranked | 165 ms | 81% |
| `SG-FREE#2` (Singapore) | unranked | **99 ms** | 77% |

Proton's first choice is two and a half times slower than one it never
suggests. The gap is not subtle and it is not noise: Singapore measures
~99 ms on every run.

## How the ranking works

1. **Filter.** Keep servers that are enabled and within your account's tier
   (`server.tier <= user_tier`, the same test Proton's client uses). Secure
   Core and Tor servers are excluded: both are slower by design.
2. **Locate.** Work out roughly where you are from the system timezone,
   looked up in `/usr/share/zoneinfo/zone.tab`. This is offline on purpose —
   a geolocation API is exactly the sort of thing the networks this project
   targets will block.
3. **Shortlist.** Take the nearest servers and the least loaded servers, up
   to `--shortlist` (default 40). Distance predicts latency well, but a
   near server that is saturated is no use, so both criteria contribute.
4. **Sweep.** Open a TCP connection to each shortlisted server's entry IP on
   port 443 and time the handshake. Port 443 is where Stealth, OpenVPN-TCP
   and WireGuard-TLS terminate, and it is the port least likely to be
   dropped by a filtering network.
5. **Refine.** Re-time the top `--refine` finalists (default 8) at low
   concurrency. See "Probes interfere with each other" below.
6. **Rank.** Combine the results.

### The rating

Each term is normalised against the range observed in that run, so the
weights express relative importance rather than pretending milliseconds and
percentages are comparable:

| term | weight | why |
|---|---|---|
| measured latency | 0.60 | the only term that reflects your actual network |
| reported load | 0.25 | a busy server answers a handshake fast and still crawls under traffic |
| distance | 0.15 | mostly redundant with latency, but it steadies a noisy probe run |

The published rating is 0-100, higher is better. Load is treated as
saturated at 95%, so the difference between 95% and 99% cannot swamp a real
latency advantage.

## Two things that will mislead you

### Probes interfere with each other

Sending 40 probes at once saturates the uplink, and every server then
reports roughly the same time. Observed on this machine: a wide burst put
Singapore and Los Angeles both at ~200 ms, indistinguishable, when their
true times are 99 ms and 149 ms. The winner would have been whichever
server was least delayed by our own traffic.

Hence the two passes: a wide sweep to find what answers, then a gentle
re-timing of the finalists. The refine pass costs about a second.

### Measuring through a tunnel measures the tunnel

If the VPN is already up, a probe leaves through the current exit. It times
the tunnel plus the hop beyond it, not your path to the server. Observed:
connected via Mexico City, the ranking picked Mexico City; connected via
Tokyo, it picked Tokyo. It was following whichever server it was already on.

So:

- `pvpn best --connect` disconnects first, then measures, then connects.
- `pvpn best` on its own warns you and suggests `pvpn down && pvpn best`.

## Free accounts

Proton's CLI refuses to let a free account name a server:

```
$ protonvpn connect SG-FREE#13
Error: Server selection by ID is not available on the free plan.
```

That check treats "chose a server" as if it meant "chose a paid server".
Choosing among the ~100 free servers is not a paid feature — Proton's own
desktop app allows it — but the CLI's check does not distinguish the two,
which would leave `pvpn best` able to measure Singapore and unable to use it.

`lib/sitecustomize.py` replaces that blanket refusal with the precise
entitlement test the rest of the client already uses, `server.tier <=
user_tier`. A server above your tier still falls through to the original
code and is still refused, and Proton's backend enforces tier independently
of anything a client asks for. Nothing is unlocked that the account did not
already have; only the ability to say which of its own servers it wants.

The patch applies only to processes `pvpn` launches, via `PYTHONPATH`.

## Retries go somewhere new

Before this, a failed connect retried by asking Proton for "the fastest"
again — and got the same server. The README recorded two consecutive
attempts landing on `US-FREE#15`.

`pvpn up` now holds a ranked list, so attempt 2 goes to the second-best
server, attempt 3 to the third. A genuinely dead server is no longer
retried against itself.

## Reference

```
pvpn best [options]

  --connect, -c     connect to the best server
  --country XX      restrict to one exit country
  --limit N         how many results to show (default 10)
  --quick, -q       skip measuring; rank by distance and load only
  --free            free-tier servers only, even on a paid account
  --json            machine-readable output on stdout
```

`--json` keeps progress notes on stderr, so it is safe to pipe.

### Environment

| variable | default | meaning |
|---|---|---|
| `PVPN_BEST` | 1 | set to 0 to leave the choice to Proton |
| `PVPN_BEST_TIMEOUT` | 90 | seconds allowed for a measurement run |
| `PVPN_BEST_LIMIT` | 10 | default number of results |
| `PVPN_BEST_COUNTRY` | — | restrict to one country |
| `PVPN_BEST_FREE` | 0 | set to 1 for free servers only |
| `PVPN_SERVERLIST` | Proton's cache | server list to read (used by the tests) |

### The helper on its own

`lib/best-server.py` is a standalone script with no dependencies beyond the
standard library, so it can be run and tested without `pvpn`:

```bash
python3 lib/best-server.py --limit 5
python3 lib/best-server.py --country JP --format json
python3 lib/best-server.py --serverlist ./fixture.json --no-probe
```

## Testing

```bash
tests/run-tests.sh
```

The suite is offline and read-only: it never connects, disconnects, or
changes routing, and it does not need you to be signed in. The only sockets
it opens are to a listener it starts on loopback. Safe to run with the VPN
up.
