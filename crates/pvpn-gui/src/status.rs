//! What is true right now, and what changed since the last look.
//!
//! The window watches; it never reconnects. Everything that rebuilds a
//! tunnel for you is still `pvpn-autoconnect` and `pvpn watch`, gated by
//! `autoconnect_networks` / `autoconnect_never` — the window only notices
//! them working and says so. A second thing deciding when to connect is
//! exactly the daemon this project deleted.

use pvpn_core::config::Config;
use pvpn_core::{intent, lock, proc, scope};
use std::path::PathBuf;

/// Who holds the connect lock, when somebody does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Busy {
    pub pid: u32,
    /// The holder's own note: `pid N: pvpn up`.
    pub note: String,
    /// Started by the autoconnect hook or the watch timer, not a person.
    pub automatic: bool,
    /// A `pvpn down` rather than a connect.
    pub disconnecting: bool,
}

/// One reading, taken off the main thread. Only cheap sources — files,
/// the system bus, the routing table — except `carrying`, which sends a
/// request through the tunnel and is only asked for on a slower clock.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub network: String,
    /// Proton's client, or NetworkManager, believes a tunnel is up.
    pub connected: bool,
    /// The kernel actually routes through a tunnel device.
    pub tunneled: bool,
    pub server: Option<String>,
    pub protocol: String,
    /// `None` when not asked this time.
    pub carrying: Option<bool>,
    pub stray_route: Option<String>,
    pub busy: Option<Busy>,
    /// `pvpn-autoconnect` is running (it holds its own lock while it does).
    pub autoconnect_running: bool,
    pub down_by_user: bool,
    pub autoconnect_off: bool,
    /// Would a reconnect be allowed here, and why (or why not).
    pub autoconnect_here: Result<String, String>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            network: String::new(),
            connected: false,
            tunneled: false,
            server: None,
            protocol: String::new(),
            carrying: None,
            stray_route: None,
            busy: None,
            autoconnect_running: false,
            down_by_user: false,
            autoconnect_off: false,
            autoconnect_here: Err(String::new()),
        }
    }
}

impl Default for Busy {
    fn default() -> Self {
        Self {
            pid: 0,
            note: String::new(),
            automatic: false,
            disconnecting: false,
        }
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe_uid())))
}

fn unsafe_uid() -> u32 {
    // /proc/self is ours; its owner is our uid. Avoids a libc dependency.
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1000)
}

/// Was `pid` started with `PVPN_AUTOCONNECT` set (the hook's `pvpn up`)?
fn started_by_autoconnect(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/environ"))
        .map(|env| {
            env.split(|b| *b == 0)
                .any(|kv| kv.starts_with(b"PVPN_AUTOCONNECT=") && kv != b"PVPN_AUTOCONNECT=0")
        })
        .unwrap_or(false)
}

/// Who holds the connect lock, and whether the autoconnect hook is running.
/// Only `/proc` reads — cheap enough for the main thread every second,
/// which matters: the full reading below can stall for seconds on
/// NetworkManager while it activates a tunnel, which is exactly when this
/// needs answering.
pub fn read_busy(own_child: Option<u32>) -> (Option<Busy>, bool) {
    let busy = lock::in_flight().map(|(pid, note)| {
        let ours = own_child == Some(pid);
        Busy {
            automatic: !ours && (note.contains(" watch") || started_by_autoconnect(pid)),
            disconnecting: note.ends_with(" down") || note.contains(" down "),
            pid,
            note,
        }
    });
    let autoconnect_running = lock::locked_by(&runtime_dir().join("pvpn-autoconnect.lock")).is_some();
    (busy, autoconnect_running)
}

/// Read everything. Blocking — run it on a worker thread.
pub fn read(probe_traffic: bool, own_child: Option<u32>) -> Snapshot {
    let network = proc::active_network_key();
    let persisted = proc::proton_client_persisted();
    let nm_server = pvpn_core::dbus::active_proton_server_or_nmcli();
    let connected = persisted.is_some() || nm_server.is_some();
    let server = nm_server
        .clone()
        .or_else(|| persisted.as_ref().and_then(|p| p.server.clone()));
    let protocol = match persisted.as_ref().filter(|p| !p.protocol.is_empty()) {
        Some(p) => p.protocol.clone(),
        None => proc::active_profile_protocol().unwrap_or_else(proc::current_protocol),
    };
    let tunneled = connected && proc::tunnel_is_real();
    let stray_route = if tunneled {
        None
    } else {
        proc::stray_leak_route()
    };
    let carrying = (probe_traffic && tunneled).then(|| pvpn_core::net::net_works_within(3));

    let (busy, autoconnect_running) = read_busy(own_child);

    let config = Config::load(&Config::default_path()).unwrap_or_default();
    let started = scope::started_network(&Config::data_dir());
    let autoconnect_here = scope::allowed(
        &config.autoconnect_networks,
        &config.autoconnect_never,
        &network,
        started.as_deref(),
    );

    Snapshot {
        network,
        connected,
        tunneled,
        server,
        protocol,
        carrying,
        stray_route,
        busy,
        autoconnect_running,
        down_by_user: intent::is_down_by_user(&Config::data_dir()),
        autoconnect_off: intent::autoconnect_is_off(&Config::config_dir()),
        autoconnect_here,
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
        if snap.autoconnect_running {
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
