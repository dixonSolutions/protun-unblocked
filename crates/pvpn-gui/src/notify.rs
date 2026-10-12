//! Desktop notifications over `org.freedesktop.Notifications`.
//!
//! The freedesktop interface rather than `GNotification` because it works
//! whether or not the `.desktop` file is installed — `cargo run` included —
//! and because one notification can replace the last, so a flapping tunnel
//! leaves one bubble rather than a stack of them.

use crate::settings::{Notifications, APP_ID};
use crate::status::Note;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;

thread_local! {
    static LAST_ID: Cell<u32> = const { Cell::new(0) };
}

/// Is this kind of note switched on?
pub fn wanted(prefs: &Notifications, note: &Note) -> bool {
    prefs.enabled
        && match note {
            Note::Connected { .. } => prefs.connected,
            Note::Reconnecting => prefs.reconnecting,
            Note::Reconnected { .. } => prefs.reconnected,
            Note::Failure { .. } => prefs.failure,
            Note::Disconnected { .. } => prefs.disconnected,
        }
}

/// Summary, body, icon name, urgency (0 low, 1 normal, 2 critical).
pub fn render(note: &Note) -> (String, String, &'static str, u8) {
    match note {
        Note::Connected { server } => (
            "VPN connected".into(),
            format!("Traffic goes through {server}."),
            "network-vpn-symbolic",
            1,
        ),
        Note::Reconnecting => (
            "VPN reconnecting".into(),
            "The tunnel dropped; rebuilding it.".into(),
            "network-vpn-acquiring-symbolic",
            1,
        ),
        Note::Reconnected { server } => (
            "VPN reconnected".into(),
            format!("Back through {server}."),
            "network-vpn-symbolic",
            1,
        ),
        Note::Failure { what } => (
            "VPN problem".into(),
            what.clone(),
            "network-vpn-disconnected-symbolic",
            2,
        ),
        Note::Disconnected { expected } => (
            "VPN disconnected".into(),
            if *expected {
                "Normal internet restored.".into()
            } else {
                "The tunnel went away. Traffic is no longer protected.".into()
            },
            "network-vpn-disconnected-symbolic",
            if *expected { 1 } else { 2 },
        ),
    }
}

pub fn send(note: &Note) {
    let (summary, body, icon, urgency) = render(note);
    send_raw(&summary, &body, icon, urgency);
}

pub fn send_raw(summary: &str, body: &str, icon: &str, urgency: u8) {
    if std::env::var_os("PVPN_GUI_DEBUG").is_some() {
        eprintln!("notify: {summary} — {body}");
    }
    let summary = summary.to_string();
    let body = body.to_string();
    let icon = icon.to_string();
    glib::spawn_future_local(async move {
        let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let mut hints: HashMap<String, glib::Variant> = HashMap::new();
        hints.insert("urgency".into(), urgency.to_variant());
        hints.insert("desktop-entry".into(), APP_ID.to_variant());
        let params = (
            "Protun Unblocked",
            LAST_ID.with(|c| c.get()),
            icon,
            summary,
            body,
            vec!["default".to_string(), "Open".to_string()],
            hints,
            -1i32,
        )
            .to_variant();
        let reply = bus
            .call_future(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "Notify",
                Some(&params),
                Some(glib::VariantTy::new("(u)").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await;
        if let Ok(reply) = reply {
            if let Some((id,)) = reply.get::<(u32,)>() {
                LAST_ID.with(|c| c.set(id));
            }
        }
    });
}

/// Call `on_open` when one of ours is clicked.
pub fn on_activated(on_open: impl Fn() + 'static) {
    glib::spawn_future_local(async move {
        let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        #[allow(deprecated)]
        let id = bus.signal_subscribe(
            Some("org.freedesktop.Notifications"),
            Some("org.freedesktop.Notifications"),
            Some("ActionInvoked"),
            Some("/org/freedesktop/Notifications"),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                if let Some((nid, _action)) = params.get::<(u32, String)>() {
                    if nid == LAST_ID.with(|c| c.get()) {
                        on_open();
                    }
                }
            },
        );
        // Lives as long as the app; the bus connection keeps it.
        std::mem::forget(id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_switch_gates_its_own_kind() {
        let mut p = Notifications::default();
        let reconnecting = Note::Reconnecting;
        assert!(wanted(&p, &reconnecting));
        p.reconnecting = false;
        assert!(!wanted(&p, &reconnecting));
        assert!(wanted(&p, &Note::Reconnected { server: "A".into() }));
        p.enabled = false;
        assert!(!wanted(&p, &Note::Failure { what: "x".into() }));
    }
}
