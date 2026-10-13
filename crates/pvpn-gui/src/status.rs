//! What is true right now, and what changed since the last look.
//!
//! The window watches; it never reconnects. Everything that rebuilds a
//! tunnel for you is still `pvpn-autoconnect` and `pvpn watch`, gated by
//! `autoconnect_networks` / `autoconnect_never` — the window only notices
//! them working and says so. A second thing deciding when to connect is
//! exactly the daemon this project deleted.

pub use pvpn_core::snapshot::{Busy, Snapshot};

/// One full reading, from `pvpn query status`. Blocking — run it on a
/// worker thread.
///
/// `own_child` is the `pvpn` this window started: its connect is the user's,
/// not an automatic one, whatever the CLI could tell from outside.
pub fn read(probe_traffic: bool, own_child: Option<u32>) -> Snapshot {
    let args: &[&str] = if probe_traffic { &["status", "--probe"] } else { &["status"] };
    let mut snap: Snapshot = crate::cli::query(args)
        // An installed `pvpn` too old to answer: read it the same way it would.
        .unwrap_or_else(|| pvpn_core::snapshot::read(probe_traffic, own_child));
    mark_own(&mut snap.busy, own_child);
    snap
}

/// Who holds the connect lock, and whether the autoconnect hook is running,
/// from `pvpn query busy`. Blocking.
pub fn read_busy(own_child: Option<u32>) -> (Option<Busy>, bool) {
    #[derive(serde::Deserialize)]
    struct Answer {
        busy: Option<Busy>,
        autoconnect_running: bool,
    }
    let (mut busy, autoconnect_running) = match crate::cli::query::<Answer>(&["busy"]) {
        Some(answer) => (answer.busy, answer.autoconnect_running),
        None => pvpn_core::snapshot::read_busy(own_child),
    };
    mark_own(&mut busy, own_child);
    (busy, autoconnect_running)
}

fn mark_own(busy: &mut Option<Busy>, own_child: Option<u32>) {
    if let Some(busy) = busy.as_mut() {
        if own_child == Some(busy.pid) {
            busy.automatic = false;
        }
    }
}

/// The one word the orb shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Disconnected,
    Connecting { automatic: bool },
    Disconnecting,
    Connected { server: String },
    /// Routes into a tunnel that carries nothing, or a client that thinks
    /// it is connected with no tunnel under it.
    Broken { reason: String },
}

impl Phase {
    /// `last_carrying` is the most recent traffic answer, kept between the
    /// polls that do not ask.
    pub fn of(snap: &Snapshot, last_carrying: Option<bool>) -> Phase {
        if let Some(busy) = &snap.busy {
            return if busy.disconnecting {
                Phase::Disconnecting
            } else {
                Phase::Connecting {
                    automatic: busy.automatic,
                }
            };
        }
        // Only without a tunnel. With one up, the script is checking that it
        // carries and leaving — and if it does rebuild it, its `pvpn up`
        // takes the connect lock and the branch above says so. On a roaming
        // wifi it starts on every DHCP renew; reading each of those as
        // "Reconnecting…" flickered the orb and rebuilt the tray menu out
        // from under whoever had it open.
        if snap.autoconnect_running && !snap.tunneled {
            return Phase::Connecting { automatic: true };
        }
        let server = snap.server.clone().unwrap_or_else(|| "Proton".into());
        if snap.tunneled {
            if last_carrying == Some(false) {
                return Phase::Broken {
                    reason: format!("{server} is up but nothing comes back through it"),
                };
            }
            return Phase::Connected { server };
        }
        if snap.connected {
            return Phase::Broken {
                reason: format!("Proton says {server}, but no traffic goes through a tunnel"),
            };
        }
        Phase::Disconnected
    }

    pub fn title(&self) -> &'static str {
        match self {
            Phase::Disconnected => "Disconnected",
            Phase::Connecting { automatic: true } => "Reconnecting…",
            Phase::Connecting { automatic: false } => "Connecting…",
            Phase::Disconnecting => "Disconnecting…",
            Phase::Connected { .. } => "Connected",
            Phase::Broken { .. } => "Not protected",
        }
    }

    /// The CSS class the orb and tray colour key off.
    pub fn css(&self) -> &'static str {
        match self {
            Phase::Disconnected => "off",
            Phase::Connecting { .. } | Phase::Disconnecting => "busy",
            Phase::Connected { .. } => "on",
            Phase::Broken { .. } => "dead",
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, Phase::Connecting { .. } | Phase::Disconnecting)
    }
}

/// A notification-worthy change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    Connected { server: String },
    Reconnecting,
    Reconnected { server: String },
    Failure { what: String },
    Disconnected { expected: bool },
}

/// Turns a stream of phases into the five notifications, remembering what
/// a single reading cannot know: that a connect which just landed was a
/// *re*connect.
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<Phase>,
    reconnecting: bool,
}

