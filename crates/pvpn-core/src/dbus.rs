//! Minimal, typed NetworkManager access for the connection hot path.
//!
//! Proton's client ultimately creates NetworkManager profiles. Re-activating
//! a profile that has already carried traffic does not need another Python
//! client startup; it only needs one D-Bus method call. Every operation here
//! is best-effort and callers retain their existing `nmcli`/Proton fallback.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const NM_DEST: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const SETTINGS_CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const ACTIVE_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";

const NM_ACTIVE_CONNECTION_STATE_ACTIVATED: u32 = 2;
const NM_ACTIVE_CONNECTION_STATE_DEACTIVATED: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedProfile {
    pub path: String,
    pub id: String,
    pub uuid: String,
    pub kind: String,
    /// `vpn.service-type` — which NetworkManager plugin runs this profile.
    /// Empty for a profile that is not a VPN.
    pub service_type: String,
    /// `vpn.data` as NetworkManager stores it: for protun, this carries a
    /// `settings` JSON naming the ports, which is what tells `protun-tcp`
    /// from `protun-tls`.
    pub vpn_data: HashMap<String, String>,
    /// `connection.autoconnect`, which NetworkManager defaults to on.
    pub autoconnect: bool,
}

impl SavedProfile {
    /// The protocol this profile will use when activated, or `None` when
    /// it does not say. Same rules as [`crate::proc::profile_protocol`].
    pub fn protocol(&self) -> Option<String> {
        crate::proc::protocol_from_profile(&self.service_type, &self.vpn_data)
    }

