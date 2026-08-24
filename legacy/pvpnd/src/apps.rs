//! Run the Flatpak bypass audit after a successful connect. The detection
//! and fix live in `pvpn-core::apps`; this is just the "say what happened
//! on the connect path" wrapper.

use pvpn_core::apps;

pub fn enforce_app_routing(fix: bool) {
    if !apps::flatpak_available() {
        return;
    }
    let bypassing = apps::flatpak_bypassers();
    if bypassing.is_empty() {
        return;
    }
    if !fix {
        for app in &bypassing {
            tracing::warn!("{app} is routed around the VPN by a proxy setting.");
        }
        tracing::info!("Left alone because fix_apps is off. To fix: pvpn apps --fix");
        return;
    }
    let results = apps::fix_bypassers(&bypassing);
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
