# Networks that terminate TLS

Some filtered networks do not drop your packets. They accept them, complete the
TLS handshake, and close the session a fraction of a second later. Every
reachability check passes and the tunnel still refuses to run.

This is what that looks like, measured with `tcpdump -s 0` on one such network
(a school wifi, August 2026).

## One box answers for the whole internet

Three independent signals, all from a single capture:

**Every inbound packet on 443 had `ttl 63`.** 5029 packets, zero variation,
across Cloudflare, Google, GitHub and Proton entry servers in both the
Netherlands and Singapore. Real servers sit at different distances and arrive
with a spread of TTLs. One value means one sender, one hop away.

**They share a single TCP timestamp clock.** The `TSval` in the SYN-ACKs rose
monotonically with wall-clock time across every destination, at roughly 100
ticks per second. Three different IPs returned the identical value 692041459 at
the same instant. Independent machines cannot do that.

**SYN to SYN-ACK from Singapore took 2.4 ms.** The real round trip is 90-100 ms.

So on a network like this, "the TCP connection succeeded" and even "TLS
completed and I got a certificate" describe the proxy, not the server you asked
for. `vpn-check` detects the condition by opening TCP/443 to an RFC 5737
TEST-NET address: nothing is listening on those anywhere, so a connection that
"succeeds" proves something local is answering.

## How the tunnel dies

One `pvpn up` against a single entry IP, per TCP flow:

```
 port      bytes sent    closed by
43168             484    proxy       ~485B ClientHello out, ~2124B back,
36250             484    proxy       then FIN ~0.2s after the handshake
36264             486    proxy       completed - before any tunnel data
  ...             ...    proxy       (28 flows like this)
45364         2135002    -           the 29th survived and carried the tunnel
```

The attempts are **sequential**, about 3.2 s apart, so this is not a limit on
concurrent connections. There were **zero RST packets in 7979** - every close
was a clean FIN.

This matches the client log exactly:

```
TLS/handshake: completed
TLS/Sending close notify
Received TransportAlert(Shutdown(StreamId(1)))
Shutdown(StreamId(1)): reconnecting
```

and the long-run ratio on that network: 237 `HandshakeTimeout` against 41
`ProbeSuccess`.

The practical consequence is that **connecting is slow rather than impossible**.
Roughly one attempt in thirty is allowed through, at ~3.2 s per attempt, so
expect ~90 s before a tunnel establishes - which is why `pvpn up` can report
"No traffic yet after 90s" and then come good. Once a flow survives, it stays
up and moves megabytes normally.

## What decides which session lives

Not known.

Proton's Stealth transport randomises the TLS SNI on every attempt. Names
observed on the wire included `devp.info`, `kmMZOLc.com`, `gVZ.top` and
`bOiG.ru`. The generator lives inside the closed
`/usr/libexec/nm-protun-service`; there is no SNI setting in `pvpn`, in the
`proton.vpn` Python packages, or in the NetworkManager plugin.

It is tempting to conclude the proxy blocks names it cannot categorise. A
controlled test says otherwise: 14 random nonsense SNIs and 14 well-known SNIs,
sent from Python/OpenSSL to the same entry IP, **all 28 survived**. A random or
uncategorised SNI is therefore not sufficient to trigger the close.

That points at the TLS client fingerprint (JA3/JA4) of the Stealth client
rather than the name it sends, since an OpenSSL handshake to the same address
is left alone. That was not proven, and nothing here depends on it.
