//! The one desktop notification a background connect raises when the
//! network wants a sign-in first.
//!
//! One bubble, replaced in place as things move, rather than three stacked:
//! "Sign in to detnsw" (with a button that opens the page), then "Signed in
//! — connecting", then where the tunnel ended up. The first is critical so
//! GNOME keeps it on screen; the ones that replace it are not, so the
//! answer does not sit there demanding to be dismissed.
//!
//! Only the background autoconnect gets this. A command typed at a terminal
//! opens the page in the browser instead and narrates the rest itself; and
//! a reconnect that never met a portal stays silent here — the window's own
//! notifications cover that one.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The bubble this process raised, if it raised one: its id, for replacing
/// it, and the network it was about.
static NOTE: Mutex<Option<(u32, String)>> = Mutex::new(None);

/// A sign-in was asked for and has not happened (yet). Set even when no
/// bubble could be shown, so the exit code still says why.
static WAITING: AtomicBool = AtomicBool::new(false);

const APP: &str = "--app-name=Protun Unblocked";

/// "Sign in to <network>", with an "Open sign-in page" button. `false` if
/// nothing could be shown.
pub fn ask_sign_in(network: &str, page: &str) -> bool {
    WAITING.store(true, Ordering::Relaxed);
    // `--action` makes notify-send stay until the bubble is answered or
    // closed, printing the id first and then the action taken — so it is
    // started and left to it, with a thread waiting for the button.
    let child = Command::new("notify-send")
        .args([
            APP,
            "--print-id",
            "--icon=network-wireless-acquiring-symbolic",
            "--urgency=critical",
            "--action=open=Open sign-in page",
            &format!("Sign in to {network}"),
            "This network has a sign-in page. The VPN connects by itself once you are through it.",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        return true;
    };
    let mut lines = BufReader::new(stdout).lines();
    if let Some(Ok(id)) = lines.next() {
        if let Ok(id) = id.trim().parse() {
            *NOTE.lock().unwrap_or_else(|e| e.into_inner()) = Some((id, network.to_string()));
        }
    }
    let page = page.to_string();
    std::thread::spawn(move || {
        if lines.next().and_then(Result::ok).is_some_and(|a| a.trim() == "open") {
            let _ = Command::new("xdg-open")
                .arg(&page)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        let _ = child.wait();
    });
    true
}

/// The sign-in went through; the connect is under way.
pub fn signed_in() {
    WAITING.store(false, Ordering::Relaxed);
    replace(
        "network-vpn-acquiring-symbolic",
        |network| format!("Signed in to {network}"),
        "Connecting the VPN…",
    );
}

/// Where the connect that followed the sign-in ended up. Nothing to do when
/// no sign-in was asked for. `true` when the connect failed because the
/// sign-in it asked for never happened.
pub fn finish(ok: bool, server: Option<&str>, message: &str) -> bool {
    let unanswered = !ok && WAITING.swap(false, Ordering::Relaxed);
    if unanswered {
        replace(
            "network-wireless-acquiring-symbolic",
            |network| format!("Still not signed in to {network}"),
            "Gave up waiting, so the VPN is off. Sign in, then connect from the window or with `pvpn up`.",
        );
    } else if ok {
        let body = match server {
            Some(server) => format!("Traffic goes through {server}."),
            None => "The tunnel is up.".to_string(),
        };
        replace("network-vpn-symbolic", |_| "VPN connected".to_string(), &body);
    } else {
        // The first line is the reason; the rest is advice for a terminal.
        let reason = message.lines().next().unwrap_or("The connect failed.");
        replace(
            "network-vpn-disconnected-symbolic",
            |_| "VPN did not connect".to_string(),
            &format!("{reason}\nTry again from the window or with `pvpn up`."),
        );
    }
    *NOTE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    unanswered
}

fn replace(icon: &str, summary: impl FnOnce(&str) -> String, body: &str) {
    let Some((id, network)) = NOTE.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return;
    };
    // Waited for, unlike the first: it returns at once without --action,
    // and `finish` runs just before the process exits.
    let _ = Command::new("notify-send")
        .args([
            APP,
            &format!("--replace-id={id}"),
            &format!("--icon={icon}"),
            "--urgency=normal",
            &summary(&network),
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
