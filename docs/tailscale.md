# Tailscale behind the tunnel

With Proton up, Tailscale logs itself out and stays logged out:

```
$ tailscale status
# Health check:
#     - You are logged out. The last login error was: fetch control key:
#       Get "https://controlplane.tailscale.com/key?v=148": context deadline exceeded
```

Install the fix with:

```bash
./setup.sh --tailscale
```

Setup installs it on its own whenever `tailscaled` is installed — a
Tailscale that goes dark the moment the VPN connects is a broken install,
not a preference. Remove it with `./setup.sh --no-tailscale`.

## Why it happens

tailscaled marks every packet it sends itself (control plane, DERP, the
WireGuard UDP to peers) with fwmark `0x80000`, and installs rules that send
those packets to the `main` table, so that they never loop back into
`tailscale0`:

```
5210:   from all fwmark 0x80000/0xff0000 lookup main
...
32764:  from all lookup main suppress_prefixlength 0 proto static
32765:  not from all fwmark 0xea13b2c lookup 245447468 proto static   <- Proton
```

Those rules also come before Proton's. Proton's default route lives in its
own table (`245447468`, `default dev proton0`), and `main` still holds the
wifi's default route. So everything tailscaled sends leaves through the bare
network, which is the network this project assumes is filtering things.
Measured on `detnsw`:

```
curl https://controlplane.tailscale.com/key?v=148                         200   1.4s
curl --interface wlp0s20f3 https://controlplane.tailscale.com/key?v=148   000   8.0s (timeout)
```

The control server is reachable through the tunnel and blocked outside it,
and tailscaled is the one process that insists on going outside.

## The fix

`system/80-pvpn-tailscale`, a NetworkManager dispatcher script, adds one rule
ahead of Tailscale's:

```
5200:   from all fwmark 0x80000/0xff0000 lookup 245447468
```

Tailscale's own packets now go into Proton's table and out through
`proton0`. Proton's transport packets carry `0xea13b2c`, not `0x80000`, so
the tunnel itself still leaves through the wifi. Traffic *to* your tailnet
(`100.64.0.0/10` through `tailscale0`) is untouched: it is routed by
Tailscale's table 52 as before, and only the encrypted WireGuard packets
that result are tunnelled.

On every network event the script reconciles rather than reacts. It looks
up which table holds `default dev proton*` right now and makes the rule at
pref 5200 match, for IPv4 and IPv6:

- **Tunnel up:** the rule points at that table. Proton derives the number,
  so it is read, not hardcoded.
- **Tunnel gone:** the rule is removed. This matters because Proton's table
  also holds an `unreachable default` for its leak guard, and a rule left
  pointing there after a disconnect would cut Tailscale off completely.

A missed `down` is therefore corrected by the next event of any kind. The
script runs only `ip` and returns immediately, as dispatcher scripts must.

## What to expect

- **Peers go through DERP more often.** Direct UDP to peers now leaves from
  Proton's exit, behind Proton's NAT. Tailscale falls back to its relays on
  443 when hole punching fails, so peers stay reachable. They may just be
  slower than on an open network.
- **MTU is fine.** `tailscale0` uses 1280; with WireGuard's 80 bytes of
  overhead that is 1360, under `proton0`'s 1420.
- **With the tunnel down, Tailscale goes back to the bare network**, as it
  did before. On a filtered network that means logged out again until
  `pvpn up`.
- **Tailscale as an exit node is not covered.** If Tailscale owns the
  default route, the two tunnels are competing for the same job, and no
  rule makes that make sense.

Check it is in place with `ip rule show pref 5200`. Once the rule is in,
tailscaled reconnects by itself within about 30 seconds; to skip the wait,
run `sudo systemctl restart tailscaled`.
