//! Persisted, cross-run knowledge about servers *as observed on this
//! network* — two lists, populated two different ways.
//!
//! * **fast**: TLS-handshake latency, recorded from the measurement pass
//!   that `pvpn best` and `pvpn up` already run before choosing. Cheap,
//!   never a real connect.
//! * **blocked**: only ever written from the outcome of an actual connect
//!   attempt (see the CLI's `connect` module) — a fast handshake does not
//!   prove a server works; `docs/transparent-proxy.md` is the whole reason
//!   this distinction exists.
//!
//! This file is the reason a command that takes two minutes on a hostile
//! network does not have to be paid for twice.
//!
//! Stored at `~/.local/share/pvpn/state.json`, written atomically
//! (tmp file + rename) so a crash mid-write cannot corrupt it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// How quickly the exponential moving average follows a new sample.
/// Low enough that one freak measurement can't flip a server's status.
const EMA_ALPHA: f64 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    /// Measured fast recently and not currently blocked.
    Fast,
    /// Seen, but not fast enough (or not recently enough) to call fast.
    Known,
    /// A real connect attempt to this server did not produce a working
    /// tunnel — see `blocked_reason`.
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStat {
    pub ema_latency_ms: Option<f64>,
    #[serde(default)]
    pub samples: u32,
    pub last_probe_ok: Option<DateTime<Utc>>,
    pub status: ServerStatus,
    pub blocked_reason: Option<String>,
    pub blocked_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub consecutive_connect_failures: u32,
}

