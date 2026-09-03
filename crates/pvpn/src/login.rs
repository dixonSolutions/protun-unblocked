//! Login, logout, account, protocols — run directly in this process.
//! Proton's own CLI and the Python sign-in helpers stay exactly what
//! they are; we only wrap them.

use pvpn_core::net;
use pvpn_core::paths;
use pvpn_core::proc;
use std::io::{self, Write};
use std::time::Duration;

fn need_tor() -> anyhow::Result<()> {
    if !proc::tor_service_active() {
        anyhow::bail!("Tor is not running - Proton's API is blocked without it.\n  Start it with:  sudo systemctl start tor");
    }
    if !proc::tor_listening() {
        anyhow::bail!("Tor is running but nothing is listening on 127.0.0.1:9050.");
    }
    Ok(())
}

pub fn cmd_login(email: Option<String>, browser: bool) -> anyhow::Result<i32> {
    let _ = paths::ensure_shim()?;
    if browser {
        return cmd_login_bridge(email);
    }

    let user = match email {
        Some(u) if !u.is_empty() => u,
        _ => {
            print!("Proton account email (e.g. you@proton.me): ");
            io::stdout().flush()?;
            let mut buf = String::new();
            io::stdin().read_line(&mut buf)?;
            let trimmed = buf.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("no email given");
            }
            trimmed
        }
    };

    let api_timeout = std::env::var("PVPN_LOGIN_API_TIMEOUT").unwrap_or_else(|_| "60".to_string());
    let api_ok = net::api_reachable();
    if api_ok {
        println!("Proton API reachable — signing in directly (no Tor).");
    } else {
        need_tor()?;
        eprintln!("API blocked here — signing in via Tor (timeout {api_timeout}s).");
        eprintln!("If Proton asks for a CAPTCHA, use:  pvpn login --browser");
    }

    let shim = paths::shim_dir();
    let debug = shim.join("debug-signin.py");
    let python = paths::system_python();
    let timeout = api_timeout.clone();

    let rc = if api_ok {
        if debug.exists() {
            proc::run_interactive(
                python,
                &[debug.to_str().unwrap(), "signin", &user],
                &[
                    ("PYTHONPATH", shim.to_str().unwrap_or("")),
                    ("PVPN_DEBUG", "0"),
                    ("PVPN_API_TIMEOUT", &timeout),
                ],
            )?
            .code()
            .unwrap_or(1)
        } else {
            proc::run_interactive(
                "protonvpn",
                &["signin", &user],
                &[
                    ("PYTHONPATH", shim.to_str().unwrap_or("")),
                    ("PVPN_DEBUG", "0"),
                    ("PVPN_API_TIMEOUT", &timeout),
                ],
            )?
            .code()
            .unwrap_or(1)
        }
    } else if debug.exists() {
        proc::run_interactive(
            "torsocks",
            &[
                "env",
                &format!("PYTHONPATH={}", shim.display()),
                "PVPN_DEBUG=0",
                &format!("PVPN_API_TIMEOUT={timeout}"),
                python,
                debug.to_str().unwrap(),
                "signin",
                &user,
            ],
            &[],
        )?
        .code()
        .unwrap_or(1)
    } else {
        proc::run_interactive(
            "torsocks",
            &[
                "env",
                &format!("PYTHONPATH={}", shim.display()),
                "PVPN_DEBUG=0",
                &format!("PVPN_API_TIMEOUT={timeout}"),
                "protonvpn",
                "signin",
                &user,
            ],
            &[],
        )?
        .code()
        .unwrap_or(1)
    };

    if rc == 2 {
        eprintln!("CAPTCHA required — switching to browser sign-in bridge.");
        return cmd_login_bridge(Some(user));
    }
    Ok(rc)
}

