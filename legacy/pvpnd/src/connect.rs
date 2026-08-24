//! Port of `restore()`, `ensure_fresh_data()`, `cmd_up()`, and `cmd_hop()`
//! from `legacy/pvpn.sh`. Privileged steps (editing `/etc/hosts`, deleting
//! `pvpnksintrf0`) are skipped here with a warning — use `pvpn fix`.

use crate::app::{self, App, Phase};
use crate::apps as apps_hook;
use crate::blocklist::{self, ConnectOutcome};
use chrono::Utc;
use pvpn_core::cache::{self, SteerMode};
use pvpn_core::config::Config;
use pvpn_core::net;
use pvpn_core::paths;
use pvpn_core::pipeline::{self, RankRequest};
use pvpn_core::proc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct UpReport {
    pub ok: bool,
    pub message: String,
    #[allow(dead_code)]
    pub server: Option<String>,
}

/// How long to wait for normal routing after tearing a tunnel down, before
/// resorting to bouncing the wifi.
const SETTLE_AFTER_TEARDOWN_SECS: u32 = 10;

/// How long to wait for the internet after bouncing the wifi.
///
/// This was five seconds, which is shorter than a reassociation takes: on
/// a real bounce here NetworkManager reported the device activated 5s in
/// and only set the default route and DNS a further 5s after that, so the
/// wait expired in the gap. `pvpn down` then said "the network looks down"
/// about a network that was seconds from working, and bounced the wifi a
/// second time on the next attempt.
const SETTLE_AFTER_BOUNCE_SECS: u32 = 30;

/// Tear down a half-built tunnel and put normal routing back.
///
/// Never tears down a *working* tunnel just because traffic is slow —
/// callers decide that. This is the "something failed / user asked down"
/// path.
pub async fn restore() -> bool {
    app::blocking(|| {
        proc::kill_in_flight_connect();
        let _ = proc::protonvpn_disconnect();
        for (_, uuid) in proc::nmcli_proton_connections() {
            proc::nmcli_clear_autoconnect(&uuid);
        }
        for uuid in proc::nmcli_killswitch_connections() {
            proc::nmcli_delete_connection(&uuid);
        }
        if proc::link_exists("pvpnksintrf0") {
            tracing::warn!(
                "stray pvpnksintrf0 is present; not deleting it (needs sudo). Run: pvpn fix"
            );
        }
    })
    .await;

    // A leak guard left holding a default route swallows that whole address
    // family in silence, and `net_works` only ever asks IPv4, so nothing
    // below would notice.
    if let Some(dev) = app::blocking(proc::stray_leak_route).await {
        tracing::warn!(
            "{dev} is still holding a default route — traffic that way is being blackholed. Run: pvpn fix"
        );
    }

    if app::blocking(|| net::wait_for_net(SETTLE_AFTER_TEARDOWN_SECS)).await {
        return true;
    }

    let wifi = app::blocking(proc::active_wifi_connection).await;
    if let Some(name) = wifi {
        tracing::info!("bouncing wifi connection {name} as a last resort");
        app::blocking(move || proc::nmcli_connection_up(&name)).await;
        let back = app::blocking(|| net::wait_for_net(SETTLE_AFTER_BOUNCE_SECS)).await;
        if !back {
            tracing::warn!(
                "wifi came back but nothing is reaching the internet after {SETTLE_AFTER_BOUNCE_SECS}s"
            );
        }
        return back;
    }
    false
}

/// Refresh Proton's cached server list over Tor when it is stale. Runs
/// with routing untouched. `optional` means a missing Tor is a warning,
/// not a hard failure — `best` can still rank a stale list.
pub async fn ensure_fresh_data(cfg: &Config, optional: bool) -> anyhow::Result<()> {
    let path = paths::serverlist_path();
    let age = proc::file_age_hours(&path).unwrap_or(99999);
    if age < cfg.stale_hours {
        tracing::info!("server list is {age}h old — no refresh needed");
        return Ok(());
    }

    let tor_ok = app::blocking(proc::tor_available).await;
    if optional && !tor_ok {
        tracing::warn!("server list is stale and Tor is not running — using what we have");
        return Ok(());
    }
    if !tor_ok {
        anyhow::bail!("Tor is not running — Proton's API is blocked without it. Start it with: sudo systemctl start tor");
    }

    let shim = match paths::ensure_shim() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!("could not prepare shim: {err}");
            paths::shim_dir()
        }
    };
    let timeout = Duration::from_secs(cfg.refresh_timeout_secs);
    tracing::info!("server list is stale; refreshing over Tor (routing untouched)");
    let result = app::blocking(move || proc::protonvpn_servers_via_tor(&shim, timeout)).await;
    match result {
        Ok(r) if r.success && path.is_file() => {
            tracing::info!("server list cached");
        }
        Ok(_) | Err(_) => {
            tracing::warn!("refresh failed or timed out; connecting with what we have");
        }
    }
    Ok(())
}

