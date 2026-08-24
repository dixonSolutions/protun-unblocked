//! Load Proton's cache, filter, optionally probe, and rank. This is the
//! `lib/best-server.py` `main()` flow, used by `pvpn best` and by the
//! fresh rank `pvpn up` computes before it connects.

use crate::geo;
use crate::probe::{self, DEFAULT_PROBE_TIMEOUT, PROBE_PORT};
use crate::rank::{self, Candidate};
use crate::serverlist::{self, EligibilityOptions};
use crate::state::State;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct RankRequest {
    pub serverlist: std::path::PathBuf,
    pub country: Option<String>,
    pub free_only: bool,
    pub quick: bool,
    pub limit: usize,
    pub shortlist: usize,
    pub refine: usize,
    pub rounds: usize,
    /// Recently-fast servers folded into the shortlist before the
    /// distance/load cut, so a known-good server is never dropped for
    /// being slightly further than the 40th-nearest.
    pub extra_keep: Vec<String>,
    /// Currently-blocked names, excluded from the connect sequence.
    pub blocked: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct RankResult {
    pub candidates: Vec<Candidate>,
    pub origin: Option<(f64, f64)>,
    pub measured: bool,
}

impl RankRequest {
    pub fn from_config(
        cfg: &crate::config::Config,
        quick: bool,
        limit: usize,
        country: Option<String>,
    ) -> Self {
        Self {
            serverlist: crate::paths::serverlist_path(),
            country: country.or_else(|| cfg.country.as_ref().filter(|c| !c.is_empty()).cloned()),
            free_only: cfg.free_only,
            quick,
            limit,
            shortlist: cfg.probe_shortlist,
            refine: cfg.probe_refine,
            rounds: cfg.probe_rounds,
            extra_keep: Vec::new(),
            blocked: HashSet::new(),
        }
    }

    /// Fold persisted observations into the request: skip blocked servers,
    /// and keep recently-fast ones in the shortlist.
    pub fn with_state(
        mut self,
        state: &State,
        retry_after: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Self {
        self.blocked = state.blocked_names(retry_after, now).into_iter().collect();
        self.extra_keep = state.fast_list().into_iter().map(|(n, _)| n).collect();
        self
    }
}

/// Run the ranking pipeline. `quick` skips probes and orders by distance
/// and load, matching `--no-probe` / `pvpn best --quick`.
pub async fn rank_servers(req: RankRequest) -> anyhow::Result<RankResult> {
    let (max_tier, logicals) = serverlist::load_server_list(&req.serverlist)?;
    let opts = EligibilityOptions {
        country: req.country.clone(),
        free_only: req.free_only,
        excluded_features: rank::FEATURES_EXCLUDED_BY_DEFAULT as i64,
    };
    let mut candidates = serverlist::eligible_servers(&logicals, max_tier, &opts);
    candidates.retain(|c| !req.blocked.contains(&c.name));
    if candidates.is_empty() {
        anyhow::bail!(
            "No servers available to this account{}.",
            req.country
                .as_ref()
                .map(|c| format!(" in {}", c.to_uppercase()))
                .unwrap_or_default()
        );
    }

    let origin = geo::local_coordinates();
    let by_name: HashMap<String, _> = logicals
        .iter()
        .filter_map(|l| l.name.clone().map(|n| (n, l.clone())))
        .collect();
    serverlist::annotate_distances(&mut candidates, &by_name, origin);

    let results = if req.quick {
        rank::rank_without_probing(&candidates)
    } else {
        let mut probed = rank::shortlist(&candidates, req.shortlist);
        for name in &req.extra_keep {
            if probed.iter().any(|c| c.name == *name) {
                continue;
            }
            if let Some(c) = candidates.iter().find(|c| c.name == *name) {
                probed.push(c.clone());
            }
        }
        let measured = probe::measure(
            &probed,
            PROBE_PORT,
            req.rounds.max(1),
            DEFAULT_PROBE_TIMEOUT,
            req.refine.max(1),
            true,
        )
        .await;
        if measured.iter().any(|c| c.reachable()) {
            measured
        } else {
            rank::rank_without_probing(&candidates)
        }
    };

    let measured = rank::latency_is_informative(&results);
    let mut results = results;
    if req.limit > 0 && results.len() > req.limit {
        results.truncate(req.limit);
    }

    Ok(RankResult {
        candidates: results,
        origin,
        measured,
    })
}

/// Names suitable as connect targets: reachable servers if any answered,
/// otherwise the full (distance-ranked) list. Matches the Python helper's
/// `names`/`tsv` filter — a connect list is not a report, so timeouts
/// do not belong in it while something else did answer.
pub fn connect_targets(result: &RankResult) -> Vec<String> {
    let reachable: Vec<String> = result
        .candidates
        .iter()
        .filter(|c| c.reachable())
        .map(|c| c.name.clone())
        .collect();
    if !reachable.is_empty() {
        return reachable;
    }
    result.candidates.iter().map(|c| c.name.clone()).collect()
}

/// Rank one server list and nothing else: no config, no persisted state,
/// no probes. `pvpn best --serverlist` and the tests use this.
pub fn quick_request(
    serverlist: &Path,
    country: Option<&str>,
    free_only: bool,
    limit: usize,
) -> RankRequest {
    RankRequest {
        serverlist: serverlist.to_path_buf(),
        country: country.map(ToString::to_string),
        free_only,
        quick: true,
        limit,
        shortlist: 0,
        refine: 0,
        rounds: 0,
        extra_keep: Vec::new(),
        blocked: HashSet::new(),
    }
}

/// Synchronous [`quick_request`], for callers with no runtime of their
/// own. Panics inside one — use `rank_servers(quick_request(..)).await`
/// from async code.
pub fn rank_servers_quick(
    serverlist: &Path,
    country: Option<&str>,
    free_only: bool,
    limit: usize,
) -> anyhow::Result<RankResult> {
    // `--quick` never awaits a probe; block_on is instant.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(rank_servers(quick_request(
            serverlist, country, free_only, limit,
        )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("serverlist.json");
        let data = json!({
            "MaxTier": 0,
            "LogicalServers": [
                {
                    "Name": "SG-FREE#2", "ExitCountry": "SG", "Tier": 0, "Load": 77,
                    "Score": 5.0, "City": "Singapore", "Status": 1, "Features": 0,
                    "Location": {"Lat": 1.29, "Long": 103.85},
                    "Servers": [{"EntryIP": "203.0.113.1", "Status": 1, "ServicesDown": 0}]
                },
                {
                    "Name": "JP-FREE#9", "ExitCountry": "JP", "Tier": 0, "Load": 81,
                    "Score": 5.0, "City": "Tokyo", "Status": 1, "Features": 0,
                    "Location": {"Lat": 35.68, "Long": 139.69},
                    "Servers": [{"EntryIP": "203.0.113.1", "Status": 1, "ServicesDown": 0}]
                },
                {
                    "Name": "NL-FREE#15", "ExitCountry": "NL", "Tier": 0, "Load": 55,
                    "Score": 4.93, "City": "Amsterdam", "Status": 1, "Features": 0,
                    "Location": {"Lat": 52.37, "Long": 4.89},
                    "Servers": [{"EntryIP": "203.0.113.1", "Status": 1, "ServicesDown": 0}]
                },
                {
                    "Name": "NL-PLUS#1", "ExitCountry": "NL", "Tier": 2, "Load": 50,
                    "Score": 5.0, "City": "Amsterdam", "Status": 1, "Features": 0,
                    "Location": {"Lat": 52.37, "Long": 4.89},
                    "Servers": [{"EntryIP": "203.0.113.1", "Status": 1, "ServicesDown": 0}]
                }
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        path
    }

    #[test]
    fn quick_rank_hides_out_of_tier_and_keeps_free_servers() {
        let dir = std::env::temp_dir().join(format!("pvpn-pipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = fixture(&dir);
        let result = rank_servers_quick(&path, None, false, 5).unwrap();
        let names: Vec<&str> = result.candidates.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"SG-FREE#2"));
        assert!(!names.contains(&"NL-PLUS#1"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn country_filter_keeps_only_the_match() {
        let dir = std::env::temp_dir().join(format!("pvpn-pipe-jp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = fixture(&dir);
        let result = rank_servers_quick(&path, Some("JP"), false, 5).unwrap();
        let names: Vec<&str> = result.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["JP-FREE#9"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn blocked_servers_are_dropped_from_the_connect_list() {
        let dir = std::env::temp_dir().join(format!("pvpn-pipe-blk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = fixture(&dir);
        let req = RankRequest {
            serverlist: path,
            country: None,
            free_only: false,
            quick: true,
            limit: 10,
            shortlist: 0,
            refine: 0,
            rounds: 0,
            extra_keep: Vec::new(),
            blocked: ["SG-FREE#2".to_string()].into_iter().collect(),
        };
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(rank_servers(req))
            .unwrap();
        let names: Vec<&str> = result.candidates.iter().map(|c| c.name.as_str()).collect();
        assert!(!names.contains(&"SG-FREE#2"));
        std::fs::remove_dir_all(dir).ok();
    }
}
