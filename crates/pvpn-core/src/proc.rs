//! Thin process wrappers around `protonvpn`, `nmcli` and `ip`.
//!
//! `pvpn` never reimplements Proton's VPN protocols, account auth, or
//! CAPTCHA bridge — `protonvpn` (and, for login, `lib/debug-signin.py` /
//! `lib/signin-bridge.py`) stay exactly what they are today. This module is
//! the orchestration layer around them, ported from bash to Rust, not a
//! replacement for them.

use anyhow::Context;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RunResult {
    pub success: bool,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `cmd args...` with the given extra env vars, killing it if it has
/// not exited within `timeout`. Stdout/stderr are captured concurrently on
/// reader threads so a chatty child cannot deadlock on a full pipe while
/// this thread is busy polling `try_wait`.
pub fn run_with_timeout(
    cmd: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    timeout: Duration,
) -> anyhow::Result<RunResult> {
    let mut command = Command::new(cmd);
    command
        .args(args)
        .envs(envs.iter().map(|(k, v)| (*k, *v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().with_context(|| format!("spawning {cmd}"))?;

    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let sb = stdout_buf.clone();
    let eb = stderr_buf.clone();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        *sb.lock().unwrap() = buf;
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        *eb.lock().unwrap() = buf;
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap()).to_string();
    let stderr = String::from_utf8_lossy(&stderr_buf.lock().unwrap()).to_string();
    Ok(RunResult {
        success: status.map(|s| s.success()).unwrap_or(false),
        timed_out,
        stdout,
        stderr,
    })
}

pub fn run(cmd: &str, args: &[&str]) -> anyhow::Result<RunResult> {
    run_with_timeout(cmd, args, &[], Duration::from_secs(30))
}

// --- protonvpn status parsing -------------------------------------------

/// `NB: must not match "Disconnected"`, which contains "connected" — same
/// care as the bash `is_connected`.
pub fn is_connected(status_output: &str) -> bool {
    status_output.lines().any(|line| {
        line.trim_start().starts_with("Status:")
            && line.contains("Connected")
            && !line.contains("Disconnected")
    })
}

/// Proton reports `Server: SG-FREE#2 in Singapore, Singapore`. The bare
/// name is what state/ranking key on.
pub fn current_server(status_output: &str) -> Option<String> {
    for line in status_output.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("Server:") {
            return rest.split_whitespace().next().map(|s| s.to_string());
        }
    }
    None
}

pub fn current_server_desc(status_output: &str) -> Option<String> {
    for line in status_output.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("Server:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

pub fn settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/Proton/VPN/settings.json")
}

pub fn current_protocol() -> String {
    let path = settings_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return "unknown".to_string();
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("protocol")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn set_protocol(protocol: &str) -> anyhow::Result<()> {
    let path = settings_path();
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&text)?;
    value["protocol"] = serde_json::Value::String(protocol.to_string());
    std::fs::write(&path, serde_json::to_string_pretty(&value)? + "\n")?;
    Ok(())
}

// --- protonvpn actions ---------------------------------------------------

pub fn protonvpn_status() -> anyhow::Result<RunResult> {
    run("protonvpn", &["status"])
}

pub fn protonvpn_disconnect() -> anyhow::Result<RunResult> {
    run("protonvpn", &["disconnect"])
}

pub fn protonvpn_info() -> anyhow::Result<RunResult> {
    run("protonvpn", &["info"])
}

pub fn protonvpn_signout() -> anyhow::Result<RunResult> {
    run("protonvpn", &["signout"])
}

/// Refresh the cached server list through Tor. Routing is untouched, so
/// this is slow but safe — the caller's internet keeps working.
pub fn protonvpn_servers_via_tor(shim: &Path, timeout: Duration) -> anyhow::Result<RunResult> {
    let shim_str = shim.to_string_lossy().to_string();
    run_with_timeout(
        "torsocks",
        &[
            "env",
            &format!("PYTHONPATH={shim_str}"),
            "PVPN_DEBUG=0",
            "protonvpn",
            "servers",
        ],
        &[],
        timeout,
    )
}

/// Kill any `protonvpn connect` still in flight. The pattern is bracketed
/// so pkill cannot match this process's own command line.
pub fn kill_in_flight_connect() {
    let _ = Command::new("pkill")
        .args(["-f", "[p]rotonvpn connect"])
        .status();
}

pub fn link_exists(name: &str) -> bool {
    run("ip", &["link", "show", name])
        .map(|r| r.success)
        .unwrap_or(false)
}

/// Interactive `sudo` so the user can type a password. Not used by the
/// unattended — those calls are skipped with a warning. Used by `pvpn fix`.
pub fn run_interactive(
    cmd: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> anyhow::Result<std::process::ExitStatus> {
    let status = Command::new(cmd)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (*k, *v)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawning {cmd}"))?;
    Ok(status)
}

pub fn capture_interactive_stdout(
    cmd: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> anyhow::Result<(i32, String)> {
    let output = Command::new(cmd)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (*k, *v)))
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("spawning {cmd}"))?;
    let code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok((code, stdout))
}

// --- Tor -------------------------------------------------------------------

pub fn tor_listening() -> bool {
    std::net::TcpStream::connect_timeout(
        &"127.0.0.1:9050".parse().expect("static addr"),
        Duration::from_millis(400),
    )
    .is_ok()
}

pub fn tor_service_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "tor"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn tor_available() -> bool {
    tor_service_active() && tor_listening()
}

// --- protocol backends -----------------------------------------------------

const PROTOCOL_PROBE_PY: &str = r#"
import importlib, sys
proto = sys.argv[1]
from proton.vpn.core.registry import Registry
r = Registry()
for mod in (
    "proton.vpn.backend.networkmanager.protocol.openvpn",
    "proton.vpn.backend.networkmanager.protocol.wireguard",
    "proton.vpn.backend.networkmanager.protocol.protun",
):
    try:
        importlib.import_module(mod).register(r)
    except Exception:
        pass
cls = r._registry.get(proto)
sys.exit(0 if cls is not None and cls.validate() else 1)
"#;

const PROTOCOL_LIST_PY: &str = r#"
import importlib
from proton.vpn.core.registry import Registry
r = Registry()
for mod in (
    "proton.vpn.backend.networkmanager.protocol.openvpn",
    "proton.vpn.backend.networkmanager.protocol.wireguard",
    "proton.vpn.backend.networkmanager.protocol.protun",
):
    try:
        importlib.import_module(mod).register(r)
    except Exception as e:
        print(f"  (failed to load {mod}: {e})")
print("Protocol backends on this machine:")
for key in sorted(r._registry):
    cls = r._registry[key]
    ok = cls.validate()
    mark = "OK  " if ok else "MISS"
    print(f"  [{mark}] {key}")
print()
print("MISS on protun-* means Stealth is missing. Install:")
print("  python3-proton-vpn-lib   (Proton stable)")
print("  proton-vpn-linux         (NM protun plugin — Proton unstable today)")
print("On filtered wifi, fetch via Tor, e.g.:")
print("  sudo apt-get -o Acquire::https::Proxy::repo.protonvpn.com=socks5h://127.0.0.1:9050 install python3-proton-vpn-lib")
"#;

/// True if Proton's registry has a valid implementation for this protocol.
pub fn protocol_available(proto: &str) -> bool {
    Command::new(crate::paths::system_python())
        .arg("-")
        .arg(proto)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(PROTOCOL_PROBE_PY.as_bytes());
            }
            child.wait()
        })
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn list_protocols() -> anyhow::Result<String> {
    let mut child = Command::new(crate::paths::system_python())
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning python for protocol listing")?;
    {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(PROTOCOL_LIST_PY.as_bytes())?;
        }
    }
    let output = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Pick a usable protocol, preferring the requested one, then Stealth,
