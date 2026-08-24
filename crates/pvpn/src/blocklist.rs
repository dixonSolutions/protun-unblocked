//! Promote and expire blocked entries from real connect outcomes.
//!
//! A probe can never write here — a fast TLS handshake does not prove a
//! server works. Only `connect` calls these helpers.

use chrono::{DateTime, Utc};
use pvpn_core::state::State;

#[derive(Debug, Clone, Copy)]
pub enum ConnectOutcome {
    TrafficOk,
    ConnectedNoTraffic,
    Refused,
    HandshakeClosedEarly,
    /// *Our* certificate expired, not their server's fault.
    ///
    /// The protun local agent refuses a session whose client certificate
    /// has lapsed, and renewing it needs `/vpn/v1/certificate` — the very
    /// API path a filtered network blocks. Every server fails identically
    /// until the certificate is renewed, so recording this as a block
    /// retires healthy servers one after another and teaches the ranker
    /// exactly the wrong thing.
    CertificateExpired,
}

impl ConnectOutcome {
    pub fn reason(self) -> Option<&'static str> {
        match self {
            ConnectOutcome::TrafficOk | ConnectOutcome::CertificateExpired => None,
            ConnectOutcome::ConnectedNoTraffic => Some("no-traffic-after-settle"),
            ConnectOutcome::Refused => Some("refused"),
            ConnectOutcome::HandshakeClosedEarly => Some("handshake-closed-early"),
        }
    }
}

pub fn apply(state: &mut State, name: &str, outcome: ConnectOutcome, now: DateTime<Utc>) {
    match outcome {
        ConnectOutcome::TrafficOk => state.record_connect_success(name, now),
        // Neither a success nor the server's failure: leave its record
        // untouched so it keeps whatever standing it had earned.
        ConnectOutcome::CertificateExpired => {}
        other => {
            if let Some(reason) = other.reason() {
                state.record_connect_blocked(name, reason, now);
            }
        }
    }
}

/// Blocks are never permanent: after the hold elapses they become `known`
/// again so a later connect will retry them. Networks and pools change.
pub fn expire(state: &mut State, retry_after: chrono::Duration, now: DateTime<Utc>) {
    state.expire_blocks(retry_after, now);
}

/// Classify a failed `protonvpn connect` from its captured output.
pub fn classify_failure(log: &str) -> ConnectOutcome {
    let lower = log.to_lowercase();
    if log_says_expired_certificate(&lower) {
        ConnectOutcome::CertificateExpired
    } else if lower.contains("tls handshake failed") || lower.contains("tls error") {
        ConnectOutcome::HandshakeClosedEarly
    } else {
        ConnectOutcome::Refused
    }
}

/// The CLI does not always name this, which is why `connect` also consults
/// Proton's own log via [`pvpn_core::proc::cert_failure_recent`].
fn log_says_expired_certificate(lower: &str) -> bool {
    lower.contains("expiredcertificate")
        || lower.contains("certificate refresh failed")
        || lower.contains("certificate has expired")
}

pub fn log_says_missing_backend(log: &str) -> bool {
    log.to_lowercase().contains("no valid implementation found")
}

pub fn log_says_free_plan(log: &str) -> bool {
    log.to_lowercase()
        .contains("not available on the free plan")
}

pub fn log_says_hard_refusal(log: &str) -> bool {
    let lower = log.to_lowercase();
    lower.contains("not available on the free plan") || lower.contains("missing username")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pvpn_core::state::{ServerStatus, State};

    fn state_on(network: &str) -> State {
        let mut state = State::default();
        state.set_network(network);
        state
    }

    #[test]
    fn an_expired_certificate_never_blames_the_server() {
        // Every server fails identically while our certificate is stale.
        // Blocking one and moving on burned the whole shortlist in three
        // minutes without a single bad server in it.
        let mut state = state_on("wifi:school");
        let now = Utc::now();
        apply(&mut state, "SG-FREE#9", ConnectOutcome::CertificateExpired, now);
        assert!(state.servers().get("SG-FREE#9").is_none());
        assert!(!state.is_blocked("SG-FREE#9", chrono::Duration::hours(24), now));
    }

    #[test]
    fn an_expired_certificate_does_not_undo_earned_standing() {
        let mut state = state_on("wifi:school");
        let now = Utc::now();
        apply(&mut state, "SG-FREE#9", ConnectOutcome::TrafficOk, now);
        apply(&mut state, "SG-FREE#9", ConnectOutcome::CertificateExpired, now);
        assert_eq!(state.servers()["SG-FREE#9"].status, ServerStatus::Known);
        assert_eq!(state.servers()["SG-FREE#9"].consecutive_connect_failures, 0);
    }

    #[test]
    fn a_quiet_tunnel_does_block_the_server() {
        let mut state = state_on("wifi:school");
        let now = Utc::now();
        apply(
            &mut state,
            "SG-FREE#9",
            ConnectOutcome::ConnectedNoTraffic,
            now,
        );
        assert!(state.is_blocked("SG-FREE#9", chrono::Duration::hours(24), now));
        assert_eq!(
            state.servers()["SG-FREE#9"].blocked_reason.as_deref(),
            Some("no-traffic-after-settle")
        );
    }

    #[test]
    fn a_certificate_message_is_read_as_our_problem_not_theirs() {
        assert!(matches!(
            classify_failure("Reached connection error state: ExpiredCertificate (None)"),
            ConnectOutcome::CertificateExpired
        ));
        assert!(matches!(
            classify_failure("Certificate refresh failed: No working transports found"),
            ConnectOutcome::CertificateExpired
        ));
    }

    #[test]
    fn a_tls_failure_is_still_read_as_the_network_killing_the_handshake() {
        assert!(matches!(
            classify_failure("TLS handshake failed"),
            ConnectOutcome::HandshakeClosedEarly
        ));
        assert!(matches!(
            classify_failure("connection refused"),
            ConnectOutcome::Refused
        ));
    }
}