impl Tracker {
    pub fn observe(&mut self, next: &Phase) -> Option<Note> {
        let Some(prev) = self.last.replace(next.clone()) else {
            // First reading: that is how things were, not a change.
            self.reconnecting = matches!(next, Phase::Connecting { automatic: true });
            return None;
        };
        if &prev == next {
            return None;
        }
        match next {
            Phase::Connecting { automatic: true } => {
                if self.reconnecting {
                    return None;
                }
                self.reconnecting = true;
                Some(Note::Reconnecting)
            }
            Phase::Connecting { automatic: false } | Phase::Disconnecting => None,
            Phase::Connected { server } => {
                let note = if std::mem::take(&mut self.reconnecting) {
                    Note::Reconnected {
                        server: server.clone(),
                    }
                } else {
                    Note::Connected {
                        server: server.clone(),
                    }
                };
                Some(note)
            }
            Phase::Broken { reason } => {
                let was_reconnecting = std::mem::take(&mut self.reconnecting);
                Some(Note::Failure {
                    what: if was_reconnecting {
                        format!("Reconnect did not take: {reason}")
                    } else {
                        reason.clone()
                    },
                })
            }
            Phase::Disconnected => {
                let was_reconnecting = std::mem::take(&mut self.reconnecting);
                match prev {
                    Phase::Connecting { .. } => Some(Note::Failure {
                        what: if was_reconnecting {
                            "Could not reconnect — normal internet restored".into()
                        } else {
                            "Could not connect — normal internet restored".into()
                        },
                    }),
                    Phase::Disconnecting => Some(Note::Disconnected { expected: true }),
                    Phase::Connected { .. } | Phase::Broken { .. } => {
                        Some(Note::Disconnected { expected: false })
                    }
                    Phase::Disconnected => None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(s: &str) -> Phase {
        Phase::Connected { server: s.into() }
    }
    const AUTO: Phase = Phase::Connecting { automatic: true };

    #[test]
    fn autoconnect_checking_a_live_tunnel_is_not_a_reconnect() {
        let mut snap = Snapshot {
            autoconnect_running: true,
            ..Default::default()
        };
        assert_eq!(Phase::of(&snap, None), AUTO, "no tunnel: it is rebuilding one");
        snap.tunneled = true;
        snap.connected = true;
        snap.server = Some("SG-FREE#2".into());
        assert_eq!(Phase::of(&snap, Some(true)), on("SG-FREE#2"));
    }
    const USER: Phase = Phase::Connecting { automatic: false };

    fn run(phases: &[Phase]) -> Vec<Note> {
        let mut t = Tracker::default();
        phases.iter().filter_map(|p| t.observe(p)).collect()
    }

    #[test]
    fn the_first_reading_is_not_news() {
        assert!(run(&[on("SG#1")]).is_empty());
        assert!(run(&[Phase::Disconnected]).is_empty());
    }

    #[test]
    fn a_user_connect_says_connected() {
        assert_eq!(
            run(&[Phase::Disconnected, USER, on("SG#1")]),
            vec![Note::Connected { server: "SG#1".into() }]
        );
    }

    #[test]
    fn an_automatic_rebuild_says_reconnecting_then_reconnected() {
        assert_eq!(
            run(&[on("SG#1"), AUTO, AUTO, on("JP#2")]),
            vec![
                Note::Reconnecting,
                Note::Reconnected { server: "JP#2".into() }
            ]
        );
    }

    #[test]
    fn a_failed_connect_is_a_failure_not_a_disconnect() {
        let notes = run(&[Phase::Disconnected, USER, Phase::Disconnected]);
        assert!(matches!(notes.as_slice(), [Note::Failure { .. }]));
    }

    #[test]
    fn a_failed_reconnect_says_so() {
        let notes = run(&[on("A"), AUTO, Phase::Disconnected]);
        match notes.as_slice() {
            [Note::Reconnecting, Note::Failure { what }] => assert!(what.contains("reconnect")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn down_is_an_expected_disconnect_and_a_drop_is_not() {
        assert_eq!(
            run(&[on("A"), Phase::Disconnecting, Phase::Disconnected]),
            vec![Note::Disconnected { expected: true }]
        );
        assert_eq!(
            run(&[on("A"), Phase::Disconnected]),
            vec![Note::Disconnected { expected: false }]
        );
    }

    #[test]
    fn a_tunnel_that_stops_carrying_is_a_failure() {
        let notes = run(&[on("A"), Phase::Broken { reason: "dead".into() }]);
        assert!(matches!(notes.as_slice(), [Note::Failure { .. }]));
    }

    #[test]
    fn a_hop_between_servers_announces_the_new_one() {
        assert_eq!(
            run(&[on("A"), USER, on("B")]),
            vec![Note::Connected { server: "B".into() }]
        );
    }

    fn snap() -> Snapshot {
        Snapshot {
            autoconnect_here: Err(String::new()),
            ..Default::default()
        }
    }

    #[test]
    fn the_lock_outranks_the_routes() {
        let mut s = snap();
        s.tunneled = true;
        s.connected = true;
        s.busy = Some(Busy {
            automatic: true,
            ..Default::default()
        });
        assert_eq!(Phase::of(&s, None), AUTO);
        s.busy.as_mut().unwrap().disconnecting = true;
        assert_eq!(Phase::of(&s, None), Phase::Disconnecting);
    }

    #[test]
    fn routes_without_answers_are_broken() {
        let mut s = snap();
        s.tunneled = true;
        s.connected = true;
        s.server = Some("A".into());
        assert_eq!(Phase::of(&s, Some(true)), on("A"));
        assert!(matches!(Phase::of(&s, Some(false)), Phase::Broken { .. }));
        s.tunneled = false;
        assert!(matches!(Phase::of(&s, None), Phase::Broken { .. }));
    }
}