/// then OpenVPN-TCP. Writes the choice into Proton's settings.json.
pub fn ensure_connect_protocol(want: &str) -> anyhow::Result<String> {
    let fallback = "openvpn-tcp";
    let mut want = want.to_string();
    if want.is_empty() || want == "unknown" {
        want = "protun-tls".to_string();
    }
    if protocol_available(&want) {
        set_protocol(&want)?;
        return Ok(want);
    }
    if want != "protun-tls" && protocol_available("protun-tls") {
        tracing::warn!("Protocol {want} unavailable — using Stealth (protun-tls).");
        set_protocol("protun-tls")?;
        return Ok("protun-tls".to_string());
    }
    if protocol_available(fallback) {
        tracing::warn!("Protocol {want} is not available — trying {fallback}.");
        set_protocol(fallback)?;
        return Ok(fallback.to_string());
    }
    anyhow::bail!("No usable VPN protocol backend found. You are signed in. Fix packages, then: pvpn protocols");
}

pub const TRY_PROTOCOLS: [&str; 7] = [
    "protun-tls",
    "protun-udp",
    "protun-tcp",
    "protun-smart",
    "openvpn-tcp",
    "openvpn-udp",
    "wireguard",
];

pub fn account_from_info(info: &str) -> Option<String> {
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix("Account:") {
            let trimmed = rest.trim().trim_matches('\'').trim();
            if trimmed.is_empty() || trimmed == "None" {
                return None;
            }
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Age of `path` in whole hours, or `None` if it does not exist.
pub fn file_age_hours(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some(modified.elapsed().ok()?.as_secs() / 3600)
}

pub const HOSTS_MARK: &str = "# pvpn-temporary-api-blackhole";

/// Interactive /etc/hosts blackhole of Proton's API domains. Daemon mode
/// must not call this — it needs sudo.
pub fn blackhole_api_hosts() -> anyhow::Result<bool> {
    let script = format!(
        "printf '%s\\n127.0.0.1 vpn-api.proton.me\\n127.0.0.1 api.protonvpn.ch\\n127.0.0.1 account.proton.me\\n' '{HOSTS_MARK}' >> /etc/hosts"
    );
    let status = run_interactive("sudo", &["sh", "-c", &script], &[])?;
    Ok(status.success())
}

pub fn unblackhole_api_hosts() -> anyhow::Result<bool> {
    let script = "sed -i '/pvpn-temporary-api-blackhole/,+3d' /etc/hosts; \
        sed -i '/127\\.0\\.0\\.1 vpn-api\\.proton\\.me/d;/127\\.0\\.0\\.1 api\\.protonvpn\\.ch/d;/127\\.0\\.0\\.1 account\\.proton\\.me/d' /etc/hosts";
    let status = run_interactive("sudo", &["sh", "-c", script], &[])?;
    Ok(status.success())
}

pub fn delete_killswitch_interface() -> anyhow::Result<bool> {
    if !link_exists("pvpnksintrf0") {
        return Ok(false);
    }
    let status = run_interactive("sudo", &["ip", "link", "delete", "pvpnksintrf0"], &[])?;
    Ok(status.success())
}

/// Connect, optionally to a named server. Runs *without* Tor, straight to
/// Proton's server IP — see `bin/pvpn`'s `PV_DIRECT` for why: connecting
/// through Tor is what turned a ~10s connect into a multi-minute hang.
pub fn protonvpn_connect(
    target: Option<&str>,
    shim: &Path,
    timeout: Duration,
) -> anyhow::Result<RunResult> {
    let shim_str = shim.to_string_lossy().to_string();
    let envs = [("PYTHONPATH", shim_str.as_str()), ("PVPN_DEBUG", "0")];
    match target {
        Some(name) => run_with_timeout("protonvpn", &["connect", name], &envs, timeout),
        None => run_with_timeout("protonvpn", &["connect"], &envs, timeout),
    }
}

// --- NetworkManager cleanup -----------------------------------------------

/// `(name, uuid)` for every `ProtonVPN *` NetworkManager connection.
pub fn nmcli_proton_connections() -> Vec<(String, String)> {
    let Ok(result) = run("nmcli", &["-t", "-f", "NAME,UUID", "con", "show"]) else {
        return Vec::new();
    };
    result
        .stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ':');
            let name = parts.next()?;
            let uuid = parts.next()?;
            name.starts_with("ProtonVPN ")
                .then(|| (name.to_string(), uuid.to_string()))
        })
        .collect()
}

