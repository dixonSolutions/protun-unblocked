//! Did this tunnel work — and if not, who is to blame?
//!
//! The old answer was one boolean from a fixed ninety-second poll: traffic
//! flowed, or it did not. Both halves of that were wrong in a way that
//! showed up on `wifi:detnsw`.
//!
//! **It was slow.** On the attempt that prompted this module the tunnel
//! device came up at 22:09:22, Proton's own local agent gave up on the
//! session at **22:09:46**, and `pvpn` — watching only for traffic — went
//! on polling until 22:11:36 before writing the server off. The verdict had
//! been sitting in Proton's log for a hundred and ten seconds. Reading it
//! costs nothing and ends the attempt when the answer exists.
//!
//! **It blamed the wrong thing.** "No traffic" is also what a tunnel looks
//! like when the wifi drops, when the laptop roams to another SSID, or when
//! a captive portal reasserts itself. Recorded as a server failure that
//! retires a healthy server for a day, four days if it happens twice — so
//! walking out of range once makes tomorrow's connect start from a worse
//! list. Before blaming a server, ask whether the network under the tunnel
//! is still there ([`pvpn_core::link`]).
//!
//! What has *not* changed: a tunnel that is merely slow is still not a
//! failure, and running out of the settle window still does not tear
//! anything down. Every tunnel this tool ever wrote off as dead on a
//! timeout turned out to be alive moments later — which is why the early
//! exits here are only ever taken on *positive evidence*, never on a clock.

use crate::session::blocking;
use chrono::{DateTime, Utc};
use pvpn_core::link::{self, LinkHealth};
use pvpn_core::{net, proc};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

/// What became of an attempt.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Traffic reached the internet through it.
    Carrying { after: Duration },
    /// Proton declared the session over — the far end, or a middlebox
    /// pretending to be it, ended what we built.
    SessionDied { after: Duration, detail: String },
    /// *Our own* uplink went away. Says nothing about the server.
    LinkDown { after: Duration },
    /// The window elapsed with no traffic and no verdict from anyone. The
    /// weakest of the four: it means "we do not know", and it is why a
    /// quiet tunnel is kept rather than torn down.
    Quiet { after: Duration },
}

impl Verdict {
    pub fn carrying(&self) -> bool {
        matches!(self, Verdict::Carrying { .. })
    }

    pub fn after(&self) -> Duration {
        match self {
            Verdict::Carrying { after }
            | Verdict::SessionDied { after, .. }
            | Verdict::LinkDown { after }
            | Verdict::Quiet { after } => *after,
        }
    }

    pub fn seconds(&self) -> u64 {
        self.after().as_secs()
    }
}

const FAST_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STEADY_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// A tunnel that is going to work is usually carrying within this long of
/// activation — measured 0.75s to `tunnel established` on `wifi:detnsw` —
/// so this is the window in which the prover fires rounds fastest.
const FAST_POLL_WINDOW: Duration = Duration::from_secs(3);

/// How often a fresh probe round is launched while the tunnel is young,
/// and once it is not. Rounds are *not* waited for before the next one is
/// sent: a round fired into a tunnel whose handshake has not finished sits
/// there until its timeout, and waiting on it before firing another was
/// what made a tunnel established at 0.75s verify at 4.7s.
const PROVE_INTERVAL_FAST: Duration = Duration::from_millis(100);
const PROVE_INTERVAL_STEADY: Duration = Duration::from_secs(1);

/// Rounds allowed in flight at once. With a one-second probe budget the
/// fast interval fills this in about a second, after which each new round
/// waits for an old one to time out — a bound on how many curl processes a
/// tunnel that never carries can have running at once.
const PROVE_MAX_IN_FLIGHT: usize = 8;

/// Traffic is checked every poll; the log every poll (it is a file read);
/// the uplink less often, because it is a ping and an `ip` call and the
/// wifi does not vanish between one second and the next.
const LINK_CHECK_EVERY: Duration = Duration::from_secs(5);

/// Consecutive `Down` readings before the uplink is called dead.
///
/// One is not enough: a roam between APs on the same SSID drops the
/// gateway for a moment, and treating that as "the network died" would
/// abandon a connect that was about to succeed.
const LINK_DOWN_CONFIRMATIONS: u32 = 2;

/// Per-probe budget for one round of the prover. A round goes to addresses
/// looked up in advance, so each probe is one TCP connect and an HTTP
/// exchange through the tunnel: well under a second when the tunnel works,
/// and a round that has not answered in one was sent too early.
const PROVE_ROUND_TIMEOUT: Duration = Duration::from_secs(1);

