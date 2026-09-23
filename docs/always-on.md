# Surviving suspend

A tunnel does not survive a suspend, and on most laptops closing the lid
*is* a suspend. What makes this worth a document is the second half: when
the tunnel dies this way, DNS does not fall back to the network you are
actually on. It stops resolving anything at all, and stays that way until
you run `pvpn` again by hand.

Install the recovery with:

```bash
./setup.sh --always-on
```

Remove it with `./setup.sh --no-always-on`. It is opt-in because it is the
only part of this project that writes files outside `$HOME`.

## Closing the lid really does suspend

The first thing to establish is that nothing exotic is happening. The lock
screen does not touch the tunnel. A suspend does, and the journal says so
plainly:

```
systemd-logind[1570]: Lid closed.
systemd-logind[1570]: The system will suspend now!
NetworkManager[1986]: manager: sleep: sleep requested
NetworkManager[1986]: device (proton0): state change: activated -> unmanaged
kernel: PM: suspend entry (s2idle)
```

NetworkManager tears every managed device down when logind announces a
sleep. The tunnel is one of them.

Two things commonly hide this.

**Your desktop's lid setting is probably a dead key.** GNOME has
`lid-close-ac-action` and `lid-close-battery-action`, and they can both be
set to `'nothing'` while the machine suspends on every lid close anyway.
gnome-settings-daemon does not claim a `handle-lid-switch` inhibitor, so
logind's own default — `HandleLidSwitch=suspend` — is what actually
decides. Check which one is in force:

```bash
busctl get-property org.freedesktop.login1 /org/freedesktop/login1 \
    org.freedesktop.login1.Manager HandleLidSwitch
systemd-inhibit --list | grep handle-lid-switch   # usually no output
```

**Caffeine cannot help.** Caffeine and every other "keep awake" extension
takes an *idle* inhibitor. logind's lid handling backs off only for a
`handle-lid-switch` **block** inhibitor, which they do not take. This is a
structural mismatch, not a bug in the extension — it is why the tunnel
still drops with caffeine enabled.

`--always-on` offers a `logind` drop-in that makes a lid close lock instead
of suspend. It is applied with `systemctl reload systemd-logind`, never a
restart: restarting logind drops and re-creates its seat, GDM replaces your
desktop with a fresh greeter, and everything you had open is gone. `logind`
is `Type=notify-reload`, so a reload re-reads `logind.conf` in place via
SIGHUP without disturbing a single session. On a systemd too old for that,
the policy waits for a reboot — do not restart logind to hurry it along. It is a real trade-off — a closed laptop then stays fully
awake, with the heat and battery drain that implies — so it is asked about
separately, and skipped by default when the installer is not interactive.
Force it either way with `--lid-lock` / `--no-lid-lock`.

Some suspends have no lid event at all. logind logs `The system will
suspend now!` and nothing else, because a session client asked it to over
D-Bus and logind does not record the caller. If you want the caller named,
leave `busctl monitor org.freedesktop.login1` running until it happens.

## Why DNS stops working entirely

This is the part that surprises people, and it is not a lost setting.

While the tunnel is up, the Proton connection profile sets a **negative**
`ipv4.dns-priority` (-1500). In NetworkManager a negative priority means
*exclusive*: the nameservers from that connection are the only ones used,
and every other connection's DNS is suppressed outright. So your wifi's own
nameserver stops being registered at all:

```console
$ nmcli dev show wlp0s20f3 | grep IP4.DNS
IP4.DNS[1]:   10.254.254.254          # NetworkManager still has it

$ resolvectl dns
Link 3 (wlp0s20f3):                    # resolved was never told
Link 38 (proton0): 10.2.0.1
```

Then the tunnel dies, and Proton's leak guard claims the catch-all routing
domain for itself:

```
systemd-resolved[411]: ipv6leakintrf0: Bus client set search domain list to: ~.
systemd-resolved[411]: ipv6leakintrf0: Bus client set default route setting: yes
systemd-resolved[411]: ipv6leakintrf0: Bus client set DNS server list to: ::1
```

`~.` is "every domain". `::1` is systemd-resolved's own stub. Every lookup
is now routed back into the resolver that is asking, which answers nothing.
The journal fills with the sound of it:

```
systemd-resolved[411]: Using degraded feature set UDP instead of TCP for DNS server ::1
```

That claim is correct behaviour while a tunnel is meant to exist — it is
what stops queries leaking to the local network. What is missing is anyone
to withdraw it once the tunnel is gone for good. Nothing does, so DNS stays
dead. By hand:

```bash
sudo resolvectl revert ipv6leakintrf0
```

