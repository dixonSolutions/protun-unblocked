//! `pvpn apps` — find Flatpak apps whose traffic skips the tunnel.
//! Runs in this process; no daemon required.

use pvpn_core::apps;
use pvpn_core::net;

pub fn cmd_apps(fix: bool, verify: bool, wanted: &[String]) -> anyhow::Result<i32> {
    if !apps::flatpak_available() {
        println!("Flatpak is not installed - nothing to check.");
        return Ok(0);
    }

    let all = apps::list_apps();
    if all.is_empty() {
        println!("No Flatpak apps installed.");
        return Ok(0);
    }

    println!(
        "Checking {} Flatpak apps for traffic that skips the tunnel...",
        all.len()
    );
    let flagged = apps::flatpak_bypassers();

    if flagged.is_empty() {
        println!(
            "All {} apps use the tunnel - no proxy overrides found.",
            all.len()
        );
        if !verify {
            return Ok(0);
        }
    }

    for app in &flagged {
        println!();
        eprintln!("{app} is routed around the VPN:");
        for line in apps::app_proxy_vars(app) {
            eprintln!("    {line}");
        }
        eprintln!("  Its exit is decided by that proxy, not by the tunnel.");
    }

    if !flagged.is_empty() && !fix {
        println!();
        println!("To put them back on the tunnel:  pvpn apps --fix");
    }

    let mut unfixed = 0;
    if fix {
        let results = apps::fix_bypassers(&flagged);
        for result in results {
            println!();
            println!("Fixing {}...", result.app);
            for var in &result.unset {
                println!("  unset {var}");
            }
            if result.still_bypassing {
                unfixed += 1;
                eprintln!("  {} still has a proxy set.", result.app);
                eprintln!(
                    "  Inspect it with:  flatpak override --user --show {}",
                    result.app
                );
            } else {
                println!("  {} now uses the tunnel.", result.app);
            }
        }
        println!();
        eprintln!("Already-running apps keep the old setting until restarted.");
        eprintln!("To undo:  flatpak override --user --env=http_proxy=socks5://127.0.0.1:9050 APP");
    }

    let mut offtunnel = 0;
    if verify {
        println!();
        println!("Verifying real exit addresses (each app has to start, so this is slow)...");
        let host_ip = net::public_ip();
        println!(
            "Host exits at: {}",
            host_ip.as_deref().unwrap_or("<unknown>")
        );
        let to_check: Vec<String> = if !wanted.is_empty() {
            wanted.to_vec()
        } else if !flagged.is_empty() {
            flagged.clone()
        } else {
            eprintln!("Nothing flagged. Name the apps to check, e.g.");
            eprintln!("  pvpn apps --verify org.xonotic.Xonotic com.rtosta.zapzap");
            return Ok(0);
        };
        let timeout: u64 = std::env::var("PVPN_APPS_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        for app in to_check {
            match apps::sandbox_ip(&app, timeout) {
                None => eprintln!(
                    "{app}: could not tell (no network, or no curl/python3 in its runtime)"
                ),
                Some(ip) => match &host_ip {
                    None => eprintln!("{app}: {ip} - host IP unknown, cannot verify on/off tunnel"),
                    Some(host) if host == &ip => {
                        println!("{app}: {ip} - same as host, on the tunnel")
                    }
                    Some(_) => {
                        offtunnel += 1;
                        eprintln!("{app}: {ip} - NOT the host's exit");
                    }
                },
            }
        }
    }

    if !flagged.is_empty() && !fix {
        return Ok(1);
    }
    if unfixed > 0 || offtunnel > 0 {
        return Ok(1);
    }
    Ok(0)
}