/// Is the tunnel carrying traffic *right now*?
///
/// The same gate [`verify`] uses to return [`Verdict::Carrying`], minus the
/// loop: a Proton profile that NetworkManager calls active, a route table
/// that sends packets into a tunnel device, and something on the far side
/// answering. All three, because each one alone has been wrong here — see
/// [`proc::verified_tunnel_active`] for the first two.
///
/// Split out because the interesting question about a tunnel is not only
/// asked at connect time. `verify` answers it once, while a connect is in
/// flight, and then the process exits; everything that wants to ask later —
/// `pvpn status`, `pvpn watch` — needs the same answer without a settle
/// window attached to it. Having two spellings of "carrying" is how the two
/// drift apart, and the drift is always in the direction of the cheap one
/// (routes only) quietly saying yes to a dead tunnel.
pub async fn carrying_now(timeout: Duration) -> bool {
    blocking(proc::verified_tunnel_active).await && net::net_works_raced(timeout).await
}

/// Keep asking until something answers through the tunnel.
///
/// Rounds are launched on a timer and raced, not run one after another:
/// the first to come back positive ends it. Never returns on its own —
/// the caller races it against the evidence loop and the deadline.
async fn prove_traffic(probes: net::ProbeSet, begin: Instant) {
    let mut rounds: JoinSet<bool> = JoinSet::new();
    loop {
        let interval = if begin.elapsed() < FAST_POLL_WINDOW {
            PROVE_INTERVAL_FAST
        } else {
            PROVE_INTERVAL_STEADY
        };
        if rounds.len() < PROVE_MAX_IN_FLIGHT {
            let probes = probes.clone();
            rounds.spawn(async move { probes.any_answers(PROVE_ROUND_TIMEOUT).await });
        }
        let next = tokio::time::sleep(interval);
        tokio::pin!(next);
        loop {
            tokio::select! {
                _ = &mut next => break,
                finished = rounds.join_next(), if !rounds.is_empty() => {
                    if matches!(finished, Some(Ok(true))) {
                        rounds.abort_all();
                        return;
                    }
                }
            }
        }
    }
}

/// Is a tunnel *set up* — profile active, routes pointing into it — whatever
/// it is carrying?
///
/// The other half of the pair. `carrying_now` says whether it works;
/// this says whether there is one to be disappointed by. Together they
/// separate "the VPN is off" from "the VPN is on and dead", which are the
/// two states a route-only check reports identically.
pub async fn tunnel_is_up() -> bool {
    blocking(proc::verified_tunnel_active).await
}

/// Watch a fresh tunnel until it proves itself, someone declares it dead,
/// or `settle` runs out.
///
/// `started` must be the instant *this* attempt began, so the log is read
/// for this attempt and not the last one's failure. `network` is the key
/// the connect was made on: coming back on a different one is a roam, and
/// nothing measured through the new network can be attributed to the
/// server chosen on the old one.
pub async fn verify(started: DateTime<Utc>, settle: Duration, network: &str) -> Verdict {
    verify_with(started, settle, network, net::ProbeSet::unresolved()).await
}

