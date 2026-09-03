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
| polled `protonvpn status` every 5 seconds | nothing runs between events |
| retried forever, failing *at* you | at most 3 attempts, then it stops |
| persisted `want_up`, so a reboot resumed the fight | persists only `want_down`, which never starts anything |
| a service you did not know you had | opt-in, and `pvpn-autoconnect --status` says so |

Nothing is running right now because of this feature. A resume or a link
coming up starts one bounded attempt, which exits.

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

## Checking it works

All of these are read-only and safe while connected:

```bash
pvpn-autoconnect --status                 # on, or off
systemctl is-enabled pvpn-recover.service
systemctl --user start pvpn-autoconnect.service   # no-ops while a tunnel is up
journalctl --user -u pvpn-autoconnect.service -n 20
journalctl -u pvpn-recover.service -n 20
```

With a tunnel up, `pvpn-dns-unsnap` returns immediately without touching
anything — it checks `ip route get` first, and only a route through
`proton0` counts as proof of a tunnel. NetworkManager reporting "activated"
and `pvpn status` reporting connected are both true of a tunnel that is
carrying nothing.

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