/// Stop NetworkManager from putting a disconnected tunnel straight back:
/// Proton creates its profile with autoconnect enabled, so a profile left
/// behind is re-activated on its own within seconds of a disconnect.
pub fn nmcli_clear_autoconnect(uuid: &str) {
    let _ = run(
        "nmcli",
        &["con", "modify", uuid, "connection.autoconnect", "no"],
    );
}

/// Connection names/uuids matching Proton's kill-switch profile.
pub fn nmcli_killswitch_connections() -> Vec<String> {
    let Ok(result) = run("nmcli", &["-t", "-f", "NAME,UUID", "con", "show"]) else {
        return Vec::new();
    };
    result
        .stdout
        .lines()
        .filter_map(|line| {
            let lower = line.to_lowercase();
            if lower.contains("killswitch") || lower.contains("pvpnks") {
                line.splitn(2, ':').nth(1).map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Is one of Proton's own NetworkManager profiles actually active?
///
/// Proton's backend *is* NetworkManager — it creates a profile per connect
/// and calls `remove_connection_async` on disconnect. So a live tunnel
/// always has an active profile, and the absence of one while the client
/// claims `Connected` is a contradiction.
pub fn proton_connection_active() -> bool {
    let Ok(result) = run("nmcli", &["-t", "-f", "NAME,TYPE", "con", "show", "--active"]) else {
        // Cannot tell — assume the tunnel is fine. See `tunnel_is_real`.
        return true;
    };
    parse_proton_connection_active(&result.stdout)
}

fn parse_proton_connection_active(active: &str) -> bool {
    active.lines().any(|line| {
        line.split(':')
            .next()
            .unwrap_or("")
            .to_lowercase()
            .starts_with("protonvpn")
    })
}

/// Device carrying the IPv4 default route.
pub fn default_route_device() -> Option<String> {
    let result = run("ip", &["route", "show", "default"]).ok()?;
    parse_default_route_device(&result.stdout)
}

fn parse_default_route_device(routes: &str) -> Option<String> {
    routes.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        while let Some(token) = parts.next() {
            if token == "dev" {
                return parts.next().map(|d| d.to_string());
            }
        }
        None
    })
}

/// Devices NetworkManager considers this machine's physical uplink.
fn uplink_devices() -> Vec<String> {
    let Ok(result) = run("nmcli", &["-t", "-f", "DEVICE,TYPE,STATE", "dev", "status"]) else {
        return Vec::new();
    };
    parse_uplink_devices(&result.stdout)
}

fn parse_uplink_devices(dev_status: &str) -> Vec<String> {
    dev_status
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ':');
            let device = parts.next()?;
            let kind = parts.next()?;
            let state = parts.next()?;
            ((kind == "wifi" || kind == "ethernet") && state.starts_with("connected"))
                .then(|| device.to_string())
        })
        .collect()
}