/// How many servers may connect-and-carry-nothing before `up` stops
/// working down the ranked list.
///
/// Each one costs a full settle window, so walking all eight is twelve
/// minutes of an interactive command. Three is enough to tell "this one
/// server is dead" from "this network kills every session" without making
/// the user sit through the difference — and the second of those is the
/// answer far more often on the networks this tool exists for.
const QUIET_TUNNELS_BEFORE_GIVING_UP: u32 = 3;

/// Why did this attempt not produce a working tunnel?
///
/// Asks Proton's own log before blaming the server. An expired client
/// certificate fails every server identically, so attributing it to
/// whichever one we happened to try burns a healthy server off the ranked
/// list and moves to the next, which fails the same way — a whole
/// shortlist consumed in three minutes without a single bad server in it.
async fn diagnose(since: chrono::DateTime<Utc>, log: Option<&str>) -> ConnectOutcome {
    if app::blocking(move || proc::cert_failure_since(since)).await {
        return ConnectOutcome::CertificateExpired;
    }
    match log {
        Some(text) => blocklist::classify_failure(text),
        None => ConnectOutcome::ConnectedNoTraffic,
    }
}

/// Renew Proton's client certificate over Tor, returning true if the
/// refresh ran.
///
/// Proton's refresher only ever tries the direct path, and on a filtered
/// network it logs `Certificate refresh failed: No working transports
/// found`. After that every connect reaches `Connected` and the local
/// agent drops it a second later with `ExpiredCertificate`. Tor is the one
/// transport that reaches the API here, and `protonvpn servers` is what
/// drives all three refreshers — server list, certificate, client config.
/// Routing is untouched, so the caller's internet keeps working.
async fn renew_certificate(cfg: &Config) -> bool {
    if !app::blocking(proc::tor_available).await {
        tracing::warn!(
            "the client certificate needs renewing and Tor is not running — Proton's API is unreachable here. Start it with: sudo systemctl start tor"
        );
        return false;
    }
    let shim = match paths::ensure_shim() {
        Ok(p) => p,
        Err(_) => paths::shim_dir(),
    };
    let timeout = Duration::from_secs(cfg.refresh_timeout_secs);
    tracing::info!("renewing the client certificate over Tor (routing untouched)");
    let result = app::blocking(move || proc::protonvpn_servers_via_tor(&shim, timeout)).await;
    match result {
        Ok(r) if r.success => {
            tracing::info!("certificate renewed — retrying the connect");
            true
        }
        _ => {
            tracing::warn!("could not renew the certificate over Tor");
            false
        }
    }
}

async fn drop_stale_tunnel() {
    let connected = app::blocking(|| {
        proc::protonvpn_status()
            .ok()
            .map(|r| proc::is_connected(&r.stdout))
            .unwrap_or(false)
    })
    .await;
    if !connected {
        return;
    }
    tracing::warn!("something reconnected while we were working — disconnecting");
    restore().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
}

/// Fresh sweep+refine rank, persisted as `last_full_rank`, excluding
/// currently-blocked servers and folding recently-fast ones into the
/// shortlist.
pub async fn compute_full_rank(
    app: &App,
    country: Option<String>,
    quick: bool,
    limit: u32,
    free_only: bool,
) -> anyhow::Result<pipeline::RankResult> {
    let cfg = app.config.read().await.clone();
    let now = Utc::now();
    {
        let mut persist = app.persist.write().await;
        blocklist::expire(&mut persist, cfg.blocked_retry_after(), now);
    }

    let mut req = RankRequest::from_config(&cfg, quick, limit as usize, country);
    req.free_only = free_only || cfg.free_only;
    {
        let persist = app.persist.read().await;
        req = req.with_state(&persist, cfg.blocked_retry_after(), now);
    }

    let result = pipeline::rank_servers(req).await?;
    let targets = pipeline::connect_targets(&result);
    {
        let mut persist = app.persist.write().await;
        persist.set_last_full_rank(targets, now);
    }
    app.persist_save().await;
    Ok(result)
}

