//! Did the tunnel survive the night? — the question nothing used to ask.
//!
//! [`crate::verify`] watches a tunnel until it proves itself and then the
//! process exits. That was the whole of this tool's opinion about a tunnel's
//! health, and it is an opinion formed in the first fifteen seconds of a
//! connection that may last hours.
//!
//! Measured 2026-09-15 on `wifi:detnsw`, JP-FREE#11 over protun-tcp:
//!
//! | time | what happened |
//! |---|---|
//! | 07:57:35 | `tunnel established`, traffic verified |
//! | 07:57:42 | recorded `ok`, `pvpn` exited |
//! | 08:00:35 | `supervisor: timer elapsed ToDo=RetryEndpoint(…:443, WireguardTcp)` |
//! | 08:00:40 | `systemd-resolved` starts cycling feature sets on `10.2.0.1` |
//! | 08:02:43 | the user gives up and runs `pvpn down` |
//!
//! The middlebox killed the TCP carrier at the three-minute mark and the
//! client's retry never re-established it. `proton0` stayed up, the routes
//! stayed pointing into it, and so every route-based check on this machine —
//! `pvpn status`, `pvpn-autoconnect`'s `tunnel_up`, NetworkManager's own
//! `activated` — went on saying yes for the two minutes it took a human to
//! notice DNS was hanging. Nothing was written to the history, so tomorrow's
//! ranking still believes JP-FREE#11 is the best server here.
//!
//! This is not the deleted daemon. Nothing here runs in the background,
//! holds `want_up`, or has an opinion about what should be connected while
//! you are not looking. It is one short-lived process that asks whether the
//! tunnel that *already exists* is carrying, and only ever runs because
//! something started it: a timer, a resume hook, or you.
//!
//! The care taken over blame is the same care [`crate::verify`] takes, for
//! the same reason. "Nothing comes back" is also what a tunnel looks like
//! when the wifi drops, and a server written off because someone walked out
//! of range is a healthy server missing from tomorrow's list.

use crate::blocklist::ConnectOutcome;
use crate::connect::{self, UpReport};
use crate::session::{blocking, Session};
use crate::verify;
use chrono::Utc;
use pvpn_core::config::Config;
use pvpn_core::link::{self, LinkHealth};
use pvpn_core::{intent, proc};
use std::time::Duration;

/// Per-endpoint budget for one reachability probe.
///
/// Endpoints are raced, so a working tunnel answers in well under a second
/// and this number is only ever paid in full by a broken one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Consecutive failed probes before the tunnel is called dead.
///
/// Three, not one. A single failed probe is a lost packet, a DNS hiccup or
/// an endpoint having a bad minute, and acting on it would tear down a
/// working tunnel — the failure mode this codebase has consistently judged
/// worse than being slow to spot a dead one.
const CONFIRMATIONS: u32 = 3;

/// Gap between confirmation probes. Long enough that three of them sample
/// three different moments rather than one, short enough that the whole
/// check fits inside the timer interval with room to spare.
const CONFIRM_GAP: Duration = Duration::from_secs(2);

/// What the tunnel is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Nothing is up. Not this command's problem — rebuilding a tunnel that
    /// was never there is `pvpn up`'s job, and after a resume it is
    /// `pvpn-autoconnect`'s.
    NoTunnel,
    /// Up and carrying.
    Carrying,
    /// Up, traffic going into it, nothing coming back — and our own uplink
    /// is fine, so this is the tunnel's fault.
    Dead {
        server: Option<String>,
        protocol: String,
    },
    /// Up and not carrying, but *our* network is gone. Says nothing about
    /// the server and must never be recorded against one.
    LinkDown,
}

impl Health {
    pub fn describe(&self) -> String {
        match self {
            Health::NoTunnel => "no tunnel is up".to_string(),
            Health::Carrying => "the tunnel is carrying traffic".to_string(),
            Health::Dead { server, .. } => match server {
                Some(name) => format!("{name} is up but carrying nothing"),
                None => "the tunnel is up but carrying nothing".to_string(),
            },
            Health::LinkDown => "this machine's own network is down".to_string(),
        }
    }
}

