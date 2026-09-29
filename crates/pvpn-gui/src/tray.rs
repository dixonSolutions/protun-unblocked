//! The top-bar icon (StatusNotifierItem). On GNOME it needs the
//! AppIndicator extension; without a host the window works as before and
//! the tray simply is not there.
//!
//! The tray lives on its own thread (ksni's), so it never touches GTK: its
//! menu posts messages, and the window pushes state back through `update`.

use crate::icon;
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::MenuItem;

#[derive(Debug, Clone)]
pub enum TrayMsg {
    Show,
    Connect,
    Disconnect,
    Cancel,
    Hop(Option<String>),
    Check,
    Preferences,
    SetNotifications(bool),
    Quit,
}

pub struct PvpnTray {
    pub css: String,
    pub title: String,
    pub detail: String,
    pub busy: bool,
    pub connected: bool,
    pub fastest: Vec<String>,
    pub notifications: bool,
    tx: async_channel::Sender<TrayMsg>,
}

impl PvpnTray {
    fn send(&self, msg: TrayMsg) {
        let _ = self.tx.try_send(msg);
    }
}

impl ksni::Tray for PvpnTray {
    fn id(&self) -> String {
        "protun-unblocked".into()
    }

    fn title(&self) -> String {
        format!("Protun Unblocked — {}", self.title)
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Communications
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        [16, 22, 24, 32, 48]
            .into_iter()
            .map(|s| ksni::Icon {
                width: s,
                height: s,
                data: icon::tray_pixmap(s, &self.css),
            })
            .collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title.clone(),
            description: self.detail.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayMsg::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: format!("{} {}", self.title, if self.detail.is_empty() { String::new() } else { format!("· {}", self.detail) }),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];
        if self.busy {
            items.push(
                StandardItem {
                    label: "Cancel".into(),
                    icon_name: "process-stop-symbolic".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayMsg::Cancel)),
                    ..Default::default()
                }
                .into(),
            );
        } else if self.connected {
            items.push(
                StandardItem {
                    label: "Disconnect".into(),
                    icon_name: "network-vpn-disconnected-symbolic".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayMsg::Disconnect)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Switch server".into(),
                    icon_name: "media-skip-forward-symbolic".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayMsg::Hop(None))),
                    ..Default::default()
                }
                .into(),
            );
        } else {
            items.push(
                StandardItem {
                    label: "Connect".into(),
                    icon_name: "network-vpn-symbolic".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayMsg::Connect)),
                    ..Default::default()
                }
                .into(),
            );
        }
        if !self.fastest.is_empty() {
            let submenu: Vec<MenuItem<Self>> = self
                .fastest
                .iter()
                .map(|name| {
                    let n = name.clone();
                    StandardItem {
                        label: name.replace('_', "__"),
                        enabled: !self.busy,
                        activate: Box::new(move |t: &mut Self| t.send(TrayMsg::Hop(Some(n.clone())))),
                        ..Default::default()
                    }
                    .into()
                })
                .collect();
            items.push(
                SubMenu {
                    label: "Fastest here".into(),
                    enabled: !self.busy,
                    submenu,
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Check tunnel".into(),
                enabled: self.connected && !self.busy,
                activate: Box::new(|t: &mut Self| t.send(TrayMsg::Check)),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            CheckmarkItem {
                label: "Notifications".into(),
                checked: self.notifications,
                activate: Box::new(|t: &mut Self| {
                    t.notifications = !t.notifications;
                    t.send(TrayMsg::SetNotifications(t.notifications));
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Open Protun Unblocked".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayMsg::Show)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Preferences".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayMsg::Preferences)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayMsg::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

pub fn spawn(tx: async_channel::Sender<TrayMsg>, notifications: bool) -> Option<Handle<PvpnTray>> {
    let tray = PvpnTray {
        css: "off".into(),
        title: "Disconnected".into(),
        detail: String::new(),
        busy: false,
        connected: false,
        fastest: Vec::new(),
        notifications,
        tx,
    };
    // `assume_sni_available`: the tray may start before the shell's host
    // does (autostart), and should appear when it arrives.
    tray.assume_sni_available(true).spawn().ok()
}
