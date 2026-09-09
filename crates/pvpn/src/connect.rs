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
use pvpn_core::cert;
use pvpn_core::config::Config;
use pvpn_core::intent;
use pvpn_core::link::{self, LinkHealth};
use pvpn_core::net;
use pvpn_core::paths;
use pvpn_core::pipeline::{self, RankRequest};
use pvpn_core::proc;
use pvpn_core::state::Event;
use std::time::{Duration, Instant};

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
    let (dns_ok, internet_ok) = tokio::join!(blocking(net::dns_works), blocking(net::net_works));
    if !dns_ok {
        return Some(
            "Your local DNS resolver is not answering before the VPN starts. \
             No server was tried or blocked. Fix DNS, then retry."
                .to_string(),
        );
    }
    if !internet_ok {
        return Some(
            "The internet is not reachable before the VPN starts. \
             No server was tried or blocked. Restore the normal connection, then retry."
                .to_string(),
        );
    }
    None
}

async fn wait_for_no_active_tunnel(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !blocking(proc::proton_profile_active).await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_internet(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if net::net_works_raced(Duration::from_secs(1)).await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn disconnect_and_wait() {
    let _ = blocking(proc::protonvpn_disconnect).await;
    if !wait_for_no_active_tunnel(Duration::from_secs(2)).await {
        tracing::warn!(
            "the previous tunnel is still deactivating; continuing with fallback cleanup"
        );
    }
}

async fn reuse_healthy_tunnel(session: &Session) -> Option<UpReport> {
    let active = match blocking(pvpn_core::dbus::active_proton_server).await {
        Some(server) => server,
        None => blocking(proc::active_proton_server).await?,
    };
    let network = session.network().to_string();
    let started = Utc::now();
    let verdict = verify::verify(started, Duration::from_secs(2), &network).await;
    if !verdict.carrying() {
        return None;
    }
    let protocol = blocking(proc::current_protocol).await;
    tracing::info!("{active} is already carrying tunneled traffic");
    Some(UpReport {
        ok: true,
        message: format!("Already connected via {protocol}; tunnel verified."),
        server: Some(active),
    })
}

async fn activate_saved_fast_path(session: &mut Session) -> Option<UpReport> {
    let (server, _) = session.state.fastest_working_list().into_iter().next()?;
    let profile_server = server.clone();
    let profile = blocking(move || pvpn_core::dbus::find_proton_profile(&profile_server)).await;
    let started_at = Utc::now();
    let timer = Instant::now();

    let activated = if let Some(profile) = profile {
        let timeout = Duration::from_secs(session.config.connect_timeout_secs);
        blocking(move || {
            let active_path = pvpn_core::dbus::activate(&profile)?;
            Ok::<bool, anyhow::Error>(pvpn_core::dbus::await_activation(&active_path, timeout))
        })
        .await
        .unwrap_or(false)
    } else {
        let server_name = server.clone();
        blocking(move || proc::activate_verified_proton_connection(&server_name))
            .await
            .unwrap_or(false)
    };
    if !activated {
        return None;
    }

    // What the profile we just activated actually is, not what Proton's
    // settings say the next connect would use. Reading `settings.json` here
    // is what filed a profile activation as a `protun-tls` success it had no
    // part in, which then became the protocol every later connect chose. An
    // empty string is the honest answer when the profile does not say, and
    // `State::proven_protocol` skips events that give one.
    let protocol = blocking(proc::active_profile_protocol)
        .await
        .unwrap_or_default();
    tracing::info!("activated proven server {server} directly; verifying traffic");
    let verdict = verify::verify(
        started_at,
        Duration::from_secs(session.config.settle_secs),
        session.network(),
    )
    .await;
    let outcome = outcome_for(&verdict, started_at).await;
    narrate(&server, &verdict, session.config.settle_secs);
    let ready_ms = timer.elapsed().as_millis().min(u64::MAX as u128) as u64;
    record(
        session,
        &server,
        &protocol,
        outcome,
        Some(&verdict),
        verdict.carrying().then_some(ready_ms),
    )
    .await;

    if verdict.carrying() {
        let fix = session.config.fix_apps;
        blocking(move || apps_hook::enforce_app_routing(fix)).await;
        return Some(UpReport {
            ok: true,
            message: format!(
                "Connected via {protocol}; proven profile verified in {:.2}s.",
                ready_ms as f64 / 1000.0
            ),
            server: Some(server),
        });
    }

    tracing::warn!("saved profile did not verify; rebuilding through Proton's client");
    if let Some(active) = blocking(pvpn_core::dbus::active_proton_profile).await {
        let _ = blocking(move || pvpn_core::dbus::deactivate(&active.path)).await;
    } else {
        disconnect_and_wait().await;
    }
    let _ = wait_for_no_active_tunnel(Duration::from_secs(2)).await;
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
        let former_active = proc::active_proton_connection_uuids().unwrap_or_default();
        let _ = proc::protonvpn_disconnect();
        proc::nmcli_deactivate_proton_connections();
        proc::remove_former_active_proton_duplicates(&former_active);
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

    let back = wait_for_internet(Duration::from_secs(SETTLE_AFTER_TEARDOWN_SECS as u64)).await;
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
    // Before anything reads the inventory: a steer left behind by a killed
    // hop makes this account look like it owns one server, and every ranked
    // connect then fails with "no servers available" until that one is
    // written off — after which there is nothing left to try at all.
    match cache::heal_stranded_steer(&path) {
        Ok(Some(cache::Healed::FromBackup)) => {
            tracing::warn!(
                "an interrupted `pvpn hop` had left Proton's server list steered — restored it"
            );
        }
        Ok(Some(cache::Healed::Reenabled(n))) => {
            tracing::warn!(
                "Proton's cached list had {n} free servers switched off and no backup to \
                 restore — re-enabled them; the next refresh will fetch the real statuses"
            );
        }
        Ok(None) => {}
        Err(err) => tracing::warn!("could not check the server list for a stranded steer: {err}"),
    }
    if let Some(valid_for) = cache::inventory_valid_for_secs(&path) {
        if valid_for > 0 {
            let minutes = (valid_for + 59) / 60;
            tracing::info!(
                "Proton's server inventory is valid for {minutes} more minute{}",
                if minutes == 1 { "" } else { "s" }
            );
            return Ok(());
        }
    } else {
        let age = proc::file_age_hours(&path).unwrap_or(99999);
        if age < cfg.stale_hours {
            tracing::info!("server inventory has no expiry metadata and is {age}h old");
            return Ok(());
        }
    }

    if !path.is_file() {
        tracing::info!("no cached Proton server inventory");
    } else {
        tracing::info!("Proton's cached server inventory has expired");
    }

    // Ask where the API can be reached *before* reaching for Tor. Tor is
    // the workaround for a network that blocks Proton by name; it is not a
    // better path, and Proton does not answer its VPN API to Tor exits at
    // all — a refresh sent that way waits out the whole timeout and returns
    // nothing even while the API is healthy. Once a tunnel is up the block
    // Tor existed to dodge is already gone, so direct is the route that
    // works.
    let direct = blocking(|| net::proton_api_health(6)).await;
    let go_direct = matches!(direct, net::ApiHealth::Answering(_));
    let tor_ok = blocking(proc::tor_available).await;
    if !go_direct {
        if optional && !tor_ok {
            tracing::warn!("server list is stale and Tor is not running — using what we have");
            return Ok(());
        }
        if !tor_ok {
            anyhow::bail!("Tor is not running — Proton's API is blocked without it. Start it with: sudo systemctl start tor");
        }
    }

    let shim = match paths::ensure_shim() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!("could not prepare shim: {err}");
            paths::shim_dir()
        }
    };
    let timeout = Duration::from_secs(cfg.refresh_timeout_secs);
    let result = if go_direct {
        tracing::info!("server list is stale; Proton's API is reachable, refreshing directly");
        blocking(move || proc::protonvpn_servers_direct(&shim, timeout)).await
    } else {
        tracing::info!("server list is stale; refreshing over Tor (routing untouched)");
        blocking(move || proc::protonvpn_servers_via_tor(&shim, timeout)).await
    };
    // `protonvpn servers` exits 0 whether or not it reached the API — on a
    // filtered network it logs "Server list refresh failed: No working
    // transports found" and returns success. Proton stamps a new expiry only
    // when a fetch actually lands, so that, not the exit code, is the test.
    let landed = cache::inventory_valid_for_secs(&path).unwrap_or(-1) > 0;
    match result {
        Ok(r) if r.success && landed => {
            tracing::info!("server list cached");
        }
        Ok(r) if r.success => {
            // Do not guess at the reason. The refresh not landing says only
            // that it did not land: Tor being unusable and Proton's own API
            // being down produce byte-identical evidence here, and blaming
            // Tor for the second sends the user to debug a working Tor
            // while Proton is 503ing for everybody. So ask.
            match blocking(|| net::proton_api_health(8)).await {
                net::ApiHealth::Down(code) => tracing::warn!(
                    "Proton's own API is answering {code} — their outage, not this network \
                     and not Tor; connecting with the cached list"
                ),
                net::ApiHealth::Unreachable => tracing::warn!(
                    "nothing answered at Proton's API, over Tor or directly — connecting with \
                     the cached list"
                ),
                net::ApiHealth::Answering(code) => tracing::warn!(
                    "Proton's API answered {code} but the refresh still did not land; \
                     connecting with the cached list"
                ),
                net::ApiHealth::Intercepted(code) => tracing::warn!(
                    "something answered {code} for Proton's API but it was not Proton — \
                     this network is intercepting it; connecting with the cached list"
                ),
            }
        }
        Ok(_) | Err(_) => {
            tracing::warn!("refresh failed or timed out; connecting with what we have");
        }
    }
    // The refresh has now been tried and did not land. If the cache is also
    // past its expiry, Proton will refuse to connect with it at all — so
    // "connecting with what we have" above is a promise this has to keep.
    let stale_for = cfg.stale_hours.max(1) as i64;
    match blocking(move || cache::extend_inventory_expiry(&path, stale_for)).await {
        Ok(true) => tracing::warn!(
            "Proton would have refused to connect with an expired list; marked the cached \
             inventory usable for another {stale_for}h — its server data is unchanged"
        ),
        Ok(false) => {}
        Err(err) => tracing::warn!("could not extend the cached inventory's expiry: {err}"),
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
            if certificate_is_to_blame(since).await {
                ConnectOutcome::CertificateExpired
            } else if matches!(verdict, Verdict::SessionDied { .. }) {
                ConnectOutcome::SessionKilled
            } else {
                ConnectOutcome::ConnectedNoTraffic
            }
        }
    }
}