/// [`verify`], probing the given endpoints — looked up before the routes
/// changed, when the caller had the chance. See [`net::ProbeSet`].
pub async fn verify_with(
    started: DateTime<Utc>,
    settle: Duration,
    network: &str,
    probes: net::ProbeSet,
) -> Verdict {
    let begin = Instant::now();
    let deadline = begin + settle;
    let mut link_checked_at = begin;
    let mut link_down_seen: u32 = 0;

    // The prover runs the whole time, independently of the evidence checks
    // below, so an answer is noticed the moment it arrives rather than at
    // the next poll. Restarted only if an answer turns out not to have come
    // through a tunnel.
    let mut prover: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(prove_traffic(probes.clone(), begin));

    loop {
        let poll = if begin.elapsed() < FAST_POLL_WINDOW {
            FAST_POLL_INTERVAL
        } else {
            STEADY_POLL_INTERVAL
        };
        let evidence_due = tokio::time::sleep(poll);
        tokio::pin!(evidence_due);

        tokio::select! {
            _ = &mut prover => {
                // Positive NetworkManager evidence gates success. A stale
                // Proton status plus ordinary internet on the physical
                // uplink must never be mistaken for verified VPN traffic.
                // Traffic that answers while the routes do not point into a
                // tunnel is a leak, not a verdict — keep watching.
                if blocking(proc::verified_tunnel_active).await {
                    return Verdict::Carrying {
                        after: begin.elapsed(),
                    };
                }
                // Ask again, but only after a poll interval, and never past
                // the deadline. This used to restart at once and `continue`
                // straight past the deadline check below — and ordinary
                // internet answers every round, so a profile that came up
                // on the physical device kept the connect verifying forever.
                if Instant::now() >= deadline {
                    return Verdict::Quiet {
                        after: begin.elapsed(),
                    };
                }
                let probes = probes.clone();
                prover = Box::pin(async move {
                    tokio::time::sleep(poll).await;
                    prove_traffic(probes, begin).await
                });
                continue;
            }
            _ = &mut evidence_due => {}
        }

        // Proton's own verdict on the session it built. Cheap: a tail of a
        // file, no network at all — which matters, because everything else
        // that could ask a question right now is going through a tunnel
        // that may be dead.
        let log = blocking(proc::ProtonLogSnapshot::recent).await;
        if let Some(detail) = log.session_death_since(started) {
            return Verdict::SessionDied {
                after: begin.elapsed(),
                detail,
            };
        }

        if link_checked_at.elapsed() >= LINK_CHECK_EVERY {
            link_checked_at = Instant::now();

            // Roaming is the unambiguous case: whatever we learn now is
            // about a different network than the one we chose this server
            // for, so there is nothing to attribute and nothing to keep.
            let key = blocking(proc::active_network_key).await;
            if key != network {
                tracing::warn!("moved to {key} mid-connect — nothing here is this server's doing");
                return Verdict::LinkDown {
                    after: begin.elapsed(),
                };
            }

            match blocking(link::health).await {
                LinkHealth::Down => {
                    link_down_seen += 1;
                    if link_down_seen >= LINK_DOWN_CONFIRMATIONS {
                        return Verdict::LinkDown {
                            after: begin.elapsed(),
                        };
                    }
                    tracing::warn!("the uplink looks down — confirming before blaming anything");
                }
                // Anything short of positive evidence resets the count.
                // `Unknown` is the common case on APs that drop ICMP and
                // must never accumulate into a verdict.
                _ => link_down_seen = 0,
            }
        }

        if Instant::now() >= deadline {
            return Verdict::Quiet {
                after: begin.elapsed(),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_carrying_counts_as_working() {
        assert!(Verdict::Carrying {
            after: Duration::from_secs(1)
        }
        .carrying());
        assert!(!Verdict::Quiet {
            after: Duration::from_secs(90)
        }
        .carrying());
        assert!(!Verdict::LinkDown {
            after: Duration::from_secs(7)
        }
        .carrying());
        assert!(!Verdict::SessionDied {
            after: Duration::from_secs(24),
            detail: "Reached connection error state: Timeout".into()
        }
        .carrying());
    }

    #[test]
    fn every_verdict_says_how_long_it_took() {
        // The number that goes in the history, and the one to look at
        // before anyone reaches for `settle_secs`.
        assert_eq!(
            Verdict::SessionDied {
                after: Duration::from_secs(24),
                detail: String::new()
            }
            .seconds(),
            24
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_prover_returns_on_the_first_round_that_answers() {
        // Every round is a real curl to a listener that answers; the prover
        // must come back well inside one probe budget, not after it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut request = [0; 256];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let _ = std::io::Write::write_all(&mut stream, b"HTTP/1.1 204 No Content\r\n\r\n");
            }
        });
        let probes = net::ProbeSet::only_for_tests(
            format!("http://pvpn-prover.invalid:{port}/generate_204"),
            format!("pvpn-prover.invalid:{port}:127.0.0.1"),
        );
        let began = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), prove_traffic(probes, began))
            .await
            .expect("an answering endpoint must end the prover");
        assert!(began.elapsed() < PROVE_ROUND_TIMEOUT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_that_skip_the_tunnel_cannot_outlast_the_deadline() {
        // Every round answers, but not through a tunnel — the physical
        // uplink, or a profile activated on it. That is a leak, not a
        // verdict, and it must end at the deadline like silence does.
        if blocking(proc::verified_tunnel_active).await {
            return; // a real tunnel is up; the premise does not hold here
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut request = [0; 256];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let _ = std::io::Write::write_all(&mut stream, b"HTTP/1.1 204 No Content\r\n\r\n");
            }
        });
        let probes = net::ProbeSet::only_for_tests(
            format!("http://pvpn-leak.invalid:{port}/generate_204"),
            format!("pvpn-leak.invalid:{port}:127.0.0.1"),
        );
        let network = blocking(proc::active_network_key).await;
        let verdict = tokio::time::timeout(
            Duration::from_secs(15),
            verify_with(Utc::now(), Duration::from_secs(1), &network, probes),
        )
        .await
        .expect("a leak must not keep verify going past its deadline");
        assert!(!verdict.carrying(), "an answer outside the tunnel is not carrying");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_zero_window_still_returns_a_verdict_rather_than_hanging() {
        // `PVPN_SETTLE=0` is a legitimate way to ask "is it up right now?".
        let verdict = verify(Utc::now(), Duration::from_secs(0), "wifi:nowhere").await;
        assert!(
            !matches!(verdict, Verdict::SessionDied { .. }),
            "nothing in Proton's log belongs to an attempt made a moment ago"
        );
    }
}
