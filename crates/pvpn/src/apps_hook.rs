//! Run the Flatpak bypass audit after a successful connect. The detection
//! and fix live in `pvpn-core::apps`; this is just the "say what happened
//! on the connect path" wrapper. `pvpn apps` is the same audit on demand.
//!
//! Split into finding and acting, because the finding — one `flatpak info`
//! per installed app — needs nothing from the tunnel and can run while the
//! tunnel is being built, whereas acting on it only makes sense once there
//! is a tunnel for the apps to be routed onto.

use pvpn_core::apps;

/// The apps currently routed around the tunnel. Reads only; safe to start
/// before anything is connected.
pub fn audit() -> Vec<String> {
    if !apps::flatpak_available() {
        return Vec::new();
    }
    apps::flatpak_bypassers()
}

/// Act on an audit that has already been done.
pub fn enforce_app_routing_for(bypassing: &[String], fix: bool) {
    if bypassing.is_empty() {
        return;
    }
    if !fix {
        for app in bypassing {
            tracing::warn!("{app} is routed around the VPN by a proxy setting.");
        }
        tracing::info!("Left alone because fix_apps is off. To fix: pvpn apps --fix");
        return;
    }
    let results = apps::fix_bypassers(bypassing);
    for result in results {
        tracing::warn!(
            "{} was routed around the VPN by a proxy setting — fixing.",
            result.app
        );
        for var in &result.unset {
            tracing::info!("  unset {var}");
        }
        if result.still_bypassing {
            tracing::error!("  {} is STILL routed around the VPN.", result.app);
        }
    }
    tracing::info!("Already-running apps keep the old setting until restarted.");
}
