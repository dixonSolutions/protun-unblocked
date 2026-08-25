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

/// How often to ask the questions that cost a subprocess.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

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

/// Per-probe budget while verifying. Shorter than the default: a working
/// tunnel answers one of these in well under a second, and three probes
/// timing out should not stretch a one-second poll into ten.
const PROBE_TIMEOUT_SECS: u64 = 2;

/// Watch a fresh tunnel until it proves itself, someone declares it dead,
/// or `settle` runs out.
///
/// `started` must be the instant *this* attempt began, so the log is read
/// for this attempt and not the last one's failure. `network` is the key
/// the connect was made on: coming back on a different one is a roam, and
/// nothing measured through the new network can be attributed to the
/// server chosen on the old one.
pub async fn verify(started: DateTime<Utc>, settle: Duration, network: &str) -> Verdict {
    let begin = Instant::now();
    let deadline = begin + settle;
    let mut link_checked_at = begin;
    let mut link_down_seen: u32 = 0;

    loop {
        // Traffic first, and it wins outright. A tunnel that is carrying
        // packets is working, whatever anything else has to say — an error
        // logged on the way up does not matter once it came up.
        if blocking(|| net::net_works_within(PROBE_TIMEOUT_SECS)).await {
            return Verdict::Carrying {
                after: begin.elapsed(),
            };
        }

        // Proton's own verdict on the session it built. Cheap: a tail of a
        // file, no network at all — which matters, because everything else
        // that could ask a question right now is going through a tunnel
        // that may be dead.
        if let Some(detail) = blocking(move || proc::session_death_since(started)).await {
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
        tokio::time::sleep(POLL_INTERVAL).await;
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
    async fn a_zero_window_still_returns_a_verdict_rather_than_hanging() {
        // `PVPN_SETTLE=0` is a legitimate way to ask "is it up right now?".
        let verdict = verify(Utc::now(), Duration::from_secs(0), "wifi:nowhere").await;
        assert!(
            !matches!(verdict, Verdict::SessionDied { .. }),
            "nothing in Proton's log belongs to an attempt made a moment ago"
        );
    }
}
