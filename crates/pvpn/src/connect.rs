//! `up`, `down` and `hop`: everything that moves the routing table.
//!
//! This is where the daemon's hard-won behaviour ended up. It still walks
//! a freshly measured ranked list rather than accepting Proton's pick,
//! still refuses to tear down a merely slow tunnel, still tells an expired
//! certificate of ours apart from a server that is genuinely dead, and
//! still writes down what it learns. The difference is that all of it now
//! happens because someone typed a command, and stops when that command
//! returns.
//!
//! Privileged steps (editing `/etc/hosts`, deleting `pvpnksintrf0`) are
//! skipped here with a warning — use `pvpn fix`.

use crate::apps_hook;
use crate::blocklist::{self, ConnectOutcome};
use crate::session::{blocking, Session};
use crate::verify::{self, Verdict};
use chrono::{DateTime, Utc};
use pvpn_core::cache::{self, SteerMode};
use pvpn_core::config::Config;
use pvpn_core::net;
use pvpn_core::paths;
use pvpn_core::pipeline::{self, RankRequest};
use pvpn_core::proc;
use pvpn_core::state::Event;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct UpReport {
    pub ok: bool,
    pub message: String,
    #[allow(dead_code)]
    pub server: Option<String>,
}

/// How long to wait for normal routing after tearing a tunnel down.
const SETTLE_AFTER_TEARDOWN_SECS: u32 = 10;

/// Refuse to judge VPN servers when the ordinary connection is already
/// broken. In particular, a dead local DNS proxy makes every hostname-based
/// tunnel probe fail even though no VPN server caused that failure.
async fn local_network_problem() -> Option<String> {
    if !blocking(net::dns_works).await {
        return Some(
            "Your local DNS resolver is not answering before the VPN starts. \
             No server was tried or blocked. Fix DNS, then retry."
                .to_string(),
        );
    }
    if !blocking(net::net_works).await {
        return Some(
            "The internet is not reachable before the VPN starts. \
             No server was tried or blocked. Restore the normal connection, then retry."
                .to_string(),
        );
    }
    None
}

