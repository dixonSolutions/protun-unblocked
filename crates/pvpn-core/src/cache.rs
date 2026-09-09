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

#[derive(Debug)]
pub struct SteerResult {
    pub kept: u32,
    pub hidden: u32,
    pub endpoint: Option<String>,
    pub backup: PathBuf,
    /// The file that was edited, so dropping this value can put it back.
    pub serverlist: PathBuf,
}

/// Dropping a steer undoes it.
///
/// The explicit `restore_cache` calls on the success paths stay — they put
/// Proton's data back as soon as the connect is over rather than whenever
/// the value happens to die. This is the backstop for every other exit:
/// Ctrl-C (which drops the whole connect future), an early `return`, a
/// panic. A steer that outlives its process leaves the account looking
/// like it owns one server, and nothing here can tell that apart from
/// Proton having retired the rest.
impl Drop for SteerResult {
    fn drop(&mut self) {
        restore_cache(&self.backup, &self.serverlist);
    }
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

/// Fields Proton's client checks before deciding its cached list is usable.
const EXPIRY_FIELDS: [&str; 2] = ["ExpirationTime", "LoadsExpirationTime"];

/// Let Proton use the list we already have.
///
/// Past `ExpirationTime`, Proton's client refuses to connect at all — it
/// answers `Server list is outdated, updating... This may take a moment.`
/// and stops. On an open network that is a two-second refresh nobody
/// notices. On the networks this tool exists for it is a deadlock: the
/// refresh needs the API, the API is exactly what is blocked, and the cache
/// can never be renewed, so the client bricks itself over data that is
/// merely a day old while a perfectly good server list sits on disk.
///
/// Breaking that costs one local field. This does *not* forge server data —
/// the inventory is Proton's own, unmodified — it only stops the client
/// refusing to read it. Called only after a refresh has actually been tried
/// and failed, so a network that can reach the API still gets real data.
///
/// Returns `true` if the cache was expired and has been extended.
pub fn extend_inventory_expiry(serverlist: &Path, hours: i64) -> anyhow::Result<bool> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs_f64();
    if inventory_valid_for_secs_at(serverlist, now).is_some_and(|left| left > 0) {
        return Ok(false);
    }
    let text = std::fs::read_to_string(serverlist)?;
    let mut data: Value = serde_json::from_str(&text)?;
    let Some(object) = data.as_object_mut() else {
        return Ok(false);
    };
    let until = now + (hours * 3600) as f64;
    let mut touched = false;
    for field in EXPIRY_FIELDS {
        if object.contains_key(field) {
            object.insert(field.to_string(), serde_json::json!(until));
            touched = true;
        }
    }
    if !touched {
        return Ok(false);
    }
    std::fs::write(serverlist, serde_json::to_vec(&data)?)?;
    Ok(true)
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
    claim_steer(serverlist);

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
        release_steer(serverlist);
        return Err(SteerError::NoMatch);
    }
    std::fs::write(serverlist, serde_json::to_vec(&data)?)?;
    Ok(SteerResult {
        kept,
        hidden,
        endpoint,
        backup,
        serverlist: serverlist.to_path_buf(),
    })
}

/// Put Proton's data back. Safe to call when `backup` does not exist.
pub fn restore_cache(backup: &Path, serverlist: &Path) {
    if backup.exists() {
        let _ = std::fs::rename(backup, serverlist);
    }
    release_steer(serverlist);
}

// --- stranded steers -------------------------------------------------------

/// Free servers below this and the "most of the pool is offline" test says
/// nothing useful — a four-server account with two in maintenance is not
/// evidence of anything.
const MIN_FREE_TO_JUDGE: usize = 4;

/// Proton takes single servers down for maintenance; it does not retire
/// three quarters of the free pool at once. A cache that claims it did was
/// steered by us and never put back.
const STRANDED_FRACTION: f64 = 0.75;

/// The `If-Modified-Since` value Proton's client sends and treats as "I have
/// nothing", copied from `proton.vpn.session.servers.logicals.UNIX_EPOCH`.
const UNIX_EPOCH_HEADER: &str = "Thu, 01 Jan 1970 00:00:00 GMT";

fn steer_marker(serverlist: &Path) -> PathBuf {
    serverlist.with_extension("json.pvpn-steer")
}

/// Record which process owns the steer currently in the cache.
fn claim_steer(serverlist: &Path) {
    let _ = std::fs::write(steer_marker(serverlist), std::process::id().to_string());
}

fn release_steer(serverlist: &Path) {
    let _ = std::fs::remove_file(steer_marker(serverlist));
}

