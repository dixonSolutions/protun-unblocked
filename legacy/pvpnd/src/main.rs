//! `pvpnd` — long-running user daemon for pvpn.
//!
//! Keeps the ranked retry list, the fast/blocked observations, and the
//! auto-reconnect loop alive across invocations. Talks to the `pvpn` CLI
//! over `$XDG_RUNTIME_DIR/pvpn.sock`.

mod app;
mod apps;
mod blocklist;
mod connect;
mod logbus;
mod prober;
mod rpc;
mod supervisor;

use app::App;
use logbus::LogBus;
use pvpn_core::config::Config;
use pvpn_core::state::State;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::net::UnixListener;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Two sinks for one set of events: stderr (the journal, as before) and
    // the log bus, which `rpc` forwards to a waiting CLI.
    let logbus = LogBus::new(256);
    tracing_subscriber::registry()
        // `pvpn_core` too: its two warnings (a missing shim, a certificate
        // that cannot be renewed) were being dropped on the floor by a
        // filter that only named the binary's own target.
        .with(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("pvpnd=info,pvpn_core=info")))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(logbus.clone())
        .init();

    let mut config = Config::load(&Config::default_path())?;
    config.apply_env_overrides();
    if !Config::default_path().exists() {
        if let Err(err) = config.save(&Config::default_path()) {
            tracing::warn!("could not write default config: {err}");
        }
    }

    let persist = State::load(&Config::state_path())?;
    let app = App::new(config, persist, logbus);

    let socket = Config::socket_path();
    let listener = bind_socket(&socket)?;
    tracing::info!("listening on {}", socket.display());

    // Before anything reads the persisted observations: they are filed per
    // network, and nothing knows which one this is until we look.
    app.sync_network().await;

    let warmup_app = app.clone();
    tokio::spawn(async move {
        connect::warmup_rank(&warmup_app).await;
    });

    let supervisor_app = app.clone();
    tokio::spawn(async move { supervisor::run(supervisor_app).await });

    let prober_app = app.clone();
    tokio::spawn(async move { prober::run(prober_app).await });

    let rpc_app = app.clone();
    tokio::spawn(async move { rpc::run(rpc_app, listener).await });

    shutdown_signal().await;
    tracing::info!("shutting down");
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => anyhow::bail!(
                "pvpnd is already running (socket {} accepts connections)",
                path.display()
            ),
            Err(_) => {
                tracing::warn!("removing stale socket {}", path.display());
                let _ = std::fs::remove_file(path);
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("register SIGTERM");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
