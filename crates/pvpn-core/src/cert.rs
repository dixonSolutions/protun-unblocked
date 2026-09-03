//! Proton's client certificate, read from where it actually lives.
//!
//! Everything else here learns about the certificate the way the user does:
//! connect, fail, and read Proton's log for `ExpiredCertificate`. That only
//! works on the path that runs Proton's client. `activate_saved_fast_path`
//! does not — it activates the saved NetworkManager profile over D-Bus —
//! so on that path a lapsed certificate is indistinguishable from a dead
//! server, and the server is written off for it.
//!
//! Measured on `wifi:detnsw`, 2026-08-31: the certificate expired at
//! 08:20:19. SG-FREE#2 had carried traffic at 07:55 and had fourteen
//! successful connects behind it. At 08:42 the fast path activated it,
//! waited out the full ninety-second settle window with nothing in Proton's
//! log to read, and blocked it as `no-traffic-after-settle`. One `pvpn
//! forget` later it did the same thing again.
//!
//! None of that inference was necessary. The expiry is a number sitting in
//! the keyring beside the certificate, readable in a couple of hundred
//! milliseconds without touching the network or the routing table. This
//! module reads it.

use chrono::{DateTime, TimeZone, Utc};
use std::time::Duration;

/// Proton refuses a certificate with less than this remaining — see
/// `MINIMUM_VALIDITY_PERIOD_IN_SECS` in `proton/vpn/session/credentials.py`,
/// which is also what logs `CREDENTIALS.CERTIFICATE:REQUIRE_REFRESH`. There
/// is no point setting out on a connect with less than Proton will accept.
pub const MINIMUM_VALIDITY_SECS: i64 = 300;

/// Reading the keyring is local and takes about 200ms. Anything past this
/// means the secret service is not answering, which is a "we do not know",
/// not a verdict.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Walks the accounts in Proton's SSO keyring and prints the VPN
/// certificate's two timestamps as JSON.
///
/// The account key is base32 of the account name with the padding stripped
/// and lowercased — `ProtonSSO.__keyring_key_name` in `proton/sso/sso.py`.
/// Reproduced rather than imported because `proton.sso` will happily start
/// refreshing sessions, and this is meant to be a read.
const CERT_STATUS_PY: &str = r#"
import base64, json, sys
from proton.loader import Loader

keyring = Loader.get("keyring")()
found = None
for account in keyring["proton-sso-accounts"]:
    name = base64.b32encode(account.encode("utf8")).decode("ascii").rstrip("=").lower()
    try:
        data = keyring[f"proton-sso-account-{name}"]
    except Exception:
        continue
    cert = ((data or {}).get("vpn") or {}).get("certificate") or {}
    if "ExpirationTime" not in cert:
        continue
    expires = float(cert["ExpirationTime"])
    found = {"expires_at": expires, "refresh_at": float(cert.get("RefreshTime", expires))}
    break

if found is None:
    sys.exit(2)
json.dump(found, sys.stdout)
"#;

/// Asks Proton for a new certificate and prints the new timestamps.
///
/// `VPNSession.fetch_certificate` is the actual lever, and finding that out
/// took a while. The obvious candidate, `protonvpn servers`, is not one:
/// in CLI 1.0.3 it prints an account URL and exits in 0.7s. It does boot
/// the client, which schedules Proton's three refreshers, and then exits
/// long before any of them can finish — which is why renewal "over Tor"
/// reported failure for months without ever having sent a request. The same
/// trap is documented for the server list at `proc::API_TIMEOUT_REACHABLE_SECS`.
///
/// The private key is reused, exactly as Proton's own `CertificateRefresher`
/// does — this asks for a new certificate over the existing key, not a new
/// identity. `fetch_certificate` takes the session's write lock itself, so
/// the result is persisted to the keyring before this returns.
const RENEW_PY: &str = r#"
import asyncio, json, sys
sys.path.insert(0, "/usr/lib/python3/dist-packages")
from proton.sso import ProtonSSO
from proton.vpn.core.session_holder import ClientTypeMetadata
from proton.vpn.session import VPNSession
from proton.vpn.session.utils import get_core_api_semver_version