/// Interfaces Proton creates purely to blackhole traffic if a tunnel drops.
const LEAK_GUARD_INTERFACES: [&str; 2] = ["ipv6leakintrf0", "pvpnksintrf0"];

/// A default route pointing into one of Proton's leak-protection dummy
/// interfaces, if one is still there.
///
/// Doing their job during a connect, these are exactly right. Left behind
/// afterwards they silently swallow every connection of that family.
/// Observed holding the *IPv6* default route for seven minutes after the
/// tunnel was gone, following the machine across a change of wifi — and
/// `net_works` probes IPv4 only (`curl -4`), so pvpn reported a healthy
/// internet the entire time while every site with a AAAA record timed out
/// in the browser. Checking IPv4 alone cannot see this, so check the route.
pub fn stray_leak_route() -> Option<String> {
    for family in ["-6", "-4"] {
        let Ok(result) = run("ip", &[family, "route", "show", "default"]) else {
            continue;
        };
        if let Some(dev) = parse_default_route_device(&result.stdout) {
            if LEAK_GUARD_INTERFACES.contains(&dev.as_str()) {
                return Some(dev);
            }
        }
    }
    None
}

/// The client says `Connected` — is any of your traffic actually tunneled?
///
/// `protonvpn status` reports what the client believes, and that belief can
/// outlive the tunnel by a long way. Seen in the wild: the wifi changed
/// underneath an established session, Proton's state machine fell over and
/// looped `Reached connection error state: Initialized` every five seconds,
/// and `status` went on reporting `Connected — SG-FREE#21, Singapore` while
/// the IPv4 default route pointed straight out of the hotspot. Nothing
/// noticed, because the reachability check asks whether the internet works
/// — and it did, unencrypted. That is the one failure this tool must never
/// sit through quietly.
///
/// Deliberately requires *two* independent signals to agree before calling
/// the status a lie: no active Proton profile, and a default route still on
/// the physical uplink. Either signal alone means "assume it works" —
/// tearing down a healthy tunnel is worse than being slow to spot a dead
/// one, and both were unambiguously true in the case above.
pub fn tunnel_is_real() -> bool {
    decide_tunnel_is_real(
        proton_connection_active(),
        default_route_device().as_deref(),
        &uplink_devices(),
    )
}

