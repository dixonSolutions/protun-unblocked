//! Steer Proton's local `serverlist.json` so a free account can influence
//! which server the client picks. Port of `steer_cache` in `legacy/pvpn.sh`.
//!
//! A free account cannot choose a server through any documented flag —
//! `--country`, `--random` and by-ID are all refused, and a plain reconnect
//! is not random either. What works: the client picks from its local cache.
//! Mark the servers you do not want as `Status=0` and it picks something
//! else. The cache is always put back afterwards.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerMode {
    /// Hide exactly the servers matching the pattern.
    Exclude,
    /// Hide every free server *not* matching the pattern.
    Only,
    /// Select one exact cached server even when Proton marks it unavailable.
    ForceOnly,
}

#[derive(Debug, Clone)]
pub struct SteerResult {
    pub kept: u32,
    pub hidden: u32,
    pub endpoint: Option<String>,
    pub backup: PathBuf,
}

#[derive(thiserror::Error, Debug)]
pub enum SteerError {
    #[error("no cached server list at {0}")]
    Missing(PathBuf),
    #[error("no free server matches that pattern")]
    NoMatch,
    #[error("the matching server exists, but Proton currently marks it unavailable")]
    Unavailable,
    #[error("the matching server has no cached endpoint to try")]
    NoEndpoint,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

/// A pattern containing `#` is a full server id and must match exactly.
/// It used to be a substring test, so `pvpn hop SG-FREE#2` also matched
/// `SG-FREE#21`. Patterns without `#` stay substring matches, which is
/// what makes `pvpn hop JP` and `pvpn hop SG-FREE` work.
pub fn name_matches(name: &str, pattern: &str) -> bool {
    let name = name.to_uppercase();
    let pattern = pattern.to_uppercase();
    if pattern.contains('#') {
        name == pattern
    } else {
        name.contains(&pattern)
    }
}

/// Seconds until Proton's logical-server inventory expires.
///
/// The file modification time is not inventory freshness: Proton also
/// rewrites this file after lightweight load updates.
pub fn inventory_valid_for_secs(serverlist: &Path) -> Option<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    inventory_valid_for_secs_at(serverlist, now)
}

fn inventory_valid_for_secs_at(serverlist: &Path, now: f64) -> Option<i64> {
    let text = std::fs::read_to_string(serverlist).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let expires = data.get("ExpirationTime")?.as_f64()?;
    Some((expires - now).floor() as i64)
}

fn server_is_available(server: &Value) -> bool {
    if server.get("Status").and_then(Value::as_i64) != Some(1) {
        return false;
    }
    let Some(physical_servers) = server.get("Servers").and_then(Value::as_array) else {
        return true;
    };
    physical_servers.is_empty()
        || physical_servers.iter().any(|physical| {
            physical.get("Status").and_then(Value::as_i64) == Some(1)
                && physical
                    .get("ServicesDown")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    == 0
        })
}