## What gets installed

| file | scope | does |
|---|---|---|
| `~/.local/bin/pvpn-autoconnect` | you | waits for a real link, then `pvpn up`, bounded |
| `~/.config/systemd/user/pvpn-autoconnect.service` | you | runs the above with no timeout |
| `~/.config/systemd/user/pvpn-watch.service` | you | one `pvpn watch`: is the tunnel still carrying? |
| `~/.config/systemd/user/pvpn-watch.timer` | you | fires the above every two minutes |
| `/usr/local/sbin/pvpn-dns-unsnap` | root | withdraws the stale `~.` / `::1` claim |
| `/usr/local/sbin/pvpn-kick-user` | root | hands the reconnect to the logged-in user |
| `/etc/systemd/system/pvpn-recover.service` | root | the two above, on every resume |
| `/etc/NetworkManager/dispatcher.d/90-pvpn-autoconnect` | root | same, when a physical link returns |
| `/etc/systemd/logind.conf.d/10-pvpn-lid-lock.conf` | root | optional: lid locks instead of suspending |

`pvpn down` additionally writes `~/.local/share/pvpn/down-by-user`, and
`pvpn up` / `pvpn hop` remove it. That is the only change `--always-on` makes
to `pvpn` itself, and it is inert unless these hooks are installed.

DNS is repaired before the reconnect is attempted, in that order, because
the connect needs working DNS to reach Proton's API.

The reconnect runs as you rather than as root because `pvpn` reads Proton's
session from the SSO keyring, which lives on your session bus.
`pvpn-kick-user` finds whoever is logged in with a live bus rather than
having a username baked in at install time.

## Why the dispatcher script does almost nothing

`NetworkManager-dispatcher(8)` is explicit that scripts "will be killed if
they run for too long". A `protun-tls` connect takes well over a minute on
a filtered network — comfortably past that — and a connect killed partway
through leaves the kill switch armed with no tunnel behind it, which takes
all internet with it.

So the dispatcher script only ever runs `systemctl start --no-block` and
exits. The work happens in a unit, where there is no deadline. **Do not add
a `pvpn` call to it.**

(NetworkManager also refuses to run dispatcher scripts that are not
root-owned, or that are group- or world-writable. `setup.sh` installs it
`0755 root:root`.)

## This is not the daemon coming back

