//! The window's own preferences, in `~/.config/pvpn/gui.toml`.
//!
//! Kept apart from `config.toml` on purpose: that file is `pvpn`'s, read by
//! every connect, and nothing about how a window looks belongs in it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    /// Master switch; the five below only matter while this is on.
    pub enabled: bool,
    pub connected: bool,
    pub disconnected: bool,
    pub reconnecting: bool,
    pub reconnected: bool,
    pub failure: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            connected: true,
            disconnected: true,
            reconnecting: true,
            reconnected: true,
            failure: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiSettings {
    pub notifications: Notifications,
    /// Show the icon in the top bar (needs an AppIndicator/SNI host).
    pub tray: bool,
    /// Closing the window keeps the tray icon running instead of quitting.
    pub close_to_tray: bool,
    /// Start with the session.
    pub autostart: bool,
    /// When started with the session, stay in the tray.
    pub start_hidden: bool,
    /// How often the connection state is read (routes, NetworkManager).
    pub poll_secs: u32,
    /// How often a connected tunnel is asked to carry a packet; 0 = never.
    /// The watch timer already does this every two minutes; this only
    /// makes the window's own "carrying" line fresher.
    pub traffic_check_secs: u32,
    /// Which `pvpn` to run. Empty: `~/.local/bin/pvpn`, then `$PATH`.
    pub pvpn_path: String,
    /// Protocol for the orb's connect. Empty: `pvpn up`'s own choice.
    pub protocol: String,
    /// How many ranked servers get a numbered label on the globe.
    pub globe_labels: u32,
    pub globe_show_blocked: bool,
    /// Options for "Measure & rank".
    pub rank_limit: u32,
    pub rank_country: String,
    pub rank_free_only: bool,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            notifications: Notifications::default(),
            tray: true,
            close_to_tray: true,
            autostart: false,
            start_hidden: true,
            poll_secs: 3,
            traffic_check_secs: 30,
            pvpn_path: String::new(),
            protocol: String::new(),
            globe_labels: 8,
            globe_show_blocked: true,
            rank_limit: 15,
            rank_country: String::new(),
            rank_free_only: false,
        }
    }
}

pub const APP_ID: &str = "io.github.dixonsolutions.ProtunUnblocked";

impl GuiSettings {
    pub fn path() -> PathBuf {
        pvpn_core::config::Config::config_dir().join("gui.toml")
    }

    /// Never fails: a missing or broken file is the defaults, and a broken
    /// one is left in place for the person who broke it to look at.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// The `pvpn` the window drives: the installed copy, because that is the
    /// one every hook and timer runs too.
    pub fn pvpn_binary(&self) -> String {
        if !self.pvpn_path.trim().is_empty() {
            return self.pvpn_path.trim().to_string();
        }
        let installed = dirs_home().join(".local/bin/pvpn");
        if installed.is_file() {
            return installed.to_string_lossy().into_owned();
        }
        "pvpn".to_string()
    }
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `~/.config/autostart/<app id>.desktop`, written or removed to match
/// the `autostart` switch.
pub fn autostart_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".config"))
        .join("autostart")
        .join(format!("{APP_ID}.desktop"))
}

pub fn apply_autostart(settings: &GuiSettings) -> anyhow::Result<()> {
    let path = autostart_path();
    if settings.autostart {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "pvpn-gui".into());
        let hidden = if settings.start_hidden { " --hidden" } else { "" };
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=Protun Unblocked\n\
                 Comment=Proton VPN for filtered networks\nExec={exe}{hidden}\n\
                 Icon={APP_ID}\nX-GNOME-Autostart-enabled=true\nNoDisplay=false\n"
            ),
        )?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let parsed: GuiSettings =
            toml::from_str("tray = false\n[notifications]\nreconnecting = false\n").unwrap();
        assert!(!parsed.tray);
        assert!(!parsed.notifications.reconnecting);
        assert!(parsed.notifications.reconnected);
        assert_eq!(parsed.poll_secs, 3);
    }

    #[test]
    fn round_trips() {
        let mut s = GuiSettings::default();
        s.notifications.failure = false;
        s.protocol = "protun-tcp".into();
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(toml::from_str::<GuiSettings>(&text).unwrap(), s);
    }
}
