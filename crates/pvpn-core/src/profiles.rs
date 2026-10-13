//! Proton NetworkManager profiles this tool writes itself.
//!
//! The connect hot path activates a saved profile over D-Bus and never
//! starts Proton's client. That only works if the profile still holds the
//! WireGuard private key, and on 2026-10-14 none of them did. Proton's
//! client now stores the key in NetworkManager (`private-key-flags = 0`) on
//! its *transient* profile, and every copy made of it came out without the
//! key — NetworkManager does not hand secrets out with `GetSettings`, and
//! `GetSecrets` on a protun profile waits on GNOME Shell's secret agent,
//! which has no protun support and never answers. Activating such a copy
//! logs `Secrets are needed for this connection` and then waits out the
//! whole connect timeout: three proven servers, ninety seconds, before
//! Proton's client was even started.
//!
//! So the key is taken from where Proton itself takes it — the session in
//! the keyring — and written into the profile along with everything else.
//! It goes from Python's stdout into the D-Bus call and nowhere else: it is
//! never logged and never written to a file of ours. NetworkManager keeps
//! it where it keeps every other connection secret, in a root-only keyfile.
//!
//! The same lever builds a profile for any server in Proton's inventory,
//! using a profile that already worked on this network as the template for
//! everything but the peer. That is what lets `pvpn hop <server>` and the
//! ranked connect skip Proton's client — twelve to sixteen seconds of
//! Python start, a doomed API call, and its own retries — and activate in
//! one D-Bus call.

use crate::dbus::{self, SavedProfile, Settings};
use serde_json::Value as Json;
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::{OwnedValue, Value};

/// Reading the keyring is local and takes about 300ms; past this the secret
/// service is not answering.
const KEY_TIMEOUT: Duration = Duration::from_secs(10);

/// Prints the session's WireGuard private key and nothing else.
///
/// Walks the accounts exactly as `cert::CERT_STATUS_PY` does, and derives
/// the key with Proton's own `VPNSecrets` — the same object Proton's backend
/// reads `wg_private_key` from when it builds a profile.
const KEY_PY: &str = r#"
import base64, sys
from proton.loader import Loader
from proton.vpn.session.credentials import VPNSecrets

keyring = Loader.get("keyring")()
for account in keyring["proton-sso-accounts"]:
    name = base64.b32encode(account.encode("utf8")).decode("ascii").rstrip("=").lower()
    try:
        data = keyring[f"proton-sso-account-{name}"]
    except Exception:
        continue
    secrets = ((data or {}).get("vpn") or {}).get("secrets") or {}
    if secrets.get("ed25519_privatekey"):
        sys.stdout.write(VPNSecrets(secrets["ed25519_privatekey"]).wireguard_privatekey)
        sys.exit(0)
sys.exit(2)
"#;

/// The session's WireGuard private key, or `None` when the keyring cannot
/// be read (locked, signed out, no secret service).
fn wireguard_key() -> Option<String> {
    let result = crate::proc::run_with_timeout(
        crate::paths::system_python(),
        &["-c", KEY_PY],
        &[],
        KEY_TIMEOUT,
    )
    .ok()?;
    if !result.success {
        tracing::debug!("could not read the WireGuard key from the keyring");
        return None;
    }
    let key = result.stdout.trim().to_string();
    // A curve25519 key is 32 bytes: 44 characters of padded base64.
    (key.len() == 44 && key.ends_with('=')).then_some(key)
}

/// One server's endpoint, as Proton's inventory lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub server: String,
    pub endpoint: String,
    pub public_key: String,
}

