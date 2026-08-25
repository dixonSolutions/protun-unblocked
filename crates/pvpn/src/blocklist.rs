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
    /// Connected, then Proton declared the session over.
    ///
    /// Distinct from [`ConnectedNoTraffic`](Self::ConnectedNoTraffic),
    /// which is only ever "we waited and nothing happened". This one is
    /// somebody's verdict, arriving in seconds rather than after a window,
    /// and on a network that terminates TLS locally it is the outcome —
    /// the handshake completes and the session is dropped a moment later
    /// (`docs/transparent-proxy.md`).
    SessionKilled,
    /// *Our* certificate expired, not their server's fault.
    ///
    /// The protun local agent refuses a session whose client certificate
    /// has lapsed, and renewing it needs `/vpn/v1/certificate` — the very
    /// API path a filtered network blocks. Every server fails identically
    /// until the certificate is renewed, so recording this as a block
    /// retires healthy servers one after another and teaches the ranker
    /// exactly the wrong thing.
    CertificateExpired,
    /// Proton's client could not select or start the requested connection.
    ///
    /// Account restrictions, stale inventory, authentication failures and
    /// unknown client errors say nothing reliable about the server itself.
    ClientError,
    /// *Our own* uplink went away mid-attempt — the wifi dropped, or the
    /// machine roamed to another network.
    ///
    /// Says nothing whatever about the server, and for the same reason as
    /// `CertificateExpired` must never be recorded as one: a server retired
    /// because someone carried the laptop out of range is a healthy server
    /// missing from tomorrow's list, held for a day, four days if it
    /// happens twice.
    LocalNetworkDown,
}

impl ConnectOutcome {
    /// Why this server is blocked — or `None` when the outcome is not the
    /// server's fault. The whole of the blame policy is this one function.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            ConnectOutcome::TrafficOk
            | ConnectOutcome::CertificateExpired
            | ConnectOutcome::ClientError
            | ConnectOutcome::LocalNetworkDown => None,
            ConnectOutcome::ConnectedNoTraffic => Some("no-traffic-after-settle"),
            ConnectOutcome::Refused => Some("refused"),
            ConnectOutcome::HandshakeClosedEarly => Some("handshake-closed-early"),
            ConnectOutcome::SessionKilled => Some("session-killed"),
        }
    }

    /// Short tag for `pvpn history`. Every outcome has one, including the
    /// ones nobody is blamed for — an evening of `link-down` entries is
    /// the answer to "why did none of this work", and an evening with no
    /// entries at all is not.
    pub fn tag(self) -> &'static str {
        match self {
            ConnectOutcome::TrafficOk => "ok",
            ConnectOutcome::ConnectedNoTraffic => "no-traffic",
            ConnectOutcome::Refused => "refused",
            ConnectOutcome::HandshakeClosedEarly => "handshake-closed",
            ConnectOutcome::SessionKilled => "session-killed",
            ConnectOutcome::CertificateExpired => "cert-expired",
            ConnectOutcome::ClientError => "client-error",
            ConnectOutcome::LocalNetworkDown => "link-down",
        }
    }

    /// Did anything go wrong that this server should answer for?
    pub fn blames_the_server(self) -> bool {
        self.reason().is_some()
    }
}