version = get_core_api_semver_version()
meta = ClientTypeMetadata(type="cli", version=version)
sso = ProtonSSO(
    appversion=f"linux-vpn-{meta.type}@{meta.version}",
    user_agent=f"ProtonVPN/{version} (Linux; pvpn)",
)
session = sso.get_default_session(override_class=VPNSession)
if not session.authenticated:
    sys.stderr.write("not signed in\n")
    sys.exit(3)

cert = asyncio.run(session.fetch_certificate())
json.dump(
    {"expires_at": float(cert.ExpirationTime), "refresh_at": float(cert.RefreshTime)},
    sys.stdout,
)
"#;

/// Where Tor's SOCKS port is, matching [`crate::proc::tor_listening`].
///
/// `socks5h` and not `socks5`: the `h` resolves the hostname at the exit
/// node, so this machine never asks the filtered network's resolver about a
/// Proton domain — which is the thing being routed around.
const TOR_SOCKS: &str = "socks5h://127.0.0.1:9050";

/// Ask Proton for a new certificate, over Tor when `via_tor`.
///
/// Routing is untouched either way: this is one API call, not a tunnel.
/// Returns the renewed status, so the caller can report the new expiry
/// rather than assert success — see [`status`] for why an exit code is not
/// evidence here.
pub fn renew(via_tor: bool, timeout: Duration) -> anyhow::Result<CertStatus> {
    let shim = crate::paths::shim_dir().to_string_lossy().to_string();
    let budget = if via_tor {
        crate::proc::API_TIMEOUT_TOR_SECS
    } else {
        crate::proc::API_TIMEOUT_REACHABLE_SECS
    }
    .to_string();

    let mut envs: Vec<(&str, &str)> = vec![
        ("PYTHONPATH", shim.as_str()),
        ("PVPN_DEBUG", "0"),
        ("PVPN_API_TIMEOUT", budget.as_str()),
    ];
    if via_tor {
        // Read by the shim's patch 5. Not `torsocks`: LD_PRELOAD cannot
        // carry Proton's aiohttp transport, and the attempt costs eleven
        // minutes of IPv6 timeouts. The shim's comment has the measurements.
        envs.push(("PVPN_SOCKS", TOR_SOCKS));
    }

    let result = crate::proc::run_with_timeout(
        crate::paths::system_python(),
        &["-c", RENEW_PY],
        &envs,
        timeout,
    )?;
    if !result.success {
        anyhow::bail!(
            "{}",
            result
                .stderr
                .lines()
                .last()
                .unwrap_or("the certificate request failed")
                .trim()
        );
    }
    parse(&result.stdout).ok_or_else(|| anyhow::anyhow!("could not read the renewed certificate"))
}

/// When Proton's client certificate lapses, and when Proton itself wanted to
/// renew it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertStatus {
    pub expires_at: DateTime<Utc>,
    /// Proton's own renewal point: two days into a seven-day certificate.
    /// The five days between here and `expires_at` are the window in which
    /// renewing is free — see [`CertStatus::renewal_due`].
    pub refresh_at: DateTime<Utc>,
}

impl CertStatus {
    /// Too little left to start a connect with. The local agent will reject
    /// it, every server will look equally dead, and the ranked list gets
    /// burned through in three minutes with nothing wrong with any of it.
    pub fn unusable(&self, now: DateTime<Utc>) -> bool {
        self.expires_at - now <= chrono::Duration::seconds(MINIMUM_VALIDITY_SECS)
    }

    /// Past the point Proton's refresher wanted to renew, but still usable.
    ///
    /// This is the distinction that decides whether this tool ever has to
    /// reach for Tor. Renewing inside this window costs one API call over a
    /// tunnel that is already up and already working. Renewing after
    /// [`CertStatus::unusable`] needs the API *before* any tunnel exists,
    /// which on a network that filters Proton by name means Tor, and Tor is
    /// not guaranteed to be running.
    pub fn renewal_due(&self, now: DateTime<Utc>) -> bool {
        now >= self.refresh_at
    }

    pub fn remaining(&self, now: DateTime<Utc>) -> chrono::Duration {
        self.expires_at - now
    }