impl Default for ServerStat {
    fn default() -> Self {
        Self {
            ema_latency_ms: None,
            samples: 0,
            last_probe_ok: None,
            status: ServerStatus::Known,
            blocked_reason: None,
            blocked_since: None,
            consecutive_connect_failures: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankedList {
    pub computed_at: Option<DateTime<Utc>>,
    pub servers: Vec<String>,
}

/// Everything learned about servers on one network.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkState {
    #[serde(default)]
    pub servers: HashMap<String, ServerStat>,
    #[serde(default)]
    pub last_full_rank: RankedList,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// What we know, filed by network — see [`State::set_network`].
    #[serde(default)]
    pub networks: HashMap<String, NetworkState>,
    /// Which network every accessor below reads and writes.
    ///
    /// Deliberately not persisted: it describes where the machine is right
    /// now, which the next run must resolve for itself rather than inherit
    /// from wherever it was last time.
    #[serde(skip)]
    current: String,
}

/// A server is "fast" below this EMA latency, in milliseconds. Deliberately
/// generous — the point is "usable and quick", not "the single best".
///
/// Only ever a coarse label on [`ServerStat::status`]; [`State::fast_list`]
/// is the authority on what counts as fast here, and it works in ratios.
const FAST_LATENCY_MS: f64 = 220.0;

/// How much slower than the best server on this network a server may be
/// and still count as fast.
///
/// Relative, because [`FAST_LATENCY_MS`] alone made this list permanently
/// empty on the networks the list exists for. Where a transparent proxy
/// adds a few hundred milliseconds to every handshake, the *nearest*
/// server measures ~200 ms and its EMA sits well above any fixed
/// threshold — so nothing was ever fast, `pvpn fast` always printed
/// "no servers yet", and the known-good servers this is supposed to keep
/// in the shortlist were never kept.
const FAST_LATENCY_RATIO: f64 = 1.35;

/// Longest a block is stretched for a server that keeps failing, as a
/// multiple of the configured retry window.
///
/// `consecutive_connect_failures` was written and never read: a server
/// that had failed five times running was released after exactly as long
/// as one that had failed once, so the ranked list kept offering the same
/// dead servers first, every day. Four is enough to get a persistently
/// broken server out of the way without retiring it for good — networks
/// and server pools change.
const MAX_BLOCK_ESCALATION: u32 = 4;

/// How long this server's block should hold, given how badly it has been
/// failing. First failure gets the plain configured window.
fn hold_for(stat: &ServerStat, base: chrono::Duration) -> chrono::Duration {
    let escalation = stat
        .consecutive_connect_failures
        .clamp(1, MAX_BLOCK_ESCALATION);
    base * escalation as i32
}

impl State {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let state: Self = serde_json::from_str(&text).unwrap_or_default();
        // A file from before observations were filed per network holds one
        // flat map with every network's measurements mixed together. There
        // is no way to attribute those now, and keeping them would rank the
        // next network by another one's latencies — the exact bug the split
        // exists to fix. Drop them; one probe sweep rebuilds the list.
        if state.networks.is_empty() && text.contains("\"last_full_rank\"") {
            tracing::info!(
                "dropping pre-network-scoped server observations — they cannot be attributed to a network; the next probe sweep rebuilds them"
            );
        }
        Ok(state)
    }

    /// Write atomically: to a tmp file in the same directory, then rename
    /// over the target. A reader never observes a half-written file.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    // --- which network are we on ------------------------------------------

    /// Point every accessor at one network's observations.
    ///
    /// Which servers work is a property of the *network*, not of this
    /// machine: a server a school proxy kills works fine on a phone
    /// hotspot two minutes later. These used to share one flat map, so
    /// whichever network was used last overwrote what the other had
    /// learned — latencies measured through a middlebox ranked the hotspot,
    /// and blocks earned on the hotspot hid servers that were fine at
    /// school. Returns true if this is a different network than before.
    pub fn set_network(&mut self, key: impl Into<String>) -> bool {
        let key = key.into();
        let changed = key != self.current;
        self.current = key;
        changed
    }

    /// The network these observations are about.
    pub fn network(&self) -> &str {
        &self.current
    }

    /// Servers as observed on the current network.
    pub fn servers(&self) -> &HashMap<String, ServerStat> {
        static EMPTY: std::sync::OnceLock<HashMap<String, ServerStat>> = std::sync::OnceLock::new();
        self.networks
            .get(&self.current)
            .map(|n| &n.servers)
            .unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
    }

    fn here_mut(&mut self) -> &mut NetworkState {
        self.networks.entry(self.current.clone()).or_default()
    }

    /// The last full rank computed on the current network.
    pub fn last_full_rank(&self) -> RankedList {
        self.networks
            .get(&self.current)
            .map(|n| n.last_full_rank.clone())
            .unwrap_or_default()
    }

    /// Servers to try, best first, as last measured on this network — with
    /// anything currently blocked here dropped.
    ///
    /// The filter has to happen at *use* time, not only when the rank is
    /// computed. A rank stays cached for minutes, so a block earned inside
    /// that window left the blocked server sitting at the top of the retry
    /// list: the tunnel would fail, the server would be written off, and
    /// the very next attempt went straight back to it.
    pub fn ranked_targets(
        &self,
        retry_after: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Vec<String> {
        self.networks
            .get(&self.current)
            .map(|n| {
                n.last_full_rank
                    .servers
                    .iter()
                    .filter(|name| !self.is_blocked(name, retry_after, now))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    // --- observations ------------------------------------------------------

    /// Record a background probe result. Does not touch `blocked` status —
    /// a probe can only ever make a server look *fast*, never prove it
    /// works, so it must never clear a block earned by a real connect
    /// failure.
    pub fn record_probe(&mut self, name: &str, latency_ms: f64, now: DateTime<Utc>) {
        let entry = self.here_mut().servers.entry(name.to_string()).or_default();
        entry.ema_latency_ms = Some(match entry.ema_latency_ms {
            Some(prev) => EMA_ALPHA * latency_ms + (1.0 - EMA_ALPHA) * prev,
            None => latency_ms,
        });
        entry.samples += 1;
        entry.last_probe_ok = Some(now);
        if entry.status != ServerStatus::Blocked {
            entry.status = if entry.ema_latency_ms.unwrap() < FAST_LATENCY_MS {
                ServerStatus::Fast
            } else {
                ServerStatus::Known
            };
        }
    }

    /// A real connect produced a working tunnel: clear any block, reset the
    /// failure count. Earned trust, not just a fast handshake.
    pub fn record_connect_success(&mut self, name: &str, now: DateTime<Utc>) {
        let entry = self.here_mut().servers.entry(name.to_string()).or_default();
        entry.consecutive_connect_failures = 0;
        entry.blocked_reason = None;
        entry.blocked_since = None;
        entry.last_probe_ok = Some(now);
        if entry.status == ServerStatus::Blocked {
            entry.status = ServerStatus::Known;
        }
    }

    /// A real connect attempt did not produce a working tunnel. `reason`
    /// is a short machine-readable tag: `"no-traffic-after-settle"`,
    /// `"handshake-closed-early"`, `"refused"`.
    pub fn record_connect_blocked(&mut self, name: &str, reason: &str, now: DateTime<Utc>) {
        let entry = self.here_mut().servers.entry(name.to_string()).or_default();
        entry.consecutive_connect_failures += 1;
        entry.status = ServerStatus::Blocked;
        entry.blocked_reason = Some(reason.to_string());
        entry.blocked_since = Some(now);
    }

    /// Is this server currently blocked *here*, given how long blocks are
    /// held before a retry is allowed? Networks and server pools change, so
    /// a block is never permanent — but one that keeps failing is held for
    /// a multiple of the window, see [`hold_for`].
    pub fn is_blocked(
        &self,
        name: &str,
        retry_after: chrono::Duration,
        now: DateTime<Utc>,
    ) -> bool {
        match self.servers().get(name) {
            Some(stat) if stat.status == ServerStatus::Blocked => match stat.blocked_since {
                Some(since) => now - since < hold_for(stat, retry_after),
                None => true,
            },
            _ => false,
        }
    }

    /// Blocks are never permanent: once a server's hold has elapsed it
    /// becomes `known` again so a later connect will retry it. The failure
    /// count deliberately survives — it is what makes the *next* block
    /// longer than the last.
    pub fn expire_blocks(&mut self, retry_after: chrono::Duration, now: DateTime<Utc>) {
        let stale: Vec<String> = self
            .servers()
            .iter()
            .filter(|(name, stat)| {
                stat.status == ServerStatus::Blocked && !self.is_blocked(name, retry_after, now)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            if let Some(stat) = self.here_mut().servers.get_mut(&name) {
                stat.status = ServerStatus::Known;
                stat.blocked_reason = None;
                stat.blocked_since = None;
            }
        }
    }

    /// Names currently blocked here — what the ranker must skip.
    pub fn blocked_names(&self, retry_after: chrono::Duration, now: DateTime<Utc>) -> Vec<String> {
        self.servers()
            .keys()
            .filter(|name| self.is_blocked(name, retry_after, now))
            .cloned()
            .collect()
    }

    /// Servers measured fast *for this network*: within
    /// [`FAST_LATENCY_RATIO`] of the best EMA we have seen here, blocked
    /// ones excluded. Judged relative to the best rather than against a
    /// fixed millisecond threshold — see [`FAST_LATENCY_RATIO`].
    pub fn fast_list(&self) -> Vec<(String, ServerStat)> {
        let mut out: Vec<(String, ServerStat)> = self
            .servers()
            .iter()
            .filter(|(_, s)| s.status != ServerStatus::Blocked && s.ema_latency_ms.is_some())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let best = out
            .iter()
            .filter_map(|(_, s)| s.ema_latency_ms)
            .fold(f64::MAX, f64::min);
        if !best.is_finite() {
            return Vec::new();
        }
        let cutoff = best * FAST_LATENCY_RATIO;
        out.retain(|(_, s)| s.ema_latency_ms.unwrap() <= cutoff);

        out.sort_by(|a, b| {
            a.1.ema_latency_ms
                .unwrap_or(f64::MAX)
                .partial_cmp(&b.1.ema_latency_ms.unwrap_or(f64::MAX))
                .unwrap()
        });
        out
    }

    pub fn blocked_list(&self) -> Vec<(String, ServerStat)> {
        let mut out: Vec<(String, ServerStat)> = self
            .servers()
            .iter()
            .filter(|(_, s)| s.status == ServerStatus::Blocked)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by_key(|(_, s)| std::cmp::Reverse(s.blocked_since));
        out
    }

    pub fn set_last_full_rank(&mut self, servers: Vec<String>, now: DateTime<Utc>) {
        self.here_mut().last_full_rank = RankedList {
            computed_at: Some(now),
            servers,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempdir();
        let path = dir.join("state.json");
        let mut state = State::default();
        let now = Utc::now();
        state.record_probe("SG-FREE#2", 99.0, now);
        state.save(&path).unwrap();

        let reloaded = State::load(&path).unwrap();
        assert_eq!(reloaded.servers()["SG-FREE#2"].ema_latency_ms, Some(99.0));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_file_loads_as_default() {
        let state = State::load(std::path::Path::new("/nonexistent/pvpn-state.json")).unwrap();
        assert!(state.servers().is_empty());
    }

    #[test]
    fn a_fast_probe_marks_the_server_fast() {
        let mut state = State::default();
        state.record_probe("SG-FREE#2", 90.0, Utc::now());
        assert_eq!(state.servers()["SG-FREE#2"].status, ServerStatus::Fast);
    }

    #[test]
    fn a_slow_probe_marks_the_server_known_not_fast() {
        let mut state = State::default();
        state.record_probe("NL-FREE#15", 400.0, Utc::now());
        assert_eq!(state.servers()["NL-FREE#15"].status, ServerStatus::Known);
    }

    #[test]
    fn a_probe_alone_cannot_clear_a_block() {
        let mut state = State::default();
        let now = Utc::now();
        state.record_connect_blocked("US-FREE#15", "no-traffic-after-settle", now);
        state.record_probe("US-FREE#15", 10.0, now);
        assert_eq!(state.servers()["US-FREE#15"].status, ServerStatus::Blocked);
    }

    #[test]
    fn a_real_connect_success_clears_a_block() {
        let mut state = State::default();
        let now = Utc::now();
        state.record_connect_blocked("US-FREE#15", "refused", now);
        state.record_connect_success("US-FREE#15", now);
        assert_eq!(state.servers()["US-FREE#15"].status, ServerStatus::Known);
        assert!(state.servers()["US-FREE#15"].blocked_reason.is_none());
    }

    #[test]
    fn a_block_expires_after_the_retry_window() {
        let mut state = State::default();
        let old = Utc::now() - ChronoDuration::hours(48);
        state.record_connect_blocked("US-FREE#15", "refused", old);
        assert!(!state.is_blocked("US-FREE#15", ChronoDuration::hours(24), Utc::now()));
    }

    #[test]
    fn a_fresh_block_is_still_in_effect() {
        let mut state = State::default();
        let now = Utc::now();
        state.record_connect_blocked("US-FREE#15", "refused", now);
        assert!(state.is_blocked("US-FREE#15", ChronoDuration::hours(24), now));
    }

    #[test]
    fn fast_list_is_sorted_by_latency() {
        let mut state = State::default();
        let now = Utc::now();
        state.record_probe("near", 55.0, now);
        state.record_probe("fast", 50.0, now);
        let names: Vec<String> = state.fast_list().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["fast", "near"]);
    }

    #[test]
    fn a_state_file_from_the_daemon_still_loads() {
        // `want_up` was the daemon asking itself whether to reconnect while
        // nobody was watching. Nothing does that now, but the files on disk
        // still have the key and must not fail to parse.
        let dir = tempdir();
        let path = dir.join("old-state.json");
        std::fs::write(&path, r#"{"networks":{},"want_up":true}"#).unwrap();
        assert!(State::load(&path).is_ok());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fast_list_is_relative_to_this_network() {
        // Every handshake here is slowed by a middlebox, so nothing clears
        // a fixed threshold — but the nearest servers are still the fast
        // ones and the list must say so.
        let mut state = State::default();
        let now = Utc::now();
        state.record_probe("SG", 206.0, now);
        state.record_probe("JP", 228.0, now);
        state.record_probe("NL", 646.0, now);
        let names: Vec<String> = state.fast_list().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["SG", "JP"], "an absolute cutoff empties this");
    }

    #[test]
    fn fast_list_never_offers_a_blocked_server() {
        let mut state = State::default();
        let now = Utc::now();
        state.record_probe("SG", 206.0, now);
        state.record_probe("JP", 228.0, now);
        state.record_connect_blocked("SG", "no-traffic-after-settle", now);
        let names: Vec<String> = state.fast_list().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["JP"]);
    }

    #[test]
    fn what_one_network_teaches_does_not_leak_into_another() {
        // The bug this split fixes: a flat map meant latencies measured
        // through a school proxy ranked the phone hotspot, and blocks
        // earned on the hotspot hid servers that were fine at school.
        let mut state = State::default();
        let now = Utc::now();

        state.set_network("wifi:school");
        state.record_probe("SG-FREE#9", 210.0, now);
        state.record_connect_blocked("SG-FREE#9", "no-traffic-after-settle", now);

        state.set_network("wifi:hotspot");
        assert!(
            state.servers().is_empty(),
            "a different network starts from nothing, not from the last one's verdicts"
        );
        assert!(!state.is_blocked("SG-FREE#9", ChronoDuration::hours(24), now));

        state.set_network("wifi:school");
        assert!(state.is_blocked("SG-FREE#9", ChronoDuration::hours(24), now));
    }

    #[test]
    fn set_network_reports_whether_we_moved() {
        let mut state = State::default();
        assert!(state.set_network("wifi:school"));
        assert!(!state.set_network("wifi:school"));
        assert!(state.set_network("wifi:hotspot"));
    }

    #[test]
    fn the_ranked_list_belongs_to_its_network() {
        let mut state = State::default();
        let now = Utc::now();
        state.set_network("wifi:school");
        state.set_last_full_rank(vec!["SG-FREE#9".into()], now);
        state.set_network("wifi:hotspot");
        assert!(
            state
                .ranked_targets(ChronoDuration::hours(24), now)
                .is_empty(),
            "connecting elsewhere must not start from another network's order"
        );
    }

    #[test]
    fn a_blocked_server_drops_out_of_a_cached_rank() {
        // The rank is cached for minutes. A block earned inside that window
        // used to leave the dead server first in line, so the next attempt
        // went straight back to the one just written off.
        let mut state = State::default();
        let now = Utc::now();
        state.set_network("wifi:school");
        state.set_last_full_rank(
            vec!["SG-FREE#21".into(), "SG-FREE#13".into(), "JP-FREE#9".into()],
            now,
        );
        state.record_connect_blocked("SG-FREE#21", "no-traffic-after-settle", now);
        assert_eq!(
            state.ranked_targets(ChronoDuration::hours(24), now),
            vec!["SG-FREE#13", "JP-FREE#9"]
        );
    }

    #[test]
    fn an_expired_block_comes_back_into_the_rank() {
        let mut state = State::default();
        let now = Utc::now();
        state.set_network("wifi:school");
        state.set_last_full_rank(vec!["SG-FREE#21".into()], now);
        state.record_connect_blocked(
            "SG-FREE#21",
            "no-traffic-after-settle",
            now - ChronoDuration::hours(48),
        );
        assert_eq!(
            state.ranked_targets(ChronoDuration::hours(24), now),
            vec!["SG-FREE#21"]
        );
    }

    #[test]
    fn a_server_that_keeps_failing_is_held_longer_each_time() {
        // `consecutive_connect_failures` used to be written and never read,
        // so a server that had failed every day for a week was offered
        // first again 24h later, every time.
        let mut state = State::default();
        let window = ChronoDuration::hours(24);
        let first = Utc::now() - ChronoDuration::hours(30);

        state.record_connect_blocked("SG-FREE#9", "no-traffic-after-settle", first);
        assert!(
            !state.is_blocked("SG-FREE#9", window, Utc::now()),
            "one failure earns the plain window"
        );

        state.record_connect_blocked("SG-FREE#9", "no-traffic-after-settle", first);
        assert!(
            state.is_blocked("SG-FREE#9", window, Utc::now()),
            "a second failure holds it past 30h"
        );
    }

    #[test]
    fn the_escalation_is_capped_so_a_block_is_never_permanent() {
        let mut state = State::default();
        let window = ChronoDuration::hours(24);
        let long_ago = Utc::now() - ChronoDuration::days(30);
        for _ in 0..20 {
            state.record_connect_blocked("SG-FREE#9", "refused", long_ago);
        }
        assert!(!state.is_blocked("SG-FREE#9", window, Utc::now()));
    }

    #[test]
    fn expiring_a_block_keeps_the_failure_count() {
        // Otherwise every release resets the escalation and a server that
        // fails daily never gets held any longer than one that failed once.
        let mut state = State::default();
        let old = Utc::now() - ChronoDuration::hours(48);
        state.record_connect_blocked("SG-FREE#9", "refused", old);
        state.expire_blocks(ChronoDuration::hours(24), Utc::now());
        assert_eq!(state.servers()["SG-FREE#9"].status, ServerStatus::Known);
        assert_eq!(state.servers()["SG-FREE#9"].consecutive_connect_failures, 1);
    }

    #[test]
    fn a_success_forgives_the_history() {
        let mut state = State::default();
        let now = Utc::now();
        state.record_connect_blocked("SG-FREE#9", "refused", now);
        state.record_connect_blocked("SG-FREE#9", "refused", now);
        state.record_connect_success("SG-FREE#9", now);
        assert_eq!(state.servers()["SG-FREE#9"].consecutive_connect_failures, 0);
        assert!(!state.is_blocked("SG-FREE#9", ChronoDuration::hours(24), now));
    }

    #[test]
    fn a_flat_pre_network_state_file_is_not_carried_over() {
        // Those measurements mixed every network together; attributing them
        // now is impossible and guessing wrong is worse than remeasuring.
        let dir = tempdir();
        let path = dir.join("flat-state.json");
        std::fs::write(
            &path,
            r#"{"servers":{"SG-FREE#9":{"ema_latency_ms":210.0,"samples":3,"last_probe_ok":null,"status":"known","blocked_reason":null,"blocked_since":null,"consecutive_connect_failures":0}},"last_full_rank":{"computed_at":null,"servers":["SG-FREE#9"]},"want_up":true}"#,
        )
        .unwrap();
        let mut state = State::load(&path).unwrap();
        state.set_network("wifi:school");
        assert!(state.servers().is_empty());
        assert!(state
            .ranked_targets(ChronoDuration::hours(24), Utc::now())
            .is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    /// One directory per call. These tests run concurrently and used to
    /// share a single path keyed only on the pid, so whichever finished
    /// first deleted the directory the others were still writing into.
    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pvpn-core-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