[architecture.md](architecture.md#there-was-a-daemon-it-is-gone) explains
why `pvpnd` was deleted, and the objections there were right. This is
deliberately a different shape:

| pvpnd did | this does |
|---|---|
| polled `protonvpn status` every 5 seconds | asks every 2 minutes, and asks the *network*, not the client |
| retried forever, failing *at* you | at most 3 attempts, then it stops |
| persisted `want_up`, so a reboot resumed the fight | persists only `want_down`, which never starts anything |
| a service you did not know you had | opt-in, and `pvpn-autoconnect --status` says so |

Nothing is resident. A resume or a link coming up starts one bounded attempt,
which exits; `pvpn-watch.timer` starts one short-lived check, which exits.

### Why there is a timer at all

The first row of that table is the concession, so it is worth being plain
about what bought it. Every other hook here reacts to an event. The failure
they cannot see has no event:

| time | what happened on `wifi:detnsw`, 2026-09-15 |
|---|---|
| 07:57:35 | `tunnel established`; traffic verified; `pvpn` exits |
| 08:00:35 | `supervisor: timer elapsed ToDo=RetryEndpoint(…:443, WireguardTcp)` |
| 08:00:40 | `systemd-resolved` starts cycling feature sets on `10.2.0.1` |
| 08:02:43 | the user gives up and runs `pvpn down` |

The middlebox killed the TCP carrier three minutes in and the client's retry
never re-established it. No link changed, nothing suspended, NetworkManager
stayed `activated` and `proton0` kept its routes — so no dispatcher script
ran, no resume hook fired, and nothing was written to the history. Two
minutes of hanging DNS, and tomorrow's ranking still believed that server was
the best one here.

There is no event to hook. Somebody has to ask on a clock.

What keeps it from being `pvpnd` is what it asks and what it is allowed to
do. `pvpnd` polled the *client* for its opinion — the opinion that was wrong
in the table above. `pvpn watch` sends a packet and waits for an answer, exits
at once when no tunnel is up, holds no state between firings, and records what
it finds whether or not it is allowed to reconnect. `pvpn-autoconnect --off`
stops it reconnecting; it goes on telling you.

```bash
systemctl --user disable --now pvpn-watch.timer   # stop it asking entirely
systemctl --user list-timers pvpn-watch.timer     # when it next will
```

It also does not undo a deliberate `pvpn down`, awake or locked. `pvpn down`
writes `~/.local/share/pvpn/down-by-user`; `pvpn up` and `pvpn hop` clear it.
While it exists, a resume repairs DNS and leaves the tunnel down.

That is a persisted intent, which is exactly what the daemon was criticised
for — but in the opposite direction, and the direction is the whole argument.
`pvpnd` persisted `want_up`, so a reboot resumed a fight you had lost interest
in. This persists `want_down`, which can only ever cause *less* to happen.
Nothing reconnects because of that file; the worst a stale one can do is make
you type `pvpn up`.

To stop it reconnecting at all, separately from any one `pvpn down`:

```bash
pvpn-autoconnect --off      # and --on to allow it again, --status to check
```

The two are deliberately different files. `pvpn up` releases the hold from a
`pvpn down`; it does not quietly re-enable autoconnect for someone who turned
it off on purpose.

## Only on networks you chose

A laptop that resumes on a different wifi from the one it slept on is the
common case, not the edge one. Rebuilding the tunnel there is a decision
nobody made — and on a network that refuses every connect, it is two
minutes of no internet you did not ask for. So both reconnect paths, the
resume/link hook and `pvpn watch`, first ask whether this network is one
you chose, set in `~/.config/pvpn/config.toml`:

```toml
autoconnect_networks = "started"                  # default
autoconnect_networks = "all"
autoconnect_networks = ["detnsw", "wired:eth0"]   # SSIDs, or network keys
```

| value | reconnects on |
|---|---|
| `"started"` | the network where you last ran `pvpn up` or `pvpn hop` yourself |
| `"all"` | any network with a link |
| a list | those networks: a bare SSID, or a key as `pvpn servers` prints it (`wifi:…`, `wired:…`) |

`"started"` follows you only when you ask it to. Each `pvpn up` or `pvpn hop`
you type writes the network's key to `~/.local/share/pvpn/autoconnect-network`;
the hook's own reconnects run with `PVPN_AUTOCONNECT=1` and never write it,
so an automatic reconnect cannot move the line it was checked against. Until
you have run `pvpn up` somewhere, `"started"` reconnects nowhere.

Outside the chosen networks the resume still repairs DNS, and `pvpn watch`
still checks and records a dead tunnel — it just does not rebuild one. This
gates *automatic* reconnects only; a `pvpn up` you type works anywhere.

`setup.sh --always-on` asks which of the three you want, or takes
`--autoconnect-networks=started|all|"ssid1,ssid2"`. To check where you are:

```bash
pvpn-autoconnect --status   # ...and whether this network is covered, and why
```

## Checking it works

All of these are read-only and safe while connected:

```bash
pvpn-autoconnect --status                 # on, or off, and whether this network is covered
systemctl is-enabled pvpn-recover.service
systemctl --user start pvpn-autoconnect.service   # no-ops while a tunnel is up
systemctl --user list-timers pvpn-watch.timer
pvpn watch --check                        # asks now, changes nothing
journalctl --user -u pvpn-autoconnect.service -n 20
journalctl --user -u pvpn-watch.service -n 20
journalctl -u pvpn-recover.service -n 20
```

With a tunnel up, `pvpn-dns-unsnap` returns immediately without touching
anything — it checks `ip route get` first, and only a route through
`proton0` counts as proof of a tunnel.

That last sentence is true of `pvpn-dns-unsnap`'s question and false of
everyone else's, which is the trap this whole page circles. A route through
`proton0` proves traffic is *going into* a tunnel. It proves nothing about
anything coming back, because a session the network killed keeps its device
and its routes. NetworkManager reporting "activated" is wrong in the same
way. `pvpn-autoconnect` used to decide "there is already a tunnel, nothing to
do" from the route alone, and would cheerfully exit 0 over a tunnel that had
not passed a packet in two minutes; it now probes before it believes itself,
and `pvpn status` prints the answer.

## Limits

- **It cannot make a hostile network work.** If every connect fails, three
  more will fail. It gives up on purpose; that is the lesson from `pvpnd`.
- **The gap is not zero.** A resume still costs the reassociation time plus
  a connect — seconds to a minute, not instant.
- **Only NetworkManager and systemd-resolved.** The DNS repair assumes both.
  On a machine using `resolv.conf` directly, or a non-NM setup, the resume
  hook still fires but the repair is a no-op.
- **The interface names are Proton's.** `proton0`, `ipv6leakintrf0` and
  `pvpnksintrf0` are matched by name. A future Proton client that renames
  them would need these scripts updated.
