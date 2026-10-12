//! How the CLI reaches the window (`pvpn-gui`): bare `pvpn` opens it, and
//! a connect asks for its tray icon.
//!
//! Asking is all this does. The tray is a separate, long-lived process that
//! only watches — it never connects — so `pvpn` itself still exits the
//! moment its own work is done. The request goes over D-Bus rather than as
//! a child process: `pvpn` often runs inside a oneshot systemd unit (the
//! autoconnect hook, the watch timer), and a child would be killed with the
//! unit's cgroup when it finishes. D-Bus activation starts the window
//! outside it.

use std::path::PathBuf;
use std::time::Duration;

pub const APP_ID: &str = "io.github.dixonsolutions.ProtunUnblocked";
pub const APP_PATH: &str = "/io/github/dixonsolutions/ProtunUnblocked";

/// Set to anything but `0` to keep `pvpn` from opening or asking for the
/// window: tests, scripts, and people who only want the CLI.
pub const NO_GUI_ENV: &str = "PVPN_NO_GUI";

fn env_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Is there a desktop to show a window or tray on?
pub fn graphical_session() -> bool {
    !env_on(NO_GUI_ENV)
        && (std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some())
}

/// `pvpn-gui` next to this `pvpn` (how `setup.sh` installs them), else on
/// `$PATH`.
pub fn gui_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("pvpn-gui");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("pvpn-gui"))
            .find(|p| p.is_file())
    })
}

/// The window's own switch for this (`tray_with_pvpn` in
/// `~/.config/pvpn/gui.toml`, default on), read without depending on the
/// window's crate. `tray = false` there turns it off too.
pub fn tray_with_pvpn_wanted() -> bool {
    let path = crate::config::Config::config_dir().join("gui.toml");
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return true;
    };
    let on = |key: &str| value.get(key).and_then(|v| v.as_bool()).unwrap_or(true);
    on("tray") && on("tray_with_pvpn")
}

/// Start the window detached from this terminal, opening its window.
pub fn open_window(extra: &[&str]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let bin = gui_binary().ok_or_else(|| std::io::Error::other("pvpn-gui is not installed"))?;
    std::process::Command::new(bin)
        .args(extra)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
}

/// Ask for the tray icon: the running window gets `app.tray`; a window
/// that is not running is started by the session bus to receive it.
/// Best-effort and quick — a missing window is never a connect's problem.
pub fn request_tray() -> bool {
    if !graphical_session() || !tray_with_pvpn_wanted() {
        return false;
    }
    match activate_tray_over_dbus() {
        Ok(true) => true,
        // Not running and not D-Bus activatable (an install without the
        // service file): start it directly and hope we are not in a unit.
        _ => open_window(&["--tray"]).is_ok(),
    }
}

fn activate_tray_over_dbus() -> zbus::Result<bool> {
    use zbus::blocking::{Connection, Proxy};
    let conn = Connection::session()?;
    let bus = Proxy::new(&conn, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus")?;
    let running: bool = bus.call("NameHasOwner", &(APP_ID,))?;
    if !running {
        let activatable: Vec<String> = bus.call("ListActivatableNames", &())?;
        if !activatable.iter().any(|n| n == APP_ID) {
            return Ok(false);
        }
    }
    let app = zbus::blocking::proxy::Builder::<Proxy>::new(&conn)
        .destination(APP_ID)?
        .path(APP_PATH)?
        .interface("org.freedesktop.Application")?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()?;
    let params: Vec<zbus::zvariant::Value> = Vec::new();
    let platform: std::collections::HashMap<&str, zbus::zvariant::Value> = Default::default();
    // No reply wanted: activation can take a second, and a connect should
    // not wait on a window. The bus starts it regardless.
    app.call_noreply("ActivateAction", &("tray", params, platform))?;
    Ok(true)
}

/// Run [`request_tray`] off the calling thread; the returned guard waits a
/// moment for the request to leave before the process exits.
pub fn request_tray_in_background() -> TrayRequest {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(request_tray());
    });
    TrayRequest(Some(rx))
}

pub struct TrayRequest(Option<std::sync::mpsc::Receiver<bool>>);

impl Drop for TrayRequest {
    fn drop(&mut self) {
        if let Some(rx) = self.0.take() {
            let _ = rx.recv_timeout(Duration::from_millis(800));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_gui_env_turns_the_session_off() {
        // Only asserts the override direction: whether this machine has a
        // display is not the test's business.
        std::env::set_var(NO_GUI_ENV, "1");
        assert!(!graphical_session());
        std::env::remove_var(NO_GUI_ENV);
    }
}