fn decide_tunnel_is_real(
    proton_active: bool,
    route_dev: Option<&str>,
    uplinks: &[String],
) -> bool {
    if proton_active {
        return true;
    }
    let Some(route_dev) = route_dev else {
        // No default route at all is a different problem, not this one.
        return true;
    };
    if uplinks.is_empty() {
        return true;
    }
    !uplinks.iter().any(|u| u == route_dev)
}

pub fn nmcli_delete_connection(uuid: &str) {
    let _ = run("nmcli", &["con", "delete", uuid]);
}

/// The active wifi connection name, if any — used as a last-resort bounce
/// to recover an interface left half-configured after e.g. a suspend
/// mid-connect.
pub fn active_wifi_connection() -> Option<String> {
    let result = run(
        "nmcli",
        &["-t", "-f", "NAME,TYPE", "con", "show", "--active"],
    )
    .ok()?;
    result.stdout.lines().find_map(|line| {
        let mut parts = line.splitn(2, ':');
        let name = parts.next()?;
        let kind = parts.next()?;
        (kind == "802-11-wireless").then(|| name.to_string())
    })
}

pub fn nmcli_connection_up(name: &str) {
    let _ = run("nmcli", &["con", "up", name]);
}

/// SSID of the wifi we are on, or `None` when not on wifi.
///
/// Deliberately the SSID and not the NetworkManager profile name: NM
/// accumulates duplicate profiles for one network ("detnsw", "detnsw 1"),
/// and everything keyed on the profile name would treat them as two
/// different places and learn each one separately.
pub fn active_wifi_ssid() -> Option<String> {
    let result = run("nmcli", &["-t", "-f", "ACTIVE,SSID", "dev", "wifi"]).ok()?;
    result.stdout.lines().find_map(|line| {
        let rest = line.strip_prefix("yes:")?;
        let ssid = rest.replace("\\:", ":");
        (!ssid.is_empty()).then_some(ssid)
    })
}

/// A stable identity for the network this machine is attached to.
///
/// Which servers work is a property of the *network*, not of the laptop: a
/// server a school proxy kills works fine on a phone hotspot two minutes
/// later. Everything learned about servers is filed under this key so the
/// two sets of observations cannot overwrite each other — see
/// [`crate::state::State::set_network`].
pub fn active_network_key() -> String {
    if let Some(ssid) = active_wifi_ssid() {
        return format!("wifi:{ssid}");
    }
    if let Some(dev) = active_wired_device() {
        return format!("wired:{dev}");
    }
    "offline".to_string()
}

fn active_wired_device() -> Option<String> {
    let result = run("nmcli", &["-t", "-f", "DEVICE,TYPE,STATE", "dev", "status"]).ok()?;
    result.stdout.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        let device = parts.next()?;
        let kind = parts.next()?;
        let state = parts.next()?;
        (kind == "ethernet" && state.starts_with("connected")).then(|| device.to_string())
    })
}

// --- Proton's own log ------------------------------------------------------

/// Lines in Proton's log that mean *our certificate*, not their server, is
/// why a tunnel would not carry traffic.
const CERT_FAILURE_MARKERS: [&str; 2] = ["ExpiredCertificate", "Certificate refresh failed"];

/// How much of the log tail to read. Comfortably more than one connect
/// attempt writes, far less than the whole file.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Did Proton log a certificate failure since `since`?
///
/// This is the difference between "this server is blocked here" and "no
/// server can work until one API call gets through". The protun local
/// agent refuses a session whose certificate has expired, and renewing it
/// needs `/vpn/v1/certificate` — the very path a filtered network blocks.
/// Recording that as a server block retires a perfectly good server and
/// sends the next attempt to another one that fails identically, which is
/// how a whole ranked list gets burned through in three minutes.
///
/// Takes an instant rather than a window so a caller can ask about exactly
/// the attempt it just made, and is not still reading the failure that
/// prompted the renewal it has since done.
pub fn cert_failure_since(since: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(text) = log_tail(&crate::paths::proton_log_path(), LOG_TAIL_BYTES) else {
        return false;
    };
    text.lines().rev().any(|line| {
        CERT_FAILURE_MARKERS.iter().any(|m| line.contains(m)) && line_is_after(line, since)
    })
}

/// Proton's log lines start `2026-08-23T22:17:11.152816+00:00 | ...`.
fn line_is_after(line: &str, cutoff: chrono::DateTime<chrono::Utc>) -> bool {
    line.split_whitespace()
        .next()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|ts| ts.with_timezone(&chrono::Utc) >= cutoff)
        .unwrap_or(false)
}

