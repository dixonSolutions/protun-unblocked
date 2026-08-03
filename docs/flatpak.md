# Flatpak apps and the tunnel

This is handled for you. `pvpn up` finds apps that are routed around the
tunnel and puts them back on it, on every connect, and `./setup.sh` does the
same once at install. The command below is for checking on that.

```bash
pvpn apps            # which apps skip the tunnel, and why
pvpn apps --fix      # put them back on it
pvpn apps --verify   # launch them and compare their real exit address
```

## Why it runs on every connect

A bypass is set once and then lasts forever. Some older workaround gives an
app a proxy, the workaround is forgotten, and that app stays off every
tunnel raised from then on — with nothing anywhere saying so. It is not a
state that decays, so asking the user to remember to check for it means it
never gets checked.

Connecting is both the moment it matters and the moment someone is looking
at the output, so that is where the check goes. It costs 0.34s: one
`flatpak info` per app, run in parallel across 57 apps here. Serially the
same scan takes 2.5s, which is why it is worth the parallelism.

When something is found, the connect says so and what it removed:

```
✔ Connected via protun-tls.
com.rtosta.zapzap was routed around the VPN by a proxy setting - fixing.
    unset http_proxy
    unset https_proxy
    unset ALL_PROXY
  Already-running apps keep the old setting until restarted.
```

Set `PVPN_FIX_APPS=0` to be told about bypasses without changing anything.

## Flatpak is not the problem

Flatpak apps share the host's network namespace, so the tunnel applies to
them exactly as it does to anything else. Verified on this machine, with the
host exiting through a Mexico City server:

| | TCP exit | UDP exit |
|---|---|---|
| host | `205.147.22.14` | `205.147.22.14` |
| Xonotic sandbox | `205.147.22.14` | `205.147.22.14` |

Both protocols, same address. Sandboxing does not create a bypass, and UDP
is worth checking separately because a game's traffic is UDP while its
launcher's is TCP.

## A proxy is the problem

What does take an app off the tunnel is a proxy. Give an app `http_proxy`
and it hands its traffic to that proxy, whose exit is then wherever the
proxy is — the tunnel carries the first hop and nothing after it.

Found here on ZapZap:

```
http_proxy=socks5://127.0.0.1:9050
https_proxy=socks5://127.0.0.1:9050
ALL_PROXY=socks5://127.0.0.1:9050
```

Port 9050 is Tor's SOCKS port. So while the host exited in Mexico City,
ZapZap exited at `109.70.100.11` — a Tor relay in Vienna. Almost certainly a
leftover from routing that app around a filtered network before the VPN
worked; now it just means one app is on Tor while everything else is on the
VPN, which is slower and, for a messaging client, likely to trip
Tor-exit blocks.

Nothing in `pvpn status`, `pvpn ip`, or Proton's own client shows this. The
tunnel is genuinely up and genuinely carrying traffic; one app simply hands
its traffic somewhere else first. A silent failure that no status will ever
surface is exactly the kind that has to be looked for rather than reported,
which is why the connect path looks for it.

## What `pvpn apps` checks

It reads each installed app's effective environment and reports any of:

```
http_proxy  https_proxy  ftp_proxy  all_proxy
HTTP_PROXY  HTTPS_PROXY  FTP_PROXY  ALL_PROXY
```

Both cases are checked deliberately: curl and glibc read the lowercase
names, while Qt, Go and Rust programs generally read the uppercase ones. An
audit that checks only one set will miss real bypasses.

Only a non-empty value counts. `flatpak override --unset-env` leaves the
name behind with nothing after the `=`, and treating that as a proxy would
make an already-fixed app look permanently broken.

## What `--fix` does

For each proxy variable found:

```bash
flatpak override --user --unset-env=VAR APP
```

This is per-user and reversible. It does not touch anything else about the
app — no permissions, no other environment variables. To restore a proxy:

```bash
flatpak override --user --env=http_proxy=socks5://127.0.0.1:9050 com.rtosta.zapzap
```

Result on ZapZap, before and after:

```
before:  109.70.100.11   (Vienna, Tor exit)
after:   205.147.22.14   (Mexico City, the VPN exit — same as the host)
```

An app that is already running keeps the environment it started with, so a
fix applies from its next launch. `--fix` and the automatic pass both say so
rather than leaving you to wonder why a running app has not moved.

A proxy baked into an app's own manifest survives `--unset-env`. Both the
command and the automatic pass re-read the environment afterwards and report
an app that is still bypassing instead of claiming a fix that did not take.

## Verifying it yourself

`pvpn apps --verify` starts each flagged app's sandbox and asks it for its
public address, then compares that with the host's. This is the honest test,
because it runs with the app's real environment, proxy and all.

It is slow — every app has to start — and it can only report on runtimes
that ship `curl` or `python3`. Apps whose runtime has neither are reported
as unknown rather than guessed at.

To check one app by hand:

```bash
flatpak run --command=curl org.xonotic.Xonotic -s https://ifconfig.me
curl -s --noproxy '*' https://ifconfig.me      # compare with the host
```

For UDP, which matters for games and voice chat, a STUN query reports the
address as seen over UDP:

```bash
flatpak run --command=python3 org.xonotic.Xonotic -c '
import socket, struct, os
req = struct.pack(">HHI12s", 1, 0, 0x2112A442, os.urandom(12))
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(6)
s.sendto(req, ("74.125.250.129", 19302)); d, _ = s.recvfrom(2048)
i = 20
while i + 4 <= len(d):
    t, l = struct.unpack(">HH", d[i:i+4]); v = d[i+4:i+4+l]
    if t == 0x20:
        print(socket.inet_ntoa(bytes(a ^ b for a, b in zip(v[4:8], b"\x21\x12\xa4\x42"))))
        break
    i += 4 + l + ((4 - l % 4) % 4)'
```

## Known unrelated issue on ZapZap

While auditing, one thing turned up that is **not** a VPN problem and was
left alone: ZapZap sets

```
SSL_CERT_FILE=/run/host/etc/ssl/certs/ca-certificates.crt
```

and that path does not exist inside the sandbox, so OpenSSL-based tools in
it cannot do HTTPS. It comes from the app's own manifest, predates any of
this, and the app's browser engine uses its own certificate store, so
ZapZap itself is likely unaffected. If it ever does cause trouble:

```bash
flatpak override --user --unset-env=SSL_CERT_FILE com.rtosta.zapzap
flatpak override --user --unset-env=SSL_CERT_DIR com.rtosta.zapzap
```

That falls back to the runtime's own certificate bundle, which is present
and valid.