/// Is another `pvpn` steering this cache right now?
///
/// Asks the process table rather than a timeout: a hop can legitimately hold
/// a steer for minutes while it connects and settles, and healing underneath
/// a live one would hand that hop a different server than the user named.
/// The command-line check is what makes a recycled pid safe.
fn steer_owner_is_alive(serverlist: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(steer_marker(serverlist)) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    if pid == std::process::id() {
        // Nobody else can be holding our pid, so this marker is ours and the
        // steer it names is in flight.
        return true;
    }
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => String::from_utf8_lossy(&raw).contains("pvpn"),
        Err(_) => false,
    }
}

/// What healing a stranded steer had to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Healed {
    /// The steer's own backup was still there, so Proton's data went back
    /// byte for byte.
    FromBackup,
    /// No backup survived — the servers we could only have hidden ourselves
    /// were switched back on.
    Reenabled(usize),
}

/// Undo a steer that outlived the process that made it.
///
/// Two ways one survives, and they need different answers. A killed `pvpn`
/// leaves its backup behind: that is Proton's own data and goes straight
/// back. The subtler one leaves nothing — Proton's client reads the steered
/// list into memory while we hold it, and its next refresh answers 304 Not
/// Modified by *re-saving what it already had*, so our steer lands back on
/// disk after we restored it. Only the shape of the list gives that away, so
/// the fallback re-enables every free server and clears `LastModifiedTime`,
/// which is what forces the next refresh to fetch a real body instead of
/// blessing the copy in memory again.
pub fn heal_stranded_steer(serverlist: &Path) -> Result<Option<Healed>, SteerError> {
    if !serverlist.is_file() {
        return Ok(None);
    }
    if steer_owner_is_alive(serverlist) {
        return Ok(None);
    }

    let backup = serverlist.with_extension("json.pvpn-bak");
    if backup.is_file() {
        restore_cache(&backup, serverlist);
        return Ok(Some(Healed::FromBackup));
    }

    let text = std::fs::read_to_string(serverlist)?;
    let mut data: Value = serde_json::from_str(&text)?;
    let key = if data.get("LogicalServers").is_some() {
        "LogicalServers"
    } else {
        "Servers"
    };
    let Some(list) = data.get_mut(key).and_then(|v| v.as_array_mut()) else {
        return Ok(None);
    };

    // Judged over exactly what a steer can touch: free servers, by name.
    let free: Vec<&Value> = list
        .iter()
        .filter(|s| {
            s.get("Name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_uppercase()
                .contains("FREE")
        })
        .collect();
    let total = free.len();
    let offline = free
        .iter()
        .filter(|s| s["Status"] != Value::from(1))
        .count();
    if total < MIN_FREE_TO_JUDGE || (offline as f64) < total as f64 * STRANDED_FRACTION {
        return Ok(None);
    }

    let mut reenabled = 0usize;
    for srv in list.iter_mut() {
        let free = srv
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_uppercase()
            .contains("FREE");
        if free && srv["Status"] != Value::from(1) {
            srv["Status"] = Value::from(1);
            reenabled += 1;
        }
    }
    data["LastModifiedTime"] = Value::from(UNIX_EPOCH_HEADER);
    std::fs::write(serverlist, serde_json::to_vec(&data)?)?;
    Ok(Some(Healed::Reenabled(reenabled)))
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

    fn write_list_expiring(dir: &std::path::Path, expires_in_secs: f64) -> PathBuf {
        let path = write_list(dir);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let mut data: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let object = data.as_object_mut().unwrap();
        object.insert("ExpirationTime".into(), json!(now + expires_in_secs));
        object.insert("LoadsExpirationTime".into(), json!(now + expires_in_secs));
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        path
    }

    #[test]
    fn an_expired_inventory_is_made_usable_again() {
        let dir = tempdir();
        let path = write_list_expiring(&dir, -19.0 * 3600.0);
        assert!(inventory_valid_for_secs(&path).unwrap() < 0, "starts expired");
        assert!(extend_inventory_expiry(&path, 24).unwrap(), "reports the change");
        assert!(
            inventory_valid_for_secs(&path).unwrap() > 23 * 3600,
            "Proton will now read the list instead of refusing to connect"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extending_never_touches_proton_server_data() {
        let dir = tempdir();
        let path = write_list_expiring(&dir, -1.0);
        let before = enabled_free(&path);
        extend_inventory_expiry(&path, 24).unwrap();
        assert_eq!(
            enabled_free(&path),
            before,
            "only the expiry moves; the inventory is Proton's own"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_inventory_is_left_alone() {
        let dir = tempdir();
        let path = write_list_expiring(&dir, 3600.0);
        let before = inventory_valid_for_secs(&path).unwrap();
        assert!(!extend_inventory_expiry(&path, 24).unwrap(), "nothing to do");
        assert!(
            (inventory_valid_for_secs(&path).unwrap() - before).abs() <= 1,
            "a cache that can still be refreshed normally must not be extended"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        let _steered = steer_cache(&path, SteerMode::Only, "SG-FREE#2").unwrap();
        assert_eq!(enabled_free(&path), vec!["SG-FREE#2"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn longer_name_still_selects_only_that_one() {
        let dir = tempdir();
        let path = write_list(&dir);
        let _steered = steer_cache(&path, SteerMode::Only, "SG-FREE#21").unwrap();
        assert_eq!(enabled_free(&path), vec!["SG-FREE#21"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn country_pattern_matches_the_whole_family() {
        let dir = tempdir();
        let path = write_list(&dir);
        let _steered = steer_cache(&path, SteerMode::Only, "JP").unwrap();
        assert_eq!(enabled_free(&path), vec!["JP-FREE#1", "JP-FREE#12"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn excluding_a_server_leaves_its_longer_neighbour() {
        let dir = tempdir();
        let path = write_list(&dir);
        let _steered = steer_cache(&path, SteerMode::Exclude, "SG-FREE#2").unwrap();
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

    /// A pid no process can have, so the marker reads as a dead owner.
    fn dead_pid(serverlist: &Path) {
        std::fs::write(steer_marker(serverlist), "4294967290").unwrap();
    }

    #[test]
    fn a_dropped_steer_puts_the_cache_back() {
        let dir = tempdir();
        let path = write_list(&dir);
        {
            let _steered = steer_cache(&path, SteerMode::Only, "JP").unwrap();
            assert_eq!(enabled_free(&path), vec!["JP-FREE#1", "JP-FREE#12"]);
        }
        assert_eq!(
            enabled_free(&path),
            vec!["JP-FREE#1", "JP-FREE#12", "SG-FREE#2", "SG-FREE#21"]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_steer_whose_owner_died_is_restored_from_its_backup() {
        let dir = tempdir();
        let path = write_list(&dir);
        let steered = steer_cache(&path, SteerMode::Only, "SG-FREE#2").unwrap();
        std::mem::forget(steered);
        dead_pid(&path);

        assert_eq!(
            heal_stranded_steer(&path).unwrap(),
            Some(Healed::FromBackup)
        );
        assert_eq!(
            enabled_free(&path),
            vec!["JP-FREE#1", "JP-FREE#12", "SG-FREE#2", "SG-FREE#21"]
        );
        assert!(!path.with_extension("json.pvpn-bak").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Proton's client re-saves the steered list it holds in memory when the
    /// API answers 304, which lands our steer back on disk *after* the
    /// backup is gone. Shape is then the only evidence left.
    #[test]
    fn a_steer_proton_re_saved_is_re_enabled_without_a_backup() {
        let dir = tempdir();
        let path = write_list(&dir);
        let steered = steer_cache(&path, SteerMode::Only, "SG-FREE#2").unwrap();
        std::fs::remove_file(&steered.backup).unwrap();
        std::mem::forget(steered);
        dead_pid(&path);

        assert_eq!(
            heal_stranded_steer(&path).unwrap(),
            Some(Healed::Reenabled(3))
        );
        assert_eq!(
            enabled_free(&path),
            vec!["JP-FREE#1", "JP-FREE#12", "SG-FREE#2", "SG-FREE#21"]
        );
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(data["LastModifiedTime"], Value::from(UNIX_EPOCH_HEADER));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_steer_still_being_held_is_left_alone() {
        let dir = tempdir();
        let path = write_list(&dir);
        let _steered = steer_cache(&path, SteerMode::Only, "SG-FREE#2").unwrap();

        assert_eq!(heal_stranded_steer(&path).unwrap(), None);
        assert_eq!(enabled_free(&path), vec!["SG-FREE#2"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ordinary_maintenance_is_not_mistaken_for_a_steer() {
        let dir = tempdir();
        let path = dir.join("serverlist.json");
        let data = json!({
            "LogicalServers": [
                {"Name": "SG-FREE#2", "Status": 1},
                {"Name": "SG-FREE#21", "Status": 0},
                {"Name": "JP-FREE#1", "Status": 1},
                {"Name": "JP-FREE#12", "Status": 1},
                {"Name": "US-PLUS#4", "Status": 0}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();

        assert_eq!(heal_stranded_steer(&path).unwrap(), None);
        assert_eq!(
            enabled_free(&path),
            vec!["JP-FREE#1", "JP-FREE#12", "SG-FREE#2"]
        );
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