fn cmd_login_bridge(email: Option<String>) -> anyhow::Result<i32> {
    let shim = paths::ensure_shim()?;
    let bridge = shim.join("signin-bridge.py");
    if !bridge.exists() {
        anyhow::bail!("Missing {} — re-run ./setup.sh", bridge.display());
    }

    println!("Starting browser sign-in bridge...");
    eprintln!("  1) Sign in / CAPTCHA in the browser (use a hotspot if Proton is blocked here)");
    eprintln!("  2) Paste the redirected URL (contains selector=) into the helper page");
    eprintln!("  3) This imports that session into the Proton CLI keyring");

    let api_timeout = std::env::var("PVPN_LOGIN_API_TIMEOUT").unwrap_or_else(|_| "60".to_string());
    let python = paths::system_python();
    let mut args = vec![bridge.to_string_lossy().to_string(), "--helper".to_string()];
    if let Some(user) = &email {
        args.push("--email".to_string());
        args.push(user.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (rc, stdout) = proc::capture_interactive_stdout(
        python,
        &arg_refs,
        &[
            ("PYTHONPATH", shim.to_str().unwrap_or("")),
            ("PVPN_DEBUG", "0"),
            ("PVPN_API_TIMEOUT", &api_timeout),
        ],
    )?;
    let selector = stdout
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .trim_end_matches('\r')
        .to_string();
    if rc != 0 || selector.is_empty() {
        anyhow::bail!("Bridge did not receive a sign-in link.");
    }

    println!("Importing forked session into Proton CLI...");
    let import_rc = if net::api_reachable() {
        proc::run_interactive(
            python,
            &[bridge.to_str().unwrap(), "--import-selector", &selector],
            &[
                ("PYTHONPATH", shim.to_str().unwrap_or("")),
                ("PVPN_DEBUG", "0"),
                ("PVPN_API_TIMEOUT", &api_timeout),
            ],
        )?
        .code()
        .unwrap_or(1)
    } else {
        need_tor()?;
        proc::run_interactive(
            "torsocks",
            &[
                "env",
                &format!("PYTHONPATH={}", shim.display()),
                "PVPN_DEBUG=0",
                &format!("PVPN_API_TIMEOUT={api_timeout}"),
                python,
                bridge.to_str().unwrap(),
                "--import-selector",
                &selector,
            ],
            &[],
        )?
        .code()
        .unwrap_or(1)
    };
    if import_rc != 0 {
        return Ok(import_rc);
    }
    println!("Browser bridge complete.");
    let _ = cmd_account_view();
    Ok(0)
}

pub fn cmd_logout() -> anyhow::Result<i32> {
    let _ = paths::ensure_shim()?;
    let api_timeout = std::env::var("PVPN_LOGIN_API_TIMEOUT").unwrap_or_else(|_| "60".to_string());
    let shim = paths::shim_dir();
    let status = if net::api_reachable() {
        proc::protonvpn_signout()?.success
    } else {
        need_tor()?;
        proc::run_with_timeout(
            "torsocks",
            &[
                "env",
                &format!("PYTHONPATH={}", shim.display()),
                "PVPN_DEBUG=0",
                &format!("PVPN_API_TIMEOUT={api_timeout}"),
                "protonvpn",
                "signout",
            ],
            &[],
            Duration::from_secs(90),
        )?
        .success
    };
    Ok(if status { 0 } else { 1 })
}

pub fn cmd_account_view() -> anyhow::Result<i32> {
    let info = proc::protonvpn_info().map(|r| r.stdout).unwrap_or_default();
    match proc::account_from_info(&info) {
        None => {
            eprintln!("Not signed in.");
            println!("  Sign in with:  pvpn login");
            Ok(1)
        }
        Some(_) => {
            print!("{info}");
            if !info.ends_with('\n') {
                println!();
            }
            println!("Protocol: {}", proc::current_protocol());
            if let Ok(status) = proc::protonvpn_status() {
                if proc::is_connected(&status.stdout) {
                    println!("VPN: connected");
                    if let Some(ip) = net::public_ip() {
                        println!("Public IP: {ip}");
                    }
                } else {
                    println!("VPN: disconnected");
                }
            }
            Ok(0)
        }
    }
}

pub fn cmd_protocols() -> anyhow::Result<i32> {
    let text = proc::list_protocols()?;
    print!("{text}");
    Ok(0)
}

/// Walk every protocol until one carries traffic.
///
/// Each step is a full `pvpn up`, so a successful try also updates the
/// fast/blocked lists for this network — the answer to "which protocol
/// works here" is worth exactly as much as the answer to "which server".
pub async fn cmd_try() -> anyhow::Result<i32> {
    let mut session = crate::session::Session::load()?;
    println!("Trying each protocol in turn. Your internet will stall in bursts.");
    for proto in proc::TRY_PROTOCOLS {
        println!();
        println!("--- trying {proto} ---");
        let report = crate::connect::up(&mut session, Some(proto.to_string())).await;
        if report.ok {
            println!("{}", report.message);
            println!("Success with {proto}. It is now saved as your default.");
            return Ok(0);
        }
        eprintln!("{}", report.message);
    }
    eprintln!("No protocol worked on this network.");
    println!("This network blocks every Proton transport we can reach.");
    Ok(1)
}

pub fn cmd_fix(hosts: bool, unhosts: bool) -> anyhow::Result<i32> {
    if unhosts {
        return Ok(if proc::unblackhole_api_hosts()? { 0 } else { 1 });
    }
    if hosts {
        if net::api_reachable() {
            println!("Proton's API is reachable — not blackholing it.");
            return Ok(0);
        }
        println!("Blackholing Proton API hosts in /etc/hosts (needs sudo).");
        return Ok(if proc::blackhole_api_hosts()? { 0 } else { 1 });
    }

    let mut did = false;
    if proc::link_exists("pvpnksintrf0") {
        println!("Removing stray kill-switch interface pvpnksintrf0 (needs sudo).");
        did = proc::delete_killswitch_interface()?;
    } else {
        println!("No stray pvpnksintrf0 interface.");
    }
    for uuid in proc::nmcli_killswitch_connections() {
        println!("Deleting leftover kill-switch NM profile {uuid}");
        proc::nmcli_delete_connection(&uuid);
        did = true;
    }
    for (_, uuid) in proc::nmcli_proton_connections() {
        proc::nmcli_clear_autoconnect(&uuid);
    }
    let duplicates = proc::dedupe_proton_connections();
    if duplicates > 0 {
        println!("Removed {duplicates} duplicate ProtonVPN profile(s) from Network Settings.");
        did = true;
    }
    if !did {
        println!("Nothing privileged to fix. For a temporary API blackhole: pvpn fix --hosts");
    }
    Ok(0)
}
