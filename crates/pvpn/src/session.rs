//! One command's worth of state: the config, and what this machine knows
//! about servers on the network it is currently attached to.
//!
//! This is what is left of the daemon's `App` once nothing runs in the
//! background. There are no phases, no `want_up`, and no locks: a `pvpn`
//! process does exactly one thing and then exits, so the only shared
//! resource is `state.json`, which is written atomically.

use pvpn_core::config::Config;
use pvpn_core::proc;
use pvpn_core::state::State;

pub struct Session {
    pub config: Config,
    pub state: State,
}

/// What `pvpn status` reports. `tunneled` is the part `protonvpn status`
/// cannot tell you.
pub struct Status {
    pub connected: bool,
    pub server_desc: Option<String>,
    pub protocol: String,
    /// Is anything actually going through a tunnel device? A client that
    /// lost its session keeps reporting the server it lost while every
    /// packet leaves in the clear.
    pub tunneled: bool,
    /// A leak-guard interface left holding a default route with no tunnel
    /// behind it, blackholing whichever address family it owns.
    ///
    /// Only ever set when nothing is tunneled. While a tunnel is up,
    /// `ipv6leakintrf0` holding the IPv6 default route is Proton's leak
    /// protection doing its job, not a fault — reporting it there would
    /// mean `pvpn status` cried wolf on every healthy connection.
    pub stray_route: Option<String>,
}

impl Session {
    /// Load config and per-network state from disk.
    ///
    /// The network key has to be resolved before anything reads the
    /// observations, because they are filed per network: which servers
    /// work is a property of the network, not of the laptop, so moving
    /// between a filtered school wifi and a phone hotspot moves the whole
    /// set of measurements and blocks with it.
    pub fn load() -> anyhow::Result<Self> {
        let mut config = Config::load(&Config::default_path())?;
        config.apply_env_overrides();
        let mut state = State::load(&Config::state_path())?;
        state.set_network(proc::active_network_key());
        Ok(Self { config, state })
    }

    pub fn network(&self) -> &str {
        self.state.network()
    }

    /// Re-file observations if the link moved under us mid-command — a
    /// connect can take two minutes, which is long enough to roam.
    /// Returns true if this is a different network than the last check.
    pub async fn sync_network(&mut self) -> bool {
        let key = blocking(proc::active_network_key).await;
        let changed = self.state.set_network(key.clone());
        if changed {
            tracing::info!("on {key} — using what we know about servers here");
        }
        changed
    }

    pub fn save(&self) {
        let path = Config::state_path();
        if let Err(err) = self.state.save(&path) {
            tracing::warn!("failed to persist state: {err}");
        }
    }

    pub async fn status(&self) -> Status {
        let result = blocking(|| proc::protonvpn_status().ok()).await;
        let stdout = result.as_ref().map(|r| r.stdout.as_str()).unwrap_or("");
        let network_manager_server = blocking(proc::active_proton_server).await;
        let connected = proc::is_connected(stdout) || network_manager_server.is_some();
        let server_desc =
            proc::current_server_desc(stdout).or_else(|| network_manager_server.clone());
        let protocol = blocking(proc::current_protocol).await;
        // Only worth the probes when something claims to be up.
        let tunneled = connected && blocking(proc::tunnel_is_real).await;
        let stray_route = if tunneled {
            None
        } else {
            blocking(proc::stray_leak_route).await
        };
        Status {
            connected,
            server_desc,
            protocol,
            tunneled,
            stray_route,
        }
    }
}

/// Run a blocking call off the runtime threads. `protonvpn connect` can
/// hold for thirty seconds; the probe futures must keep making progress.
pub async fn blocking<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking task panicked")
}