pub async fn do_up(app: &App, protocol: Option<String>) -> UpReport {
    app.set_want_up(true).await;
    app.busy.store(true, std::sync::atomic::Ordering::SeqCst);
    app.set_phase(Phase::Connecting).await;
    let report = up_inner(app, protocol).await;
    app.busy.store(false, std::sync::atomic::Ordering::SeqCst);
    if report.ok {
        app.set_phase(Phase::Connected).await;
        *app.last_error.write().await = None;
    } else {
        app.set_phase(Phase::Disconnected).await;
        *app.last_error.write().await = Some(report.message.clone());
    }
    report
}

async fn up_inner(app: &App, protocol: Option<String>) -> UpReport {
    let _ = paths::ensure_shim();
    // What we know about servers is filed per network; make sure we are
    // reading the right set before ranking or connecting.
    app.sync_network().await;
    let cfg = app.config.read().await.clone();

    let status = app::blocking(|| proc::protonvpn_status().ok()).await;
    let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
    if proc::is_connected(stdout) {
        let now = proc::current_server(stdout);
        let ranked = app
            .persist
            .read()
            .await
            .ranked_targets(cfg.blocked_retry_after(), Utc::now());
        let wrong_server = !ranked.is_empty() && now.as_deref() != Some(ranked[0].as_str());
        // "Connected" is Proton's belief, not a fact. On the networks this
        // tool exists for a middlebox kills the session and leaves the
        // status reading Connected, and `pvpn up` answered "Already
        // connected." about exactly that — leaving the user to wait for the
        // supervisor to notice before anything happened. Spend one probe
        // here: it costs nothing on a tunnel that works.
        // Two questions, not one: is anything tunneled at all, and does
        // traffic flow through it. The first is the one that caught a
        // client still reporting a server it had lost while every packet
        // went out unencrypted.
        let tunneled = app::blocking(proc::tunnel_is_real).await;
        let carries_traffic =
            !wrong_server && tunneled && app::blocking(net::net_works).await;
        if !tunneled {
            tracing::warn!(
                "{} reports Connected but nothing is tunneled — rebuilding rather than trusting it",
                now.as_deref().unwrap_or("the client")
            );
            restore().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else if wrong_server {
            tracing::warn!(
                "something reconnected to {} — moving to {}",
                now.as_deref().unwrap_or("another server"),
                ranked[0]
            );
            restore().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else if !carries_traffic {
            tracing::warn!(
                "{} says it is connected but carries no traffic — rebuilding the tunnel",
                now.as_deref().unwrap_or("the existing tunnel")
            );
            restore().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else {
            let fix = cfg.fix_apps;
            app::blocking(move || apps_hook::enforce_app_routing(fix)).await;
            return UpReport {
                ok: true,
                message: "Already connected.".to_string(),
                server: now,
            };
        }
    }

    let proto = {
        let requested = protocol.clone();
        app::blocking(move || {
            if let Some(p) = requested {
                let _ = proc::set_protocol(&p);
            } else if proc::protocol_available("protun-tls") {
                let _ = proc::set_protocol("protun-tls");
            }
            let current = proc::current_protocol();
            proc::ensure_connect_protocol(&current)
        })
        .await
    };
    let proto = match proto {
        Ok(p) => p,
        Err(err) => {
            return UpReport {
                ok: false,
                message: err.to_string(),
                server: None,
            };
        }
    };

    if let Err(err) = ensure_fresh_data(&cfg, false).await {
        return UpReport {
            ok: false,
            message: err.to_string(),
            server: None,
        };
    }

    let mut targets = app
        .persist
        .read()
        .await
        .ranked_targets(cfg.blocked_retry_after(), Utc::now());
    let rank_is_fresh = app
        .persist
        .read()
        .await
        .last_full_rank()
        .computed_at
        .map(|t| Utc::now() - t < chrono::Duration::minutes(10))
        .unwrap_or(false);

    // `targets.is_empty()` matters as much as staleness here: the cached
    // order may have just been emptied by blocks earned minutes ago, and
    // falling through with nothing hands the choice back to Proton — which
    // is how the dead server got picked in the first place.
    if cfg.auto_best_server && (!rank_is_fresh || targets.is_empty()) {
        drop_stale_tunnel().await;
        tracing::info!("measuring servers to pick the fastest");
        match compute_full_rank(app, cfg.country.clone(), false, 8, cfg.free_only).await {
            Ok(ranked) => {
                targets = pipeline::connect_targets(&ranked);
                if let Some(best) = ranked.candidates.first() {
                    tracing::info!(
                        "picked {} ({}{}, {}% load{})",
                        best.name,
                        best.latency_ms
                            .filter(|_| ranked.measured)
                            .map(|ms| format!("{ms:.0}ms, "))
                            .unwrap_or_default(),
                        if ranked.measured {
                            "measured"
                        } else {
                            "distance/load"
                        },
                        best.load,
                        best.distance_km
                            .map(|km| format!(", {km:.0}km"))
                            .unwrap_or_default()
                    );
                    if !ranked.measured {
                        tracing::warn!(
                            "ranked by distance and load: latency probes came back too fast to be real"
                        );
                    }
                }
            }
            Err(err) => {
                tracing::warn!("could not measure servers — letting Proton choose: {err}");
                targets.clear();
            }
        }
    } else if cfg.auto_best_server {
        drop_stale_tunnel().await;
    }

    let api_ok = app::blocking(net::api_reachable).await;
    if api_ok {
        tracing::info!("Proton's API is reachable — normal network");
    } else {
        tracing::warn!(
            "filtered network — skipping /etc/hosts API blackhole in daemon mode (needs sudo). Use: pvpn fix --hosts"
        );
    }

    tracing::warn!(
        "connecting via {proto} — internet will stall for up to {}s",
        cfg.connect_timeout_secs
    );

    let attempts = if targets.is_empty() {
        3
    } else {
        targets.len().min(8)
    };
    let shim = paths::shim_dir();
    let timeout = Duration::from_secs(cfg.connect_timeout_secs);
    let settle = Duration::from_secs(cfg.settle_secs);

    // `n` indexes the ranked list; `attempt` counts tries. They are not the
    // same number, because a connect that failed on our own expired
    // certificate must not consume a server — see `renew_certificate`.
    let mut n = 0usize;
    let mut attempt = 0usize;
    let mut renewed_cert = false;
    let mut quiet_tunnels: u32 = 0;

    while n < attempts {
        let target = targets.get(n).cloned();
        if attempt > 0 {
            match &target {
                Some(name) => {
                    tracing::info!("attempt {}/{attempts} — next best server ({name})", attempt + 1)
                }
                None => tracing::info!("attempt {}/{attempts} — reconnecting", attempt + 1),
            }
            let _ = app::blocking(proc::protonvpn_disconnect).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        attempt += 1;

        let attempt_started = Utc::now();
        let shim_c = shim.clone();
        let target_c = target.clone();
        let result =
            app::blocking(move || proc::protonvpn_connect(target_c.as_deref(), &shim_c, timeout))
                .await;
        let result = match result {
            Ok(r) => r,
            Err(err) => {
                tracing::error!("spawn connect: {err}");
                n += 1;
                continue;
            }
        };
        let log = format!("{}\n{}", result.stdout, result.stderr);
        let status = app::blocking(|| proc::protonvpn_status().ok()).await;
        let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");

        if result.success && proc::is_connected(stdout) {
            let got = proc::current_server(stdout);
            if let (Some(want), Some(got_name)) = (target.as_ref(), got.as_ref()) {
                if want != got_name && n + 1 < attempts {
                    tracing::warn!("landed on {got_name} instead of {want} — retrying");
                    n += 1;
                    continue;
                }
                if want != got_name {
                    tracing::warn!("landed on {got_name}, not {want}. keeping it: a working tunnel beats no tunnel");
                }
            }

            tracing::info!("tunnel up — checking traffic");
            let settled = app::blocking(move || net::net_works_settled(settle)).await;
            let server = got.clone().unwrap_or_else(|| "unknown".to_string());
            let outcome = if settled {
                ConnectOutcome::TrafficOk
            } else {
                diagnose(attempt_started, None).await
            };

            // An expired certificate is ours, not this server's. Renew it
            // and try the same server again rather than working down the
            // list watching every one of them fail the same way.
            if matches!(outcome, ConnectOutcome::CertificateExpired) {
                tracing::warn!(
                    "{server} connected but the local agent rejected our certificate — this is not the server's fault"
                );
                if !renewed_cert {
                    renewed_cert = true;
                    if renew_certificate(&cfg).await {
                        continue;
                    }
                }
                restore().await;
                return UpReport {
                    ok: false,
                    message:
                        "The client certificate has expired and could not be renewed on this network. Connect to another network, or start Tor and retry."
                            .to_string(),
                    server: None,
                };
            }

            {
                let mut persist = app.persist.write().await;
                blocklist::apply(&mut persist, &server, outcome, Utc::now());
            }
            app.persist_save().await;

            // Just written off as blocked, so do not then settle for it
            // while another ranked server is still untried. Rule 1 — never
            // tear a tunnel down merely because traffic is slow — was
            // learned before there *was* a settle window; now
            // `net_works_settled` has polled for the whole of it and
            // returns the moment anything gets through, so "still nothing
            // after 90s" is a verdict, not impatience. Keeping it anyway
            // meant `pvpn up` reported success on a dead tunnel and left
            // the user waiting another 45s for the supervisor to reach the
            // same conclusion.
            if !settled {
                quiet_tunnels += 1;
                if quiet_tunnels < QUIET_TUNNELS_BEFORE_GIVING_UP
                    && n + 1 < attempts
                    && !targets.is_empty()
                {
                    tracing::warn!(
                        "{server} carried nothing in {}s — moving on to the next ranked server",
                        cfg.settle_secs
                    );
                    n += 1;
                    continue;
                }
            }

            let fix = cfg.fix_apps;
            app::blocking(move || apps_hook::enforce_app_routing(fix)).await;

            if settled {
                return UpReport {
                    ok: true,
                    message: format!("Connected via {proto}."),
                    server: got,
                };
            }
            // Out of servers, or out of patience. A tunnel that might yet
            // come good beats no tunnel, so it stays — but say plainly that
            // it is a last resort, and say which of the two answers this
            // is: one dead server, or a network killing all of them.
            let verdict = if quiet_tunnels >= QUIET_TUNNELS_BEFORE_GIVING_UP {
                format!(
                    "{quiet_tunnels} servers in a row connected and carried nothing — it is this network killing the sessions, not the servers"
                )
            } else {
                format!("{server} was the last server worth trying here")
            };
            return UpReport {
                ok: true,
                message: format!(
                    "No traffic after {}s. {verdict}. Keeping the tunnel in case it comes good.",
                    cfg.settle_secs
                ),
                server: got,
            };
        }

        tracing::error!("could not connect via {proto} (timed out or refused)");
        for line in log.lines().filter(|l| {
            let l = l.to_lowercase();
            l.contains("error")
                || l.contains("failed")
                || l.contains("not available")
                || l.contains("no valid implementation")
                || l.contains("unexpected")
        }) {
            tracing::error!("  {line}");
        }

        let outcome = diagnose(attempt_started, Some(&log)).await;
        if matches!(outcome, ConnectOutcome::CertificateExpired) {
            tracing::warn!("the connect failed on our own certificate, not on the server");
            if !renewed_cert {
                renewed_cert = true;
                if renew_certificate(&cfg).await {
                    continue;
                }
            }
            restore().await;
            return UpReport {
                ok: false,
                message:
                    "The client certificate has expired and could not be renewed on this network. Connect to another network, or start Tor and retry."
                        .to_string(),
                server: None,
            };
        }
        if let Some(name) = &target {
            let mut persist = app.persist.write().await;
            blocklist::apply(&mut persist, name, outcome, Utc::now());
        }
        app.persist_save().await;

        if blocklist::log_says_missing_backend(&log) {
            return UpReport {
                ok: false,
                message: format!("Backend missing for {proto}. Run: pvpn protocols"),
                server: None,
            };
        }
        if target.is_some() && blocklist::log_says_free_plan(&log) {
            tracing::warn!("this install refuses named servers on a free plan; falling back to Proton's choice");
            targets.clear();
            n += 1;
            continue;
        }
        if blocklist::log_says_hard_refusal(&log) {
            break;
        }
        if proto.starts_with("openvpn-") && log.to_lowercase().contains("tls") {
            tracing::warn!("OpenVPN reached the server but TLS was blocked (DPI). Stealth is required on this network.");
            break;
        }
        n += 1;
    }

    tracing::warn!("restoring your normal connection");
    let restored = restore().await;
    UpReport {
        ok: false,
        message: if restored {
            "Could not connect. Internet restored.".to_string()
        } else {
            "Could not connect. Internet still down — try: pvpn down".to_string()
        },
        server: None,
    }
}

pub async fn do_down(app: &App) -> UpReport {
    app.set_want_up(false).await;
    app.busy.store(true, std::sync::atomic::Ordering::SeqCst);
    app.set_phase(Phase::Disconnected).await;
    let restored = restore().await;
    let stray = app::blocking(proc::stray_leak_route).await;
    app.busy.store(false, std::sync::atomic::Ordering::SeqCst);
    if let Some(dev) = stray {
        // Say this even when IPv4 came back: "Internet is working" while a
        // leak guard eats every IPv6 connection is how the user ends up
        // staring at a browser that will not load anything.
        return UpReport {
            ok: true,
            message: format!(
                "Disconnected, but {dev} is still holding a default route and blackholing traffic. Run: pvpn fix"
            ),
            server: None,
        };
    }
    if restored {
        UpReport {
            ok: true,
            message: "Disconnected. Internet is working.".to_string(),
            server: None,
        }
    } else {
        UpReport {
            ok: true,
            message: "Disconnected, but the network looks down.".to_string(),
            server: None,
        }
    }
}

pub async fn do_hop(app: &App, pattern: Option<String>) -> UpReport {
    app.set_want_up(true).await;
    app.busy.store(true, std::sync::atomic::Ordering::SeqCst);
    app.set_phase(Phase::Connecting).await;
    let report = hop_inner(app, pattern).await;
    app.busy.store(false, std::sync::atomic::Ordering::SeqCst);
    if report.ok {
        app.set_phase(Phase::Connected).await;
    } else {
        app.set_phase(Phase::Disconnected).await;
    }
    report
}

async fn hop_inner(app: &App, pattern: Option<String>) -> UpReport {
    let _ = paths::ensure_shim();
    // What we know about servers is filed per network; make sure we are
    // reading the right set before ranking or connecting.
    app.sync_network().await;
    let cfg = app.config.read().await.clone();
    let serverlist = paths::serverlist_path();

    let before = app::blocking(|| {
        proc::protonvpn_status()
            .ok()
            .and_then(|r| proc::current_server(&r.stdout))
    })
    .await;

    let steered = {
        let serverlist = serverlist.clone();
        let pattern = pattern.clone();
        let before = before.clone();
        app::blocking(move || -> Result<cache::SteerResult, String> {
            match pattern {
                Some(want) => cache::steer_cache(&serverlist, SteerMode::Only, &want)
                    .map_err(|e| e.to_string()),
                None => {
                    let Some(current) = before else {
                        return Err("not-connected".to_string());
                    };
                    cache::steer_cache(&serverlist, SteerMode::Exclude, &current)
                        .map_err(|e| e.to_string())
                }
            }
        })
        .await
    };

    let steered = match steered {
        Ok(s) => s,
        Err(msg) if msg == "not-connected" => {
            tracing::info!("not connected — just connecting");
            return up_inner(app, None).await;
        }
        Err(err) => {
            return UpReport {
                ok: false,
                message: err,
                server: None,
            };
        }
    };

    let proto = app::blocking(|| {
        if proc::protocol_available("protun-tls") {
            let _ = proc::set_protocol("protun-tls");
        }
        proc::ensure_connect_protocol(&proc::current_protocol())
    })
    .await;
    if let Err(err) = proto {
        cache::restore_cache(&steered.backup, &serverlist);
        return UpReport {
            ok: false,
            message: err.to_string(),
            server: None,
        };
    }

    let _ = app::blocking(proc::protonvpn_disconnect).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let shim = paths::shim_dir();
    let timeout = Duration::from_secs(cfg.connect_timeout_secs);
    let attempt_started = Utc::now();
    let result = app::blocking(move || proc::protonvpn_connect(None, &shim, timeout)).await;
    cache::restore_cache(&steered.backup, &serverlist);

    let result = match result {
        Ok(r) => r,
        Err(err) => {
            restore().await;
            return UpReport {
                ok: false,
                message: err.to_string(),
                server: None,
            };
        }
    };
    let status = app::blocking(|| proc::protonvpn_status().ok()).await;
    let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
    if !result.success || !proc::is_connected(stdout) {
        restore().await;
        return UpReport {
            ok: false,
            message: "Hop failed to connect.".to_string(),
            server: None,
        };
    }

    let after = proc::current_server(stdout);
    let settle = Duration::from_secs(cfg.settle_secs);
    let settled = app::blocking(move || net::net_works_settled(settle)).await;
    let outcome = if settled {
        ConnectOutcome::TrafficOk
    } else {
        diagnose(attempt_started, None).await
    };
    if matches!(outcome, ConnectOutcome::CertificateExpired) {
        tracing::warn!("the hop landed but our certificate has expired — not the server's fault");
    }
    if let Some(name) = &after {
        let mut persist = app.persist.write().await;
        blocklist::apply(&mut persist, name, outcome, Utc::now());
    }
    app.persist_save().await;
    let fix = cfg.fix_apps;
    app::blocking(move || apps_hook::enforce_app_routing(fix)).await;

    let from = before.unwrap_or_default();
    let to = after.clone().unwrap_or_default();
    if settled {
        UpReport {
            ok: true,
            message: if from.is_empty() {
                format!("Hopped to {to}")
            } else {
                format!("Hopped: {from} -> {to}")
            },
            server: after,
        }
    } else {
        UpReport {
            ok: true,
            message: format!(
                "{to} has not passed traffic yet after {}s. Keeping it.",
                cfg.settle_secs
            ),
            server: after,
        }
    }
}

/// What one supervisor reconnect actually achieved.
///
/// Three outcomes, not two. A tunnel that comes up and passes nothing is
/// kept — rule 1 — but it is not a recovery, and reporting it as one let
/// the supervisor log "brought the tunnel back" about a session the far
/// end was already tearing down, reset its backoff, and start the same
/// cycle again on the same server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectOutcome {
    /// Up and carrying traffic.
    Working,
    /// Up, but nothing has come through yet. Kept, and watched.
    UpButQuiet,
    Failed,
}

/// One reconnect attempt for the supervisor: restore, then walk the last
/// ranked list (or Proton's pick). Used after an unexpected drop.
pub async fn reconnect_once(app: &App) -> ReconnectOutcome {
    app.busy.store(true, std::sync::atomic::Ordering::SeqCst);
    app.set_phase(Phase::Reconnecting).await;
    restore().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    // The link may have moved while the tunnel was dying — a dropped
    // tunnel and a change of wifi look the same from here.
    app.sync_network().await;

    let cfg = app.config.read().await.clone();
    let mut targets = if cfg.auto_best_server {
        app.persist
            .read()
            .await
            .ranked_targets(cfg.blocked_retry_after(), Utc::now())
    } else {
        Vec::new()
    };
    if cfg.auto_best_server && targets.is_empty() {
        if let Ok(ranked) =
            compute_full_rank(app, cfg.country.clone(), false, 8, cfg.free_only).await
        {
            targets = pipeline::connect_targets(&ranked);
        }
    }

    let (report, settled) = connect_targets_only(app, &cfg, &targets).await;
    app.busy.store(false, std::sync::atomic::Ordering::SeqCst);
    if !report.ok {
        app.set_phase(Phase::Disconnected).await;
        *app.last_error.write().await = Some(report.message);
        return ReconnectOutcome::Failed;
    }
    app.set_phase(Phase::Connected).await;
    if settled {
        *app.last_error.write().await = None;
        ReconnectOutcome::Working
    } else {
        *app.last_error.write().await = Some(report.message);
        ReconnectOutcome::UpButQuiet
    }
}

/// Returns the report and whether the tunnel actually passed traffic. The
/// second half matters: a quiet tunnel is kept but is not a recovery.
async fn connect_targets_only(
    app: &App,
    cfg: &Config,
    targets: &[String],
) -> (UpReport, bool) {
    let _ = paths::ensure_shim();
    let proto = app::blocking(|| {
        if proc::protocol_available("protun-tls") {
            let _ = proc::set_protocol("protun-tls");
        }
        proc::ensure_connect_protocol(&proc::current_protocol())
    })
    .await;
    let proto = match proto {
        Ok(p) => p,
        Err(err) => {
            return (
                UpReport {
                    ok: false,
                    message: err.to_string(),
                    server: None,
                },
                false,
            );
        }
    };
    let shim = paths::shim_dir();
    let timeout = Duration::from_secs(cfg.connect_timeout_secs);
    let settle = Duration::from_secs(cfg.settle_secs);
    let attempts = if targets.is_empty() {
        1
    } else {
        targets.len().min(8)
    };

    let mut n = 0usize;
    let mut attempt = 0usize;
    let mut renewed_cert = false;

    while n < attempts {
        let target = targets.get(n).cloned();
        if attempt > 0 {
            let _ = app::blocking(proc::protonvpn_disconnect).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        attempt += 1;

        let attempt_started = Utc::now();
        let shim_c = shim.clone();
        let target_c = target.clone();
        let result =
            app::blocking(move || proc::protonvpn_connect(target_c.as_deref(), &shim_c, timeout))
                .await;
        let Ok(result) = result else {
            n += 1;
            continue;
        };
        let status = app::blocking(|| proc::protonvpn_status().ok()).await;
        let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");

        if result.success && proc::is_connected(stdout) {
            let got = proc::current_server(stdout);
            let settled = app::blocking(move || net::net_works_settled(settle)).await;
            let outcome = if settled {
                ConnectOutcome::TrafficOk
            } else {
                diagnose(attempt_started, None).await
            };

            // Ours, not theirs. Renew and retry this same server instead of
            // marking it blocked and walking the list into the same wall.
            if matches!(outcome, ConnectOutcome::CertificateExpired) {
                if !renewed_cert {
                    renewed_cert = true;
                    if renew_certificate(cfg).await {
                        continue;
                    }
                }
                return (
                    UpReport {
                        ok: false,
                        message: "The client certificate has expired and could not be renewed here."
                            .to_string(),
                        server: None,
                    },
                    false,
                );
            }

            if let Some(name) = &got {
                let mut persist = app.persist.write().await;
                blocklist::apply(&mut persist, name, outcome, Utc::now());
            }
            app.persist_save().await;
            let message = if settled {
                format!("Reconnected via {proto}.")
            } else {
                format!(
                    "Reconnected via {proto}, but nothing has come through after {}s.",
                    cfg.settle_secs
                )
            };
            return (
                UpReport {
                    ok: true,
                    message,
                    server: got,
                },
                settled,
            );
        }

        let log = format!("{}\n{}", result.stdout, result.stderr);
        let outcome = diagnose(attempt_started, Some(&log)).await;
        if matches!(outcome, ConnectOutcome::CertificateExpired) {
            if !renewed_cert {
                renewed_cert = true;
                if renew_certificate(cfg).await {
                    continue;
                }
            }
            return (
                UpReport {
                    ok: false,
                    message: "The client certificate has expired and could not be renewed here."
                        .to_string(),
                    server: None,
                },
                false,
            );
        }
        if let Some(name) = &target {
            let mut persist = app.persist.write().await;
            blocklist::apply(&mut persist, name, outcome, Utc::now());
        }
        app.persist_save().await;
        n += 1;
    }
    (
        UpReport {
            ok: false,
            message: "reconnect attempt failed".to_string(),
            server: None,
        },
        false,
    )
}

/// Seed `last_full_rank` on daemon start when `auto_best_server` is on.
pub async fn warmup_rank(app: &App) {
    let cfg = app.config.read().await.clone();
    if !cfg.auto_best_server {
        return;
    }
    if !paths::serverlist_path().is_file() {
        tracing::info!("no cached server list yet — skipping startup rank");
        return;
    }
    tracing::info!("computing initial server rank");
    match compute_full_rank(app, cfg.country.clone(), false, 8, cfg.free_only).await {
        Ok(ranked) => {
            tracing::info!(
                "initial rank: {}",
                ranked
                    .candidates
                    .iter()
                    .take(5)
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Err(err) => tracing::warn!("startup rank failed: {err}"),
    }
}
