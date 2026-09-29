//! Account, certificate, the rest of the CLI's tools, and the services.

use crate::app::App;
use crate::services::{self, Unit, UNITS};
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

fn button(label: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::builder().label(label).tooltip_text(tip).valign(gtk::Align::Center).build();
    b
}

/// A row with one button that runs `pvpn <args>` and shows the output.
fn tool_row(app: &Rc<App>, title: &str, subtitle: &str, label: &str, args: &'static [&'static str]) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    let b = button(label, &format!("pvpn {}", args.join(" ")));
    let a = app.clone();
    let t = title.to_string();
    b.connect_clicked(move |_| a.run_to_dialog(&t, args));
    row.add_suffix(&b);
    row
}

pub fn build(app: &Rc<App>) -> gtk::Widget {
    let page = adw::PreferencesPage::new();

    // ---------- account ----------
    let account = adw::PreferencesGroup::builder().title("Proton account").build();
    let who = adw::ActionRow::builder().title("Account").subtitle("…").build();
    let view = button("Details", "pvpn account --view");
    {
        let a = app.clone();
        view.connect_clicked(move |_| a.run_to_dialog("Account", &["account", "--view"]));
    }
    who.add_suffix(&view);
    let plan = button("Plan", "protonvpn info — Proton's own account summary");
    {
        let a = app.clone();
        plan.connect_clicked(move |_| {
            let a = a.clone();
            glib::spawn_future_local(async move {
                let out = crate::runner::capture(vec!["protonvpn".into(), "info".into()]).await;
                a.show_text("Proton account", &out.combined());
            });
        });
    }
    who.add_suffix(&plan);
    account.add(&who);
    let signin = adw::ActionRow::builder()
        .title("Sign in")
        .subtitle("Opens a terminal for the password and 2FA prompts. The browser bridge solves a CAPTCHA in your browser.")
        .build();
    let signin_b = button("Sign in", "pvpn login");
    {
        let a = app.clone();
        signin_b.connect_clicked(move |_| a.run_in_terminal(&["login"]));
    }
    let bridge_b = button("Browser", "pvpn login --browser");
    {
        let a = app.clone();
        bridge_b.connect_clicked(move |_| a.run_in_terminal(&["login", "--browser"]));
    }
    signin.add_suffix(&signin_b);
    signin.add_suffix(&bridge_b);
    account.add(&signin);
    let signout = adw::ActionRow::builder().title("Sign out").subtitle("Clears the local session and credentials").build();
    let signout_b = button("Sign out", "pvpn logout");
    signout_b.add_css_class("destructive-action");
    {
        let a = app.clone();
        signout_b.connect_clicked(move |_| {
            let a2 = a.clone();
            a.confirm("Sign out of Proton?", "You will need to sign in again before the next connect.", "Sign out", true, move || {
                a2.run_capture(&["logout"], |a, out| a.toast(&out.headline()));
            });
        });
    }
    signout.add_suffix(&signout_b);
    account.add(&signout);
    page.add(&account);

    // ---------- certificate ----------
    let cert = adw::PreferencesGroup::builder()
        .title("Certificate")
        .description("Every server shares it. An expired one fails every connect identically, and no server is blamed for it.")
        .build();
    let cert_row = adw::ActionRow::builder().title("Client certificate").subtitle("…").build();
    let renew = button("Renew now", "pvpn cert --renew (over Tor if this network blocks Proton's API)");
    {
        let a = app.clone();
        renew.connect_clicked(move |_| a.run_net("Renewing certificate", &["cert", "--renew"]));
    }
    let cert_details = button("Details", "pvpn cert");
    {
        let a = app.clone();
        cert_details.connect_clicked(move |_| a.run_to_dialog("Certificate", &["cert"]));
    }
    cert_row.add_suffix(&cert_details);
    cert_row.add_suffix(&renew);
    cert.add(&cert_row);
    page.add(&cert);

    // ---------- connection tools ----------
    let tools = adw::PreferencesGroup::builder().title("Connection tools").build();
    tools.add(&tool_row(app, "Full status", "Connection, protocol, certificate, and whether the tunnel carries", "Status", &["status"]));
    tools.add(&tool_row(app, "Public IP", "The address the internet sees", "Show", &["ip"]));
    tools.add(&tool_row(app, "Protocols", "Which backends this install actually has", "List", &["protocols"]));
    tools.add(&tool_row(app, "Check the tunnel", "Send a packet through it and wait for one back; changes nothing", "Check", &["watch", "--check"]));
    let watch = adw::ActionRow::builder()
        .title("Check and repair")
        .subtitle("pvpn watch: a dead tunnel is recorded against its server and rebuilt — only where autoconnect allows")
        .build();
    let watch_b = button("Run", "pvpn watch");
    {
        let a = app.clone();
        watch_b.connect_clicked(move |_| a.run_net("Checking the tunnel", &["watch"]));
    }
    watch.add_suffix(&watch_b);
    tools.add(&watch);
    let try_row = adw::ActionRow::builder()
        .title("Try every protocol")
        .subtitle("Full connects, one protocol after another, until one carries. The winner becomes the default.")
        .build();
    let try_b = button("Try", "pvpn try");
    {
        let a = app.clone();
        try_b.connect_clicked(move |_| {
            let a2 = a.clone();
            a.confirm(
                "Try every protocol?",
                "Your internet will stall in bursts while each protocol is tried. Cancel stops it cleanly.",
                "Try",
                false,
                move || a2.run_net("Trying every protocol", &["try"]),
            );
        });
    }
    try_row.add_suffix(&try_b);
    tools.add(&try_row);
    page.add(&tools);

    // ---------- flatpak ----------
    let apps = adw::PreferencesGroup::builder()
        .title("Flatpak apps")
        .description("Apps with a proxy override route around the tunnel.")
        .build();
    apps.add(&tool_row(app, "Audit", "List apps whose traffic skips the tunnel", "Audit", &["apps"]));
    apps.add(&tool_row(app, "Verify", "Launch each flagged app and compare its exit IP", "Verify", &["apps", "--verify"]));
    let fix = adw::ActionRow::builder().title("Fix").subtitle("Remove the proxy settings that route apps around the tunnel").build();
    let fix_b = button("Fix", "pvpn apps --fix");
    {
        let a = app.clone();
        fix_b.connect_clicked(move |_| {
            let a2 = a.clone();
            a.confirm("Remove the proxy overrides?", "Flagged apps will send their traffic through the tunnel.", "Fix", false, move || {
                a2.run_to_dialog("Flatpak fix", &["apps", "--fix"]);
            });
        });
    }
    fix.add_suffix(&fix_b);
    apps.add(&fix);
    page.add(&apps);

    // ---------- privileged ----------
    let privileged = adw::PreferencesGroup::builder()
        .title("Privileged repairs")
        .description("These need sudo and open in a terminal.")
        .build();
    for (title, subtitle, label, args) in [
        ("Repair networking", "pvpn fix: undo whatever a failed or interrupted connect left behind", "Repair", &["fix"][..]),
        (
            "Blackhole Proton's API",
            "pvpn fix --hosts: stop Proton's client stalling on a blocked API. Remember to remove it — a stale entry forces every renewal onto Tor.",
            "Add",
            &["fix", "--hosts"][..],
        ),
        ("Remove the API blackhole", "pvpn fix --unhosts", "Remove", &["fix", "--unhosts"][..]),
    ] {
        let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
        let b = button(label, &format!("pvpn {}", args.join(" ")));
        let a = app.clone();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        b.connect_clicked(move |_| {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            a.run_in_terminal(&refs);
        });
        row.add_suffix(&b);
        privileged.add(&row);
    }
    page.add(&privileged);

    // ---------- services ----------
    let svc = adw::PreferencesGroup::builder()
        .title("Services")
        .description("pvpn's own units, and the system services the tunnel depends on. System units ask for your password.")
        .build();
    let refresh = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh").valign(gtk::Align::Center).build();
    refresh.add_css_class("flat");
    svc.set_header_suffix(Some(&refresh));
    let mut rows: Vec<(&'static Unit, adw::ActionRow)> = Vec::new();
    for unit in UNITS {
        let row = adw::ActionRow::builder()
            .title(format!("{} — {}", unit.title, unit.name))
            .subtitle(unit.blurb)
            .subtitle_lines(3)
            .build();
        let state = gtk::Label::new(Some("…"));
        state.add_css_class("caption");
        state.set_valign(gtk::Align::Center);
        state.set_wrap(true);
        state.set_max_width_chars(28);
        state.set_widget_name("state");
        row.add_suffix(&state);
        let menu = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        menu.add_css_class("linked");
        menu.set_valign(gtk::Align::Center);
        for verb in unit.verbs {
            let b = gtk::Button::with_label(verb.label());
            let a = app.clone();
            let verb = *verb;
            let refresh = refresh.clone();
            b.connect_clicked(move |_| {
                let run = {
                    let a = a.clone();
                    let refresh = refresh.clone();
                    move || {
                        let a = a.clone();
                        let refresh = refresh.clone();
                        glib::spawn_future_local(async move {
                            let out = services::run(unit, verb).await;
                            a.log_output(&services::command(unit, verb), &out);
                            a.toast(&if out.ok {
                                format!("{} {}", verb.label(), unit.name)
                            } else {
                                out.headline()
                            });
                            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || refresh.emit_clicked());
                        });
                    }
                };
                match unit.caution {
                    Some(why) => a.confirm(&format!("{} {}?", verb.label(), unit.title), why, verb.label(), true, run),
                    None => run(),
                }
            });
            menu.append(&b);
        }
        let journal = gtk::Button::builder().icon_name("text-x-generic-symbolic").tooltip_text("Recent journal").build();
        {
            let a = app.clone();
            journal.connect_clicked(move |_| {
                let argv = services::journal_command(unit.name, unit.user, 200);
                let a2 = a.clone();
                glib::spawn_future_local(async move {
                    let out = crate::runner::capture(argv).await;
                    a2.show_text(&format!("{} journal", unit.name), &out.combined());
                });
            });
        }
        menu.append(&journal);
        row.add_suffix(&menu);
        svc.add(&row);
        rows.push((unit, row));
    }
    page.add(&svc);

    let rows = Rc::new(rows);
    let states: Rc<dyn Fn()> = {
        let rows = rows.clone();
        Rc::new(move || {
            for (unit, row) in rows.iter() {
                let row = row.clone();
                let unit: &'static Unit = unit;
                glib::spawn_future_local(async move {
                    let st = services::state(unit).await;
                    if let Some(label) = find_named(row.upcast_ref(), "state") {
                        if let Ok(label) = label.downcast::<gtk::Label>() {
                            label.set_label(&st.summary());
                            for c in ["success", "warning", "error"] {
                                label.remove_css_class(c);
                            }
                            label.add_css_class(match st.active.as_str() {
                                "active" | "activating" => "success",
                                "failed" => "error",
                                _ if !st.loaded => "warning",
                                _ => "dim-label",
                            });
                        }
                    }
                    row.set_sensitive(st.loaded);
                });
            }
        })
    };
    {
        let s = states.clone();
        refresh.connect_clicked(move |_| s());
    }

    // Account and certificate lines: cheap, from files and the keyring
    // helper; refreshed whenever the page is shown.
    let who_c = who.clone();
    let cert_c = cert_row.clone();
    let a = app.clone();
    let details: Rc<dyn Fn()> = Rc::new(move || {
        let now = chrono::Utc::now();
        cert_c.set_subtitle(&match pvpn_core::cert::status_fast() {
            Some(c) => glib::markup_escape_text(&c.describe(now)).to_string(),
            None => "Unknown — Proton's client has not written one here".into(),
        });
        let who = who_c.clone();
        a.run_capture(&["account", "--view"], move |_, out| {
            let text = if out.stdout.trim().is_empty() { out.combined() } else { out.stdout.clone() };
            let first: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(2).collect();
            who.set_subtitle(&glib::markup_escape_text(&first.join(" · ")));
        });
    });
    {
        let s = states.clone();
        let d = details.clone();
        page.connect_map(move |_| {
            s();
            d();
        });
    }
    {
        let s = states.clone();
        let p = page.clone();
        glib::timeout_add_seconds_local(5, move || {
            if p.is_mapped() {
                s();
            }
            glib::ControlFlow::Continue
        });
    }
    page.upcast()
}

fn find_named(root: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
    if root.widget_name() == name {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_named(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}
