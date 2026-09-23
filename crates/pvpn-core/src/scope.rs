//! Which networks something may rebuild the tunnel on for you.
//!
//! A reconnect on the network you asked for a tunnel on is the job. The same
//! reconnect on the café wifi you roamed onto afterwards is a decision nobody
//! made — and on a network that refuses every connect it is two minutes of no
//! internet you did not ask for. So the resume hook and `pvpn watch` both ask
//! here first, and the default answers "only where you last ran `pvpn up`".
//!
//! `~/.config/pvpn/config.toml`:
//!
//! ```toml
//! autoconnect_networks = "started"              # default: where you last ran pvpn up/hop
//! autoconnect_networks = "all"                  # anywhere with a link
//! autoconnect_networks = ["detnsw", "wired:eth0"]  # these, by SSID or network key
//! ```
//!
//! Like [`crate::intent`], this can only ever cause *less* to happen. It
//! gates automatic reconnects; nothing you type is ever refused because of it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Set by `pvpn-autoconnect` on the `pvpn up` it runs, so a reconnect the
/// user did not type is not mistaken for one they did and moved the
/// "started" network along with it.
pub const AUTOCONNECT_ENV: &str = "PVPN_AUTOCONNECT";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AutoconnectNetworks {
    /// `"started"` or `"all"`.
    Mode(Mode),
    /// A fixed list of SSIDs or network keys.
    List(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Started,
    All,
}

impl Default for AutoconnectNetworks {
    fn default() -> Self {
        Self::Mode(Mode::Started)
    }
}

/// The file holding the network key of the last manual `pvpn up`/`hop`.
/// Data, not config: it is an observation, rewritten every time you ask.
pub fn started_marker(data_dir: &Path) -> PathBuf {
    data_dir.join("autoconnect-network")
}

/// Remember `network` as where the user last asked for a tunnel. Best-effort,
/// like the down marker — a failed write means one reconnect not happening,
/// never a connect failing. `offline` is not a network and is not recorded.
pub fn record_started(data_dir: &Path, network: &str) {
    if network.is_empty() || network == "offline" {
        return;
    }
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(started_marker(data_dir), network);
}

pub fn started_network(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(started_marker(data_dir)).ok()?;
    let key = text.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// Was this `pvpn` started by the autoconnect hook rather than by a person?
pub fn launched_by_autoconnect() -> bool {
    std::env::var(AUTOCONNECT_ENV).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// `wifi:detnsw` matches both `wifi:detnsw` and bare `detnsw`, because an
/// SSID is what anybody writing a config file actually knows.
fn matches(entry: &str, network: &str) -> bool {
    let entry = entry.trim();
    entry == network || network.strip_prefix("wifi:") == Some(entry)
}

/// May a reconnect happen on `network`? `Ok` carries why it may, `Err` why
/// not — both are printed, because "it didn't reconnect" with no reason is
/// the bug report this exists to prevent.
pub fn allowed(
    scope: &AutoconnectNetworks,
    network: &str,
    started: Option<&str>,
) -> Result<String, String> {
    if network == "offline" {
        return Err("no network is up".to_string());
    }
    match scope {
        AutoconnectNetworks::Mode(Mode::All) => {
            Ok(format!("{network}: autoconnect_networks = \"all\""))
        }
        AutoconnectNetworks::Mode(Mode::Started) => match started {
            Some(s) if s == network => {
                Ok(format!("{network} is where you last ran `pvpn up`"))
            }
            Some(s) => Err(format!(
                "on {network}, but you last ran `pvpn up` on {s} — run it here to move autoconnect"
            )),
            None => Err(format!(
                "on {network}, and you have not run `pvpn up` anywhere yet"
            )),
        },
        AutoconnectNetworks::List(list) => {
            if list.iter().any(|entry| matches(entry, network)) {
                Ok(format!("{network} is in autoconnect_networks"))
            } else {
                Err(format!("{network} is not in autoconnect_networks"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> AutoconnectNetworks {
        AutoconnectNetworks::Mode(Mode::Started)
    }

    #[test]
    fn default_is_started() {
        assert_eq!(AutoconnectNetworks::default(), started());
    }

    #[test]
    fn started_allows_only_the_recorded_network() {
        assert!(allowed(&started(), "wifi:home", Some("wifi:home")).is_ok());
        assert!(allowed(&started(), "wifi:cafe", Some("wifi:home")).is_err());
    }

    /// Never having asked for a tunnel is not permission to build one.
    #[test]
    fn started_with_nothing_recorded_refuses() {
        assert!(allowed(&started(), "wifi:home", None).is_err());
    }

    #[test]
    fn all_allows_any_real_network() {
        let all = AutoconnectNetworks::Mode(Mode::All);
        assert!(allowed(&all, "wifi:anything", None).is_ok());
        assert!(allowed(&all, "offline", None).is_err());
    }

    #[test]
    fn list_matches_bare_ssids_and_full_keys() {
        let list = AutoconnectNetworks::List(vec!["detnsw".into(), "wired:eth0".into()]);
        assert!(allowed(&list, "wifi:detnsw", None).is_ok());
        assert!(allowed(&list, "wired:eth0", None).is_ok());
        assert!(allowed(&list, "wifi:eth0", None).is_err());
        assert!(allowed(&list, "wifi:cafe", None).is_err());
    }

    #[test]
    fn parses_every_form_from_toml() {
        #[derive(Deserialize)]
        struct T {
            autoconnect_networks: AutoconnectNetworks,
        }
        let parse = |s: &str| toml::from_str::<T>(s).unwrap().autoconnect_networks;
        assert_eq!(parse(r#"autoconnect_networks = "started""#), started());
        assert_eq!(
            parse(r#"autoconnect_networks = "all""#),
            AutoconnectNetworks::Mode(Mode::All)
        );
        assert_eq!(
            parse(r#"autoconnect_networks = ["a", "b"]"#),
            AutoconnectNetworks::List(vec!["a".into(), "b".into()])
        );
        assert!(toml::from_str::<T>(r#"autoconnect_networks = "sometimes""#).is_err());
    }

    #[test]
    fn record_round_trips_and_skips_offline() {
        let dir = std::env::temp_dir().join(format!("pvpn-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(started_network(&dir), None);
        record_started(&dir, "wifi:home");
        assert_eq!(started_network(&dir).as_deref(), Some("wifi:home"));
        record_started(&dir, "offline");
        assert_eq!(started_network(&dir).as_deref(), Some("wifi:home"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
