//! The window's only way of learning anything: ask `pvpn`.
//!
//! Every reading goes through `pvpn query …`, every change through a `pvpn`
//! command — the same binary every hook and timer runs, so the window can
//! never disagree with the CLI about what is true, and never needs its own
//! copy of how to find out. The binary is the one Preferences names
//! ([`crate::settings::GuiSettings::pvpn_binary`]), set at startup and whenever
//! that preference changes.

use serde::de::DeserializeOwned;
use std::process::{Command, Stdio};
use std::sync::Mutex;

static BINARY: Mutex<Option<String>> = Mutex::new(None);

pub fn set_binary(path: String) {
    if let Ok(mut binary) = BINARY.lock() {
        *binary = Some(path);
    }
}

/// The `pvpn` to run.
pub fn binary() -> String {
    BINARY
        .lock()
        .ok()
        .and_then(|b| b.clone())
        .unwrap_or_else(|| crate::settings::GuiSettings::load().pvpn_binary())
}

/// `[pvpn, args…]`, for the runner and `runner::capture`.
pub fn argv(args: &[&str]) -> Vec<String> {
    std::iter::once(binary())
        .chain(args.iter().map(|a| a.to_string()))
        .collect()
}

/// Run `pvpn args…` and return its stdout, or `None` if it failed. Blocking:
/// call it off the main thread.
pub fn stdout(args: &[&str]) -> Option<String> {
    let output = Command::new(binary())
        .args(args)
        .env("PVPN_NO_GUI", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `pvpn query …`, parsed. `None` when the installed `pvpn` cannot answer —
/// too old for `query`, or not there at all.
pub fn query<T: DeserializeOwned>(args: &[&str]) -> Option<T> {
    let mut full = vec!["query"];
    full.extend_from_slice(args);
    let text = stdout(&full)?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(err) => {
            eprintln!("pvpn query {}: {err}", args.join(" "));
            None
        }
    }
}

/// The effective `pvpn` configuration.
pub fn config() -> pvpn_core::config::Config {
    query(&["config"]).unwrap_or_default()
}

/// Replace `config.toml` with `text`; `pvpn` validates it before writing.
pub fn write_config(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new(binary())
        .args(["config", "write"])
        .env("PVPN_NO_GUI", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run pvpn: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The certificate's state, as `pvpn query cert` reports it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Cert {
    pub describe: String,
    pub unusable: bool,
}

pub fn cert() -> Option<Cert> {
    query(&["cert"])
}

/// Proton's server inventory, as `pvpn query inventory` hands it over.
pub fn inventory() -> Option<String> {
    stdout(&["query", "inventory"])
}
