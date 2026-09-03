//! Read Proton's cached `serverlist.json` and turn it into `Candidate`s.
//! Port of the "server list" section of `lib/best-server.py`.

use crate::rank::{Candidate, FEATURES_EXCLUDED_BY_DEFAULT, STATUS_ENABLED};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct RawServerList {
    #[serde(rename = "MaxTier", default)]
    max_tier: i64,
    #[serde(rename = "LogicalServers", default)]
    logical_servers: Vec<RawLogical>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RawLogical {
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
    #[serde(rename = "ExitCountry", default)]
    pub exit_country: Option<String>,
    #[serde(rename = "Tier", default)]
    pub tier: Option<i64>,
    #[serde(rename = "Load", default)]
    pub load: Option<i64>,
    #[serde(rename = "Score", default)]
    pub score: Option<f64>,
    #[serde(rename = "City", default)]
    pub city: Option<String>,
    #[serde(rename = "Region", default)]
    pub region: Option<String>,
    #[serde(rename = "Status", default)]
    pub status: Option<i64>,
    #[serde(rename = "Features", default)]
    pub features: Option<i64>,
    #[serde(rename = "Location", default)]
    pub location: Option<RawLocation>,
    #[serde(rename = "Servers", default)]
    pub servers: Vec<RawPhysical>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RawLocation {
    #[serde(rename = "Lat", default)]
    pub lat: Option<f64>,
    #[serde(rename = "Long", default)]
    pub long: Option<f64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RawPhysical {
    #[serde(rename = "EntryIP", default)]
    pub entry_ip: Option<String>,
    #[serde(rename = "Status", default)]
    pub status: Option<i64>,
    #[serde(rename = "ServicesDown", default)]
    pub services_down: Option<i64>,
}

#[derive(thiserror::Error, Debug)]
pub enum LoadError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

/// Read Proton's cached server list.
///
/// Returns the account's max tier and the raw logical server records.
pub fn load_server_list(path: &Path) -> Result<(i64, Vec<RawLogical>), LoadError> {
    let text = std::fs::read_to_string(path)?;
    let parsed: RawServerList = serde_json::from_str(&text)?;
    Ok((parsed.max_tier, parsed.logical_servers))
}

/// Pick an entry IP from a logical server's physical nodes.
fn first_enabled_entry_ip(physicals: &[RawPhysical]) -> Option<String> {
    for physical in physicals {
        if physical.status != Some(STATUS_ENABLED) {
            continue;
        }
        if physical.services_down.unwrap_or(0) != 0 {
            continue;
        }
        if let Some(ip) = &physical.entry_ip {
            if !ip.is_empty() {
                return Some(ip.clone());
            }
        }
    }
    None
}

#[derive(Debug, Default, Clone)]
pub struct EligibilityOptions {
    pub country: Option<String>,
    pub free_only: bool,
    pub excluded_features: i64,
}

impl EligibilityOptions {
    pub fn default_excluded() -> Self {
        Self {
            country: None,
            free_only: false,
            excluded_features: FEATURES_EXCLUDED_BY_DEFAULT as i64,
        }
    }
}

/// Select the servers this account is allowed to connect to.
///
/// The tier check is the same one Proton's own client applies
/// (`server.tier <= user_tier`); everything else is a speed filter.
pub fn eligible_servers(
    logicals: &[RawLogical],
    max_tier: i64,
    opts: &EligibilityOptions,
) -> Vec<Candidate> {
    let tier_ceiling = if opts.free_only { 0 } else { max_tier };
    let wanted_country = opts.country.as_ref().map(|c| c.to_uppercase());
    let mut candidates = Vec::new();

    for server in logicals {
        if server.status != Some(STATUS_ENABLED) {
            continue;
        }
        let tier = server.tier.unwrap_or(99);
        if tier > tier_ceiling {
            continue;
        }
        if server.features.unwrap_or(0) & opts.excluded_features != 0 {
            continue;
        }
        let exit_country = server.exit_country.clone().unwrap_or_default();
        if let Some(wanted) = &wanted_country {
            if exit_country.to_uppercase() != *wanted {
                continue;
            }
        }

        let entry_ip = match first_enabled_entry_ip(&server.servers) {
            Some(ip) => ip,
            None => continue,
        };

        candidates.push(Candidate {
            name: server.name.clone().unwrap_or_else(|| "?".to_string()),
            entry_ip,
            country: if exit_country.is_empty() {
                "??".to_string()
            } else {
                exit_country
            },
            city: server
                .city
                .clone()
                .filter(|c| !c.is_empty())
                .or_else(|| server.region.clone())
                .unwrap_or_default(),
            tier,
            load: server.load.unwrap_or(100),
            proton_score: server.score.unwrap_or(0.0),
            distance_km: None,
            latency_ms: None,
            rating: None,
            carry: None,
        });
    }

    candidates
}

/// Fill in `distance_km` for each candidate, when we know where we are.
pub fn annotate_distances(
    candidates: &mut [Candidate],
    logicals_by_name: &HashMap<String, RawLogical>,
    origin: Option<(f64, f64)>,
) {
    let Some(origin) = origin else { return };
    for candidate in candidates.iter_mut() {
        let Some(logical) = logicals_by_name.get(&candidate.name) else {
            continue;
        };
        let Some(location) = &logical.location else {
            continue;
        };
        let (Some(lat), Some(long)) = (location.lat, location.long) else {
            continue;
        };
        candidate.distance_km = Some(crate::geo::haversine_km(origin, (lat, long)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rank::{FEATURE_SECURE_CORE, FEATURE_TOR};

    fn make_logical(
        name: &str,
        tier: i64,
        load: i64,
        score: f64,
        country: &str,
        city: &str,
        status: i64,
        features: i64,
        lat: f64,
        long: f64,
        entry_ip: &str,
        physical_status: i64,
        services_down: i64,
    ) -> RawLogical {
        RawLogical {
            name: Some(name.to_string()),
            exit_country: Some(country.to_string()),
            tier: Some(tier),
            load: Some(load),
            score: Some(score),
            city: Some(city.to_string()),
            region: None,
            status: Some(status),
            features: Some(features),
            location: Some(RawLocation {
                lat: Some(lat),
                long: Some(long),
            }),
            servers: vec![RawPhysical {
                entry_ip: Some(entry_ip.to_string()),
                status: Some(physical_status),
                services_down: Some(services_down),
            }],
        }
    }

    fn simple(name: &str, tier: i64) -> RawLogical {
        make_logical(
            name,
            tier,
            50,
            1.0,
            "XX",
            "Nowhere",
            1,
            0,
            0.0,
            0.0,
            "203.0.113.1",
            1,
            0,
        )
    }

    #[test]
    fn excludes_servers_above_the_account_tier() {
        let logicals = vec![simple("FREE", 0), simple("PLUS", 2)];
        let names: Vec<String> =
            eligible_servers(&logicals, 0, &EligibilityOptions::default_excluded())
                .into_iter()
                .map(|c| c.name)
                .collect();
        assert_eq!(names, vec!["FREE"]);
    }

    #[test]
    fn includes_paid_servers_for_a_paid_account() {
        let logicals = vec![simple("FREE", 0), simple("PLUS", 2)];
        let mut names: Vec<String> =
            eligible_servers(&logicals, 2, &EligibilityOptions::default_excluded())
                .into_iter()
                .map(|c| c.name)
                .collect();
        names.sort();
        assert_eq!(names, vec!["FREE", "PLUS"]);
    }

    #[test]
    fn free_only_overrides_a_paid_account() {
        let logicals = vec![simple("FREE", 0), simple("PLUS", 2)];
        let mut opts = EligibilityOptions::default_excluded();
        opts.free_only = true;
        let names: Vec<String> = eligible_servers(&logicals, 2, &opts)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["FREE"]);
    }

    #[test]
    fn excludes_disabled_servers() {
        let logicals = vec![make_logical(
            "DOWN",
            0,
            50,
            1.0,
            "XX",
            "Nowhere",
            0,
            0,
            0.0,
            0.0,
            "203.0.113.1",
            1,
            0,
        )];
        assert!(eligible_servers(&logicals, 0, &EligibilityOptions::default_excluded()).is_empty());
    }

    #[test]
    fn excludes_secure_core_and_tor() {
        let logicals = vec![
            make_logical(
                "SC",
                0,
                50,
                1.0,
                "XX",
                "Nowhere",
                1,
                FEATURE_SECURE_CORE as i64,
                0.0,
                0.0,
                "203.0.113.1",
                1,
                0,
            ),
            make_logical(
                "TOR",
                0,
                50,
                1.0,
                "XX",
                "Nowhere",
                1,
                FEATURE_TOR as i64,
                0.0,
                0.0,
                "203.0.113.1",
                1,
                0,
            ),
            make_logical(
                "PLAIN",
                0,
                50,
                1.0,
                "XX",
                "Nowhere",
                1,
                0,
                0.0,
                0.0,
                "203.0.113.1",
                1,
                0,
            ),
            make_logical(
                "IPV6",
                0,
                50,
                1.0,
                "XX",
                "Nowhere",
                1,
                16,
                0.0,
                0.0,
                "203.0.113.1",
                1,
                0,
            ),
        ];
        let mut names: Vec<String> =
            eligible_servers(&logicals, 0, &EligibilityOptions::default_excluded())
                .into_iter()
                .map(|c| c.name)
                .collect();
        names.sort();
        assert_eq!(names, vec!["IPV6", "PLAIN"]);
    }

    #[test]
    fn filters_by_country_case_insensitively() {
        let logicals = vec![
            make_logical(
                "JP-FREE#1",
                0,
                50,
                1.0,
                "JP",
                "Nowhere",
                1,
                0,
                0.0,
                0.0,
                "203.0.113.1",
                1,
                0,
            ),
            make_logical(
                "SG-FREE#1",
                0,
                50,
                1.0,
                "SG",
                "Nowhere",
                1,
                0,
                0.0,
                0.0,
                "203.0.113.1",
                1,
                0,
            ),
        ];
        let mut opts = EligibilityOptions::default_excluded();
        opts.country = Some("jp".to_string());
        let names: Vec<String> = eligible_servers(&logicals, 0, &opts)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["JP-FREE#1"]);
    }

    #[test]
    fn skips_servers_with_no_usable_entry_ip() {
        let logicals = vec![
            make_logical(
                "NODE-DOWN",
                0,
                50,
                1.0,
                "XX",
                "Nowhere",
                1,
                0,
                0.0,
                0.0,
                "203.0.113.1",
                0,
                0,
            ),
            make_logical(
                "SERVICES-DOWN",
                0,
                50,
                1.0,
                "XX",
                "Nowhere",
                1,
                0,
                0.0,
                0.0,
                "203.0.113.1",
                1,
                1,
            ),
        ];
        assert!(eligible_servers(&logicals, 0, &EligibilityOptions::default_excluded()).is_empty());
    }

    #[test]
    fn carries_metadata_onto_the_candidate() {
        let logicals = vec![make_logical(
            "JP-FREE#9",
            0,
            81,
            5.0,
            "JP",
            "Tokyo",
            1,
            0,
            0.0,
            0.0,
            "203.0.113.1",
            1,
            0,
        )];
        let result = &eligible_servers(&logicals, 0, &EligibilityOptions::default_excluded())[0];
        assert_eq!(result.name, "JP-FREE#9");
        assert_eq!(result.load, 81);
        assert_eq!(result.city, "Tokyo");
        assert_eq!(result.country, "JP");
        assert_eq!(result.proton_score, 5.0);
    }

    #[test]
    fn annotate_distances_fills_distance_from_origin() {
        let logicals = vec![make_logical(
            "SG",
            0,
            50,
            1.0,
            "SG",
            "Nowhere",
            1,
            0,
            1.29,
            103.85,
            "203.0.113.1",
            1,
            0,
        )];
        let mut candidates =
            eligible_servers(&logicals, 0, &EligibilityOptions::default_excluded());
        let by_name: HashMap<String, RawLogical> = logicals
            .iter()
            .map(|l| (l.name.clone().unwrap(), l.clone()))
            .collect();
        annotate_distances(&mut candidates, &by_name, Some((-33.87, 151.22)));
        assert!((candidates[0].distance_km.unwrap() - 6300.0).abs() < 100.0);
    }

    #[test]
    fn annotate_distances_leaves_unset_without_an_origin() {
        let logicals = vec![make_logical(
            "SG",
            0,
            50,
            1.0,
            "SG",
            "Nowhere",
            1,
            0,
            1.29,
            103.85,
            "203.0.113.1",
            1,
            0,
        )];
        let mut candidates =
            eligible_servers(&logicals, 0, &EligibilityOptions::default_excluded());
        let by_name: HashMap<String, RawLogical> = logicals
            .iter()
            .map(|l| (l.name.clone().unwrap(), l.clone()))
            .collect();
        annotate_distances(&mut candidates, &by_name, None);
        assert!(candidates[0].distance_km.is_none());
    }
}