    /// The server this `ProtonVPN <server>` profile is for, if it is one.
    pub fn server(&self) -> Option<String> {
        let server = self.id.strip_prefix("ProtonVPN ")?;
        if server.starts_with("pvpn-preserve-") {
            return None;
        }
        Some(
            server
                .strip_suffix(" (verified)")
                .unwrap_or(server)
                .to_string(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveProfile {
    pub path: String,
    pub id: String,
}

fn system_bus() -> Option<Connection> {
    Connection::system().ok()
}

fn string_value(section: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    <&str>::try_from(section.get(key)?).ok().map(str::to_owned)
}

/// `vpn.data` is `a{ss}` on the bus.
fn string_map(section: &HashMap<String, OwnedValue>, key: &str) -> HashMap<String, String> {
    section
        .get(key)
        .and_then(|value| <HashMap<String, String>>::try_from(value.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn saved_profiles_with(bus: &Connection) -> Vec<SavedProfile> {
    let Ok(proxy) = Proxy::new(bus, NM_DEST, SETTINGS_PATH, SETTINGS_IFACE) else {
        return Vec::new();
    };
    let Ok(paths) = proxy.call::<_, _, Vec<OwnedObjectPath>>("ListConnections", &()) else {
        return Vec::new();
    };

    paths
        .into_iter()
        .filter_map(|path| {
            let proxy = Proxy::new(bus, NM_DEST, path.as_str(), SETTINGS_CONNECTION_IFACE).ok()?;
            let settings: HashMap<String, HashMap<String, OwnedValue>> =
                proxy.call("GetSettings", &()).ok()?;
            let connection = settings.get("connection")?;
            let (service_type, vpn_data) = match settings.get("vpn") {
                Some(vpn) => (
                    string_value(vpn, "service-type").unwrap_or_default(),
                    string_map(vpn, "data"),
                ),
                None => (String::new(), HashMap::new()),
            };
            Some(SavedProfile {
                path: path.to_string(),
                id: string_value(connection, "id")?,
                uuid: string_value(connection, "uuid")?,
                kind: string_value(connection, "type")?,
                service_type,
                vpn_data,
                autoconnect: connection
                    .get("autoconnect")
                    .and_then(|v| bool::try_from(v).ok())
                    .unwrap_or(true),
            })
        })
        .collect()
}

/// Every saved `ProtonVPN <server>` profile, in one walk of the bus.
///
/// One call for the whole list rather than one [`find_proton_profile`] per
/// candidate: the connect hot path wants to know which of several proven
/// servers have a profile worth activating, and each walk is a `GetSettings`
/// per connection on the machine.
pub fn saved_proton_profiles() -> Vec<SavedProfile> {
    let Some(bus) = system_bus() else {
        return Vec::new();
    };
    saved_profiles_with(&bus)
        .into_iter()
        .filter(|profile| {
            matches!(profile.kind.as_str(), "vpn" | "wireguard") && profile.server().is_some()
        })
        .collect()
}

fn profile_matches_server(profile: &SavedProfile, server: &str) -> bool {
    let plain = format!("ProtonVPN {server}");
    profile.id.eq_ignore_ascii_case(&plain)
        || profile
            .id
            .eq_ignore_ascii_case(&format!("{plain} (verified)"))
}

pub fn find_proton_profile(server: &str) -> Option<SavedProfile> {
    let bus = system_bus()?;
    saved_profiles_with(&bus).into_iter().find(|profile| {
        matches!(profile.kind.as_str(), "vpn" | "wireguard")
            && profile_matches_server(profile, server)
    })
}

fn active_profiles_with(bus: &Connection) -> Vec<ActiveProfile> {
    let Ok(proxy) = Proxy::new(bus, NM_DEST, NM_PATH, NM_IFACE) else {
        return Vec::new();
    };
    let Ok(paths) = proxy.get_property::<Vec<OwnedObjectPath>>("ActiveConnections") else {
        return Vec::new();
    };
    paths
        .into_iter()
        .filter_map(|path| {
            let proxy = Proxy::new(bus, NM_DEST, path.as_str(), ACTIVE_IFACE).ok()?;
            let kind = proxy.get_property::<String>("Type").ok()?;
            if !matches!(kind.as_str(), "vpn" | "wireguard") {
                return None;
            }
            Some(ActiveProfile {
                path: path.to_string(),
                id: proxy.get_property::<String>("Id").ok()?,
            })
        })
        .collect()
}

pub fn active_proton_profile() -> Option<ActiveProfile> {
    active_proton_profile_checked().ok().flatten()
}

const ACCESS_POINT_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";

/// The SSID of the active wifi connection, asked of NetworkManager over
/// the bus: 5ms against `nmcli dev wifi`'s 50, and every command resolves
/// the network key before it does anything else. `Err` means the bus could
/// not answer at all, so the caller can fall back to `nmcli`; `Ok(None)`
/// means there is no active wifi.
pub fn active_wifi_ssid_checked() -> Result<Option<String>, ()> {
    let bus = system_bus().ok_or(())?;
    let proxy = Proxy::new(&bus, NM_DEST, NM_PATH, NM_IFACE).map_err(|_| ())?;
    let paths = proxy
        .get_property::<Vec<OwnedObjectPath>>("ActiveConnections")
        .map_err(|_| ())?;
    for path in paths {
        let Ok(active) = Proxy::new(&bus, NM_DEST, path.as_str(), ACTIVE_IFACE) else {
            continue;
        };
        if active.get_property::<String>("Type").ok().as_deref() != Some("802-11-wireless") {
            continue;
        }
        let Ok(ap_path) = active.get_property::<OwnedObjectPath>("SpecificObject") else {
            continue;
        };
        if ap_path.as_str() == "/" {
            continue;
        }
        let Ok(ap) = Proxy::new(&bus, NM_DEST, ap_path.as_str(), ACCESS_POINT_IFACE) else {
            continue;
        };
        if let Ok(ssid) = ap.get_property::<Vec<u8>>("Ssid") {
            let ssid = String::from_utf8_lossy(&ssid).to_string();
            if !ssid.is_empty() {
                return Ok(Some(ssid));
            }
        }
    }
    Ok(None)
}

/// [`active_proton_profile`], telling "the bus could not be reached" apart
/// from "nothing is active" — so a caller with an `nmcli` fallback spends
/// it only on the first.
pub fn active_proton_profile_checked() -> Result<Option<ActiveProfile>, ()> {
    let bus = system_bus().ok_or(())?;
    Ok(active_profiles_with(&bus)
        .into_iter()
        .find(|profile| profile.id.starts_with("ProtonVPN ")))
}

/// The active Proton server, over D-Bus with `nmcli` as the fallback only
/// when the bus itself is unavailable.
pub fn active_proton_server_or_nmcli() -> Option<String> {
    match active_proton_profile_checked() {
        Ok(profile) => profile.and_then(|p| {
            let server = p.id.strip_prefix("ProtonVPN ")?;
            Some(
                server
                    .strip_suffix(" (verified)")
                    .unwrap_or(server)
                    .to_string(),
            )
        }),
        Err(()) => crate::proc::active_proton_server(),
    }
}

pub fn active_proton_server() -> Option<String> {
    let id = active_proton_profile()?.id;
    let server = id.strip_prefix("ProtonVPN ")?;
    Some(
        server
            .strip_suffix(" (verified)")
            .unwrap_or(server)
            .to_string(),
    )
}

pub fn activate(profile: &SavedProfile) -> anyhow::Result<String> {
    let bus = Connection::system()?;
    let proxy = Proxy::new(&bus, NM_DEST, NM_PATH, NM_IFACE)?;
    let profile_path = OwnedObjectPath::try_from(profile.path.as_str())?;
    let root = OwnedObjectPath::try_from("/")?;
    let active: OwnedObjectPath =
        proxy.call("ActivateConnection", &(&profile_path, &root, &root))?;
    Ok(active.to_string())
}

pub fn deactivate(active_path: &str) -> anyhow::Result<()> {
    let bus = Connection::system()?;
    let proxy = Proxy::new(&bus, NM_DEST, NM_PATH, NM_IFACE)?;
    let path = OwnedObjectPath::try_from(active_path)?;
    proxy.call::<_, _, ()>("DeactivateConnection", &(&path,))?;
    Ok(())
}

/// How an activation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Activated,
    /// The plugin asked for the profile's secrets: the WireGuard key is not
    /// stored in it. Nothing will supply one — GNOME Shell's agent has no
    /// dialog for protun and never answers — so waiting is pointless.
    NeedsSecrets,
    Failed,
}

/// `NM_VPN_CONNECTION_STATE_NEED_AUTH`.
const NM_VPN_CONNECTION_STATE_NEED_AUTH: u32 = 2;
const VPN_CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.VPN.Connection";

/// Wait for an activation to settle, reading NetworkManager's state for it.
///
/// A profile without its key used to cost the whole connect timeout here.
/// Measured on `wifi:detnsw`, 2026-10-14: three saved profiles, each one
/// logging `Secrets are needed for this connection` within 3ms of
/// activation and then waiting thirty seconds for a secret agent that never
/// replies — ninety seconds before Proton's client was even started. The
/// VPN state says `NEED_AUTH` the moment the plugin asks, so that is a
/// verdict in milliseconds rather than a timeout.
pub fn await_activation(active_path: &str, timeout: Duration) -> Activation {
    let Some(bus) = system_bus() else {
        return Activation::Failed;
    };
    let Ok(proxy) = Proxy::new(&bus, NM_DEST, active_path, ACTIVE_IFACE) else {
        return Activation::Failed;
    };
    let vpn = Proxy::new(&bus, NM_DEST, active_path, VPN_CONNECTION_IFACE).ok();
    let deadline = Instant::now() + timeout;
    loop {
        match proxy.get_property::<u32>("State") {
            Ok(NM_ACTIVE_CONNECTION_STATE_ACTIVATED) => return Activation::Activated,
            Ok(NM_ACTIVE_CONNECTION_STATE_DEACTIVATED) | Err(_) => return Activation::Failed,
            _ => {}
        }
        if vpn
            .as_ref()
            .and_then(|vpn| vpn.get_property::<u32>("VpnState").ok())
            == Some(NM_VPN_CONNECTION_STATE_NEED_AUTH)
        {
            return Activation::NeedsSecrets;
        }
        if Instant::now() >= deadline {
            return Activation::Failed;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A profile's settings as NetworkManager returns them, secrets excluded.
pub type Settings = HashMap<String, HashMap<String, OwnedValue>>;

/// `GetSettings` for one saved profile. Never asks for secrets: on a protun
/// profile `GetSecrets` waits on GNOME Shell's agent, which does not answer.
pub fn settings_of(profile_path: &str) -> Option<Settings> {
    let bus = system_bus()?;
    let proxy = Proxy::new(&bus, NM_DEST, profile_path, SETTINGS_CONNECTION_IFACE).ok()?;
    proxy.call("GetSettings", &()).ok()
}

/// The active Proton profile, read while it is still there to read.
pub struct LiveProfile {
    pub server: String,
    pub uuid: String,
    /// The active connection, for `DeactivateConnection`.
    pub active_path: String,
    /// The profile behind it, for `Update2` and `Delete`.
    pub profile_path: String,
    /// Proton's client builds an in-memory profile per connect and deletes
    /// it on disconnect; a saved one this tool activated is on disk already.
    pub transient: bool,
    pub settings: Settings,
}

pub fn active_proton_live() -> Option<LiveProfile> {
    let bus = system_bus()?;
    let active = active_profiles_with(&bus)
        .into_iter()
        .find(|profile| profile.id.starts_with("ProtonVPN "))?;
    let proxy = Proxy::new(&bus, NM_DEST, active.path.as_str(), ACTIVE_IFACE).ok()?;
    let profile_path = proxy.get_property::<OwnedObjectPath>("Connection").ok()?;
    let uuid = proxy.get_property::<String>("Uuid").ok()?;
    let saved = Proxy::new(&bus, NM_DEST, profile_path.as_str(), SETTINGS_CONNECTION_IFACE).ok()?;
    let transient = saved.get_property::<bool>("Unsaved").unwrap_or(true)
        || saved
            .get_property::<String>("Filename")
            .is_ok_and(|f| f.starts_with("/run/"));
    let server = active.id.strip_prefix("ProtonVPN ")?;
    Some(LiveProfile {
        server: server.strip_suffix(" (verified)").unwrap_or(server).to_string(),
        uuid,
        active_path: active.path.clone(),
        profile_path: profile_path.to_string(),
        transient,
        settings: settings_of(profile_path.as_str())?,
    })
}

/// The settings of the active Proton profile, with the server it is for.
pub fn active_proton_settings() -> Option<(String, Settings)> {
    let live = active_proton_live()?;
    Some((live.server, live.settings))
}

/// Save a new profile to disk, secrets included. Returns its object path.
pub fn add_connection(settings: &Settings) -> anyhow::Result<String> {
    let bus = Connection::system()?;
    let proxy = Proxy::new(&bus, NM_DEST, SETTINGS_PATH, SETTINGS_IFACE)?;
    let path: OwnedObjectPath = proxy.call("AddConnection", &(settings,))?;
    Ok(path.to_string())
}

/// Replace a saved profile's settings, secrets included.
pub fn update_connection(profile_path: &str, settings: &Settings) -> anyhow::Result<()> {
    let bus = Connection::system()?;
    let proxy = Proxy::new(&bus, NM_DEST, profile_path, SETTINGS_CONNECTION_IFACE)?;
    proxy.call::<_, _, ()>("Update", &(settings,))?;
    Ok(())
}

/// `NM_SETTINGS_UPDATE2_FLAG_TO_DISK`.
const UPDATE2_TO_DISK: u32 = 0x1;

/// Replace a profile's settings and move it to disk, keeping the secrets
/// NetworkManager already holds for it — an in-memory profile becomes a
/// saved one without its key ever leaving NetworkManager.
pub fn persist_connection(profile_path: &str, settings: &Settings) -> anyhow::Result<()> {
    let bus = Connection::system()?;
    let proxy = Proxy::new(&bus, NM_DEST, profile_path, SETTINGS_CONNECTION_IFACE)?;
    let args: HashMap<String, OwnedValue> = HashMap::new();
    proxy.call::<_, _, HashMap<String, OwnedValue>>("Update2", &(settings, UPDATE2_TO_DISK, args))?;
    Ok(())
}

/// Replace a profile's settings, keeping the secrets NetworkManager holds
/// for it — what `nmcli connection modify` does. Unlike [`update_connection`]
/// the settings need not carry the key.
pub fn update_keeping_secrets(profile_path: &str, settings: &Settings) -> anyhow::Result<()> {
    let bus = Connection::system()?;
    let proxy = Proxy::new(&bus, NM_DEST, profile_path, SETTINGS_CONNECTION_IFACE)?;
    let args: HashMap<String, OwnedValue> = HashMap::new();
    proxy.call::<_, _, HashMap<String, OwnedValue>>("Update2", &(settings, 0u32, args))?;
    Ok(())
}

/// Every Proton kill-switch profile Proton's client left (`pvpn-killswitch`,
/// `pvpn-killswitch-ipv6`), by object path.
pub fn killswitch_profiles() -> Vec<String> {
    let Some(bus) = system_bus() else {
        return Vec::new();
    };
    saved_profiles_with(&bus)
        .into_iter()
        .filter(|p| p.id.starts_with("pvpn-killswitch") || p.id.contains("pvpnks"))
        .map(|p| p.path)
        .collect()
}

/// Is any Proton profile still active?
pub fn proton_profile_active() -> Option<bool> {
    active_proton_profile_checked().ok().map(|p| p.is_some())
}

pub fn delete_connection(profile_path: &str) -> anyhow::Result<()> {
    let bus = Connection::system()?;
    let proxy = Proxy::new(&bus, NM_DEST, profile_path, SETTINGS_CONNECTION_IFACE)?;
    proxy.call::<_, _, ()>("Delete", &())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, service_type: &str, data: &[(&str, &str)]) -> SavedProfile {
        SavedProfile {
            path: "/profile".into(),
            id: id.into(),
            uuid: "uuid".into(),
            kind: "vpn".into(),
            service_type: service_type.into(),
            vpn_data: data
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            autoconnect: false,
        }
    }

    #[test]
    fn profile_matching_is_exact_and_case_insensitive() {
        let profile = profile("ProtonVPN SG-FREE#2", "", &[]);
        assert!(profile_matches_server(&profile, "sg-free#2"));
        assert!(!profile_matches_server(&profile, "SG-FREE#21"));
    }

    #[test]
    fn a_saved_protun_profile_says_which_transport_it_is() {
        // The exact `vpn.data` of `ProtonVPN JP-FREE#33` on 2026-09-21, which
        // the fast path activated over D-Bus and carried in under a second.
        let profile = profile(
            "ProtonVPN JP-FREE#33",
            "org.freedesktop.NetworkManager.protun",
            &[
                ("private-key-flags", "1"),
                (
                    "settings",
                    r#"{"version": 1, "peers": [{"id": "JP-FREE#33", "endpoint": "149.88.103.161", "public-key": "qhEO97nKps2D1JsZjw3AiSuVJVbrBROV3Gpvong0hgI=", "udp-ports": [], "tcp-ports": [443], "tls-ports": [], "priority": 0}], "pcap-file": null}"#,
                ),
            ],
        );
        assert_eq!(profile.protocol().as_deref(), Some("protun-tcp"));
        assert_eq!(profile.server().as_deref(), Some("JP-FREE#33"));
    }

    #[test]
    fn a_preserve_scratch_profile_is_not_a_server() {
        assert_eq!(
            profile("ProtonVPN pvpn-preserve-SG-FREE#2", "", &[]).server(),
            None
        );
        assert_eq!(profile("detnsw 1", "", &[]).server(), None);
    }
}