/// Ask what the tunnel is doing, confirming a bad answer before believing it.
///
/// Returns on the *first* successful probe, so the overwhelmingly common
/// case — a healthy tunnel — costs one round trip and no sleeping at all.
/// Only a tunnel that looks dead pays for the confirmations.
pub async fn check() -> Health {
    if !verify::tunnel_is_up().await {
        return Health::NoTunnel;
    }

    for attempt in 0..CONFIRMATIONS {
        if verify::carrying_now(PROBE_TIMEOUT).await {
            return Health::Carrying;
        }
        // Between probes, not before the first: the uplink question only
        // becomes interesting once something has already failed.
        if matches!(blocking(link::health).await, LinkHealth::Down) {
            return Health::LinkDown;
        }
        // The tunnel can be torn down under us mid-check — by a manual
        // `pvpn down` in another terminal, or by the plugin giving up. That
        // is not a dead server, it is no server.
        if !verify::tunnel_is_up().await {
            return Health::NoTunnel;
        }
        if attempt + 1 < CONFIRMATIONS {
            tokio::time::sleep(CONFIRM_GAP).await;
        }
    }

    let server = match blocking(pvpn_core::dbus::active_proton_server).await {
        Some(name) => Some(name),
        None => blocking(proc::active_proton_server).await,
    };
    // What the *active profile* says it is, not what Proton's settings say
    // the next connect would use — reading the latter is what once filed a
    // protun-tls success that protun-tcp had earned.
    let protocol = blocking(proc::active_profile_protocol)
        .await
        .unwrap_or_default();
    Health::Dead { server, protocol }
}

/// `pvpn watch` — check the tunnel, record what is found, and put it back.
///
/// `reconnect` is what separates "tell me" from "fix it". Recording happens
/// either way: the history is how tomorrow's ranking learns that a server
/// which connects here does not necessarily *stay* connected here, and that
/// lesson is worth writing down even when the user does not want anything
/// rebuilt right now.
pub async fn watch(session: &mut Session, reconnect: bool) -> UpReport {
    // Anchors the log read below. Only a death Proton logged *while we were
    // watching* can honestly be attributed to the tunnel we are watching —
    // a wider window would happily quote the failure of the connect before
    // this one.
    let watching_since = Utc::now();
    let watching_on = session.network().to_string();
    let health = check().await;

    // A check takes seconds and a laptop can be carried between networks in
    // that time. Everything below files an observation against
    // `session.network()`, which was resolved before the check began: coming
    // back on a different one means the server would be blamed on a network
    // it was never tried on. `verify` refuses to attribute across a roam for
    // the same reason.
    if session.sync_network().await {
        return UpReport {
            ok: true,
            message: format!(
                "Moved from {watching_on} to {} while checking — nothing here is \
                 this server's doing.",
                session.network()
            ),
            server: None,
        };
    }

    match health {
        Health::Carrying => UpReport {
            ok: true,
            message: "Tunnel is carrying traffic.".to_string(),
            server: None,
        },
        Health::NoTunnel => UpReport {
            ok: true,
            message: "No tunnel is up — nothing to watch.".to_string(),
            server: None,
        },
        // Nobody is blamed and nothing is rebuilt: there is nothing to build
        // a tunnel *over*. Recorded all the same, because an evening of
        // `link-down` entries is the answer to "why did none of this work",
        // and an evening with no entries at all is not.
        Health::LinkDown => {
            if let Some(server) = current_server().await {
                let protocol = blocking(proc::active_profile_protocol)
                    .await
                    .unwrap_or_default();
                connect::record_post_connect_outcome(
                    session,
                    &server,
                    &protocol,
                    ConnectOutcome::LocalNetworkDown,
                    None,
                    None,
                )
                .await;
            }
            UpReport {
                ok: true,
                message: "This machine's network is down — the tunnel is not at fault.".to_string(),
                server: None,
            }
        }
        Health::Dead { server, protocol } => {
            let Some(server) = server else {
                // A tunnel device carrying nothing, with no Proton profile to
                // attribute it to. Nothing to record against and nothing to
                // hop away from; `pvpn fix` is the command for this.
                return UpReport {
                    ok: false,
                    message: "A tunnel is up and carrying nothing, but no Proton profile \
                              claims it.\nRun: pvpn fix"
                        .to_string(),
                    server: None,
                };
            };

            // Usually `None`, and that is the honest answer: on the
            // 2026-09-15 case Proton logged one `RetryEndpoint` line and then
            // nothing at all, so the only evidence the tunnel was dead was
            // that nothing came back. Quote the client when it has something
            // to say; never invent a reason when it does not.
            let detail = blocking(proc::ProtonLogSnapshot::recent)
                .await
                .session_death_since(watching_since);

            connect::record_post_connect_outcome(
                session,
                &server,
                &protocol,
                ConnectOutcome::SessionKilled,
                detail.clone(),
                None,
            )
            .await;

            let said = detail
                .map(|detail| format!(" Proton's log says: {detail}"))
                .unwrap_or_default();

            if !reconnect {
                return UpReport {
                    ok: false,
                    message: format!(
                        "{server} is up and carrying nothing — recorded against it here.{said}\n\
                         Run: pvpn up"
                    ),
                    server: Some(server),
                };
            }

            if intent::autoconnect_is_off(&Config::config_dir()) {
                return UpReport {
                    ok: false,
                    message: format!(
                        "{server} is up and carrying nothing — recorded against it here.{said}\n\
                         Not reconnecting: autoconnect is off. Run `pvpn up`, or \
                         `pvpn-autoconnect --on`."
                    ),
                    server: Some(server),
                };
            }

            // `connect::up` does not take the connect lock — every caller in
            // `main` takes it for them — so this has to, and it has to be
            // taken here rather than around the whole command. A health check
            // is read-only, and making a manual `pvpn up` queue behind a
            // routine one would be the tool getting in its own way.
            let _guard = pvpn_core::lock::acquire().await;

            // Waiting for that lock means somebody else was mid-connect, and
            // what they built may be exactly what was missing. Ask once more
            // before tearing anything down: the whole point of the lock is
            // that two connects must not overlap, and a connect launched onto
            // a tunnel that just came up is the overlap arriving late.
            if verify::carrying_now(PROBE_TIMEOUT).await {
                return UpReport {
                    ok: true,
                    message: format!(
                        "{server} stopped carrying and something else has already put a \
                         tunnel back.{said}"
                    ),
                    server: Some(server),
                };
            }

            tracing::info!("{server} stopped carrying — reconnecting somewhere else");
            // `record` above blocked this server, and `up` reads the
            // blocklist before it chooses, so this lands somewhere else
            // without having to be told to.
            connect::up(session, None).await
        }
    }
}

