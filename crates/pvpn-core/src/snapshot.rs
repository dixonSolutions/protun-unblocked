//! One reading of what is true right now: the tunnel, who is connecting,
//! and whether a reconnect would be allowed here. `pvpn query status`
//! prints it as JSON, and that is how the window reads it — the CLI is the
//! one place that knows how to ask.

use crate::config::Config;
use crate::{intent, lock, proc, scope};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Who holds the connect lock, when somebody does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Busy {
    pub pid: u32,
    /// The holder's own note: `pid N: pvpn up`.
    pub note: String,
    /// Started by the autoconnect hook or the watch timer, not a person.
    pub automatic: bool,
    /// A `pvpn down` rather than a connect.
    pub disconnecting: bool,
}

/// One reading, taken off the main thread. Only cheap sources — files,
/// the system bus, the routing table — except `carrying`, which sends a
/// request through the tunnel and is only asked for on a slower clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub network: String,
    /// Proton's client, or NetworkManager, believes a tunnel is up.
    pub connected: bool,
    /// The kernel actually routes through a tunnel device.
    pub tunneled: bool,
    pub server: Option<String>,
    pub protocol: String,
    /// `None` when not asked this time.
    pub carrying: Option<bool>,
    pub stray_route: Option<String>,
    pub busy: Option<Busy>,
    /// `pvpn-autoconnect` is running (it holds its own lock while it does).
    pub autoconnect_running: bool,
    pub down_by_user: bool,
    pub autoconnect_off: bool,
    /// Would a reconnect be allowed here, and why (or why not).
    pub autoconnect_here: Result<String, String>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            network: String::new(),
            connected: false,
            tunneled: false,
            server: None,
            protocol: String::new(),
            carrying: None,
            stray_route: None,
            busy: None,
            autoconnect_running: false,
            down_by_user: false,
            autoconnect_off: false,
            autoconnect_here: Err(String::new()),
        }
    }
}

impl Default for Busy {
    fn default() -> Self {
        Self {
            pid: 0,
            note: String::new(),
            automatic: false,
            disconnecting: false,
        }
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe_uid())))
}

fn unsafe_uid() -> u32 {
    // /proc/self is ours; its owner is our uid. Avoids a libc dependency.
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1000)
}

/// Was `pid` started with `PVPN_AUTOCONNECT` set (the hook's `pvpn up`)?
fn started_by_autoconnect(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/environ"))
        .map(|env| {
            env.split(|b| *b == 0)
                .any(|kv| kv.starts_with(b"PVPN_AUTOCONNECT=") && kv != b"PVPN_AUTOCONNECT=0")
        })
        .unwrap_or(false)
}

/// Who holds the connect lock, and whether the autoconnect hook is running.
/// Only `/proc` reads — cheap enough for the main thread every second,
/// which matters: the full reading below can stall for seconds on
/// NetworkManager while it activates a tunnel, which is exactly when this
/// needs answering.
pub fn read_busy(own_child: Option<u32>) -> (Option<Busy>, bool) {
    let busy = lock::in_flight().map(|(pid, note)| {
        let ours = own_child == Some(pid);
        Busy {
            automatic: !ours && (note.contains(" watch") || started_by_autoconnect(pid)),
            disconnecting: note.ends_with(" down") || note.contains(" down "),
            pid,
            note,
        }
    });
    let autoconnect_running = lock::locked_by(&runtime_dir().join("pvpn-autoconnect.lock")).is_some();
    (busy, autoconnect_running)
}

/// Read everything. Blocking — run it on a worker thread.
pub fn read(probe_traffic: bool, own_child: Option<u32>) -> Snapshot {
    let network = proc::active_network_key();
    let persisted = proc::proton_client_persisted();
    let nm_server = crate::dbus::active_proton_server_or_nmcli();
    let connected = persisted.is_some() || nm_server.is_some();
    let server = nm_server
        .clone()
        .or_else(|| persisted.as_ref().and_then(|p| p.server.clone()));
    let protocol = match persisted.as_ref().filter(|p| !p.protocol.is_empty()) {
        Some(p) => p.protocol.clone(),
        None => proc::active_profile_protocol().unwrap_or_else(proc::current_protocol),
    };
    let tunneled = connected && proc::tunnel_is_real();
    let stray_route = if tunneled {
        None
    } else {
        proc::stray_leak_route()
    };
    let carrying = (probe_traffic && tunneled).then(|| crate::net::net_works_within(3));

    let (busy, autoconnect_running) = read_busy(own_child);

    let config = Config::load(&Config::default_path()).unwrap_or_default();
    let started = scope::started_network(&Config::data_dir());
    let autoconnect_here = scope::allowed(
        &config.autoconnect_networks,
        &config.autoconnect_never,
        &network,
        started.as_deref(),
    );

    Snapshot {
        network,
        connected,
        tunneled,
        server,
        protocol,
        carrying,
        stray_route,
        busy,
        autoconnect_running,
        down_by_user: intent::is_down_by_user(&Config::data_dir()),
        autoconnect_off: intent::autoconnect_is_off(&Config::config_dir()),
        autoconnect_here,
    }
}