pub fn apply(state: &mut State, name: &str, outcome: ConnectOutcome, now: DateTime<Utc>) {
    match outcome {
        ConnectOutcome::TrafficOk => state.record_connect_success(name, now),
        // Neither a success nor the server's failure. Its standing is left
        // exactly as it was — but the attempt still happened, and a
        // history that does not show it is a history that lies about the
        // evening you go looking at.
        ConnectOutcome::CertificateExpired
        | ConnectOutcome::ClientError
        | ConnectOutcome::LocalNetworkDown => state.record_connect_attempt(name, now),
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
    } else if lower.contains("connect timeout") || lower.contains("error state: timeout") {
        // The local agent opened its session to the node and never heard
        // back. Not a refusal — something answered, then stopped.
        ConnectOutcome::SessionKilled
    } else if lower.contains("connection refused") || lower.contains("actively refused") {
        ConnectOutcome::Refused
    } else {
        ConnectOutcome::ClientError
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
        apply(
            &mut state,
            "SG-FREE#9",
            ConnectOutcome::CertificateExpired,
            now,
        );
        assert!(!state.is_blocked("SG-FREE#9", chrono::Duration::hours(24), now));
        assert_eq!(state.servers()["SG-FREE#9"].status, ServerStatus::Known);
        assert_eq!(
            state.servers()["SG-FREE#9"].connect_attempts,
            1,
            "not blamed, but it was tried, and the history has to say so"
        );
    }

    #[test]
    fn our_own_wifi_dropping_never_costs_a_server_its_place() {
        // Carry the laptop out of range mid-connect and the tunnel stops
        // carrying traffic — indistinguishable from a dead server if all
        // you measure is traffic. Blocked, it would be gone from the
        // ranked list for a day, four days the second time.
        let mut state = state_on("wifi:detnsw");
        let now = Utc::now();
        apply(&mut state, "JP-FREE#11", ConnectOutcome::TrafficOk, now);
        apply(
            &mut state,
            "JP-FREE#11",
            ConnectOutcome::LocalNetworkDown,
            now,
        );
        assert!(!state.is_blocked("JP-FREE#11", chrono::Duration::hours(24), now));
        assert_eq!(
            state.servers()["JP-FREE#11"].consecutive_connect_failures,
            0
        );
        assert_eq!(
            state.working_list().len(),
            1,
            "and it keeps the standing it earned by working"
        );
    }

    #[test]
    fn a_killed_session_is_the_servers_to_answer_for() {
        let mut state = state_on("wifi:detnsw");
        let now = Utc::now();
        apply(&mut state, "SG-FREE#13", ConnectOutcome::SessionKilled, now);
        assert!(state.is_blocked("SG-FREE#13", chrono::Duration::hours(24), now));
        assert_eq!(
            state.servers()["SG-FREE#13"].blocked_reason.as_deref(),
            Some("session-killed")
        );
    }

    #[test]
    fn every_outcome_has_a_tag_and_only_some_of_them_blame_anyone() {
        for outcome in [
            ConnectOutcome::TrafficOk,
            ConnectOutcome::ConnectedNoTraffic,
            ConnectOutcome::Refused,
            ConnectOutcome::HandshakeClosedEarly,
            ConnectOutcome::SessionKilled,
            ConnectOutcome::CertificateExpired,
            ConnectOutcome::ClientError,
            ConnectOutcome::LocalNetworkDown,
        ] {
            assert!(!outcome.tag().is_empty());
        }
        assert!(ConnectOutcome::SessionKilled.blames_the_server());
        assert!(!ConnectOutcome::LocalNetworkDown.blames_the_server());
        assert!(!ConnectOutcome::CertificateExpired.blames_the_server());
        assert!(!ConnectOutcome::ClientError.blames_the_server());
        assert!(!ConnectOutcome::TrafficOk.blames_the_server());
    }

    #[test]
    fn an_agent_that_never_answered_is_not_read_as_a_refusal() {
        // "Refused" means nothing was there. Here something was there, took
        // the handshake, and went quiet — which is what this network does.
        assert!(matches!(
            classify_failure("Reached connection error state: Timeout (None)"),
            ConnectOutcome::SessionKilled
        ));
        assert!(matches!(
            classify_failure("localagent_mixin:228 | INFO | Connect timeout"),
            ConnectOutcome::SessionKilled
        ));
    }

    #[test]
    fn an_expired_certificate_does_not_undo_earned_standing() {
        let mut state = state_on("wifi:school");
        let now = Utc::now();
        apply(&mut state, "SG-FREE#9", ConnectOutcome::TrafficOk, now);
        apply(
            &mut state,
            "SG-FREE#9",
            ConnectOutcome::CertificateExpired,
            now,
        );
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

    #[test]
    fn an_unknown_client_or_account_error_never_blames_the_server() {
        assert!(matches!(
            classify_failure("Unable to select a server for this account"),
            ConnectOutcome::ClientError
        ));
        assert!(matches!(
            classify_failure("unexpected proton client failure"),
            ConnectOutcome::ClientError
        ));
    }
}