    /// "expired 22m ago", "valid for 4d". The number people need in order
    /// to tell "renew it" from "this is not the certificate's fault".
    pub fn describe(&self, now: DateTime<Utc>) -> String {
        let left = self.remaining(now);
        if left <= chrono::Duration::zero() {
            format!("expired {} ago", span(-left))
        } else {
            format!("valid for {}", span(left))
        }
    }
}

fn span(delta: chrono::Duration) -> String {
    let minutes = delta.num_minutes();
    if minutes < 1 {
        "less than a minute".to_string()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 48 {
        format!("{}h", delta.num_hours())
    } else {
        format!("{}d", delta.num_days())
    }
}

/// Read the certificate's expiry, or `None` if the keyring cannot be read.
///
/// `None` means "we do not know" and never "it is fine". Every caller falls
/// back to the log-reading path it used before rather than acting on the
/// absence of an answer — a headless run with no secret service must not
/// start reporting healthy certificates as expired.
pub fn status() -> Option<CertStatus> {
    let result = crate::proc::run_with_timeout(
        crate::paths::system_python(),
        &["-c", CERT_STATUS_PY],
        &[],
        READ_TIMEOUT,
    )
    .ok()?;
    if !result.success {
        return None;
    }
    parse(&result.stdout)
}

fn parse(stdout: &str) -> Option<CertStatus> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    Some(CertStatus {
        expires_at: epoch(value.get("expires_at")?.as_f64()?)?,
        refresh_at: epoch(value.get("refresh_at")?.as_f64()?)?,
    })
}

fn epoch(seconds: f64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(seconds as i64, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc()
    }

    /// The real numbers from the 2026-08-31 failure, so the thresholds stay
    /// pinned to the case they were written for.
    fn detnsw() -> CertStatus {
        CertStatus {
            expires_at: at("2026-08-30T22:20:19Z"),
            refresh_at: at("2026-08-29T04:20:19Z"),
        }
    }

    #[test]
    fn parses_the_helper_output() {
        let parsed = parse(r#"{"expires_at": 1788128419.0, "refresh_at": 1787977219.0}"#).unwrap();
        assert_eq!(parsed, detnsw());
    }

    #[test]
    fn nothing_is_read_out_of_a_failed_read() {
        assert!(parse("").is_none());
        assert!(parse("Traceback (most recent call last):").is_none());
        assert!(parse(r#"{"expires_at": 1788128419.0}"#).is_none());
    }

    #[test]
    fn the_connect_that_worked_saw_a_usable_certificate() {
        // 07:55 local, twenty-five minutes before the lapse: SG-FREE#2
        // connected and carried traffic in six seconds.
        assert!(!detnsw().unusable(at("2026-08-30T21:55:12Z")));
    }

    #[test]
    fn the_connects_that_failed_saw_an_unusable_one() {
        // 08:42 and 08:43 local. Both were blamed on the server.
        assert!(detnsw().unusable(at("2026-08-30T22:42:39Z")));
        assert!(detnsw().unusable(at("2026-08-30T22:43:37Z")));
    }

    #[test]
    fn the_last_five_minutes_do_not_count_as_usable() {
        // Proton would reject it, so setting out with it is a wasted
        // settle window per server on the list.
        assert!(detnsw().unusable(at("2026-08-30T22:16:00Z")));
        assert!(!detnsw().unusable(at("2026-08-30T22:14:00Z")));
    }

    #[test]
    fn says_how_long_in_words_a_person_can_act_on() {
        assert_eq!(
            detnsw().describe(at("2026-08-30T22:42:39Z")),
            "expired 22m ago"
        );
        assert_eq!(detnsw().describe(at("2026-08-26T22:20:19Z")), "valid for 4d");
    }

    #[test]
    fn renewal_was_due_two_days_before_anything_broke() {
        // The whole point: there was a five-day window in which renewing
        // over a working tunnel would have cost one API call.
        assert!(detnsw().renewal_due(at("2026-08-29T05:00:00Z")));
        assert!(!detnsw().renewal_due(at("2026-08-28T05:00:00Z")));
        // And it was still merely due, not fatal, on the days between.
        assert!(!detnsw().unusable(at("2026-08-29T05:00:00Z")));
    }
}
