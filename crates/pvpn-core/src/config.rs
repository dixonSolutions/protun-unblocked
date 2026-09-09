//! `~/.config/pvpn/config.toml` — the persistent equivalents of today's
//! `PVPN_*` environment variables. Env vars still override at runtime, for
//! one-off tuning without editing a file (`apply_env_overrides`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Unknown keys are ignored rather than rejected, so a config file left
/// over from the daemon — `auto_reconnect`, `reconnect_backoff_secs`, and
/// the rest of the settings for work that no longer happens in the
/// background — still loads without complaint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Connect down a freshly-measured ranked list rather than taking
    /// Proton's own pick. The knob this whole tool exists to add.
    pub auto_best_server: bool,
    pub connect_timeout_secs: u64,
    /// Grace period for traffic to start before a tunnel is (still) kept.
    pub settle_secs: u64,
    /// Re-try a blocked server after this long — networks and server pools
    /// change, so a block is never permanent.
    pub blocked_retry_after_hours: i64,
    pub country: Option<String>,
    pub free_only: bool,
    /// Servers to spend one measurement pass on before ranking.
    pub probe_shortlist: usize,
    /// Finalists re-timed without contention.
    pub probe_refine: usize,
    pub probe_rounds: usize,
    /// Hours before a cached server list is treated as stale.
    pub stale_hours: u64,
    pub refresh_timeout_secs: u64,
    pub best_timeout_secs: u64,
    /// Put Flatpak apps back on the tunnel after a successful connect.
    pub fix_apps: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_best_server: true,
            connect_timeout_secs: 30,
            settle_secs: 90,
            blocked_retry_after_hours: 24,
            country: None,
            free_only: false,
            probe_shortlist: 40,
            probe_refine: 8,
            probe_rounds: 2,
            stale_hours: 24,
            refresh_timeout_secs: 600,
            best_timeout_secs: 90,
            fix_apps: true,
        }
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pvpn")
    }

    pub fn default_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn data_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pvpn")
    }

    pub fn state_path() -> PathBuf {
        Self::data_dir().join("state.json")
    }

    /// Load `path`, falling back to defaults for anything missing or if the
    /// file does not exist yet. Never fails on a missing file — an unwritten
    /// config is not an error, it is the default.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Apply `PVPN_*` env vars on top of a loaded config, same names as the
    /// bash tool used, so muscle memory and existing scripts keep working.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("PVPN_BEST") {
            self.auto_best_server = v != "0";
        }
        if let Ok(v) = std::env::var("PVPN_TIMEOUT") {
            if let Ok(n) = v.parse() {
                self.connect_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PVPN_SETTLE") {
            if let Ok(n) = v.parse() {
                self.settle_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PVPN_BEST_COUNTRY") {
            if !v.is_empty() {
                self.country = Some(v);
            }
        }
        if let Ok(v) = std::env::var("PVPN_BEST_FREE") {
            self.free_only = v == "1";
        }
        if let Ok(v) = std::env::var("PVPN_STALE_HOURS") {
            if let Ok(n) = v.parse() {
                self.stale_hours = n;
            }
        }
        if let Ok(v) = std::env::var("PVPN_FIX_APPS") {
            self.fix_apps = v != "0";
        }
        if self.country.as_deref() == Some("") {
            self.country = None;
        }
    }

    pub fn blocked_retry_after(&self) -> chrono::Duration {
        chrono::Duration::hours(self.blocked_retry_after_hours.max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert!(cfg.auto_best_server);
        assert_eq!(cfg.settle_secs, 90);
    }

    #[test]
    fn missing_file_loads_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/pvpn-config.toml")).unwrap();
        assert!(cfg.auto_best_server);
    }

    #[test]
    fn a_config_written_by_the_daemon_still_loads() {
        // Those knobs described background work that no longer happens.
        // Refusing the file over them would strand anyone upgrading.
        let dir = std::env::temp_dir().join(format!("pvpn-core-oldcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "auto_reconnect = true\nreconnect_attempts = 0\n\
             reconnect_backoff_secs = [5, 15]\nprobe_interval_secs = 1800\n\
             settle_secs = 120\n",
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.settle_secs, 120,
            "the keys that still mean something survive"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn round_trips_through_disk() {
        let dir =
            std::env::temp_dir().join(format!("pvpn-core-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.auto_best_server = false;
        cfg.save(&path).unwrap();
        let reloaded = Config::load(&path).unwrap();
        assert!(!reloaded.auto_best_server);
        std::fs::remove_dir_all(dir).ok();
    }
}
