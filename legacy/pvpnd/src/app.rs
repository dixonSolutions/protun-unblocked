//! Shared daemon state. Every long-running task (`rpc`, `supervisor`,
//! `prober`) holds a clone of `App`.

use crate::logbus::LogBus;
use pvpn_core::config::Config;
use pvpn_core::ipc::StatusPayload;
use pvpn_core::proc;
use pvpn_core::state::State;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Disconnected => "Disconnected",
            Phase::Connecting => "Connecting",
            Phase::Connected => "Connected",
            Phase::Reconnecting => "Reconnecting",
        }
    }
}

#[derive(Clone)]
pub struct App {
    pub config: Arc<RwLock<Config>>,
    pub persist: Arc<RwLock<State>>,
    pub phase: Arc<RwLock<Phase>>,
    /// User wants a tunnel. Cleared by `Down`. Supervisor reconnects only
    /// while this is true and `auto_reconnect` is on.
    pub want_up: Arc<AtomicBool>,
    /// A connect/hop/measure is in flight. The prober skips those windows
    /// so it never competes for uplink with a real attempt.
    pub busy: Arc<AtomicBool>,
    /// Serialises restore/connect/hop so two RPC clients cannot tear the
    /// routing table in opposite directions at once.
    pub connect_lock: Arc<Mutex<()>>,
    pub last_error: Arc<RwLock<Option<String>>>,
    /// Daemon log events, so `rpc` can narrate a long request back to the
    /// CLI that asked for it instead of only to the journal.
    pub logbus: LogBus,
}

impl App {
    pub fn new(config: Config, persist: State, logbus: LogBus) -> Self {
        // Resume whatever the last run was told to want.
        let want_up = persist.want_up;
        Self {
            config: Arc::new(RwLock::new(config)),
            persist: Arc::new(RwLock::new(persist)),
            phase: Arc::new(RwLock::new(Phase::Disconnected)),
            want_up: Arc::new(AtomicBool::new(want_up)),
            busy: Arc::new(AtomicBool::new(false)),
            connect_lock: Arc::new(Mutex::new(())),
            last_error: Arc::new(RwLock::new(None)),
            logbus,
        }
    }

    pub async fn set_phase(&self, phase: Phase) {
        *self.phase.write().await = phase;
    }

    /// Record that the user does (or does not) want a tunnel, on disk as
    /// well as in memory, so a restart resumes instead of forgetting.
    pub async fn set_want_up(&self, want: bool) {
        tracing::info!(
            "{}",
            if want {
                "a tunnel is wanted — the supervisor will keep one up"
            } else {
                "staying down until asked otherwise"
            }
        );
        self.want_up.store(want, Ordering::SeqCst);
        self.persist.write().await.want_up = want;
        self.persist_save().await;
    }

    /// File observations under whatever network this machine is on now.
    ///
    /// Must run before anything reads or writes them. Which servers work is
    /// a property of the network, not of the laptop — see
    /// [`pvpn_core::state::State::set_network`] — so moving between a
    /// filtered school wifi and a phone hotspot has to move the whole set
    /// of measurements and blocks with it, not blend them.
    /// Returns true if this is a different network than the last check —
    /// callers use that to throw away verdicts formed on the old one.
    pub async fn sync_network(&self) -> bool {
        let key = blocking(proc::active_network_key).await;
        let changed = self.persist.write().await.set_network(key.clone());
        if changed {
            tracing::info!("on {key} — using what we know about servers here");
        }
        changed
    }

    pub async fn persist_save(&self) {
        let path = Config::state_path();
        let guard = self.persist.read().await;
        if let Err(err) = guard.save(&path) {
            tracing::warn!("failed to persist state: {err}");
        }
    }

    pub async fn snapshot_status(&self) -> StatusPayload {
        let result = tokio::task::spawn_blocking(|| proc::protonvpn_status().ok())
            .await
            .ok()
            .flatten();
        let stdout = result.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
        let connected = proc::is_connected(stdout);
        let server = proc::current_server(stdout);
        let server_desc = proc::current_server_desc(stdout);
        let protocol = tokio::task::spawn_blocking(proc::current_protocol)
            .await
            .unwrap_or_else(|_| "unknown".to_string());
        let supervisor_state = self.phase.read().await.as_str().to_string();
        StatusPayload {
            connected,
            server,
            server_desc,
            protocol,
            supervisor_state,
            message: None,
        }
    }
}

pub async fn blocking<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking task panicked")
}
