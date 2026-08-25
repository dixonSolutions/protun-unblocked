//! Reachability checks. Every curl uses `--noproxy '*'` so a leftover
//! `HTTPS_PROXY`/`ALL_PROXY` cannot make a working tunnel look dead —
//! see the comment on `CURL` in `legacy/pvpn.sh`.

use std::process::{Command, Stdio};
use std::time::Duration;

/// A hostname that should resolve on any ordinary internet connection.
const DNS_PROBE_HOST: &str = "connectivitycheck.gstatic.com";

/// Small, unauthenticated endpoints that answered quickly through a working
/// tunnel. Deliberately not `1.1.1.1` — on a filtered network that address
/// burns the full timeout whether or not the tunnel is up.
pub const NET_PROBE_URLS: [&str; 3] = [
    "http://connectivitycheck.gstatic.com/generate_204",
    "http://detectportal.firefox.com/success.txt",
    "https://cloudflare.com",
];

pub const PROTON_API_PING: &str = "https://vpn-api.proton.me/tests/ping";
pub const PUBLIC_IP_URL: &str = "https://ifconfig.me";

fn curl_ok(url: &str, timeout_secs: u64, ipv4_only: bool) -> bool {
    let mut cmd = Command::new("curl");
    cmd.arg("-s")
        .arg("--noproxy")
        .arg("*")
        .arg("-o")
        .arg("/dev/null")
        .arg("--max-time")
        .arg(timeout_secs.to_string());
    if ipv4_only {
        cmd.arg("-4");
    }
    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn curl_body(url: &str, timeout_secs: u64) -> Option<String> {
    let output = Command::new("curl")
        .args([
            "-s",
            "--noproxy",
            "*",
            "--max-time",
            &timeout_secs.to_string(),
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

/// Is the internet actually usable right now?
pub fn net_works() -> bool {
    net_works_within(NET_PROBE_TIMEOUT_SECS)
}

/// The default per-probe budget. Three seconds is comfortably more than a
/// working tunnel needs for a 204 and short enough that all three probes
/// failing still leaves a poll loop responsive.
pub const NET_PROBE_TIMEOUT_SECS: u64 = 3;

/// Is the machine's configured resolver answering before a tunnel changes
/// any routes?
///
/// This is deliberately separate from [`net_works`]. A local filtering
/// resolver can accept queries while all of its upstreams are dead; in that
/// state every hostname-based traffic probe fails and a VPN server would be
/// blamed for a problem that existed before it was contacted.
pub fn dns_works() -> bool {
    resolves_within(DNS_PROBE_HOST, NET_PROBE_TIMEOUT_SECS)
}

fn resolves_within(host: &str, timeout_secs: u64) -> bool {
    crate::proc::run_with_timeout(
        "getent",
        &["ahostsv4", host],
        &[],
        Duration::from_secs(timeout_secs),
    )
    .map(|result| result.success && !result.stdout.trim().is_empty())
    .unwrap_or(false)
}

/// `net_works` with the per-probe timeout spelled out.
///
/// Returns on the *first* endpoint that answers, so a healthy tunnel costs
/// one round trip rather than the timeout — this is the fast half of
/// verifying a connect, and the reason a tunnel that works is confirmed in
/// about a second instead of after a fixed wait.
pub fn net_works_within(timeout_secs: u64) -> bool {
    NET_PROBE_URLS
        .iter()
        .any(|url| curl_ok(url, timeout_secs, true))
}

/// Poll `net_works` until `settle` elapses. Returns as soon as traffic
/// flows. Running out of the window is *not* a reason to tear a tunnel
/// down — every tunnel written off as dead turned out to be merely slow.
pub fn net_works_settled(settle: Duration) -> bool {
    let deadline = std::time::Instant::now() + settle;
    loop {
        if net_works() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Can we reach Proton's account API directly? Cheap HEAD-style GET.
/// True on an open network; false where those domains are DNS/IP-blocked.
pub fn api_reachable() -> bool {
    curl_ok(PROTON_API_PING, 4, false)
}

/// Current public IPv4 as seen from this machine (or the tunnel).
pub fn public_ip() -> Option<String> {
    curl_body(PUBLIC_IP_URL, 10)
}

/// Wait up to `attempts` seconds for `net_works`, sleeping 1s between tries.
pub fn wait_for_net(attempts: u32) -> bool {
    for _ in 0..attempts {
        if net_works() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    net_works()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_local_hosts_database_resolves_without_the_internet() {
        assert!(super::resolves_within("localhost", 1));
    }

    #[test]
    fn probe_url_list_is_the_known_good_set() {
        assert!(super::NET_PROBE_URLS
            .iter()
            .any(|u| u.contains("gstatic.com")));
        assert!(super::NET_PROBE_URLS
            .iter()
            .any(|u| u.contains("cloudflare.com")));
    }
}
