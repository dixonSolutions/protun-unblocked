//! Connection state machine and auto-reconnect loop.
//!
//! Polls `protonvpn status` every few seconds. On an unexpected drop
//! while `auto_reconnect` is on and the user still wants a tunnel, runs
//! the same restore + ranked-connect sequence as `pvpn up`, with capped
//! exponential backoff between attempts.
//!
//! Two rules this whole tool exists for:
//! 1. never tear down a tunnel just because traffic hasn't started within
//!    the settle window;
//! 2. never spend a retry re-asking Proton for "the fastest" only to get
//!    the same dead server back.

use crate::app::{self, App, Phase};
use crate::blocklist::{self, ConnectOutcome};
use crate::connect::{self, ReconnectOutcome};
use chrono::Utc;
use pvpn_core::net;
use pvpn_core::proc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const POLL_SECS: u64 = 5;

/// How often to spend real packets on proving a tunnel still works.
///
/// `protonvpn status` reports what the client *believes*, which is not the
/// same thing. A wifi drop, a roam between APs, or — on the networks this
/// tool exists for — a middlebox quietly killing the session all leave the
/// interface up and the status reading "Connected" while nothing gets
/// through. Without this, an unusable tunnel simply stays.
const TRAFFIC_CHECK_SECS: u64 = 15;

/// Consecutive failed traffic checks before calling it a drop.
///
/// More than one, because rule 1 above was learned the hard way: a tunnel
/// that goes quiet for a moment is usually a tunnel, not a corpse. Three
/// spreads the verdict over ~45s of genuinely dead link.
const TRAFFIC_FAILURES_BEFORE_DROP: u32 = 3;

