//! Flatpak bypass audit/fix. Port of the "Flatpak routing" section of
//! `legacy/pvpn.sh` and `docs/flatpak.md`.
//!
//! Flatpak apps share the host network namespace, so the tunnel applies
//! to them — unless a proxy override sends their traffic somewhere else.
//! An empty `VAR=` left behind by `flatpak override --unset-env` is *not*
//! a proxy and must not be reported as one.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Both cases are listed deliberately. curl and glibc read the lowercase
/// names; Qt, Go and Rust programs generally read the uppercase ones.
const PROXY_VARS: [&str; 8] = [
    "http_proxy",
    "https_proxy",
    "ftp_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "FTP_PROXY",
    "ALL_PROXY",
];

/// Does this `VAR=VALUE` line redirect traffic around the tunnel?
///
/// Only a non-empty value redirects anything. `flatpak override --unset-env`
/// leaves the name behind with nothing after the `=`.
pub fn is_proxy_override(line: &str) -> bool {
    let line = line.trim();
    let Some((name, value)) = line.split_once('=') else {
        return false;
    };
    if value.is_empty() {
        return false;
    }
    PROXY_VARS.iter().any(|v| *v == name)
}

pub fn flatpak_available() -> bool {
    Command::new("flatpak")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Installed Flatpak application IDs.
pub fn list_apps() -> Vec<String> {
    let output = Command::new("flatpak")
        .args(["list", "--app", "--columns=application"])
        .stdin(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Proxy settings in an app's effective environment, as `VAR=VALUE` lines
/// that actually redirect traffic (non-empty values only).
pub fn app_proxy_vars(app: &str) -> Vec<String> {
    let output = Command::new("flatpak")
        .args(["info", "--show-permissions", app])
        .stdin(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| is_proxy_override(line))
        .map(|line| line.trim().to_string())
        .collect()
}

/// App IDs that are routed around the tunnel.
///
/// One `flatpak info` per app, in parallel. Measured across 57 apps:
/// 2.5s serially, 0.34s at 12-way. Serially this is too slow to sit in
/// the connect path, which is the only place the audit is any use.
pub fn flatpak_bypassers() -> Vec<String> {
    if !flatpak_available() {
        return Vec::new();
    }
    let apps = list_apps();
    if apps.is_empty() {
        return Vec::new();
    }

    let flagged = Arc::new(Mutex::new(Vec::new()));
    let chunk = (apps.len() / 12).max(1);
    std::thread::scope(|scope| {
        for group in apps.chunks(chunk.max(1)) {
            let flagged = flagged.clone();
            let group = group.to_vec();
            scope.spawn(move || {
                let mut local = Vec::new();
                for app in group {
                    if !app_proxy_vars(&app).is_empty() {
                        local.push(app);
                    }
                }
                flagged.lock().unwrap().extend(local);
            });
        }
    });

    let mut out = flagged.lock().unwrap().clone();
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Clone)]
pub struct AppFix {
    pub app: String,
    pub unset: Vec<String>,
    pub still_bypassing: bool,
}

/// Put any app that is routed around the tunnel back on it.
///
/// Re-reads rather than trusting the unset: a proxy baked into an app's
/// own manifest survives an override, and claiming a fix that did not
/// take is worse than the bypass.
pub fn fix_bypassers(apps: &[String]) -> Vec<AppFix> {
    let mut results = Vec::new();
    for app in apps {
        let vars: Vec<String> = app_proxy_vars(app)
            .iter()
            .filter_map(|line| line.split_once('=').map(|(n, _)| n.to_string()))
            .collect();
        let mut unset = Vec::new();
        for var in &vars {
            let ok = Command::new("flatpak")
                .args(["override", "--user", &format!("--unset-env={var}"), app])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                unset.push(var.clone());
            }
        }
        results.push(AppFix {
            app: app.clone(),
            unset,
            still_bypassing: !app_proxy_vars(app).is_empty(),
        });
    }
    results
}

/// Public IP as seen from inside an app's sandbox. Empty when the sandbox
/// has no tool to ask with, or when the app has no network at all.
///
/// The http:// fallback is not laziness. Some apps ship an `SSL_CERT_FILE`
/// pointing at a path that does not exist in their sandbox, which breaks
/// HTTPS for command-line tools inside it.
pub fn sandbox_ip(app: &str, timeout_secs: u64) -> Option<String> {
    let script = r#"
        for url in https://ifconfig.me http://ifconfig.me; do
            if command -v curl >/dev/null 2>&1; then
                out=$(curl -s --max-time 10 "$url") && [ -n "$out" ] && { echo "$out"; exit 0; }
            elif command -v python3 >/dev/null 2>&1; then
                python3 -c "import urllib.request as u; print(u.urlopen(\"$url\", timeout=10).read().decode())" && exit 0
            fi
        done
    "#;
    let output = Command::new("timeout")
        .args([
            timeout_secs.to_string(),
            "flatpak".to_string(),
            "run".to_string(),
            "--command=sh".to_string(),
            app.to_string(),
            "-c".to_string(),
            script.to_string(),
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let re = regex::Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").ok()?;
    re.find(&text).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_real_proxy_settings() {
        for setting in [
            "http_proxy=socks5://127.0.0.1:9050",
            "https_proxy=socks5://127.0.0.1:9050",
            "ALL_PROXY=socks5://127.0.0.1:9050",
            "HTTP_PROXY=http://corp:3128",
            "ftp_proxy=http://x",
        ] {
            assert!(is_proxy_override(setting), "should match {setting}");
        }
    }

    #[test]
    fn ignores_empty_and_harmless_settings() {
        for setting in [
            "http_proxy=",
            "https_proxy=",
            "ALL_PROXY=",
            "no_proxy=localhost",
            "SSL_CERT_DIR=/x",
        ] {
            assert!(!is_proxy_override(setting), "should not match {setting}");
        }
    }
}