/// The Proton profile currently attached, however little it is carrying.
async fn current_server() -> Option<String> {
    match blocking(pvpn_core::dbus::active_proton_server).await {
        Some(name) => Some(name),
        None => blocking(proc::active_proton_server).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_tunnel_names_the_server_it_blames() {
        let health = Health::Dead {
            server: Some("JP-FREE#11".into()),
            protocol: "protun-tcp".into(),
        };
        assert!(health.describe().contains("JP-FREE#11"));
    }

    /// The one distinction the whole module exists to make. Both of these
    /// are "nothing comes back"; only one of them is a server's fault.
    #[test]
    fn a_dead_tunnel_and_a_dead_uplink_are_different_answers() {
        assert_ne!(
            Health::LinkDown,
            Health::Dead {
                server: Some("JP-FREE#11".into()),
                protocol: "protun-tcp".into(),
            }
        );
        assert!(!Health::LinkDown.describe().contains("tunnel is up"));
    }

    #[test]
    fn no_tunnel_is_not_reported_as_a_failure() {
        assert_eq!(Health::NoTunnel.describe(), "no tunnel is up");
    }

    /// Three probes two seconds apart have to finish well inside the
    /// interval the timer fires on, or checks pile up on each other.
    #[test]
    fn a_full_confirmation_run_fits_inside_the_timer_interval() {
        let worst_case =
            PROBE_TIMEOUT * CONFIRMATIONS + CONFIRM_GAP * (CONFIRMATIONS.saturating_sub(1));
        assert!(
            worst_case < Duration::from_secs(60),
            "a check must not outlast the gap between checks; worst case is {worst_case:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_tunnel_is_decided_without_probing_anything() {
        // With nothing up, `check` must return before it ever reaches the
        // confirmation loop — otherwise every idle timer firing costs three
        // probe timeouts.
        let began = std::time::Instant::now();
        let health = check().await;
        if matches!(health, Health::NoTunnel) {
            assert!(
                began.elapsed() < PROBE_TIMEOUT,
                "NoTunnel must short-circuit before the first probe"
            );
        }
    }
}
