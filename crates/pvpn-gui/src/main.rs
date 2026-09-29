//! `pvpn-gui` — Protun Unblocked: a GTK4/libadwaita window and tray icon
//! over the `pvpn` CLI.
//!
//!   pvpn-gui            open the window
//!   pvpn-gui --hidden   start in the tray (what autostart runs)
//!
//! A second launch raises the first. The window watches and asks; it never
//! reconnects on its own — see `status.rs`.

mod app;
mod data;
mod globe;
mod icon;
mod notify;
mod pages;
mod prefs;
mod runner;
mod services;
mod settings;
mod status;
mod tray;

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> gtk::glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("pvpn-gui — Protun Unblocked\n\n  pvpn-gui            open the window\n  pvpn-gui --hidden   start in the tray");
        return gtk::glib::ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version") {
        println!("pvpn-gui {}", env!("CARGO_PKG_VERSION"));
        return gtk::glib::ExitCode::SUCCESS;
    }
    let hidden = args.iter().any(|a| a == "--hidden");

    let mut flags = gtk::gio::ApplicationFlags::empty();
    if std::env::var_os("PVPN_GUI_SNAPSHOT_DIR").is_some() {
        // A snapshot run must not just raise the window already open.
        flags |= gtk::gio::ApplicationFlags::NON_UNIQUE;
    }
    let gapp = adw::Application::builder().application_id(settings::APP_ID).flags(flags).build();
    let instance: Rc<RefCell<Option<Rc<app::App>>>> = Rc::new(RefCell::new(None));
    let first = Rc::new(std::cell::Cell::new(true));
    gapp.connect_activate(move |gapp| {
        if let Some(existing) = instance.borrow().as_ref() {
            existing.present();
            return;
        }
        let app = app::App::new(gapp);
        {
            let weak = Rc::downgrade(&app);
            app.set_prefs_opener(move |page| {
                if let Some(a) = weak.upgrade() {
                    prefs::open(&a, page);
                }
            });
        }
        // Hidden only if there is a tray to find it in again.
        let start_hidden = first.replace(false) && hidden && app.settings.borrow().tray;
        if !start_hidden {
            app.present();
        }
        *instance.borrow_mut() = Some(app);
    });
    // GTK would reject `--hidden` as an unknown option.
    gapp.run_with_args(&args[..1])
}