fn preferred_endpoint(server: &Value) -> Option<String> {
    let physical_servers = server.get("Servers")?.as_array()?;
    physical_servers
        .iter()
        .find(|physical| {
            physical.get("Status").and_then(Value::as_i64) == Some(1)
                && physical
                    .get("ServicesDown")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    == 0
                && physical.get("EntryIP").and_then(Value::as_str).is_some()
        })
        .or_else(|| {
            physical_servers
                .iter()
                .find(|physical| physical.get("EntryIP").and_then(Value::as_str).is_some())
        })
        .and_then(|physical| physical.get("EntryIP"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn force_server_available(server: &mut Value) -> Option<String> {
    let endpoint = preferred_endpoint(server)?;
    server["Status"] = Value::from(1);
    if let Some(physical_servers) = server.get_mut("Servers").and_then(Value::as_array_mut) {
        for physical in physical_servers {
            let selected =
                physical.get("EntryIP").and_then(Value::as_str) == Some(endpoint.as_str());
            physical["Status"] = Value::from(i64::from(selected));
            if selected {
                physical["ServicesDown"] = Value::from(0);
            }
        }
    }
    Some(endpoint)
}

/// Copy `serverlist` aside and mark unwanted free servers offline.
/// Paid servers are left alone — they are unreachable on a free account
/// anyway, and editing them is not this tool's job.
pub fn steer_cache(
    serverlist: &Path,
    mode: SteerMode,
    pattern: &str,
) -> Result<SteerResult, SteerError> {
    if !serverlist.is_file() {
        return Err(SteerError::Missing(serverlist.to_path_buf()));
    }
    let text = std::fs::read_to_string(serverlist)?;
    let mut data: Value = serde_json::from_str(&text)?;
    let key = if data.get("LogicalServers").is_some() {
        "LogicalServers"
    } else {
        "Servers"
    };
    let Some(list) = data.get_mut(key).and_then(|v| v.as_array_mut()) else {
        return Err(SteerError::NoMatch);
    };

    if matches!(mode, SteerMode::Only | SteerMode::ForceOnly) {
        let matches: Vec<&Value> = list
            .iter()
            .filter(|server| {
                let name = server.get("Name").and_then(Value::as_str).unwrap_or("");
                name.to_uppercase().contains("FREE") && name_matches(name, pattern)
            })
            .collect();
        if matches.is_empty() {
            return Err(SteerError::NoMatch);
        }
        if !matches.iter().any(|server| server_is_available(server)) {
            if matches!(mode, SteerMode::Only) {
                return Err(SteerError::Unavailable);
            }
            if !matches
                .iter()
                .any(|server| preferred_endpoint(server).is_some())
            {
                return Err(SteerError::NoEndpoint);
            }
        }
    }

    let backup = serverlist.with_extension("json.pvpn-bak");
    std::fs::copy(serverlist, &backup)?;

    let mut kept = 0u32;
    let mut hidden = 0u32;
    let mut endpoint = None;
    for srv in list.iter_mut() {
        let name = srv
            .get("Name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !name.to_uppercase().contains("FREE") {
            continue;
        }
        let matched = name_matches(&name, pattern);
        let hide = match mode {
            SteerMode::Exclude => matched,
            SteerMode::Only | SteerMode::ForceOnly => !matched,
        };
        if hide {
            srv["Status"] = Value::from(0);
            hidden += 1;
        } else {
            if matched {
                endpoint = if matches!(mode, SteerMode::ForceOnly) {
                    force_server_available(srv)
                } else {
                    preferred_endpoint(srv)
                }
                .or(endpoint);
            }
            kept += 1;
        }
    }
    if kept == 0 {
        let _ = std::fs::rename(&backup, serverlist);
        return Err(SteerError::NoMatch);
    }
    std::fs::write(serverlist, serde_json::to_vec(&data)?)?;
    Ok(SteerResult {
        kept,
        hidden,
        endpoint,
        backup,
    })
}

/// Put Proton's data back. Safe to call when `backup` does not exist.
pub fn restore_cache(backup: &Path, serverlist: &Path) {
    if backup.exists() {
        let _ = std::fs::rename(backup, serverlist);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_list(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("serverlist.json");
        let data = json!({
            "LogicalServers": [
                {"Name": "SG-FREE#2", "Status": 1},
                {"Name": "SG-FREE#21", "Status": 1},
                {"Name": "JP-FREE#1", "Status": 1},
                {"Name": "JP-FREE#12", "Status": 1},
                {"Name": "US-PLUS#4", "Status": 1}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        path
    }

    fn enabled_free(path: &Path) -> Vec<String> {
        let data: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut names: Vec<String> = data["LogicalServers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["Status"] == 1 && s["Name"].as_str().unwrap().contains("FREE"))
            .map(|s| s["Name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pvpn-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn exact_server_excludes_its_longer_neighbours() {
        let dir = tempdir();
        let path = write_list(&dir);
        steer_cache(&path, SteerMode::Only, "SG-FREE#2").unwrap();
        assert_eq!(enabled_free(&path), vec!["SG-FREE#2"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn longer_name_still_selects_only_that_one() {
        let dir = tempdir();
        let path = write_list(&dir);
        steer_cache(&path, SteerMode::Only, "SG-FREE#21").unwrap();
        assert_eq!(enabled_free(&path), vec!["SG-FREE#21"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn country_pattern_matches_the_whole_family() {
        let dir = tempdir();
        let path = write_list(&dir);
        steer_cache(&path, SteerMode::Only, "JP").unwrap();
        assert_eq!(enabled_free(&path), vec!["JP-FREE#1", "JP-FREE#12"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn excluding_a_server_leaves_its_longer_neighbour() {
        let dir = tempdir();
        let path = write_list(&dir);
        steer_cache(&path, SteerMode::Exclude, "SG-FREE#2").unwrap();
        assert_eq!(
            enabled_free(&path),
            vec!["JP-FREE#1", "JP-FREE#12", "SG-FREE#21"]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn restore_puts_the_original_back() {
        let dir = tempdir();
        let path = write_list(&dir);
        let steered = steer_cache(&path, SteerMode::Only, "JP").unwrap();
        restore_cache(&steered.backup, &path);
        assert_eq!(
            enabled_free(&path),
            vec!["JP-FREE#1", "JP-FREE#12", "SG-FREE#2", "SG-FREE#21"]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn no_match_restores_and_errors() {
        let dir = tempdir();
        let path = write_list(&dir);
        let err = steer_cache(&path, SteerMode::Only, "NO-SUCH").unwrap_err();
        assert!(matches!(err, SteerError::NoMatch));
        assert_eq!(
            enabled_free(&path),
            vec!["JP-FREE#1", "JP-FREE#12", "SG-FREE#2", "SG-FREE#21"]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_named_but_disabled_server_is_not_treated_as_selectable() {
        let dir = tempdir();
        let path = write_list(&dir);
        let mut data: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        data["LogicalServers"][2]["Status"] = Value::from(0);
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();

        let err = steer_cache(&path, SteerMode::Only, "JP-FREE#1").unwrap_err();

        assert!(matches!(err, SteerError::Unavailable));
        assert!(!path.with_extension("json.pvpn-bak").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_server_with_no_usable_physical_node_is_unavailable() {
        let dir = tempdir();
        let path = write_list(&dir);
        let mut data: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        data["LogicalServers"][2]["Servers"] = json!([
            {"Status": 1, "ServicesDown": 1},
            {"Status": 0, "ServicesDown": 0}
        ]);
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();

        let err = steer_cache(&path, SteerMode::Only, "JP-FREE#1").unwrap_err();

        assert!(matches!(err, SteerError::Unavailable));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_explicit_force_uses_protons_cached_endpoint() {
        let dir = tempdir();
        let path = write_list(&dir);
        let mut data: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        data["LogicalServers"][2]["Status"] = Value::from(0);
        data["LogicalServers"][2]["Servers"] = json!([
            {"EntryIP": "192.0.2.10", "Status": 0, "ServicesDown": 0},
            {"EntryIP": "192.0.2.11", "Status": 0, "ServicesDown": 1}
        ]);
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();

        let steered = steer_cache(&path, SteerMode::ForceOnly, "JP-FREE#1").unwrap();

        assert_eq!(steered.endpoint.as_deref(), Some("192.0.2.10"));
        assert_eq!(enabled_free(&path), vec!["JP-FREE#1"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn inventory_freshness_uses_protons_expiration_not_the_file_timestamp() {
        let dir = tempdir();
        let path = dir.join("serverlist.json");
        std::fs::write(&path, r#"{"ExpirationTime":1060.5}"#).unwrap();

        assert_eq!(inventory_valid_for_secs_at(&path, 1000.0), Some(60));
        assert_eq!(inventory_valid_for_secs_at(&path, 1061.0), Some(-1));
        std::fs::remove_dir_all(dir).ok();
    }
}
