//! `pvpn-gui` — Protun Unblocked: a GTK4/libadwaita window and tray icon
//! over the `pvpn` CLI and Proton's own client.
//!
//!   pvpn-gui            open the window (also: bare `pvpn`, `pvpn gui`)
//!   pvpn-gui --tray     just the tray icon (also: `--hidden`, `pvpn tray`)
//!
//! One instance per session. A second launch hands its command line to the
//! first; `pvpn up` and friends reach it over D-Bus (`app.tray`), which
//! starts it if it is not running. The window watches and asks; it never
//! reconnects on its own — see `status.rs`.

mod app;
mod countries;
mod data;
mod globe;
mod icon;
mod notify;
mod pages;
mod prefs;
mod proton;
mod runner;
mod services;
mod settings;
mod status;
mod tray;

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> glib::ExitCode {
    let mut flags = gio::ApplicationFlags::HANDLES_COMMAND_LINE;
    if std::env::var_os("PVPN_GUI_SNAPSHOT_DIR").is_some() {
        // A snapshot run must not just hand itself to the window already open.
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let gapp = adw::Application::builder().application_id(settings::APP_ID).flags(flags).build();
    gapp.add_main_option("tray", glib::Char::from(b't'), glib::OptionFlags::NONE, glib::OptionArg::None, "Start with just the tray icon", None);
    gapp.add_main_option("hidden", glib::Char::from(0), glib::OptionFlags::HIDDEN, glib::OptionArg::None, "Same as --tray", None);
    gapp.add_main_option("version", glib::Char::from(0), glib::OptionFlags::NONE, glib::OptionArg::None, "Print the version", None);
    gapp.connect_handle_local_options(|_, options| {
        if options.contains("version") {
            println!("pvpn-gui {}", env!("CARGO_PKG_VERSION"));
            return std::ops::ControlFlow::Break(glib::ExitCode::SUCCESS);
        }
        std::ops::ControlFlow::Continue(())
    });

    // Built once, at startup, whether the first request is for the window,
    // the tray, or a D-Bus action with neither (service activation).
    let instance: Rc<RefCell<Option<Rc<app::App>>>> = Rc::new(RefCell::new(None));
    {
        let instance = instance.clone();
        gapp.connect_startup(move |gapp| {
            let app = app::App::new(gapp);
            let weak = Rc::downgrade(&app);
            app.set_prefs_opener(move |page| {
                if let Some(a) = weak.upgrade() {
                    prefs::open(&a, page);
                }
            });
            *instance.borrow_mut() = Some(app);
        });
    }
    {
        let instance = instance.clone();
        gapp.connect_activate(move |_| {
            if let Some(app) = instance.borrow().as_ref() {
                app.present();
            }
        });
    }
    {
        let instance = instance.clone();
        gapp.connect_command_line(move |gapp, cmdline| {
            let options = cmdline.options_dict();
            let tray_only = options.contains("tray") || options.contains("hidden");
            match instance.borrow().as_ref() {
                Some(app) if tray_only => app.ensure_tray(),
                _ => gapp.activate(),
            }
            glib::ExitCode::SUCCESS
        });
    }
    gapp.run()
}