/// Tear down a half-built tunnel and put normal routing back.
///
/// Never tears down a *working* tunnel just because traffic is slow —
/// callers decide that. This is the "something failed / user asked down"
/// path.
pub async fn restore() -> bool {
    blocking(|| {
        proc::kill_in_flight_connect();
        let _ = proc::protonvpn_disconnect();
        proc::nmcli_deactivate_proton_connections();
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
    if let Some(dev) = blocking(proc::stray_leak_route).await {
        tracing::warn!(
            "{dev} is still holding a default route — traffic that way is being blackholed. Run: pvpn fix"
        );
    }

    let back = blocking(|| net::wait_for_net(SETTLE_AFTER_TEARDOWN_SECS)).await;
    if !back {
        tracing::warn!("nothing is reaching the internet after {SETTLE_AFTER_TEARDOWN_SECS}s");
    }
    back
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

    let tor_ok = blocking(proc::tor_available).await;
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
    let result = blocking(move || proc::protonvpn_servers_via_tor(&shim, timeout)).await;
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
/// Turn a [`Verdict`] into who is to blame.
///
/// The certificate check comes first and outranks everything, because a
/// lapsed certificate fails *every* server identically: read as a session
/// kill it would retire one healthy server per attempt and burn a whole
/// shortlist in three minutes without a bad server in it.
async fn outcome_for(verdict: &Verdict, since: DateTime<Utc>) -> ConnectOutcome {
    match verdict {
        Verdict::Carrying { .. } => ConnectOutcome::TrafficOk,
        Verdict::LinkDown { .. } => ConnectOutcome::LocalNetworkDown,
        Verdict::SessionDied { .. } | Verdict::Quiet { .. } => {
            if blocking(move || proc::cert_failure_since(since)).await {
                ConnectOutcome::CertificateExpired
            } else if matches!(verdict, Verdict::SessionDied { .. }) {
                ConnectOutcome::SessionKilled
            } else {
                ConnectOutcome::ConnectedNoTraffic
            }
        }
    }
}

/// Say what happened, in the terms the user can act on. A connect on a
/// hostile network takes minutes, and watching it work is most of how you
/// tell "slow" from "blocked" — so the distinction this module now draws
/// has to reach the terminal, not just the state file.
fn narrate(server: &str, verdict: &Verdict, settle_secs: u64) {
    let secs = verdict.seconds();
    match verdict {
        Verdict::Carrying { .. } => {
            tracing::info!("{server} is carrying traffic — verified in {secs}s")
        }
        Verdict::SessionDied { detail, .. } => tracing::warn!(
            "{server} connected, then the session was killed after {secs}s — Proton: {detail}"
        ),
        Verdict::LinkDown { .. } => tracing::warn!(
            "your own uplink went away after {secs}s — not {server}'s doing, so nothing is being written off"
        ),
        Verdict::Quiet { .. } => {
            tracing::warn!("{server} carried nothing in {settle_secs}s and said nothing either")
        }
    }
}

async fn maintain_system_profile(server: &str) {
    let server_name = server.to_string();
    match blocking(move || proc::reconcile_verified_proton_connection(&server_name)).await {
        Ok(_) => {}
        Err(err) => tracing::warn!("could not save {server} in Network Settings: {err}"),
    }
}

fn server_is_proven_here(session: &Session, server: &str) -> bool {
    session
        .state
        .servers()
        .get(server)
        .is_some_and(|stat| stat.connect_successes > 0)
        && !session
            .state
            .is_blocked(server, session.config.blocked_retry_after(), Utc::now())
}

fn automatic_connection_is_wrong(
    proton_cli_connected: bool,
    current: Option<&str>,
    ranked: &[String],
) -> bool {
    proton_cli_connected && !ranked.is_empty() && current != Some(ranked[0].as_str())
}

async fn preserve_then_disconnect(server: &str) {
    let server_name = server.to_string();
    if let Err(err) =
        blocking(move || proc::disconnect_preserving_verified_connection(&server_name)).await
    {
        tracing::warn!("could not preserve {server} in Network Settings: {err}");
        let _ = blocking(proc::protonvpn_disconnect).await;
    }
}

/// Write one attempt into this network's history, so tomorrow morning can
/// tell four bad servers from one bad network.
async fn record(
    session: &mut Session,
    server: &str,
    protocol: &str,
    outcome: ConnectOutcome,
    verdict: Option<&Verdict>,
) {
    let now = Utc::now();
    blocklist::apply(&mut session.state, server, outcome, now);
    session.state.record_event(Event {
        at: now,
        server: server.to_string(),
        protocol: protocol.to_string(),
        outcome: outcome.tag().to_string(),
        detail: match verdict {
            Some(Verdict::SessionDied { detail, .. }) => Some(detail.clone()),
            _ => None,
        },
        seconds: verdict.map(|v| v.seconds()),
    });
    session.save();
    if outcome.blames_the_server() {
        let server_name = server.to_string();
        let removed = blocking(move || proc::remove_verified_proton_connection(&server_name)).await;
        if removed > 0 {
            tracing::info!("{server}'s saved system VPN was removed");
        }
        // Say it out loud. A server quietly disappearing from tomorrow's
        // ranked list, with nothing in the terminal to say why, is how the
        // list ends up empty and nobody knows what emptied it.
        tracing::info!("{server} written off on this network for now — see `pvpn blocked`");
    } else if matches!(outcome, ConnectOutcome::TrafficOk) {
        maintain_system_profile(server).await;
    }
}

async fn diagnose(since: chrono::DateTime<Utc>, log: Option<&str>) -> ConnectOutcome {
    if blocking(move || proc::cert_failure_since(since)).await {
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
    if !blocking(proc::tor_available).await {
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
    let result = blocking(move || proc::protonvpn_servers_via_tor(&shim, timeout)).await;
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
    let connected = blocking(|| {
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
///
/// This is also what keeps the **fast** list alive. A background sweep
/// used to maintain it; with nothing running in the background, the
/// measurements every `pvpn best` and `pvpn up` already pay for are the
/// ones that get written down. Same rule as before — a handshake time is
/// a latency observation and never a verdict on whether a server works.
pub async fn compute_full_rank(
    session: &mut Session,
    country: Option<String>,
    quick: bool,
    limit: u32,
    free_only: bool,
) -> anyhow::Result<pipeline::RankResult> {
    let cfg = session.config.clone();
    let now = Utc::now();
    blocklist::expire(&mut session.state, cfg.blocked_retry_after(), now);

    let mut req = RankRequest::from_config(&cfg, quick, limit as usize, country);
    req.free_only = free_only || cfg.free_only;
    req = req.with_state(&session.state, cfg.blocked_retry_after(), now);

    let result = pipeline::rank_servers(req).await?;

    // Only when the numbers mean something. Where a transparent proxy
    // answers every handshake, `measured` is false and the latencies are
    // the middlebox's, not the servers' — recording those would fill the
    // fast list with a description of the proxy.
    if result.measured {
        for candidate in &result.candidates {
            if let Some(ms) = candidate.latency_ms {
                session.state.record_probe(&candidate.name, ms, now);
            }
        }
    }

    let targets = pipeline::connect_targets(&result);
    session.state.set_last_full_rank(targets, now);
    session.save();
    Ok(result)
}

pub async fn up(session: &mut Session, protocol: Option<String>) -> UpReport {
    let _ = paths::ensure_shim();
    // What we know about servers is filed per network; make sure we are
    // reading the right set before ranking or connecting.
    session.sync_network().await;
    let cfg = session.config.clone();

    let status = blocking(|| proc::protonvpn_status().ok()).await;
    let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
    let status_connected = proc::is_connected(stdout);
    let now = if status_connected {
        proc::current_server(stdout)
    } else {
        blocking(proc::active_proton_server).await
    };
    if status_connected || now.is_some() {
        let ranked = session
            .state
            .ranked_targets(cfg.blocked_retry_after(), Utc::now());
        // A desktop toggle is an explicit user choice. Proton's CLI does not
        // know about that activation, so do not replace it merely because
        // today's automatic rank prefers another server.
        let wrong_server = automatic_connection_is_wrong(status_connected, now.as_deref(), &ranked);
        // "Connected" is Proton's belief, not a fact. On the networks this
        // tool exists for a middlebox kills the session and leaves the
        // status reading Connected, and `pvpn up` answered "Already
        // connected." about exactly that. Nothing is watching in the
        // background any more, so if this command does not spend one probe
        // on the question, nothing ever will — and it costs nothing on a
        // tunnel that works.
        // Two questions, not one: is anything tunneled at all, and does
        // traffic flow through it. The first is the one that caught a
        // client still reporting a server it had lost while every packet
        // went out unencrypted.
        let tunneled = blocking(proc::tunnel_is_real).await;
        let carries_traffic = !wrong_server && tunneled && blocking(net::net_works).await;
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
            blocking(move || apps_hook::enforce_app_routing(fix)).await;
            if let Some(server) = now.as_deref() {
                maintain_system_profile(server).await;
            }
            return UpReport {
                ok: true,
                message: "Already connected.".to_string(),
                server: now,
            };
        }
    }

    let proto = {
        let requested = protocol.clone();
        blocking(move || {
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

    let mut targets = session
        .state
        .ranked_targets(cfg.blocked_retry_after(), Utc::now());
    let rank_is_fresh = session
        .state
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
        match compute_full_rank(session, cfg.country.clone(), false, 8, cfg.free_only).await {
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

    let api_ok = blocking(net::api_reachable).await;
    if api_ok {
        tracing::info!("Proton's API is reachable — normal network");
    } else {
        tracing::warn!(
            "filtered network — skipping the /etc/hosts API blackhole (needs sudo). Use: pvpn fix --hosts"
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
    // The network these servers were chosen for. If we come back on a
    // different one, nothing measured belongs to the server we picked.
    let network = session.network().to_string();

    while n < attempts {
        let target = targets.get(n).cloned();
        if attempt > 0 {
            match &target {
                Some(name) => {
                    tracing::info!(
                        "attempt {}/{attempts} — next best server ({name})",
                        attempt + 1
                    )
                }
                None => tracing::info!("attempt {}/{attempts} — reconnecting", attempt + 1),
            }
            let _ = blocking(proc::protonvpn_disconnect).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if let Some(problem) = local_network_problem().await {
            restore().await;
            return UpReport {
                ok: false,
                message: problem,
                server: None,
            };
        }
        attempt += 1;

        let attempt_started = Utc::now();
        let shim_c = shim.clone();
        let target_c = target.clone();
        let result =
            blocking(move || proc::protonvpn_connect(target_c.as_deref(), &shim_c, timeout)).await;
        let result = match result {
            Ok(r) => r,
            Err(err) => {
                tracing::error!("spawn connect: {err}");
                n += 1;
                continue;
            }
        };
        let log = format!("{}\n{}", result.stdout, result.stderr);
        let status = blocking(|| proc::protonvpn_status().ok()).await;
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

            let server = got.clone().unwrap_or_else(|| "unknown".to_string());
            tracing::info!("tunnel up — verifying that {server} carries traffic");
            let verdict = verify::verify(attempt_started, settle, &network).await;
            let outcome = outcome_for(&verdict, attempt_started).await;
            narrate(&server, &verdict, cfg.settle_secs);
            let settled = verdict.carrying();

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

            record(session, &server, &proto, outcome, Some(&verdict)).await;

            // Our own network, not this server. Walking to the next one
            // would measure the same broken uplink again and write off a
            // healthy server for every attempt it takes to run out of list.
            if matches!(outcome, ConnectOutcome::LocalNetworkDown) {
                restore().await;
                return UpReport {
                    ok: false,
                    message: format!(
                        "Your own network went away {}s into the connect, so there is nothing \
                         to say about {server}.\n\
                         No server was written off. Run `pvpn up` again once the wifi is back.",
                        verdict.seconds()
                    ),
                    server: None,
                };
            }

            // Just written off as blocked, so do not then settle for it
            // while another ranked server is still untried. Rule 1 — never
            // tear a tunnel down merely because traffic is slow — was
            // learned before there *was* a settle window; `verify` polls
            // for the whole of it and returns the moment anything gets
            // through, so "still nothing, and Proton says the session is
            // over" is a verdict, not impatience. Keeping it anyway meant
            // `pvpn up` reported success on a dead tunnel.
            if !settled {
                quiet_tunnels += 1;
                if quiet_tunnels < QUIET_TUNNELS_BEFORE_GIVING_UP
                    && n + 1 < attempts
                    && !targets.is_empty()
                {
                    tracing::warn!("moving on to the next ranked server");
                    n += 1;
                    continue;
                }
            }

            // Out of servers, or out of patience — but *why* decides
            // whether the tunnel stays. Rule 1 — never tear a tunnel down
            // merely because traffic is slow — is about not knowing. A
            // session Proton has declared over is not slow, it is
            // finished, and keeping it means leaving the routing table
            // pointed into a hole while telling the user to go and check
            // `pvpn status`. So that one comes down, before the apps hook
            // spends any effort on a tunnel nothing is going to use.
            if matches!(outcome, ConnectOutcome::SessionKilled) {
                let killed = quiet_tunnels >= QUIET_TUNNELS_BEFORE_GIVING_UP;
                let restored = restore().await;
                return UpReport {
                    ok: false,
                    message: format!(
                        "{}\n{}",
                        if killed {
                            format!("{quiet_tunnels} servers in a row connected and had their sessions killed — it is this network doing that, not the servers.")
                        } else {
                            format!("{server}'s session was killed and there was no other server worth trying here.")
                        },
                        if restored {
                            "Internet restored. `pvpn blocked` lists what has been written off here."
                        } else {
                            "Internet still down — try: pvpn down"
                        }
                    ),
                    server: None,
                };
            }

            let fix = cfg.fix_apps;
            blocking(move || apps_hook::enforce_app_routing(fix)).await;

            if settled {
                return UpReport {
                    ok: true,
                    message: format!("Connected via {proto}."),
                    server: got,
                };
            }

            // Nothing declared this one dead — it is simply quiet. A tunnel
            // that might yet come good beats no tunnel, so it stays, but
            // say plainly that it is a last resort, and which of the two
            // answers this is: one dead server, or a network killing all of
            // them. And say what happens next, which is nothing: no
            // supervisor is going to notice this and try again. Reporting
            // "keeping the tunnel" without that reads as a promise nobody
            // is left to keep.
            let summary = if quiet_tunnels >= QUIET_TUNNELS_BEFORE_GIVING_UP {
                format!(
                    "{quiet_tunnels} servers in a row connected and carried nothing — it is this network killing the sessions, not the servers"
                )
            } else {
                format!("{server} was the last server worth trying here")
            };
            return UpReport {
                ok: true,
                message: format!(
                    "No traffic after {}s, and nothing declared it dead. {summary}.\n\
                     Keeping the tunnel in case it comes good — check with `pvpn status`, \
                     then `pvpn hop` or `pvpn down`.",
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
            record(session, name, &proto, outcome, None).await;
        }

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

pub async fn down() -> UpReport {
    let current = blocking(|| {
        proc::protonvpn_status()
            .ok()
            .and_then(|status| proc::current_server(&status.stdout))
            .or_else(proc::active_proton_server)
    })
    .await;
    if let (Some(server), Ok(mut session)) = (current.as_deref(), Session::load()) {
        session.sync_network().await;
        if server_is_proven_here(&session, server) {
            preserve_then_disconnect(server).await;
        }
    }
    let restored = restore().await;
    let stray = blocking(proc::stray_leak_route).await;
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
            ok: false,
            message: "Disconnected, but the network is still down. Run `pvpn fix`; \
                      if only DNS fails, restart your local DNS service."
                .to_string(),
            server: None,
        }
    }
}

/// How many servers a single `pvpn hop` will work through.
///
/// Fewer than `up`'s eight on purpose: `hop` is what you type when the
/// tunnel you have is bad and you want a different one *now*, so it should
/// come back with an answer rather than spend twelve minutes proving the
/// network is hostile. `pvpn up` is the command for that.
const HOP_ATTEMPTS: usize = 4;

/// Where to hop, best first: servers that have actually carried traffic on
/// this network, then the measured rank.
///
/// The order is the point. Latency says how quickly something answered a
/// handshake — on a network with a transparent proxy, how quickly the
/// *proxy* answered. A server that carried real traffic here yesterday is
/// evidence, and evidence goes first. This is also what makes the lists
/// worth maintaining: every connect writes to them, and every hop reads
/// them back.
fn hop_candidates(session: &Session, exclude: Option<&str>) -> Vec<String> {
    let retry_after = session.config.blocked_retry_after();
    let now = Utc::now();
    let mut out: Vec<String> = Vec::new();
    let proven = session.state.working_list().into_iter().map(|(n, _)| n);
    for name in proven.chain(session.state.ranked_targets(retry_after, now)) {
        if Some(name.as_str()) == exclude || out.contains(&name) {
            continue;
        }
        out.push(name);
    }
    out
}

pub async fn hop(session: &mut Session, pattern: Option<String>) -> UpReport {
    let _ = paths::ensure_shim();
    // What we know about servers is filed per network; make sure we are
    // reading the right set before ranking or connecting.
    session.sync_network().await;

    let before = blocking(|| {
        proc::protonvpn_status()
            .ok()
            .and_then(|r| proc::current_server(&r.stdout))
            .or_else(proc::active_proton_server)
    })
    .await;

    // A named hop can also be the first connect. Guard that path just like
    // `up`; otherwise a dead local resolver would be recorded against the
    // named server before any healthy baseline had been established.
    if pattern.is_some() && before.is_none() {
        if let Some(problem) = local_network_problem().await {
            return UpReport {
                ok: false,
                message: problem,
                server: None,
            };
        }
    }
    if let Some(server) = before.as_deref() {
        if server_is_proven_here(session, server) {
            preserve_then_disconnect(server).await;
        }
    }

    match pattern {
        Some(want) => hop_to_pattern(session, want, before).await,
        None => hop_to_next_best(session, before).await,
    }
}

/// `pvpn hop` with nothing named: go to the best server that is not this
/// one, and if that does not work, the next, without being asked again.
///
/// This used to hand the choice back to Proton — steer the cache to
/// "anything but the current server" and take whatever came out — and then
/// accept the result after a single attempt. On a network that kills most
/// sessions that is one roll of the dice against a list this tool has
/// already measured and written down.
async fn hop_to_next_best(session: &mut Session, before: Option<String>) -> UpReport {
    let Some(current) = before.clone() else {
        tracing::info!("not connected — just connecting");
        return up(session, None).await;
    };
    let cfg = session.config.clone();

    let mut candidates = hop_candidates(session, Some(&current));
    if candidates.is_empty() {
        tracing::info!("nothing measured on this network yet — ranking servers first");
        if let Err(err) =
            compute_full_rank(session, cfg.country.clone(), false, 8, cfg.free_only).await
        {
            return UpReport {
                ok: false,
                message: format!("Nowhere to hop to: {err}"),
                server: None,
            };
        }
        candidates = hop_candidates(session, Some(&current));
    }
    if candidates.is_empty() {
        return UpReport {
            ok: false,
            message: "No server to hop to here — everything known on this network is blocked.\n\
                      See why with `pvpn blocked`, or clear it with `pvpn forget --all`."
                .to_string(),
            server: None,
        };
    }

    let proto = match prepare_protocol().await {
        Ok(p) => p,
        Err(err) => {
            return UpReport {
                ok: false,
                message: err.to_string(),
                server: None,
            }
        }
    };
    let network = session.network().to_string();
    let attempts = candidates.len().min(HOP_ATTEMPTS);
    tracing::info!(
        "hopping off {current} — {} to try, best first ({})",
        attempts,
        candidates[..attempts].join(", ")
    );

    for (i, target) in candidates.iter().take(attempts).enumerate() {
        tracing::info!("attempt {}/{attempts} — {target}", i + 1);
        let attempt = match connect_and_verify(session, Some(target), &proto, &network).await {
            Some(a) => a,
            None => continue,
        };
        match attempt.outcome {
            ConnectOutcome::TrafficOk => {
                let fix = cfg.fix_apps;
                blocking(move || apps_hook::enforce_app_routing(fix)).await;
                let landed = attempt.server;
                return UpReport {
                    ok: true,
                    message: format!("Hopped: {current} -> {landed}"),
                    server: Some(landed),
                };
            }
            ConnectOutcome::LocalNetworkDown => {
                restore().await;
                return UpReport {
                    ok: false,
                    message: "Your own network went away mid-hop, so nothing here is any \
                              server's doing.\nNothing was written off. Try again once the \
                              wifi is back."
                        .to_string(),
                    server: None,
                };
            }
            ConnectOutcome::CertificateExpired => {
                restore().await;
                return UpReport {
                    ok: false,
                    message: "The client certificate has expired — every server will fail the \
                              same way until it is renewed.\nStart Tor and run `pvpn up`, or \
                              connect to another network."
                        .to_string(),
                    server: None,
                };
            }
            _ => {}
        }
    }

    let restored = restore().await;
    UpReport {
        ok: false,
        message: format!(
            "Tried {attempts} servers and none of them carried traffic — on this network that \
             is the network's answer, not the servers'.\n{}",
            if restored {
                "Internet restored. `pvpn blocked` lists what has been written off here."
            } else {
                "Internet still down — try: pvpn down"
            }
        ),
        server: None,
    }
}

/// `pvpn hop JP`, `pvpn hop SG-FREE#12`: you named it, so you get it — one
/// attempt, and the same verification everything else gets.
async fn hop_to_pattern(session: &mut Session, want: String, before: Option<String>) -> UpReport {
    let cfg = session.config.clone();
    // Unlike the ranked hop path, this reads Proton's raw account cache
    // directly. Refresh it first so "not found" means the server is absent
    // from this account's inventory, not merely that the cache predates it.
    if let Err(err) = ensure_fresh_data(&cfg, true).await {
        tracing::warn!("could not refresh the server inventory: {err}");
    }
    let serverlist = paths::serverlist_path();
    let steered = {
        let serverlist = serverlist.clone();
        let want = want.clone();
        blocking(move || cache::steer_cache(&serverlist, SteerMode::Only, &want)).await
    };
    let steered = match steered {
        Ok(s) => s,
        Err(cache::SteerError::NoMatch) => {
            return UpReport {
                ok: false,
                message: format!(
                    "{want} is not in this account's Proton server inventory. \
                     Free accounts can receive different server pools; `pvpn best` lists yours."
                ),
                server: None,
            }
        }
        Err(err) => {
            return UpReport {
                ok: false,
                message: err.to_string(),
                server: None,
            }
        }
    };

    let proto = match prepare_protocol().await {
        Ok(p) => p,
        Err(err) => {
            cache::restore_cache(&steered.backup, &serverlist);
            return UpReport {
                ok: false,
                message: err.to_string(),
                server: None,
            };
        }
    };

    let network = session.network().to_string();
    let attempt = connect_and_verify(session, None, &proto, &network).await;
    cache::restore_cache(&steered.backup, &serverlist);

    let Some(attempt) = attempt else {
        restore().await;
        return UpReport {
            ok: false,
            message: format!("Nothing matching {want} would connect."),
            server: None,
        };
    };

    let fix = cfg.fix_apps;
    blocking(move || apps_hook::enforce_app_routing(fix)).await;

    let from = before.unwrap_or_default();
    let to = attempt.server;
    if attempt.verdict.carrying() {
        return UpReport {
            ok: true,
            message: if from.is_empty() {
                format!("Hopped to {to}")
            } else {
                format!("Hopped: {from} -> {to}")
            },
            server: Some(to),
        };
    }
    if matches!(attempt.outcome, ConnectOutcome::LocalNetworkDown) {
        restore().await;
        return UpReport {
            ok: false,
            message: "Your own network went away mid-hop, so nothing here is the server's \
                      doing. Nothing was written off."
                .to_string(),
            server: None,
        };
    }
    if matches!(attempt.outcome, ConnectOutcome::SessionKilled) {
        let restored = restore().await;
        return UpReport {
            ok: false,
            message: format!(
                "{to}'s session was killed {}s in. {}",
                attempt.verdict.seconds(),
                if restored {
                    "Internet restored."
                } else {
                    "Internet still down — try: pvpn down"
                }
            ),
            server: None,
        };
    }
    UpReport {
        ok: true,
        message: format!(
            "{to} has not passed traffic yet after {}s. Keeping it — it may still come good.",
            cfg.settle_secs
        ),
        server: Some(to),
    }
}

/// Put the connect protocol where it needs to be — Stealth if this install
/// has it, since that is the only one these networks let through.
async fn prepare_protocol() -> anyhow::Result<String> {
    blocking(|| {
        if proc::protocol_available("protun-tls") {
            let _ = proc::set_protocol("protun-tls");
        }
        proc::ensure_connect_protocol(&proc::current_protocol())
    })
    .await
}

/// One connect attempt with the full verification behind it, recorded.
/// `None` means the tunnel never came up at all.
struct Attempt {
    server: String,
    outcome: ConnectOutcome,
    verdict: Verdict,
}

async fn connect_and_verify(
    session: &mut Session,
    target: Option<&String>,
    proto: &str,
    network: &str,
) -> Option<Attempt> {
    let cfg = session.config.clone();
    let _ = blocking(proc::protonvpn_disconnect).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let shim = paths::shim_dir();
    let timeout = Duration::from_secs(cfg.connect_timeout_secs);
    let started = Utc::now();
    let target_c = target.cloned();
    let result = blocking(move || proc::protonvpn_connect(target_c.as_deref(), &shim, timeout))
        .await
        .ok()?;

    let status = blocking(|| proc::protonvpn_status().ok()).await;
    let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
    if !result.success || !proc::is_connected(stdout) {
        let log = format!("{}\n{}", result.stdout, result.stderr);
        let outcome = diagnose(started, Some(&log)).await;
        if let Some(name) = target {
            record(session, name, proto, outcome, None).await;
        }
        tracing::warn!(
            "{} did not connect",
            target.map(String::as_str).unwrap_or("that server")
        );
        return None;
    }

    let server = proc::current_server(stdout).unwrap_or_else(|| "unknown".to_string());
    tracing::info!("tunnel up — verifying that {server} carries traffic");
    let settle = Duration::from_secs(cfg.settle_secs);
    let verdict = verify::verify(started, settle, network).await;
    let outcome = outcome_for(&verdict, started).await;
    narrate(&server, &verdict, cfg.settle_secs);
    record(session, &server, proto, outcome, Some(&verdict)).await;
    Some(Attempt {
        server,
        outcome,
        verdict,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_desktop_selected_server_is_not_replaced_by_the_automatic_rank() {
        let ranked = vec!["SG-FREE#2".to_string()];
        assert!(!super::automatic_connection_is_wrong(
            false,
            Some("JP-FREE#33"),
            &ranked
        ));
        assert!(super::automatic_connection_is_wrong(
            true,
            Some("JP-FREE#33"),
            &ranked
        ));
    }
}