pub async fn run(app: App) {
    let mut interval = tokio::time::interval(Duration::from_secs(POLL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reconnect_attempt: u32 = 0;
    let mut traffic_failures: u32 = 0;
    let mut last_traffic_check: Option<Instant> = None;
    // When the tunnel now being judged came up. The certificate check asks
    // about this session only, so a failure from an earlier attempt that
    // has since been renewed cannot be read as a fresh one.
    let mut cert_watch_from = Utc::now();

    loop {
        interval.tick().await;
        if app.busy.load(Ordering::SeqCst) {
            continue;
        }

        // The server name comes back with the verdict, because a tunnel
        // found dead here has to be attributed to *something* — see the
        // no-traffic branch below.
        let (connected, current) = tokio::task::spawn_blocking(|| {
            proc::protonvpn_status()
                .ok()
                .map(|r| (proc::is_connected(&r.stdout), proc::current_server(&r.stdout)))
                .unwrap_or((false, None))
        })
        .await
        .unwrap_or((false, None));

        let want_up = app.want_up.load(Ordering::SeqCst);
        let cfg = app.config.read().await.clone();
        let phase = *app.phase.read().await;

        if !want_up {
            reconnect_attempt = 0;
            traffic_failures = 0;
            last_traffic_check = None;
            if connected {
                // Proton's own daemon put a tunnel back after the user
                // asked us to stay down. Tear it down again — but only if
                // nothing else is mid-connect, or this races a `pvpn up`
                // that set `want_up` after this tick read it and tears
                // down the tunnel it just built. Same reasoning as the
                // reconnect path below.
                match app.connect_lock.try_lock() {
                    Ok(_guard) if !app.want_up.load(Ordering::SeqCst) => {
                        tracing::warn!("a tunnel came back but none was wanted — disconnecting");
                        connect::restore().await;
                        app.set_phase(Phase::Disconnected).await;
                    }
                    _ => tracing::debug!("a connect is in flight — leaving it alone"),
                }
            } else if phase != Phase::Disconnected {
                app.set_phase(Phase::Disconnected).await;
            }
            continue;
        }

        if connected {
            // Before anything else: is this "Connected" true? A wedged
            // client keeps reporting a server it lost, and the traffic
            // check below cannot tell the difference — traffic flows
            // either way, just unencrypted. Checking reachability alone
            // let the machine sit leaking with pvpn reporting Singapore.
            if !app::blocking(proc::tunnel_is_real).await {
                tracing::warn!(
                    "{} reports Connected but nothing is tunneled — your traffic is going out in the clear",
                    current.as_deref().unwrap_or("the client")
                );
                // No block: the client wedged, the server did nothing
                // wrong, and blaming it would retire a good server.
                if !want_up || app.busy.load(Ordering::SeqCst) {
                    continue;
                }
                let _guard = match app.connect_lock.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => continue,
                };
                tracing::warn!("clearing the stale connection and rebuilding");
                let outcome = connect::reconnect_once(&app).await;
                traffic_failures = 0;
                last_traffic_check = Some(Instant::now());
                cert_watch_from = Utc::now();
                if outcome == ReconnectOutcome::Working {
                    reconnect_attempt = 0;
                } else {
                    reconnect_attempt += 1;
                }
                continue;
            }

            if phase != Phase::Connected {
                app.set_phase(Phase::Connected).await;
                cert_watch_from = Utc::now();
            }

            // Does it actually carry traffic, or does it only say so?
            let due = last_traffic_check
                .map(|at| at.elapsed() >= Duration::from_secs(TRAFFIC_CHECK_SECS))
                .unwrap_or(true);
            if !cfg.auto_reconnect || !due {
                reconnect_attempt = 0;
                continue;
            }
            last_traffic_check = Some(Instant::now());
            // Cheap enough at this cadence, and the alternative is judging
            // servers on one network with what another taught us.
            if app.sync_network().await {
                // The link moved under us. A tunnel built on the old
                // network is dead because of the move, not because of the
                // server it was built on — and the verdict would now be
                // filed against the network we just arrived at. Start over.
                traffic_failures = 0;
                cert_watch_from = Utc::now();
                continue;
            }

            if app::blocking(net::net_works).await {
                reconnect_attempt = 0;
                traffic_failures = 0;
                continue;
            }

            // The verdict above cost several seconds of blocking probes,
            // and it was formed against a snapshot taken before them. Ask
            // again before acting on it: a `pvpn down` that landed in the
            // meantime means this tunnel is gone because the user said so,
            // and counting that as the server's failure would block a
            // server for a whole day over a deliberate disconnect. It has
            // already been seen printing "no traffic (2/3)" eight seconds
            // after the user asked it to stay down.
            if !app.want_up.load(Ordering::SeqCst) || app.busy.load(Ordering::SeqCst) {
                traffic_failures = 0;
                last_traffic_check = Some(Instant::now());
                continue;
            }

            traffic_failures += 1;
            if traffic_failures < TRAFFIC_FAILURES_BEFORE_DROP {
                tracing::warn!(
                    "tunnel says connected but no traffic ({traffic_failures}/{TRAFFIC_FAILURES_BEFORE_DROP})"
                );
                continue;
            }
            tracing::warn!("tunnel is up but carries no traffic — treating it as dropped");

            // Write the verdict down. This is the whole point of the
            // blocklist and it was the one path that never reached it: a
            // middlebox that lets the handshake through and kills the
            // session seconds later never fails a *connect*, so `connect`
            // never saw it, and the next rank happily offered the same
            // server first. Unless it was our own certificate that
            // expired, in which case no server would have worked and
            // blaming this one teaches the ranker a lie.
            if let Some(name) = &current {
                let outcome = if app::blocking(move || proc::cert_failure_since(cert_watch_from))
                    .await
                {
                    tracing::warn!("...because our certificate expired, not because of {name}");
                    ConnectOutcome::CertificateExpired
                } else {
                    ConnectOutcome::ConnectedNoTraffic
                };
                {
                    let mut persist = app.persist.write().await;
                    blocklist::apply(&mut persist, name, outcome, Utc::now());
                }
                app.persist_save().await;
            }
            // Fall through to the reconnect path below.
        } else {
            traffic_failures = 0;
            if phase == Phase::Connected {
                tracing::warn!("tunnel dropped unexpectedly");
            } else if phase == Phase::Disconnected && reconnect_attempt == 0 {
                // Nothing is up and the last run said the user wanted a
                // tunnel: this is a resume after a restart or a reboot.
                tracing::info!("no tunnel but one was wanted — connecting");
            }
        }

        if !cfg.auto_reconnect {
            if phase != Phase::Disconnected {
                app.set_phase(Phase::Disconnected).await;
            }
            continue;
        }

        if cfg.reconnect_attempts > 0 && reconnect_attempt >= cfg.reconnect_attempts {
            tracing::error!(
                "gave up after {reconnect_attempt} reconnect attempts (reconnect_attempts={})",
                cfg.reconnect_attempts
            );
            continue;
        }

        let delay = cfg.backoff_secs(reconnect_attempt);
        if reconnect_attempt > 0 {
            tracing::info!("reconnect backoff {delay}s (attempt {reconnect_attempt})");
            tokio::time::sleep(Duration::from_secs(delay)).await;
            if !app.want_up.load(Ordering::SeqCst) || app.busy.load(Ordering::SeqCst) {
                continue;
            }
        }

        // Same again before touching the routing table: `reconnect_once`
        // is the most expensive thing here and the least welcome when the
        // user has just asked for the tunnel to stay down. `do_down` does
        // not take `connect_lock`, so the lock below cannot be relied on
        // to notice it.
        if !app.want_up.load(Ordering::SeqCst) || app.busy.load(Ordering::SeqCst) {
            traffic_failures = 0;
            last_traffic_check = Some(Instant::now());
            continue;
        }

        // Take the lock without waiting, or don't act at all.
        //
        // `connected` and `busy` were read at the top of this tick, before
        // any lock. Blocking on the lock meant queueing behind whoever
        // held it — usually a `pvpn up` — and then acting on a verdict
        // about a tunnel that no longer exists: the supervisor was seen
        // tearing down the connection the user had asked for one second
        // earlier, and connecting all over again. If someone else is
        // changing the routing table, wait for the next tick and look
        // again.
        let _guard = match app.connect_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::debug!("another connect is in flight — leaving it alone");
                traffic_failures = 0;
                last_traffic_check = Some(Instant::now());
                continue;
            }
        };

        tracing::warn!("restoring and reconnecting");
        let outcome = connect::reconnect_once(&app).await;
        // Whatever happened, the old verdict is stale: a fresh tunnel gets
        // a fresh full window to prove itself rather than inheriting the
        // dead one's strikes.
        traffic_failures = 0;
        last_traffic_check = Some(Instant::now());
        cert_watch_from = Utc::now();

        if outcome == ReconnectOutcome::UpButQuiet {
            // Keep it — rule 1 — but do not call this a recovery. Saying
            // "brought the tunnel back" here meant announcing success about
            // a session the far end had already started tearing down, and
            // resetting the backoff so the next attempt came straight back
            // to the same server.
            tracing::warn!("a tunnel is up but has not passed traffic yet — watching it");
            reconnect_attempt += 1;
        } else if outcome == ReconnectOutcome::Working {
            tracing::info!("supervisor brought the tunnel back");
            reconnect_attempt = 0;
        } else if !app::blocking(net::net_works).await {
            // `reconnect_once` restored normal routing before it tried, so
            // this is the underlying link — and it is down. That attempt
            // proved nothing about any server, so hold the backoff where
            // it is instead of escalating toward a five-minute sleep. The
            // wifi coming back should bring the tunnel back with it, not
            // five minutes later.
            tracing::info!("network is down — waiting for it, not backing off further");
        } else {
            reconnect_attempt += 1;
        }
    }
}
