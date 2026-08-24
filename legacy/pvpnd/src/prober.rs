//! Background latency sweep that maintains the persisted **fast** list.
//!
//! Cheap TLS-handshake timing only — never a real VPN connect. Paused
//! while a connect/reconnect is in flight so probes never compete for
//! uplink with a real attempt (same lesson as "probes interfere with
//! each other" in `docs/best-server.md`).

use crate::app::App;
use chrono::Utc;
use pvpn_core::geo;
use pvpn_core::paths;
use pvpn_core::probe::{self, DEFAULT_PROBE_TIMEOUT, PROBE_PORT, SWEEP_CONCURRENCY};
use pvpn_core::rank;
use pvpn_core::serverlist::{self, EligibilityOptions};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub async fn run(app: App) {
    // First sweep after a short delay so startup rank (if any) goes first.
    tokio::time::sleep(Duration::from_secs(15)).await;

    loop {
        let interval = {
            let cfg = app.config.read().await;
            Duration::from_secs(cfg.probe_interval_secs.max(60))
        };

        if app.busy.load(Ordering::SeqCst) {
            tracing::debug!("prober paused — connect in flight");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        if let Err(err) = sweep_once(&app).await {
            tracing::warn!("background probe sweep failed: {err}");
        }

        tokio::time::sleep(interval).await;
    }
}

async fn sweep_once(app: &App) -> anyhow::Result<()> {
    if app.busy.load(Ordering::SeqCst) {
        return Ok(());
    }
    let path = paths::serverlist_path();
    if !path.is_file() {
        return Ok(());
    }
    let cfg = app.config.read().await.clone();
    let (max_tier, logicals) = serverlist::load_server_list(&path)?;
    let opts = EligibilityOptions {
        country: cfg.country.clone(),
        free_only: cfg.free_only,
        excluded_features: rank::FEATURES_EXCLUDED_BY_DEFAULT as i64,
    };
    let mut candidates = serverlist::eligible_servers(&logicals, max_tier, &opts);
    if candidates.is_empty() {
        return Ok(());
    }
    let origin = geo::local_coordinates();
    let by_name: HashMap<_, _> = logicals
        .iter()
        .filter_map(|l| l.name.clone().map(|n| (n, l.clone())))
        .collect();
    serverlist::annotate_distances(&mut candidates, &by_name, origin);

    let mut shortlist = rank::shortlist(&candidates, cfg.probe_shortlist);
    {
        let persist = app.persist.read().await;
        for (name, _) in persist.fast_list() {
            if shortlist.iter().any(|c| c.name == name) {
                continue;
            }
            if let Some(c) = candidates.iter().find(|c| c.name == name) {
                shortlist.push(c.clone());
            }
        }
    }

    tracing::info!("background sweep of {} servers", shortlist.len());
    probe::probe_all(
        &mut shortlist,
        PROBE_PORT,
        1,
        DEFAULT_PROBE_TIMEOUT,
        SWEEP_CONCURRENCY,
        true,
    )
    .await;

    if app.busy.load(Ordering::SeqCst) {
        tracing::debug!("discarding sweep — a connect started mid-probe");
        return Ok(());
    }

    let now = Utc::now();
    {
        let mut persist = app.persist.write().await;
        for candidate in &shortlist {
            if let Some(ms) = candidate.latency_ms {
                persist.record_probe(&candidate.name, ms, now);
            }
        }
        crate::blocklist::expire(&mut persist, cfg.blocked_retry_after(), now);
    }
    app.persist_save().await;
    let fast = app.persist.read().await.fast_list().len();
    tracing::info!("fast list now has {fast} server(s)");
    Ok(())
}