/// Was it our own certificate, rather than this server?
///
/// Two independent answers, because neither covers both paths. The keyring
/// holds the expiry outright and can be read from anywhere — including the
/// fast path, which activates a saved NetworkManager profile over D-Bus and
/// so never makes Proton write the log line the other check reads. That gap
/// is what let a server with fourteen successful connects behind it be
/// written off twice in ten minutes on 2026-08-31.
///
/// Proton's log stays because it catches what the keyring cannot see: a
/// certificate the *node* rejects while our copy still looks valid, and a
/// keyring that will not open at all.
async fn certificate_is_to_blame(since: DateTime<Utc>) -> bool {
    if let Some(status) = blocking(cert::status).await {
        if status.unusable(Utc::now()) {
            return true;
        }
    }
    blocking(move || proc::cert_failure_since(since)).await
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
    tidy_duplicate_profiles().await;
}

/// Remove duplicate `ProtonVPN` entries left by interrupted teardowns.
///
/// Separate from [`maintain_system_profile`]'s reconcile, which only ever
/// looks at the server being connected. Duplicates belong to whichever server
/// was up when a connect was interrupted, and nothing was ever going to
/// connect to that one again just to tidy it — so Network Settings quietly
/// accumulated two `ProtonVPN JP-FREE#10` entries and kept them.
async fn tidy_duplicate_profiles() {
    let removed = blocking(proc::dedupe_proton_connections).await;
    if removed > 0 {
        tracing::info!("removed {removed} duplicate ProtonVPN profile(s) from Network Settings");
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

async fn preserve_then_disconnect(server: &str) {
    let server_name = server.to_string();
    match blocking(move || proc::disconnect_preserving_verified_connection(&server_name)).await {
        Ok(true) => {}
        Ok(false) => {
            // NetworkManager already removed the tunnel; only Proton's stale
            // internal state may remain.
            let _ = blocking(proc::protonvpn_disconnect).await;
        }
        Err(err) => {
            tracing::warn!("could not preserve {server} in Network Settings: {err}");
            let _ = blocking(proc::protonvpn_disconnect).await;
        }
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
    ready_ms: Option<u64>,
) {
    let now = Utc::now();
    blocklist::apply(&mut session.state, server, outcome, now);
    if matches!(outcome, ConnectOutcome::TrafficOk) {
        if let Some(milliseconds) = ready_ms {
            session.state.record_verified_ready(server, milliseconds);
        }
    }
    session.state.record_event(Event {
        at: now,
        server: server.to_string(),
        protocol: protocol.to_string(),
        outcome: outcome.tag().to_string(),
        detail: match verdict {
            Some(Verdict::SessionDied { detail, .. }) => Some(detail.clone()),
            _ => None,
        },
        seconds: ready_ms
            .map(|milliseconds| milliseconds.div_ceil(1000))
            .or_else(|| verdict.map(|v| v.seconds())),
        ready_ms,
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
        // Every path that produces a working tunnel comes through here, so
        // this is where the certificate gets renewed while renewing it is
        // still cheap. See `renew_certificate_opportunistically`.
        let cfg = session.config.clone();
        renew_certificate_opportunistically(&cfg).await;
    }
}

async fn diagnose(since: chrono::DateTime<Utc>, log: Option<&str>) -> ConnectOutcome {
    if blocking(move || proc::cert_failure_since(since)).await {
        return ConnectOutcome::CertificateExpired;
    }
    if matches!(blocking(link::health).await, LinkHealth::Down) {
        return ConnectOutcome::LocalNetworkDown;
    }
    // Proton prints "Authentication required / Please sign in" when Secret
    // Service is locked and the session cannot be loaded — even though the
    // account is still in the keyring. Prefer Proton's own log over that
    // wording; signing in again is the wrong fix.
    if log.is_some_and(blocklist::log_says_auth_required)
        && blocking(move || proc::keyring_locked_since(since)).await
    {
        return ConnectOutcome::KeyringLocked;
    }
    match log {
        Some(text) => blocklist::classify_failure(text),
        None => ConnectOutcome::ConnectedNoTraffic,
    }
}

/// Said whenever the certificate is the reason and we could not fix it.
///
/// Worth stating in full every time, because the failure it describes looks
/// exactly like a network killing every server, and the two send the reader
/// to opposite places. Nothing is written off for this — see
/// `certificate_is_to_blame`.
const CERT_DEAD_END: &str = "The client certificate has expired and could not be renewed on this network.\n\
     Nothing is wrong with any server, and none has been written off.\n\
     Start Tor (sudo systemctl start tor) and run `pvpn up` again, or connect \
     from a network that does not block Proton's API.";

/// Proton said "sign in" because the desktop keyring was locked.
const KEYRING_LOCKED_DETAIL: &str = "the desktop keyring was locked, so Proton could not read your \
still-signed-in session. Unlock the keyring (or unlock your desktop login) and try again — \
do not sign out; `protonvpn signin` will only say you are already signed in.";

/// Said when every ranked attempt dies on a locked keyring.
const KEYRING_LOCKED_DEAD_END: &str = "The desktop keyring was locked, so Proton could not read your \
still-signed-in session.\nNothing is wrong with any server, and none has been written off.\n\
Unlock the keyring (or unlock your desktop login) and run `pvpn up` again — do not sign out.";

/// Renew Proton's client certificate, returning true if the renewal landed.
///
/// Proton's own refresher only ever tries the direct path, and on a filtered
/// network it logs `Certificate refresh failed: No working transports
/// found`. After that every connect reaches `Connected` and the local agent
/// drops it a second later with `ExpiredCertificate`. `protonvpn servers`
/// is what drives all three refreshers — server list, certificate, client
/// config — so it is the lever for all of them.
///
/// Direct whenever the API answers, Tor only when it does not. Direct is
/// the faster route and the one Proton serves reliably, and the case this
/// is most often reached from — a tunnel that is already up and carrying —
/// has already routed around the block Tor existed to dodge. Routing is
/// untouched either way, so the caller's internet keeps working.
pub async fn renew_certificate(cfg: &Config) -> bool {
    let direct = matches!(
        blocking(|| net::proton_api_health(6)).await,
        net::ApiHealth::Answering(_)
    );
    let via_tor = !direct && blocking(proc::tor_available).await;
    if !direct && !via_tor {
        tracing::warn!(
            "the client certificate needs renewing, Proton's API is blocked here and Tor is not running. Start it with: sudo systemctl start tor"
        );
        return false;
    }

    let _ = paths::ensure_shim();
    let timeout = Duration::from_secs(cfg.refresh_timeout_secs);
    if direct {
        tracing::info!("renewing the client certificate (Proton's API is reachable from here)");
    } else {
        tracing::info!("renewing the client certificate over Tor (routing untouched)");
    }

    match blocking(move || cert::renew(via_tor, timeout)).await {
        Ok(renewed) => {
            tracing::info!(
                "certificate renewed — {}",
                renewed.describe(Utc::now())
            );
            true
        }
        Err(err) => {
            tracing::warn!("could not renew the certificate: {err}");
            false
        }
    }
}

/// Renew opportunistically, from inside a tunnel that already works.
///
/// This is the one that stops the deadlock happening at all. Proton issues
/// a seven-day certificate and wants it renewed on day two, leaving a
/// five-day window in which renewal costs one API call over a tunnel that
/// is already carrying — no Tor, no interception to route around, nothing
/// for the user to notice. Miss the whole window and the renewal has to
/// happen *before* any tunnel exists, on a network that filters Proton by
/// name, which is exactly the corner this tool kept ending up in.
///
/// Never fails a connect. The tunnel is up and working; a certificate for
/// next week is a bonus, not a precondition.
async fn renew_certificate_opportunistically(cfg: &Config) {
    let Some(status) = blocking(cert::status).await else {
        return;
    };
    let now = Utc::now();
    if !status.renewal_due(now) {
        return;
    }
    tracing::info!(
        "client certificate {} and Proton's renewal point has passed — renewing through the tunnel",
        status.describe(now)
    );
    if !renew_certificate(cfg).await {
        // Worth saying and not worth acting on: this tunnel is fine, and
        // the next connect will try again while there is still time.
        tracing::warn!(
            "could not renew the certificate through this tunnel — it still works, but renew before {}",
            status.describe(now)
        );
    }
}

/// Refuse to start a connect on a certificate Proton will reject.
///
/// An expired certificate fails every server identically, one full settle
/// window each. Before this existed the only way to find out was to spend
/// those windows: the fast path cannot detect it at all, and the slow path
/// only learns after Proton has written `ExpiredCertificate` into its log.
/// Reading the expiry costs a couple of hundred milliseconds and happens
/// before anything touches the routing table.
///
/// Returns the reason to stop, or `None` to carry on.
async fn ensure_usable_certificate(cfg: &Config) -> Option<String> {
    // No keyring, no opinion — the log-reading path still catches this
    // after the fact, exactly as it did before.
    let status = blocking(cert::status).await?;
    let now = Utc::now();
    if !status.unusable(now) {
        return None;
    }

    tracing::warn!(
        "Proton's client certificate {} — no server can work until it is renewed, so nothing will be blamed on one",
        status.describe(now)
    );
    if renew_certificate(cfg).await {
        return None;
    }
    Some(CERT_DEAD_END.to_string())
}

async fn drop_stale_tunnel() {
    let connected = if blocking(proc::proton_profile_active).await {
        true
    } else {
        blocking(|| {
            proc::protonvpn_status()
                .ok()
                .map(|result| proc::is_connected(&result.stdout))
                .unwrap_or(false)
        })
        .await
    };
    if !connected {
        return;
    }
    tracing::warn!("something reconnected while we were working — disconnecting");
    restore().await;
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
    // Asking for a tunnel cancels an earlier `pvpn down`. Cleared on entry,
    // not on success: the user asked for a tunnel either way, so a later
    // resume should try again rather than stay down because this network
    // happened to be hostile tonight.
    intent::clear_down(&Config::data_dir());
    // What we know about servers is filed per network; make sure we are
    // reading the right set before ranking or connecting.
    session.sync_network().await;
    let cfg = session.config.clone();

    if let Some(report) = reuse_healthy_tunnel(session).await {
        return report;
    }

    let dbus_now = blocking(pvpn_core::dbus::active_proton_server).await;
    let status = if dbus_now.is_none() {
        blocking(|| proc::protonvpn_status().ok()).await
    } else {
        None
    };
    let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
    let status_connected = proc::is_connected(stdout);
    let mut now = dbus_now.or_else(|| {
        if status_connected {
            proc::current_server(stdout)
        } else {
            None
        }
    });
    if now.is_none() {
        now = blocking(proc::active_proton_server).await;
    }
    if status_connected || now.is_some() {
        tracing::info!(
            "disconnecting {} before establishing a fresh connection",
            now.as_deref().unwrap_or("the active VPN")
        );
        if let Some(server) = now.as_deref() {
            if server_is_proven_here(session, server) {
                preserve_then_disconnect(server).await;
            }
        }
        restore().await;
    }

    if let Some(problem) = local_network_problem().await {
        return UpReport {
            ok: false,
            message: problem,
            server: None,
        };
    }

    // Before the fast path, not after it. The fast path activates a saved
    // NetworkManager profile over D-Bus and never runs Proton's client, so
    // it has no log to read and cannot tell an expired certificate of ours
    // from a dead server — it just waits out the settle window and blocks
    // whatever it was pointed at.
    if let Some(problem) = ensure_usable_certificate(&cfg).await {
        return UpReport {
            ok: false,
            message: problem,
            server: None,
        };
    }

    if let Some(report) = activate_saved_fast_path(session).await {
        return report;
    }

    // This check only affects narration, so hide it behind the work that
    // actually decides and establishes the connection.
    let api_check = tokio::task::spawn_blocking(|| net::proton_api_health(4));

    let proto = prepare_protocol(protocol.clone(), session).await;
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

    // Reachability and usability are different questions, and only the
    // first one decides whether to blackhole the API in `/etc/hosts`: a
    // 503 still proves this network delivers our packets to Proton, so
    // there is nothing here to route around. It is worth *saying*, though
    // — an outage reported as a "normal network" sends the user looking
    // for a fault on their side that does not exist.
    let health = api_check.await.unwrap_or(net::ApiHealth::Unreachable);
    let blackholed_by_us = matches!(health, net::ApiHealth::Unreachable)
        && blocking(proc::api_hosts_blackholed).await;
    match health {
        net::ApiHealth::Down(code) => tracing::warn!(
            "Proton's API answers {code} — this network is fine, Proton is not"
        ),
        net::ApiHealth::Answering(_) => {
            tracing::info!("Proton's API is reachable — normal network")
        }
        net::ApiHealth::Intercepted(code) => tracing::warn!(
            "something answered {code} for Proton's API but it was not Proton — \
             this network is intercepting it"
        ),
        // Ask who made it unreachable before blaming the network. Once our
        // own blackhole is in, it *is* the reason the API does not answer —
        // so the un-checked version of this line told you to apply a fix you
        // had already applied, every single connect, forever.
        net::ApiHealth::Unreachable if blackholed_by_us => {
            tracing::info!(
                "Proton's API is blackholed in /etc/hosts by us — that is why it does not answer. \
                 Undo with: pvpn fix --unhosts"
            )
        }
        net::ApiHealth::Unreachable => tracing::warn!(
            "filtered network — skipping the /etc/hosts API blackhole (needs sudo). Use: pvpn fix --hosts"
        ),
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
            disconnect_and_wait().await;
        }
        let current_network = blocking(proc::active_network_key).await;
        if current_network != network {
            restore().await;
            return UpReport {
                ok: false,
                message: format!(
                    "The network changed from {network} to {current_network} while preparing the connection. Run `pvpn up` again so server evidence stays on the correct network."
                ),
                server: None,
            };
        }
        attempt += 1;

        let attempt_started = Utc::now();
        let attempt_timer = Instant::now();
        let api_budget = api_budget_secs().await;
        let shim_c = shim.clone();
        let target_c = target.clone();
        let result =
            blocking(move || proc::protonvpn_connect(target_c.as_deref(), &shim_c, timeout, api_budget)).await;
        let result = match result {
            Ok(r) => r,
            Err(err) => {
                tracing::error!("spawn connect: {err}");
                n += 1;
                continue;
            }
        };
        let log = format!("{}\n{}", result.stdout, result.stderr);
        let dbus_server = blocking(pvpn_core::dbus::active_proton_server).await;
        let status = if dbus_server.is_none() {
            blocking(|| proc::protonvpn_status().ok()).await
        } else {
            None
        };
        let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");

        if result.success && (dbus_server.is_some() || proc::is_connected(stdout)) {
            let got = dbus_server.or_else(|| proc::current_server(stdout));
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
                        CERT_DEAD_END.to_string(),
                    server: None,
                };
            }

            let ready_ms = attempt_timer.elapsed().as_millis().min(u64::MAX as u128) as u64;
            record(
                session,
                &server,
                &proto,
                outcome,
                Some(&verdict),
                settled.then_some(ready_ms),
            )
            .await;

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
                    CERT_DEAD_END.to_string(),
                server: None,
            };
        }
        if matches!(outcome, ConnectOutcome::KeyringLocked) {
            tracing::warn!(
                "Proton asked for sign-in because the desktop keyring was locked — the account is still signed in"
            );
            if let Some(name) = &target {
                record(session, name, &proto, outcome, None, None).await;
            }
            restore().await;
            return UpReport {
                ok: false,
                message: KEYRING_LOCKED_DEAD_END.to_string(),
                server: None,
            };
        }
        if let Some(name) = &target {
            record(session, name, &proto, outcome, None, None).await;
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
    // Record the intent before touching anything. The opt-in resume hook
    // reads this and stays out of the way; without it, a suspend would put
    // back the tunnel the user just asked to be rid of. Set it first so it
    // holds even when the teardown below ends in a warning.
    intent::mark_down(&Config::data_dir());

    // NetworkManager owns the actual tunnel. Proton's status can remain
    // "Connected" after that profile has already disappeared.
    let current = blocking(proc::active_proton_server).await;
    if let (Some(server), Ok(mut session)) = (current.as_deref(), Session::load()) {
        session.sync_network().await;
        if server_is_proven_here(&session, server) {
            preserve_then_disconnect(server).await;
        }
    }
    let restored = restore().await;
    tidy_duplicate_profiles().await;
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

/// `pvpn up` takes no arguments. Say so, and say where the argument goes.
///
/// `up` used to accept a protocol, which read as harmless until you typed
/// `pvpn up SG-FREE#2`: no protocol is named `SG-FREE#2`, so it fell back to
/// Stealth, connected to whatever the ranking had picked, and reported
/// success for a server nobody asked for. Three commands in a row can be
/// spent that way with nothing in the output admitting the target was
/// discarded.
///
/// Choosing anything is `hop`'s job, so the fix is to have one verb that
/// chooses for you and one you steer, rather than a first positional whose
/// meaning depends on whether it happens to collide with a protocol name.
/// Returns the message to print, or `None` when there is nothing to reject.
pub fn up_takes_no_arguments(args: &[String]) -> Option<String> {
    let first = args.first()?;
    let suggestion = if looks_like_protocol(first) {
        format!("For a protocol, name a server too: pvpn hop <server> {first}")
    } else {
        format!("To choose a server: pvpn hop {first}")
    };
    Some(format!(
        "pvpn up takes no arguments — it connects to the fastest measured server.\n{suggestion}"
    ))
}

fn looks_like_protocol(arg: &str) -> bool {
    proc::TRY_PROTOCOLS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(arg))
        // Anything from a backend family we know, so an unfamiliar protocol
        // is still recognised as one and pointed at the right verb.
        || ["protun", "openvpn", "wireguard"]
            .iter()
            .any(|family| arg.to_lowercase().starts_with(family))
}

pub async fn hop(
    session: &mut Session,
    pattern: Option<String>,
    protocol: Option<String>,
) -> UpReport {
    let _ = paths::ensure_shim();
    // Same as `up`: hopping is asking for a tunnel.
    intent::clear_down(&Config::data_dir());
    // What we know about servers is filed per network; make sure we are
    // reading the right set before ranking or connecting.
    session.sync_network().await;

    // Preserve only a profile that NetworkManager says is active. Proton's
    // status is advisory and commonly lags behind a failed tunnel.
    let before = blocking(proc::active_proton_server).await;

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
        // `pvpn up SG-FREE#2` lands here, and an expired certificate fails
        // every server identically. Without this the named server takes the
        // blame and gets blocked for a fault that was ours.
        if let Some(problem) = ensure_usable_certificate(&session.config.clone()).await {
            return UpReport {
                ok: false,
                message: problem,
                server: None,
            };
        }
    }
    if pattern.is_none() {
        if let Some(server) = before.as_deref() {
            if server_is_proven_here(session, server) {
                preserve_then_disconnect(server).await;
            }
        }
    }

    match pattern {
        Some(want) => hop_to_pattern(session, want, before, protocol).await,
        None => hop_to_next_best(session, before, protocol).await,
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
async fn hop_to_next_best(
    session: &mut Session,
    before: Option<String>,
    protocol: Option<String>,
) -> UpReport {
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

    let proto = match prepare_protocol(protocol, session).await {
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
        let attempt = match connect_and_verify(
            session,
            Some(target),
            Some(target.as_str()),
            &proto,
            &network,
            true,
        )
        .await
        {
            ConnectAttempt::Connected(attempt) => attempt,
            ConnectAttempt::Failed { .. } => continue,
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
async fn hop_to_pattern(
    session: &mut Session,
    want: String,
    before: Option<String>,
    protocol: Option<String>,
) -> UpReport {
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
    let (steered, forced_unavailable, local_only) = match steered {
        Ok(s) => (Some(s), false, false),
        Err(cache::SteerError::NoMatch) if want.contains('#') => {
            tracing::warn!(
                "{want} is absent from Proton's current inventory; trying the locally saved \
                 verified profile"
            );
            (None, true, true)
        }
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
        Err(cache::SteerError::Unavailable) => {
            let forced = {
                let serverlist = serverlist.clone();
                let want = want.clone();
                blocking(move || cache::steer_cache(&serverlist, SteerMode::ForceOnly, &want)).await
            };
            match forced {
                Ok(steered) => {
                    tracing::warn!(
                        "Proton marks {want} unavailable; trying its current cached endpoint {} \
                         anyway, then the locally saved profile if needed",
                        steered.endpoint.as_deref().unwrap_or("unknown")
                    );
                    (Some(steered), true, false)
                }
                Err(cache::SteerError::NoEndpoint) => {
                    tracing::warn!(
                        "{want} has no endpoint in Proton's current inventory; trying the locally \
                         saved verified profile"
                    );
                    (None, true, true)
                }
                Err(err) => {
                    return UpReport {
                        ok: false,
                        message: err.to_string(),
                        server: before,
                    }
                }
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

    let proto = match prepare_protocol(protocol, session).await {
        Ok(p) => p,
        Err(err) => {
            if let Some(steered) = &steered {
                cache::restore_cache(&steered.backup, &serverlist);
            }
            return UpReport {
                ok: false,
                message: err.to_string(),
                server: None,
            };
        }
    };

    if let Some(server) = before.as_deref() {
        if server_is_proven_here(session, server) {
            preserve_then_disconnect(server).await;
        }
    }

    let network = session.network().to_string();
    let primary = if local_only {
        match activate_saved_and_verify(session, &want, &network).await {
            Ok(Some(attempt)) => ConnectAttempt::Connected(attempt),
            Ok(None) => ConnectAttempt::Failed {
                outcome: ConnectOutcome::ClientError,
                detail: "no locally saved verified profile exists".to_string(),
            },
            Err(err) => ConnectAttempt::Failed {
                outcome: ConnectOutcome::ClientError,
                detail: err.to_string(),
            },
        }
    } else {
        connect_and_verify(
            session,
            None,
            Some(&want),
            &proto,
            &network,
            !forced_unavailable,
        )
        .await
    };
    if let Some(steered) = &steered {
        cache::restore_cache(&steered.backup, &serverlist);
    }

    let mut used_local_fallback = local_only;
    let attempt = match primary {
        ConnectAttempt::Connected(attempt)
            if forced_unavailable && !local_only && !attempt.verdict.carrying() =>
        {
            restore().await;
            tracing::warn!(
                "Proton's current endpoint for {want} did not carry traffic; trying the locally \
                 saved verified profile"
            );
            match activate_saved_and_verify(session, &want, &network).await {
                Ok(Some(fallback)) => {
                    used_local_fallback = true;
                    fallback
                }
                Ok(None) => {
                    record(
                        session,
                        &attempt.server,
                        &proto,
                        attempt.outcome,
                        Some(&attempt.verdict),
                        None,
                    )
                    .await;
                    attempt
                }
                Err(err) => {
                    tracing::warn!("local profile fallback failed: {err}");
                    record(
                        session,
                        &attempt.server,
                        &proto,
                        attempt.outcome,
                        Some(&attempt.verdict),
                        None,
                    )
                    .await;
                    attempt
                }
            }
        }
        ConnectAttempt::Connected(attempt) => {
            if forced_unavailable && !local_only {
                record(
                    session,
                    &attempt.server,
                    &proto,
                    attempt.outcome,
                    Some(&attempt.verdict),
                    None,
                )
                .await;
            }
            attempt
        }
        ConnectAttempt::Failed { outcome, detail } if local_only => {
            record(session, &want, &proto, outcome, None, None).await;
            restore().await;
            return UpReport {
                ok: false,
                message: format!(
                    "Proton's current inventory had no endpoint for {want}, and the local saved \
                     profile could not connect: {detail}"
                ),
                server: None,
            };
        }
        ConnectAttempt::Failed { outcome, detail }
            if !local_only
                && (forced_unavailable || matches!(outcome, ConnectOutcome::KeyringLocked)) =>
        {
            restore().await;
            if matches!(outcome, ConnectOutcome::KeyringLocked) {
                tracing::warn!(
                    "Proton could not read the signed-in session (keyring locked); trying the \
                     locally saved verified profile for {want}"
                );
            } else {
                tracing::warn!(
                    "Proton's current endpoint could not start {want}; trying the locally saved \
                     verified profile"
                );
            }
            match activate_saved_and_verify(session, &want, &network).await {
                Ok(Some(fallback)) => {
                    used_local_fallback = true;
                    fallback
                }
                fallback => {
                    // Normal hops already recorded the primary failure inside
                    // connect_and_verify. The forced-unavailable path deferred
                    // that so a successful local fallback would not leave a
                    // bogus block attempt behind.
                    if forced_unavailable {
                        record(session, &want, &proto, outcome, None, None).await;
                    }
                    let fallback_detail = match fallback {
                        Ok(None) => "no locally saved profile exists".to_string(),
                        Err(err) => err.to_string(),
                        Ok(Some(_)) => unreachable!(),
                    };
                    return UpReport {
                        ok: false,
                        message: if matches!(outcome, ConnectOutcome::KeyringLocked) {
                            format!(
                                "{want}: {detail}\nLocal saved-profile fallback also failed: \
                                 {fallback_detail}"
                            )
                        } else {
                            format!(
                                "Proton marks {want} unavailable. Its current endpoint failed: \
                                 {detail}\nLocal fallback also failed: {fallback_detail}"
                            )
                        },
                        server: None,
                    };
                }
            }
        }
        ConnectAttempt::Failed { outcome, detail } => {
            restore().await;
            return UpReport {
                ok: false,
                message: if outcome.blames_the_server() {
                    format!(
                        "{want} was found in the account inventory, but its connection failed: \
                         {detail}\nIt was blocked here and its saved system VPN was removed."
                    )
                } else {
                    format!(
                        "{want} was found in the account inventory, but Proton could not start \
                         its tunnel: {detail}\nThis was classified as {}, so the server was not \
                         blocked and its saved system VPN was kept.",
                        outcome.tag()
                    )
                },
                server: None,
            };
        }
    };

    let fix = cfg.fix_apps;
    blocking(move || apps_hook::enforce_app_routing(fix)).await;

    let from = before.unwrap_or_default();
    let to = attempt.server;
    if attempt.verdict.carrying() {
        let source = if used_local_fallback {
            " using the locally saved profile"
        } else if forced_unavailable {
            " using Proton's current cached endpoint despite its unavailable flag"
        } else {
            ""
        };
        return UpReport {
            ok: true,
            message: if from.is_empty() {
                format!("Hopped to {to}{source}")
            } else {
                format!("Hopped: {from} -> {to}{source}")
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

/// How long the connect's own refresher may spend on Proton's API.
///
/// The shim fast-fails API calls so a blocked network does not stall every
/// connect. That is the right default and the wrong one whenever the API can
/// actually be reached — a `connect` is the only thing that refreshes the
/// server list, so starving it there is what leaves the cache permanently
/// stale. Ask first, then spend accordingly.
async fn api_budget_secs() -> u64 {
    match blocking(|| net::proton_api_health(4)).await {
        net::ApiHealth::Answering(_) => proc::API_TIMEOUT_REACHABLE_SECS,
        _ => proc::API_TIMEOUT_BLOCKED_SECS,
    }
}

/// Put the connect protocol where it needs to be.
///
/// Stealth is the default because it is the only one the filtered networks
/// this tool exists for reliably let through: they pass TCP 443 and drop
/// UDP to every port OpenVPN and WireGuard use. `want` overrides that, so a
/// network where the assumption does not hold can be tested and used
/// without editing Proton's settings by hand — on detnsw, Stealth clears the
/// port filter but dies waiting on Proton's local agent, which makes the
/// override the difference between one working protocol and none.
async fn prepare_protocol(want: Option<String>, session: &Session) -> anyhow::Result<String> {
    let want = want.or_else(|| {
        let proven = session.state.proven_protocol();
        if let Some(p) = &proven {
            if p != DEFAULT_PROTOCOL {
                tracing::info!("{p} is what last carried traffic on this network — using it");
            }
        }
        proven
    });
    let want = want.unwrap_or_else(|| DEFAULT_PROTOCOL.to_string());
    blocking(move || proc::ensure_connect_protocol(&want)).await
}

/// The opening guess on a network nothing is known about yet.
///
/// Stealth-over-TLS because these networks pass TCP 443 and little else.
/// It is only ever a guess: [`State::proven_protocol`] replaces it as soon
/// as one connect on this network has actually carried traffic.
const DEFAULT_PROTOCOL: &str = "protun-tls";

/// One connect attempt with the full verification behind it, recorded.
/// `None` means the tunnel never came up at all.
struct Attempt {
    server: String,
    outcome: ConnectOutcome,
    verdict: Verdict,
}

enum ConnectAttempt {
    Connected(Attempt),
    Failed {
        outcome: ConnectOutcome,
        detail: String,
    },
}

fn connect_failure_detail(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .find(|line| {
            let lower = line.to_lowercase();
            lower.contains("error")
                || lower.contains("failed")
                || lower.contains("not available")
                || lower.contains("not allowed")
                || lower.contains("unable")
                || lower.contains("authentication required")
        })
        .or_else(|| lines.last())
        .copied()
        .unwrap_or("Proton exited before creating a tunnel")
        .to_string()
}

/// Prefer a human explanation over Proton's false "please sign in" wording.
fn explain_connect_failure(outcome: ConnectOutcome, log: &str) -> String {
    match outcome {
        ConnectOutcome::KeyringLocked => KEYRING_LOCKED_DETAIL.to_string(),
        _ => connect_failure_detail(log),
    }
}

fn selected_server_matches(actual: &str, expected: Option<&str>) -> bool {
    expected.is_none_or(|want| cache::name_matches(actual, want))
}

async fn verify_connected_server(
    session: &mut Session,
    server: String,
    proto: &str,
    network: &str,
    started: DateTime<Utc>,
    timer: Instant,
    record_result: bool,
) -> Attempt {
    let settle_secs = session.config.settle_secs;
    tracing::info!("tunnel up — verifying that {server} carries traffic");
    let verdict = verify::verify(started, Duration::from_secs(settle_secs), network).await;
    let outcome = outcome_for(&verdict, started).await;
    narrate(&server, &verdict, settle_secs);
    if record_result {
        let ready_ms = timer.elapsed().as_millis().min(u64::MAX as u128) as u64;
        record(
            session,
            &server,
            proto,
            outcome,
            Some(&verdict),
            verdict.carrying().then_some(ready_ms),
        )
        .await;
    }
    Attempt {
        server,
        outcome,
        verdict,
    }
}

/// Activate the locally saved profile for a server and see whether it works.
///
/// Deliberately takes no protocol: the caller's intended protocol is not what
/// a saved NetworkManager profile uses, and recording it as though it were is
/// what taught this tool that `protun-tls` was the fast choice on a network
/// where it takes a minute. The protocol is read back off the profile that
/// actually came up.
async fn activate_saved_and_verify(
    session: &mut Session,
    server: &str,
    network: &str,
) -> anyhow::Result<Option<Attempt>> {
    let started = Utc::now();
    let timer = Instant::now();
    let server_name = server.to_string();
    let activated =
        blocking(move || proc::activate_verified_proton_connection(&server_name)).await?;
    if !activated {
        return Ok(None);
    }
    let active = blocking(proc::active_proton_server).await;
    let Some(active) = active.filter(|active| active.eq_ignore_ascii_case(server)) else {
        anyhow::bail!("NetworkManager activated a different VPN profile");
    };
    let protocol = blocking(proc::active_profile_protocol)
        .await
        .unwrap_or_default();
    Ok(Some(
        verify_connected_server(session, active, &protocol, network, started, timer, true).await,
    ))
}

async fn connect_and_verify(
    session: &mut Session,
    connect_target: Option<&String>,
    expected_server: Option<&str>,
    proto: &str,
    network: &str,
    record_result: bool,
) -> ConnectAttempt {
    let cfg = session.config.clone();
    disconnect_and_wait().await;

    let shim = paths::shim_dir();
    let timeout = Duration::from_secs(cfg.connect_timeout_secs);
    let started = Utc::now();
    let timer = Instant::now();
    let api_budget = api_budget_secs().await;
    let target_c = connect_target.cloned();
    let expected = expected_server
        .map(str::to_string)
        .or_else(|| connect_target.cloned());
    let result = match blocking(move || {
        proc::protonvpn_connect(target_c.as_deref(), &shim, timeout, api_budget)
    })
    .await
    {
        Ok(result) => result,
        Err(err) => {
            let outcome = ConnectOutcome::ClientError;
            if record_result {
                if let Some(name) = expected.as_deref() {
                    record(session, name, proto, outcome, None, None).await;
                }
            }
            return ConnectAttempt::Failed {
                outcome,
                detail: err.to_string(),
            };
        }
    };

    let dbus_server = blocking(pvpn_core::dbus::active_proton_server).await;
    let status = if dbus_server.is_none() {
        blocking(|| proc::protonvpn_status().ok()).await
    } else {
        None
    };
    let stdout = status.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
    if !result.success || (dbus_server.is_none() && !proc::is_connected(stdout)) {
        let log = format!("{}\n{}", result.stdout, result.stderr);
        let outcome = diagnose(started, Some(&log)).await;
        if record_result {
            if let Some(name) = expected.as_deref() {
                record(session, name, proto, outcome, None, None).await;
            }
        }
        let detail = explain_connect_failure(outcome, &log);
        tracing::warn!(
            "{} did not connect: {detail}",
            expected.as_deref().unwrap_or("that server")
        );
        return ConnectAttempt::Failed { outcome, detail };
    }

    let server = dbus_server
        .or_else(|| proc::current_server(stdout))
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(want) = expected.as_deref() {
        if !selected_server_matches(&server, Some(want)) {
            let detail = format!("Proton selected {server} instead of the requested target {want}");
            if record_result && want.contains('#') {
                record(
                    session,
                    want,
                    proto,
                    ConnectOutcome::ClientError,
                    None,
                    None,
                )
                .await;
            }
            let _ = blocking(proc::protonvpn_disconnect).await;
            tracing::warn!("{detail}");
            return ConnectAttempt::Failed {
                outcome: ConnectOutcome::ClientError,
                detail,
            };
        }
    }
    ConnectAttempt::Connected(
        verify_connected_server(
            session,
            server,
            proto,
            network,
            started,
            timer,
            record_result,
        )
        .await,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pre_tunnel_error_keeps_the_useful_proton_explanation() {
        assert_eq!(
            super::connect_failure_detail(
                "noise\nError: server selection is not available for this account\nmore noise"
            ),
            "Error: server selection is not available for this account"
        );
    }

    #[test]
    fn a_locked_keyring_is_not_reported_as_please_sign_in() {
        let detail = super::explain_connect_failure(
            crate::blocklist::ConnectOutcome::KeyringLocked,
            "Error: Authentication required.Please sign in with 'protonvpn signin' before connecting.",
        );
        assert!(detail.contains("keyring was locked"));
        assert!(detail.contains("still-signed-in"));
        assert!(!detail.to_lowercase().contains("please sign in"));
    }

    #[test]
    fn a_different_server_never_satisfies_an_exact_hop() {
        assert!(!super::selected_server_matches(
            "JP-FREE#11",
            Some("JP-FREE#33")
        ));
        assert!(super::selected_server_matches(
            "JP-FREE#33",
            Some("JP-FREE#33")
        ));
    }

    #[test]
    fn a_country_hop_accepts_a_server_in_that_country() {
        assert!(super::selected_server_matches("JP-FREE#11", Some("JP")));
        assert!(!super::selected_server_matches("SG-FREE#11", Some("JP")));
    }

    #[test]
    fn a_server_name_given_to_up_is_refused_and_redirected() {
        // `pvpn up SG-FREE#2` used to resolve as a protocol, fail, fall back
        // to Stealth, and connect to whatever the ranking had picked — which
        // on 2026-09-02 was SG-FREE#13, announced as a success. Anything but
        // silence is an improvement; naming the right command is the point.
        let message = super::up_takes_no_arguments(&["SG-FREE#2".to_string()])
            .expect("an argument must be refused, never quietly discarded");
        assert!(message.contains("takes no arguments"));
        assert!(message.contains("pvpn hop SG-FREE#2"));
    }

    #[test]
    fn a_protocol_given_to_up_is_pointed_at_hops_second_argument() {
        for name in pvpn_core::proc::TRY_PROTOCOLS {
            let message = super::up_takes_no_arguments(&[name.to_string()])
                .unwrap_or_else(|| panic!("{name} must be refused"));
            assert!(
                message.contains(&format!("pvpn hop <server> {name}")),
                "{name} should be pointed at hop's protocol argument, got: {message}"
            );
        }
        // A backend this build has not heard of is still recognisably a
        // protocol, so it gets the protocol advice rather than being read as
        // a server name.
        let message = super::up_takes_no_arguments(&["protun-quic".to_string()]).unwrap();
        assert!(message.contains("pvpn hop <server> protun-quic"));
    }

    #[test]
    fn bare_up_still_chooses_for_itself() {
        assert_eq!(super::up_takes_no_arguments(&[]), None);
    }
}
