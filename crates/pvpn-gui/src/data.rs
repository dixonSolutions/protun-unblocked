//! Read-only views of what `pvpn` has written down: per-network server
//! records, attempts, the last ranking, and where each server is.
//!
//! The window never writes `state.json` — every change to it (a forgotten
//! block, a new measurement) goes through `pvpn`, the one writer.

use chrono::{DateTime, Utc};
use pvpn_core::config::Config;
use pvpn_core::state::{Event, ServerStat, ServerStatus, State};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Carried real traffic here.
    Working,
    /// Measured quickly here; never proof it works.
    Fast,
    /// Failed a real connect here.
    Blocked,
    Known,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Working => "working",
            Kind::Fast => "fast",
            Kind::Blocked => "blocked",
            Kind::Known => "measured",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub kind: Kind,
    pub stat: ServerStat,
    /// Place in the last full ranking, 1-based.
    pub rank: Option<usize>,
    pub block_lifts: Option<DateTime<Utc>>,
}

/// One network's record, sorted the way the servers list shows it.
#[derive(Debug, Clone, Default)]
pub struct NetworkView {
    pub servers: Vec<Server>,
    pub ranked_at: Option<DateTime<Utc>>,
}

pub fn load_state() -> State {
    State::load(&Config::state_path()).unwrap_or_default()
}

pub fn networks(state: &State) -> Vec<String> {
    let mut nets = state.known_networks();
    nets.sort();
    nets
}

pub fn view(state: &State, network: &str, config: &Config) -> NetworkView {
    let mut state = state.clone();
    state.set_network(network.to_string());
    let rank = state.last_full_rank();
    let rank_of: HashMap<&str, usize> = rank
        .servers
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i + 1))
        .collect();
    let fast: std::collections::HashSet<String> =
        state.fast_list().into_iter().map(|(n, _)| n).collect();
    let retry = config.blocked_retry_after();
    let mut servers: Vec<Server> = state
        .servers()
        .iter()
        .map(|(name, stat)| {
            let kind = if stat.status == ServerStatus::Blocked {
                Kind::Blocked
            } else if stat.connect_successes > 0 {
                Kind::Working
            } else if fast.contains(name) {
                Kind::Fast
            } else {
                Kind::Known
            };
            Server {
                name: name.clone(),
                kind,
                stat: stat.clone(),
                rank: rank_of.get(name.as_str()).copied(),
                block_lifts: state.block_expires_at(name, retry),
            }
        })
        .collect();
    servers.sort_by(|a, b| order_key(a).partial_cmp(&order_key(b)).unwrap_or(std::cmp::Ordering::Equal));
    NetworkView {
        servers,
        ranked_at: rank.computed_at,
    }
}

/// Ranked first in rank order, then working, fast, measured, blocked —
/// each by latency.
fn order_key(s: &Server) -> (u8, f64, f64) {
    let group = match (s.rank, s.kind) {
        (_, Kind::Blocked) => 4,
        (Some(_), _) => 0,
        (None, Kind::Working) => 1,
        (None, Kind::Fast) => 2,
        (None, Kind::Known) => 3,
    };
    (
        group,
        s.rank.unwrap_or(usize::MAX) as f64,
        s.stat.ema_latency_ms.unwrap_or(f64::MAX),
    )
}

/// Events for one network, or every network tagged, newest first.
pub fn history(state: &State, network: Option<&str>) -> Vec<(String, Event)> {
    let mut out: Vec<(String, Event)> = match network {
        Some(n) => {
            let mut s = state.clone();
            s.set_network(n.to_string());
            s.events().into_iter().map(|e| (n.to_string(), e)).collect()
        }
        None => state.all_events(),
    };
    out.sort_by(|a, b| b.1.at.cmp(&a.1.at));
    out
}

/// Where a server is, and what Proton says about it now.
#[derive(Debug, Clone)]
pub struct Place {
    pub lat: f64,
    pub lon: f64,
    pub country: String,
    pub city: String,
    pub load: Option<i64>,
}

#[derive(Deserialize)]
struct RawList {
    #[serde(rename = "LogicalServers", default)]
    logical: Vec<pvpn_core::serverlist::RawLogical>,
}

/// Name → place, from Proton's cached server list.
pub fn places() -> HashMap<String, Place> {
    let path = pvpn_core::paths::serverlist_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(list) = serde_json::from_str::<RawList>(&text) else {
        return HashMap::new();
    };
    list.logical
        .into_iter()
        .filter_map(|l| {
            let loc = l.location?;
            Some((
                l.name?,
                Place {
                    lat: loc.lat?,
                    lon: loc.long?,
                    country: l.exit_country.unwrap_or_default(),
                    city: l.city.unwrap_or_default(),
                    load: l.load,
                },
            ))
        })
        .collect()
}

/// One row of `pvpn best --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Ranked {
    pub name: String,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<f64>,
    #[serde(default)]
    pub load: Option<i64>,
    #[serde(default)]
    pub distance_km: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BestResult {
    #[serde(default)]
    pub results: Vec<Ranked>,
}

pub fn last_best_path() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::settings::dirs_home().join(".cache"))
        .join("pvpn-gui")
        .join("last-best.json")
}

pub fn parse_best(json: &str) -> Option<BestResult> {
    serde_json::from_str(json).ok()
}

pub fn load_last_best() -> Option<BestResult> {
    parse_best(&std::fs::read_to_string(last_best_path()).ok()?)
}

pub fn save_last_best(json: &str) {
    let path = last_best_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, json);
}

pub fn ago(when: DateTime<Utc>) -> String {
    let secs = (Utc::now() - when).num_seconds();
    match secs {
        s if s < 0 => format!("in {}", span(-s)),
        s => format!("{} ago", span(s)),
    }
}

pub fn span(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_json_parses_with_missing_fields() {
        let r = parse_best(r#"{"results":[{"name":"SG-FREE#13","latency_ms":null,"load":71}]}"#).unwrap();
        assert_eq!(r.results[0].name, "SG-FREE#13");
        assert_eq!(r.results[0].load, Some(71));
    }

    #[test]
    fn ranked_servers_sort_before_the_rest_and_blocked_last() {
        let mut state = State::default();
        state.set_network("wifi:t".to_string());
        let now = Utc::now();
        state.record_probe("A", 50.0, now);
        state.record_probe("B", 90.0, now);
        state.record_probe("C", 40.0, now);
        state.record_connect_blocked("C", "test", now);
        state.set_last_full_rank(vec!["B".into()], now);
        let v = view(&state, "wifi:t", &Config::default());
        let names: Vec<&str> = v.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["B", "A", "C"]);
        assert_eq!(v.servers[0].rank, Some(1));
        assert_eq!(v.servers[2].kind, Kind::Blocked);
    }

    #[test]
    fn spans_read_naturally() {
        assert_eq!(span(5), "5s");
        assert_eq!(span(125), "2m");
        assert_eq!(span(7200), "2h");
        assert_eq!(span(200_000), "2d");
    }
}
