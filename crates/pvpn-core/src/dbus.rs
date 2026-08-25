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
            Some(SavedProfile {
                path: path.to_string(),
                id: string_value(connection, "id")?,
                uuid: string_value(connection, "uuid")?,
                kind: string_value(connection, "type")?,
            })
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
    let bus = system_bus()?;
    active_profiles_with(&bus)
        .into_iter()
        .find(|profile| profile.id.starts_with("ProtonVPN "))
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

pub fn await_activation(active_path: &str, timeout: Duration) -> bool {
    let Some(bus) = system_bus() else {
        return false;
    };
    let Ok(proxy) = Proxy::new(&bus, NM_DEST, active_path, ACTIVE_IFACE) else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match proxy.get_property::<u32>("State") {
            Ok(NM_ACTIVE_CONNECTION_STATE_ACTIVATED) => return true,
            Ok(NM_ACTIVE_CONNECTION_STATE_DEACTIVATED) | Err(_) => return false,
            _ if Instant::now() >= deadline => return false,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_matching_is_exact_and_case_insensitive() {
        let profile = SavedProfile {
            path: "/profile".into(),
            id: "ProtonVPN SG-FREE#2".into(),
            uuid: "uuid".into(),
            kind: "vpn".into(),
        };
        assert!(profile_matches_server(&profile, "sg-free#2"));
        assert!(!profile_matches_server(&profile, "SG-FREE#21"));
    }
}
