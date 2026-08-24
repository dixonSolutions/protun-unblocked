//! Reachability checks. Every curl uses `--noproxy '*'` so a leftover
//! `HTTPS_PROXY`/`ALL_PROXY` cannot make a working tunnel look dead —
//! see the comment on `CURL` in `legacy/pvpn.sh`.

use std::process::{Command, Stdio};
use std::time::Duration;

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
    NET_PROBE_URLS.iter().any(|url| curl_ok(url, 3, true))
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
    fn probe_url_list_is_the_known_good_set() {
        assert!(super::NET_PROBE_URLS
            .iter()
            .any(|u| u.contains("gstatic.com")));
        assert!(super::NET_PROBE_URLS
            .iter()
            .any(|u| u.contains("cloudflare.com")));
    }
}
