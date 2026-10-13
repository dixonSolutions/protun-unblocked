//! Taking a tunnel down in NetworkManager's own terms, without Proton's
//! client.
//!
//! `pvpn down` used to run `protonvpn disconnect` whenever Proton's client
//! had built the tunnel: a Python start, Proton's import graph, and — on a
//! network that blackholes the API — a location request that waits out its
//! timeout after the tunnel is already gone. Preserving the profile cost a
//! second Python start to read the key back out of the keyring. Neither is
//! needed. NetworkManager owns the tunnel; deactivating it is one call.
//! Proton's transient profile already holds the key, so saving it is one
//! `Update2` that moves it to disk in place. What Proton's client keeps for
//! itself — the kill-switch profiles and `connection_persistence.json` — is
//! removed the way its own disconnect would.
//!
//! Every step is a bus call; anything that fails returns `Err` so the caller
//! can fall back to the slow path, which still knows how to do all of it.

use crate::dbus::{self, Settings};
use std::time::{Duration, Instant};
use zbus::zvariant::{OwnedValue, Value};

/// What came down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Down {
    /// The server whose tunnel was active, if one was.
    pub server: Option<String>,
    /// Its profile was kept in Network Settings for the hot path.
    pub preserved: bool,
}

fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(value.into()).expect("a value without file descriptors")
}

/// `settings` with autoconnect off and NetworkManager's bookkeeping removed.
fn manual_toggle(settings: &Settings) -> Option<Settings> {
    let mut copy: Settings = settings
        .iter()
        .map(|(name, section)| {
            let section = section
                .iter()
                .map(|(k, v)| Some((k.clone(), v.try_clone().ok()?)))
                .collect::<Option<_>>()?;
            Some((name.clone(), section))
        })
        .collect::<Option<_>>()?;
    let connection = copy.get_mut("connection")?;
    connection.insert("autoconnect".into(), owned(false));
    connection.remove("timestamp");
    Some(copy)
}

/// Wait until no Proton profile is active. Polled: deactivation is
/// asynchronous and the plugin holds the old connection for a moment.
pub fn wait_released(patience: Duration) -> bool {
    let deadline = Instant::now() + patience;
    loop {
        match dbus::proton_profile_active() {
            Some(false) => return true,
            None => return false,
            Some(true) if Instant::now() >= deadline => return false,
            Some(true) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Take down whatever Proton tunnel is up. `preserve(server)` says whether
/// that server has earned a saved profile (it carried traffic here).
pub fn down(preserve: impl Fn(&str) -> bool) -> anyhow::Result<Down> {
    let mut out = Down::default();
    if let Some(live) = dbus::active_proton_live() {
        let keep = live.transient && preserve(&live.server);
        if keep {
            // One profile per server: drop stale saved copies first.
            for profile in dbus::saved_proton_profiles() {
                if profile.path != live.profile_path
                    && profile
                        .server()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&live.server))
                {
                    let _ = dbus::delete_connection(&profile.path);
                }
            }
            let settings = manual_toggle(&live.settings)
                .ok_or_else(|| anyhow::anyhow!("could not copy {}'s settings", live.server))?;
            dbus::persist_connection(&live.profile_path, &settings)?;
            out.preserved = true;
        }
        dbus::deactivate(&live.active_path)?;
        if !wait_released(Duration::from_secs(3)) {
            anyhow::bail!("NetworkManager is still holding {}'s tunnel", live.server);
        }
        if live.transient && !keep {
            // What Proton's own disconnect does with its profile.
            let _ = dbus::delete_connection(&live.profile_path);
        }
        if live.transient {
            // Proton's client believes in a connection until this file
            // goes; its own disconnect removes it the same way.
            let _ = std::fs::remove_file(crate::proc::proton_persistence_path());
        }
        out.server = Some(live.server);
    } else if dbus::proton_profile_active().is_none() {
        anyhow::bail!("the system bus is not answering");
    }

    for path in dbus::killswitch_profiles() {
        let _ = dbus::delete_connection(&path);
    }
    // A Proton profile left on autoconnect puts the tunnel straight back.
    for profile in dbus::saved_proton_profiles() {
        if !profile.autoconnect {
            continue;
        }
        if let Some(settings) = dbus::settings_of(&profile.path).and_then(|s| manual_toggle(&s)) {
            let _ = dbus::update_keeping_secrets(&profile.path, &settings);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn a_kept_profile_is_manual_toggle_and_otherwise_untouched() {
        let settings: Settings = HashMap::from([
            (
                "connection".to_string(),
                HashMap::from([
                    ("id".to_string(), owned("ProtonVPN SG-FREE#2")),
                    ("autoconnect".to_string(), owned(true)),
                    ("timestamp".to_string(), owned(1791925586u64)),
                ]),
            ),
            (
                "vpn".to_string(),
                HashMap::from([("service-type".to_string(), owned("x.protun"))]),
            ),
        ]);
        let kept = manual_toggle(&settings).unwrap();
        let connection = &kept["connection"];
        assert!(!bool::try_from(&connection["autoconnect"]).unwrap());
        assert!(!connection.contains_key("timestamp"));
        assert_eq!(<&str>::try_from(&connection["id"]).unwrap(), "ProtonVPN SG-FREE#2");
        assert!(kept.contains_key("vpn"));
    }
}