/// Last `max` bytes of a file as text, or `None` if it cannot be read.
fn log_tail(path: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// --- reachability ----------------------------------------------------------

/// A handful of small, unauthenticated endpoints known to answer quickly
/// through a working tunnel. Deliberately NOT `1.1.1.1` — measured on a
/// filtered network, that address burns the full timeout whether or not
/// the tunnel is up, which made every health check slow regardless of
/// outcome. See `bin/pvpn`'s `NET_PROBES` for the same lesson.
pub const NET_PROBE_HOSTS: [&str; 3] = [
    "cloudflare.com",
    "connectivitycheck.gstatic.com",
    "detectportal.firefox.com",
];

/// Can we reach Proton's API directly? True on an open network; false on
/// one that DNS/IP-blocks Proton's account infrastructure specifically
/// (the tunnel itself can still work — this only tells the caller whether the
/// *account* API needs Tor).
pub const PROTON_API_HOST: &str = "vpn-api.proton.me";

#[cfg(test)]
mod tests {
    #[test]
    fn a_leak_guard_holding_the_default_route_is_recognised() {
        // Verbatim from the machine, seven minutes after the tunnel died.
        let route =
            "default via fdeb:446c:912d:8da::1 dev ipv6leakintrf0 proto static metric 95 pref medium\n";
        let dev = super::parse_default_route_device(route).unwrap();
        assert_eq!(dev, "ipv6leakintrf0");
        assert!(super::LEAK_GUARD_INTERFACES.contains(&dev.as_str()));
    }

    #[test]
    fn an_ordinary_default_route_is_not_a_leak_guard() {
        let route = "default via 172.20.10.1 dev wlp0s20f3 proto dhcp src 172.20.10.2 metric 600\n";
        let dev = super::parse_default_route_device(route).unwrap();
        assert!(!super::LEAK_GUARD_INTERFACES.contains(&dev.as_str()));
    }

    // Captured verbatim from the machine while `protonvpn status` was
    // reporting "Connected — SG-FREE#21, Singapore" and every packet was
    // leaving unencrypted through the hotspot.
    const LEAKING_ACTIVE_CONNS: &str = "\
Kate’s iPhone:802-11-wireless
pvpn-killswitch-ipv6:dummy
tailscale0:tun
docker0:bridge
lo:loopback
";
    const LEAKING_ROUTES: &str =
        "default via 172.20.10.1 dev wlp0s20f3 proto dhcp src 172.20.10.2 metric 600\n";
    const LEAKING_DEV_STATUS: &str = "\
wlp0s20f3:wifi:connected
tailscale0:tun:connected (externally)
docker0:bridge:connected (externally)
lo:loopback:connected (externally)
enp2s0:ethernet:unavailable
";

    #[test]
    fn the_leak_that_reported_itself_as_connected_is_caught() {
        let uplinks = super::parse_uplink_devices(LEAKING_DEV_STATUS);
        assert_eq!(uplinks, vec!["wlp0s20f3"], "tailscale is not an uplink");
        assert!(!super::parse_proton_connection_active(LEAKING_ACTIVE_CONNS));
        assert_eq!(
            super::parse_default_route_device(LEAKING_ROUTES).as_deref(),
            Some("wlp0s20f3")
        );
        assert!(
            !super::decide_tunnel_is_real(false, Some("wlp0s20f3"), &uplinks),
            "status said Connected while the default route was the bare wifi"
        );
    }

    #[test]
    fn a_live_proton_profile_is_believed() {
        assert!(super::parse_proton_connection_active(
            "ProtonVPN SG-FREE#21:vpn\nKate’s iPhone:802-11-wireless\n"
        ));
        // Believed even if the route check would have disagreed: one
        // signal is never enough to tear a tunnel down.
        assert!(super::decide_tunnel_is_real(
            true,
            Some("wlp0s20f3"),
            &["wlp0s20f3".to_string()]
        ));
    }

    #[test]
    fn a_default_route_off_the_uplink_counts_as_tunneled() {
        assert!(super::decide_tunnel_is_real(
            false,
            Some("proton0"),
            &["wlp0s20f3".to_string()]
        ));
    }

    #[test]
    fn anything_we_cannot_read_is_given_the_benefit_of_the_doubt() {
        // Never tear down a working tunnel over a command that failed.
        assert!(super::decide_tunnel_is_real(false, None, &["wlp0s20f3".to_string()]));
        assert!(super::decide_tunnel_is_real(false, Some("wlp0s20f3"), &[]));
    }

    /// A real pair of lines from Proton's log on a filtered network: the
    /// renewal could not reach the API, and the tunnel it then built was
    /// refused by the local agent. Neither line is the server's fault.
    const CERT_FAILURE_LOG: &str = "\
2026-08-23T22:14:18.470712+00:00 | proton.vpn.session.utils:108 | INFO | API:REQUEST | '/vpn/v1/certificate'
2026-08-23T22:14:21.596805+00:00 | proton.vpn.core.refresher.certificate_refresher:114 | WARNING | Certificate refresh failed: No working transports found
2026-08-23T22:14:27.060376+00:00 | proton.vpn.connection.states:401 | WARNING | Reached connection error state: ExpiredCertificate (None)
";

    /// `PVPN_PROTON_LOG` is process-wide and these tests run concurrently,
    /// so they take turns rather than overwriting each other's fixture.
    static LOG_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_log(body: &str, f: impl FnOnce()) {
        let _guard = LOG_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!(
            "pvpn-proton-log-{}-{}.log",
            std::process::id(),
            body.len()
        ));
        std::fs::write(&path, body).unwrap();
        std::env::set_var("PVPN_PROTON_LOG", &path);
        f();
        std::env::remove_var("PVPN_PROTON_LOG");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_certificate_failure_in_the_window_is_seen() {
        with_log(CERT_FAILURE_LOG, || {
            let before = "2026-08-23T22:14:00+00:00".parse().unwrap();
            assert!(super::cert_failure_since(before));
        });
    }

    #[test]
    fn a_certificate_failure_from_before_the_attempt_is_ignored() {
        // Otherwise a renewal that has since succeeded still reads as a
        // live failure, and every later connect is blamed on the
        // certificate forever.
        with_log(CERT_FAILURE_LOG, || {
            let after = "2026-08-23T22:30:00+00:00".parse().unwrap();
            assert!(!super::cert_failure_since(after));
        });
    }

    #[test]
    fn an_ordinary_log_is_not_a_certificate_failure() {
        with_log(
            "2026-08-23T22:20:23+00:00 | proton.vpn.core.vpnconnector:479 | INFO | CONN:STATE_CHANGED | Connected\n",
            || {
                let before = "2026-08-23T22:00:00+00:00".parse().unwrap();
                assert!(!super::cert_failure_since(before));
            },
        );
    }

    #[test]
    fn a_missing_log_is_not_a_certificate_failure() {
        let _guard = LOG_ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("PVPN_PROTON_LOG", "/nonexistent/vpn-cli.log");
        let before = "2026-08-23T22:00:00+00:00".parse().unwrap();
        assert!(!super::cert_failure_since(before));
        std::env::remove_var("PVPN_PROTON_LOG");
    }

    use super::*;

    #[test]
    fn is_connected_matches_connected_status() {
        assert!(is_connected(
            "Status: Connected\nServer: SG-FREE#2 in Singapore, Singapore\n"
        ));
    }

    #[test]
    fn is_connected_does_not_match_disconnected() {
        assert!(!is_connected("Status: Disconnected\n"));
    }

    #[test]
    fn current_server_extracts_the_bare_name() {
        assert_eq!(
            current_server("Server: SG-FREE#2 in Singapore, Singapore\n"),
            Some("SG-FREE#2".to_string())
        );
    }

    #[test]
    fn current_server_desc_keeps_the_full_phrase() {
        assert_eq!(
            current_server_desc("Server: SG-FREE#2 in Singapore, Singapore\n"),
            Some("SG-FREE#2 in Singapore, Singapore".to_string())
        );
    }

    #[test]
    fn current_server_is_none_when_disconnected() {
        assert_eq!(current_server("Status: Disconnected\n"), None);
    }

    #[test]
    fn run_with_timeout_captures_stdout() {
        let result = run_with_timeout("printf", &["hello"], &[], Duration::from_secs(2)).unwrap();
        assert!(result.success);
        assert_eq!(result.stdout, "hello");
        assert!(!result.timed_out);
    }

    #[test]
    fn run_with_timeout_kills_a_hung_process() {
        let result = run_with_timeout("sleep", &["5"], &[], Duration::from_millis(200)).unwrap();
        assert!(result.timed_out);
        assert!(!result.success);
    }
}