/// The endpoint Proton would pick for each of `servers`: an enabled physical
/// server with nothing down, else any that lists an entry IP and key.
pub fn peers_from_inventory(inventory: &Json, servers: &[String]) -> HashMap<String, Peer> {
    let mut out = HashMap::new();
    let Some(logicals) = inventory.get("LogicalServers").and_then(Json::as_array) else {
        return out;
    };
    for logical in logicals {
        let Some(name) = logical.get("Name").and_then(Json::as_str) else {
            continue;
        };
        let Some(wanted) = servers.iter().find(|s| s.eq_ignore_ascii_case(name)) else {
            continue;
        };
        let physicals = logical
            .get("Servers")
            .and_then(Json::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let usable = |p: &&Json| {
            p.get("EntryIP").and_then(Json::as_str).is_some()
                && p.get("X25519PublicKey").and_then(Json::as_str).is_some()
        };
        let healthy = |p: &&Json| {
            p.get("Status").and_then(Json::as_i64) == Some(1)
                && p.get("ServicesDown").and_then(Json::as_i64).unwrap_or(0) == 0
        };
        let Some(physical) = physicals
            .iter()
            .filter(usable)
            .find(healthy)
            .or_else(|| physicals.iter().find(usable))
        else {
            continue;
        };
        out.insert(
            wanted.clone(),
            Peer {
                server: name.to_string(),
                endpoint: physical["EntryIP"].as_str().unwrap_or_default().to_string(),
                public_key: physical["X25519PublicKey"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            },
        );
    }
    out
}

fn load_inventory() -> Option<Json> {
    let text = std::fs::read_to_string(crate::paths::serverlist_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(value.into()).expect("a value without file descriptors")
}

fn string_map(section: &HashMap<String, OwnedValue>, key: &str) -> HashMap<String, String> {
    section
        .get(key)
        .and_then(|value| <HashMap<String, String>>::try_from(value.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn clone_settings(settings: &Settings) -> Option<Settings> {
    settings
        .iter()
        .map(|(name, section)| {
            let copied: Option<HashMap<String, OwnedValue>> = section
                .iter()
                .map(|(k, v)| Some((k.clone(), v.try_clone().ok()?)))
                .collect();
            Some((name.clone(), copied?))
        })
        .collect()
}

/// The protocol a profile's settings run, by the same rules as
/// [`SavedProfile::protocol`].
pub fn protocol_of(settings: &Settings) -> Option<String> {
    let vpn = settings.get("vpn")?;
    let service_type = <&str>::try_from(vpn.get("service-type")?).ok()?.to_string();
    crate::proc::protocol_from_profile(&service_type, &string_map(vpn, "data"))
}

fn new_uuid() -> Option<String> {
    let uuid = std::fs::read_to_string("/proc/sys/kernel/random/uuid").ok()?;
    Some(uuid.trim().to_string())
}

/// The port lists that make the protun plugin run `protocol`, or `None` for
/// a transport this does not know how to ask for.
///
/// The transport is only the ports: the same server, key and profile run
/// protun-tcp with `tcp-ports: [443]` and protun-tls with `tls-ports:
/// [443]`. Measured 2026-10-14 on `wifi:detnsw`: protun-tls to JP-FREE#11
/// took 14–58s to carry — the plugin sends a random SNI per attempt, the
/// network's filter rejects most of them, and each rejection costs a
/// three-second retry — while protun-tcp to the same server carried in
/// 0.97–1.05s, three times out of three.
fn transport_ports(protocol: &str) -> Option<[(&'static str, Vec<u16>); 3]> {
    let (tcp, tls) = match protocol {
        "protun-tcp" => (vec![443], vec![]),
        "protun-tls" => (vec![], vec![443]),
        _ => return None,
    };
    Some([("udp-ports", vec![]), ("tcp-ports", tcp), ("tls-ports", tls)])
}

/// Proton's settings JSON with its peer swapped for `peer` and its transport
/// set to `protocol`, escaped the way Proton writes it. Whatever is not
/// named stays the template's.
fn retarget_settings_json(template: &str, peer: Option<&Peer>, protocol: Option<&str>) -> Option<String> {
    let mut value: Json = serde_json::from_str(&template.trim().replace("\\,", ",")).ok()?;
    let first = value.get_mut("peers")?.as_array_mut()?.first_mut()?;
    if let Some(peer) = peer {
        first["id"] = Json::from(peer.server.as_str());
        first["endpoint"] = Json::from(peer.endpoint.as_str());
        first["public-key"] = Json::from(peer.public_key.as_str());
    }
    if let Some(ports) = protocol.and_then(transport_ports) {
        for (key, list) in ports {
            first[key] = Json::from(list);
        }
    }
    if let Some(peers) = value.get_mut("peers").and_then(Json::as_array_mut) {
        peers.truncate(1);
    }
    Some(serde_json::to_string(&value).ok()?.replace(',', "\\,"))
}

/// A saved, manual-toggle copy of `template` for `server`, carrying `key`.
///
/// With `peer` the profile is retargeted at that endpoint; without one it
/// keeps the template's own peer (preserving a live tunnel's profile).
pub fn build(
    template: &Settings,
    server: &str,
    peer: Option<&Peer>,
    protocol: Option<&str>,
    key: &str,
) -> Option<Settings> {
    let mut settings = clone_settings(template)?;

    let connection = settings.get_mut("connection")?;
    connection.insert("id".into(), owned(format!("ProtonVPN {server}")));
    connection.insert("uuid".into(), owned(new_uuid()?));
    connection.insert("autoconnect".into(), owned(false));
    connection.remove("timestamp");

    let vpn = settings.get_mut("vpn")?;
    let mut data = string_map(vpn, "data");
    if peer.is_some() || protocol.is_some() {
        let retargeted = retarget_settings_json(data.get("settings")?, peer, protocol)?;
        data.insert("settings".into(), retargeted);
    }
    data.insert("private-key-flags".into(), "0".into());
    vpn.insert("data".into(), owned(data));
    let secrets: HashMap<String, String> =
        HashMap::from([("private-key".to_string(), key.to_string())]);
    vpn.insert("secrets".into(), owned(secrets));

    // The deprecated forms ride along in `GetSettings` beside their
    // replacements; sending both back invites a disagreement between them.
    for family in ["ipv4", "ipv6"] {
        if let Some(section) = settings.get_mut(family) {
            for deprecated in ["addresses", "routes", "dns"] {
                section.remove(deprecated);
            }
        }
    }
    Some(settings)
}

/// A profile that has run `protocol` here to copy everything but the peer
/// from: the live tunnel's first, then any saved one.
fn template_for(protocol: Option<&str>) -> Option<Settings> {
    // Any protun profile will do for a transport set by its ports alone.
    let settable = protocol.is_some_and(|p| transport_ports(p).is_some());
    let fits = |settings: &Settings| match (protocol, protocol_of(settings)) {
        (Some(want), Some(have)) => have == want || (settable && have.starts_with("protun-")),
        (None, Some(_)) => true,
        (_, None) => false,
    };
    if let Some((_, live)) = dbus::active_proton_settings() {
        if fits(&live) {
            return Some(live);
        }
    }
    dbus::saved_proton_profiles()
        .into_iter()
        .filter_map(|profile| dbus::settings_of(&profile.path))
        .find(|settings| fits(settings))
}

fn has_profile(profiles: &[SavedProfile], server: &str) -> bool {
    profiles
        .iter()
        .any(|p| p.server().is_some_and(|s| s.eq_ignore_ascii_case(server)))
}

/// Make sure each of `servers` has a saved profile running `protocol`,
/// building the missing ones from Proton's inventory. Returns every saved
/// Proton profile afterwards.
///
/// Builds nothing — and costs nothing beyond one walk of the bus — when
/// every server already has one. A server Proton's inventory does not list,
/// or no template for the protocol, or no key, leaves the gap for Proton's
/// client to fill as before.
pub fn ensure_profiles(servers: &[String], protocol: Option<&str>) -> Vec<SavedProfile> {
    let profiles = dbus::saved_proton_profiles();
    let missing: Vec<String> = servers
        .iter()
        .filter(|server| !has_profile(&profiles, server))
        .cloned()
        .collect();
    if missing.is_empty() {
        return profiles;
    }
    let Some(template) = template_for(protocol) else {
        tracing::debug!("no profile to build {} from", missing.join(", "));
        return profiles;
    };
    let Some(inventory) = load_inventory() else {
        return profiles;
    };
    let peers = peers_from_inventory(&inventory, &missing);
    if peers.is_empty() {
        return profiles;
    }
    let Some(key) = wireguard_key() else {
        tracing::debug!("the keyring did not give up the WireGuard key; leaving profiles to Proton");
        return profiles;
    };
    let mut built = 0;
    for server in &missing {
        let Some(peer) = peers.get(server) else {
            continue;
        };
        let Some(settings) = build(&template, &peer.server, Some(peer), protocol, &key) else {
            continue;
        };
        match dbus::add_connection(&settings) {
            Ok(_) => built += 1,
            Err(err) => tracing::debug!("NetworkManager would not save {server}'s profile: {err}"),
        }
    }
    if built == 0 {
        return profiles;
    }
    tracing::debug!("built {built} profile(s) from Proton's inventory");
    dbus::saved_proton_profiles()
}

/// `profile`, made to run `protocol` before it is activated.
///
/// The transport is a property of the network, not of the server: a profile
/// saved from a protun-tls connect is the right profile for protun-tcp once
/// that is what works here. Rewriting its ports is one `Update2` that keeps
/// the key NetworkManager already holds. Returns the profile unchanged when
/// it already fits, or when it cannot be rewritten (not protun, unknown
/// transport) — the caller activates it as it is.
pub fn align_transport(profile: &SavedProfile, protocol: &str) -> SavedProfile {
    let have = profile.protocol();
    if have.as_deref() == Some(protocol)
        || !have.as_deref().is_some_and(|p| p.starts_with("protun-"))
        || transport_ports(protocol).is_none()
    {
        return profile.clone();
    }
    let Some(mut settings) = dbus::settings_of(&profile.path) else {
        return profile.clone();
    };
    let Some(vpn) = settings.get_mut("vpn") else {
        return profile.clone();
    };
    let mut data = string_map(vpn, "data");
    let Some(retargeted) = data
        .get("settings")
        .and_then(|json| retarget_settings_json(json, None, Some(protocol)))
    else {
        return profile.clone();
    };
    data.insert("settings".into(), retargeted);
    vpn.insert("data".into(), owned(data.clone()));
    if dbus::update_keeping_secrets(&profile.path, &settings).is_err() {
        return profile.clone();
    }
    tracing::debug!(
        "{} now runs {protocol} (was {})",
        profile.id,
        have.as_deref().unwrap_or("?")
    );
    SavedProfile {
        vpn_data: data,
        ..profile.clone()
    }
}

/// Write the session's key into every saved Proton profile. Returns how many
/// were updated, or `None` when the key could not be read.
///
/// Every profile carries the same key — it is the account session's, not
/// the server's — so one keyring read repairs them all, including profiles
/// saved by older builds whose key went missing, and profiles still holding
/// the key of a session since replaced by a new sign-in.
pub fn rekey_all() -> Option<usize> {
    let key = wireguard_key()?;
    let mut updated = 0;
    for profile in dbus::saved_proton_profiles() {
        if !profile.service_type.ends_with(".protun") {
            continue;
        }
        let Some(current) = dbus::settings_of(&profile.path) else {
            continue;
        };
        let Some(server) = profile.server() else {
            continue;
        };
        let Some(mut settings) = build(&current, &server, None, None, &key) else {
            continue;
        };
        // Same profile, same identity: only the key and its flag change.
        if let Some(connection) = settings.get_mut("connection") {
            connection.insert("uuid".into(), owned(profile.uuid.clone()));
            connection.insert("id".into(), owned(profile.id.clone()));
        }
        if dbus::update_connection(&profile.path, &settings).is_ok() {
            updated += 1;
        }
    }
    Some(updated)
}

/// Save the live tunnel's profile as `ProtonVPN <server>`, key included.
/// `live` is its settings read before teardown, while it still existed.
pub fn save_live(server: &str, live: &Settings) -> anyhow::Result<()> {
    let key = wireguard_key()
        .ok_or_else(|| anyhow::anyhow!("the keyring did not give up the WireGuard key"))?;
    let settings = build(live, server, None, None, &key)
        .ok_or_else(|| anyhow::anyhow!("{server}'s profile could not be copied"))?;
    dbus::add_connection(&settings)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> Json {
        serde_json::json!({
            "LogicalServers": [
                {"Name": "SG-FREE#2", "Servers": [
                    {"EntryIP": "10.0.0.9", "X25519PublicKey": "down=", "Status": 0, "ServicesDown": 0},
                    {"EntryIP": "149.50.211.129", "X25519PublicKey": "mYGJ=", "Status": 1, "ServicesDown": 0}
                ]},
                {"Name": "JP-FREE#11", "Servers": [
                    {"EntryIP": "37.19.205.145", "X25519PublicKey": "59gf=", "Status": 0, "ServicesDown": 1}
                ]},
                {"Name": "NL-FREE#1", "Servers": []}
            ]
        })
    }

    #[test]
    fn a_peer_is_the_healthy_endpoint_and_falls_back_to_any_listed_one() {
        let wanted = ["sg-free#2".to_string(), "JP-FREE#11".to_string(), "NL-FREE#1".to_string()];
        let peers = peers_from_inventory(&inventory(), &wanted);
        let sg = &peers["sg-free#2"];
        assert_eq!(sg.server, "SG-FREE#2", "named as Proton names it");
        assert_eq!(sg.endpoint, "149.50.211.129");
        assert_eq!(sg.public_key, "mYGJ=");
        assert_eq!(peers["JP-FREE#11"].endpoint, "37.19.205.145");
        assert!(!peers.contains_key("NL-FREE#1"), "nothing to connect to");
    }

    #[test]
    fn retargeting_swaps_only_the_peer_and_keeps_protons_escaping() {
        let template = r#"{"version": 1\, "peers": [{"id": "JP-FREE#11"\, "endpoint": "37.19.205.145"\, "public-key": "59gf="\, "udp-ports": []\, "tcp-ports": []\, "tls-ports": [443]\, "priority": 0}]\, "pcap-file": null}"#;
        let peer = Peer {
            server: "SG-FREE#2".into(),
            endpoint: "149.50.211.129".into(),
            public_key: "mYGJ=".into(),
        };
        let out = retarget_settings_json(template, Some(&peer), None).unwrap();
        assert_eq!(
            out.matches(',').count(),
            out.matches("\\,").count(),
            "every comma escaped: {out}"
        );
        let parsed: Json = serde_json::from_str(&out.replace("\\,", ",")).unwrap();
        let first = &parsed["peers"][0];
        assert_eq!(first["id"], "SG-FREE#2");
        assert_eq!(first["endpoint"], "149.50.211.129");
        assert_eq!(first["public-key"], "mYGJ=");
        assert_eq!(first["tls-ports"], serde_json::json!([443]));
        assert_eq!(parsed["version"], 1);
        let data = HashMap::from([("settings".to_string(), out)]);
        assert_eq!(
            crate::proc::protocol_from_profile("org.freedesktop.NetworkManager.protun", &data)
                .as_deref(),
            Some("protun-tls"),
            "still reads as the template's transport"
        );
    }

    #[test]
    fn the_transport_is_only_the_ports() {
        let tls = r#"{"version": 1\, "peers": [{"id": "JP-FREE#11"\, "endpoint": "37.19.205.145"\, "public-key": "59gf="\, "udp-ports": []\, "tcp-ports": []\, "tls-ports": [443]\, "priority": 0}]\, "pcap-file": null}"#;
        let tcp = retarget_settings_json(tls, None, Some("protun-tcp")).unwrap();
        let data = HashMap::from([("settings".to_string(), tcp.clone())]);
        assert_eq!(
            crate::proc::protocol_from_profile("org.freedesktop.NetworkManager.protun", &data)
                .as_deref(),
            Some("protun-tcp")
        );
        assert!(tcp.contains("37.19.205.145"), "same server: {tcp}");
        let back = retarget_settings_json(&tcp, None, Some("protun-tls")).unwrap();
        let data = HashMap::from([("settings".to_string(), back)]);
        assert_eq!(
            crate::proc::protocol_from_profile("org.freedesktop.NetworkManager.protun", &data)
                .as_deref(),
            Some("protun-tls")
        );
        assert!(transport_ports("openvpn-udp").is_none(), "not a protun transport");
    }

    fn template() -> Settings {
        let settings = r#"{"version": 1\, "peers": [{"id": "JP-FREE#11"\, "endpoint": "37.19.205.145"\, "public-key": "59gf="\, "udp-ports": []\, "tcp-ports": []\, "tls-ports": [443]\, "priority": 0}]\, "pcap-file": null}"#;
        let data = HashMap::from([
            ("settings".to_string(), settings.to_string()),
            ("private-key-flags".to_string(), "1".to_string()),
        ]);
        Settings::from([
            (
                "connection".to_string(),
                HashMap::from([
                    ("id".to_string(), owned("ProtonVPN JP-FREE#11")),
                    ("uuid".to_string(), owned("e35b3b6d-fff5-4206-967d-46cdbe0c9ef8")),
                    ("type".to_string(), owned("vpn")),
                    ("timestamp".to_string(), owned(1791925586u64)),
                ]),
            ),
            (
                "vpn".to_string(),
                HashMap::from([
                    (
                        "service-type".to_string(),
                        owned("org.freedesktop.NetworkManager.protun"),
                    ),
                    ("data".to_string(), owned(data)),
                ]),
            ),
            (
                "ipv4".to_string(),
                HashMap::from([
                    ("method".to_string(), owned("manual")),
                    ("dns".to_string(), owned(vec![16777738u32])),
                ]),
            ),
        ])
    }

    #[test]
    fn a_built_profile_carries_the_key_and_a_new_identity() {
        let peer = Peer {
            server: "SG-FREE#2".into(),
            endpoint: "149.50.211.129".into(),
            public_key: "mYGJ=".into(),
        };
        let built = build(&template(), "SG-FREE#2", Some(&peer), None, "k".repeat(43).as_str()).unwrap();
        let connection = &built["connection"];
        assert_eq!(<&str>::try_from(&connection["id"]).unwrap(), "ProtonVPN SG-FREE#2");
        assert_ne!(
            <&str>::try_from(&connection["uuid"]).unwrap(),
            "e35b3b6d-fff5-4206-967d-46cdbe0c9ef8"
        );
        assert_eq!(bool::try_from(&connection["autoconnect"]).unwrap(), false);
        assert!(!connection.contains_key("timestamp"));

        let data = string_map(&built["vpn"], "data");
        assert_eq!(data["private-key-flags"], "0", "NetworkManager keeps the key");
        assert!(data["settings"].contains("149.50.211.129"));
        let secrets = string_map(&built["vpn"], "secrets");
        assert_eq!(secrets["private-key"].len(), 43);
        assert!(!built["ipv4"].contains_key("dns"), "deprecated form dropped");
        assert_eq!(protocol_of(&built).as_deref(), Some("protun-tls"));
    }
}
