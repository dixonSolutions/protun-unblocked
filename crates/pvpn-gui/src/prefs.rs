//! Preferences: the window's own, all of `config.toml`, where autoconnect
//! may act, and editors for every file the tunnel reads.
//!
//! `config.toml` is edited in place with `toml_edit`, so comments and keys
//! this window does not know about survive; every write is checked against
//! `pvpn`'s own `Config` first, so the window cannot save a file `pvpn`
//! would refuse.

use crate::app::App;
use crate::notify;
use crate::runner;
use crate::services::{self, UNITS};
use crate::settings::{self, GuiSettings};
use adw::prelude::*;
use gtk::{gio, glib};
use pvpn_core::config::Config;
use pvpn_core::intent;
use pvpn_core::scope::{AutoconnectNetworks, Mode};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---------- config.toml ----------

pub fn validate_config(text: &str) -> Result<(), String> {
    toml::from_str::<Config>(text).map(|_| ()).map_err(|e| e.to_string())
}

/// Apply `f` to the config document and write it back, if `pvpn` would
/// still accept the result.
pub fn edit_config(f: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<(), String> {
    let path = Config::default_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    f(&mut doc);
    let new = doc.to_string();
    validate_config(&new)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, new).map_err(|e| e.to_string())
}

fn set_value(key: &str, value: impl Into<toml_edit::Value>) -> Result<(), String> {
    let key = key.to_string();
    let value = value.into();
    edit_config(move |doc| {
        doc[&key] = toml_edit::Item::Value(value);
    })
}

fn set_list(key: &str, list: &[String]) -> Result<(), String> {
    let arr: toml_edit::Array = list.iter().map(|s| s.as_str()).collect();
    set_value(key, arr)
}

fn load_config() -> Config {
    Config::load(&Config::default_path()).unwrap_or_default()
}

/// `wifi:home` → `home`: what a person would type, and what `scope`
/// matches against either form.
fn bare(network: &str) -> String {
    network.strip_prefix("wifi:").unwrap_or(network).to_string()
}

fn in_list(list: &[String], network: &str) -> bool {
    list.iter().any(|e| e.trim() == network || e.trim() == bare(network))
}

// ---------- building blocks ----------

fn report(app: &Rc<App>, r: Result<(), String>) {
    if let Err(e) = r {
        app.toast(&format!("Not saved: {e}"));
    }
}

fn switch_row(title: &str, subtitle: &str, active: bool, f: impl Fn(bool) + 'static) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(title).subtitle(subtitle).active(active).build();
    row.connect_active_notify(move |r| f(r.is_active()));
    row
}

fn spin_row(title: &str, subtitle: &str, min: f64, max: f64, value: f64, f: impl Fn(f64) + 'static) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, 1.0);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_value(value);
    row.connect_value_notify(move |r| f(r.value()));
    row
}

fn entry_row(title: &str, text: &str, f: impl Fn(String) + 'static) -> adw::EntryRow {
    let row = adw::EntryRow::builder().title(title).text(text).show_apply_button(true).build();
    row.connect_apply(move |r| f(r.text().to_string()));
    row
}

fn with_gui(app: &Rc<App>, f: impl FnOnce(&mut GuiSettings)) {
    f(&mut app.settings.borrow_mut());
    app.save_settings();
}

// ---------- the dialog ----------

pub fn open(app: &Rc<App>, page: Option<&str>) {
    let dialog = adw::PreferencesDialog::builder().search_enabled(true).build();
    dialog.add(&general(app));
    dialog.add(&notifications(app));
    dialog.add(&proton_page(app));
    dialog.add(&connection(app));
    dialog.add(&autoconnect(app, &dialog));
    dialog.add(&files(app));
    if let Some(p) = page {
        dialog.set_visible_page_name(p);
    }
    dialog.present(Some(&app.window));
}

fn general(app: &Rc<App>) -> adw::PreferencesPage {
    let s = app.settings.borrow().clone();
    let page = adw::PreferencesPage::builder().title("General").icon_name("preferences-system-symbolic").name("general").build();

    let window = adw::PreferencesGroup::builder().title("Window and tray").build();
    {
        let a = app.clone();
        window.add(&switch_row("Tray icon", "In the top bar. On GNOME this needs the AppIndicator extension.", s.tray, move |v| {
            with_gui(&a, |g| g.tray = v);
            a.apply_tray();
        }));
    }
    {
        let a = app.clone();
        window.add(&switch_row("Tray icon whenever pvpn connects", "pvpn up, hop and try bring the icon up even when this window was never opened", s.tray_with_pvpn, move |v| {
            with_gui(&a, |g| g.tray_with_pvpn = v)
        }));
    }
    {
        let a = app.clone();
        window.add(&switch_row("Close to tray", "Closing the window keeps watching from the tray", s.close_to_tray, move |v| {
            with_gui(&a, |g| g.close_to_tray = v)
        }));
    }
    {
        let a = app.clone();
        window.add(&switch_row("Start with the session", "Adds an autostart entry", s.autostart, move |v| {
            with_gui(&a, |g| g.autostart = v);
            if let Err(e) = settings::apply_autostart(&a.settings.borrow()) {
                a.toast(&format!("Autostart: {e}"));
            }
        }));
    }
    {
        let a = app.clone();
        window.add(&switch_row("Start hidden", "When started with the session, stay in the tray", s.start_hidden, move |v| {
            with_gui(&a, |g| g.start_hidden = v);
            let _ = settings::apply_autostart(&a.settings.borrow());
        }));
    }
    page.add(&window);

    let status = adw::PreferencesGroup::builder()
        .title("Watching")
        .description("The window only watches. Rebuilding a dead tunnel is the watch timer's and autoconnect's job, on the networks you allow.")
        .build();
    {
        let a = app.clone();
        status.add(&spin_row("Read the state every", "Seconds. Routes and NetworkManager only; costs nothing.", 1.0, 60.0, s.poll_secs as f64, move |v| {
            with_gui(&a, |g| g.poll_secs = v as u32);
            a.restart_polling();
        }));
    }
    {
        let a = app.clone();
        status.add(&spin_row("Check traffic every", "Seconds; 0 never. Sends a request through the tunnel to prove it carries.", 0.0, 3600.0, s.traffic_check_secs as f64, move |v| {
            with_gui(&a, |g| g.traffic_check_secs = v as u32)
        }));
    }
    page.add(&status);

    let connect = adw::PreferencesGroup::builder().title("Connecting").build();
    let mut labels = vec!["Automatic (pvpn's choice)"];
    labels.extend(crate::pages::connect::PROTOCOLS);
    let proto = adw::ComboRow::builder()
        .title("Protocol")
        .subtitle("For Connect and Switch server. Stealth (protun-tls) is the default on filtered networks.")
        .model(&gtk::StringList::new(&labels))
        .build();
    proto.set_selected(
        crate::pages::connect::PROTOCOLS.iter().position(|p| *p == s.protocol).map(|i| i as u32 + 1).unwrap_or(0),
    );
    {
        let a = app.clone();
        proto.connect_selected_notify(move |r| {
            let i = r.selected() as usize;
            with_gui(&a, |g| g.protocol = if i == 0 { String::new() } else { crate::pages::connect::PROTOCOLS[i - 1].to_string() });
        });
    }
    connect.add(&proto);
    {
        let a = app.clone();
        let row = entry_row("pvpn to run (empty: ~/.local/bin/pvpn, then PATH)", &s.pvpn_path, move |t| {
            with_gui(&a, |g| g.pvpn_path = t.trim().to_string());
            a.toast(&format!("Using {}", a.settings.borrow().pvpn_binary()));
        });
        connect.add(&row);
    }
    page.add(&connect);

    let rank = adw::PreferencesGroup::builder().title("Measure &amp; rank").description("Options for pvpn best from the Servers page and the orb.").build();
    {
        let a = app.clone();
        rank.add(&spin_row("Servers to rank", "", 1.0, 100.0, s.rank_limit as f64, move |v| with_gui(&a, |g| g.rank_limit = v as u32)));
    }
    {
        let a = app.clone();
        rank.add(&entry_row("Only this country (e.g. JP; empty for any)", &s.rank_country, move |t| {
            with_gui(&a, |g| g.rank_country = t.trim().to_uppercase())
        }));
    }
    {
        let a = app.clone();
        rank.add(&switch_row("Free servers only", "", s.rank_free_only, move |v| with_gui(&a, |g| g.rank_free_only = v)));
    }
    page.add(&rank);

    let globe = adw::PreferencesGroup::builder().title("Globe").build();
    {
        let a = app.clone();
        globe.add(&spin_row("Numbered labels", "How many ranked servers get their name on the globe", 0.0, 50.0, s.globe_labels as f64, move |v| {
            with_gui(&a, |g| g.globe_labels = v as u32);
            a.reload_data();
        }));
    }
    {
        let a = app.clone();
        globe.add(&switch_row("Show blocked servers", "", s.globe_show_blocked, move |v| {
            with_gui(&a, |g| g.globe_show_blocked = v);
            a.reload_data();
        }));
    }
    page.add(&globe);
    page
}

fn notifications(app: &Rc<App>) -> adw::PreferencesPage {
    let n = app.settings.borrow().notifications.clone();
    let page = adw::PreferencesPage::builder().title("Notifications").icon_name("preferences-system-notifications-symbolic").name("notifications").build();
    let master = adw::PreferencesGroup::builder().build();
    let kinds = adw::PreferencesGroup::builder().title("Tell me when").sensitive(n.enabled).build();
    {
        let a = app.clone();
        let k = kinds.clone();
        master.add(&switch_row("Notifications", "Desktop notifications for connection changes", n.enabled, move |v| {
            with_gui(&a, |g| g.notifications.enabled = v);
            k.set_sensitive(v);
        }));
    }
    let test = gtk::Button::builder().label("Send a test").valign(gtk::Align::Center).build();
    test.connect_clicked(|_| notify::send_raw("Protun Unblocked", "Notifications work.", "network-vpn-symbolic", 1));
    master.set_header_suffix(Some(&test));
    page.add(&master);

    type Field = fn(&mut settings::Notifications) -> &mut bool;
    let items: [(&str, &str, bool, Field); 5] = [
        ("Connected", "A connect you asked for landed", n.connected, |x| &mut x.connected),
        ("Disconnected", "The tunnel went down — asked for or not", n.disconnected, |x| &mut x.disconnected),
        ("Reconnecting", "Autoconnect or the watch timer started rebuilding a tunnel", n.reconnecting, |x| &mut x.reconnecting),
        ("Reconnected", "That rebuild landed", n.reconnected, |x| &mut x.reconnected),
        ("Failure", "A connect failed, or a tunnel stopped carrying traffic", n.failure, |x| &mut x.failure),
    ];
    for (title, subtitle, value, field) in items {
        let a = app.clone();
        kinds.add(&switch_row(title, subtitle, value, move |v| with_gui(&a, |g| *field(&mut g.notifications) = v)));
    }
    page.add(&kinds);
    page
}

/// Proton's own client settings, through `protonvpn config set`.
fn proton_page(app: &Rc<App>) -> adw::PreferencesPage {
    use crate::proton::{self, Kind, SETTINGS};
    let page = adw::PreferencesPage::builder().title("Proton VPN").icon_name("security-high-symbolic").name("proton").build();
    let json = std::fs::read_to_string(proton::settings_path()).unwrap_or_default();
    let initial = proton::from_settings_json(&json);
    let group = adw::PreferencesGroup::builder()
        .title("Proton VPN client")
        .description("The settings Proton's own app has, applied with `protonvpn config set` so Proton's checks apply. They take effect on the next connect. Checking what your plan includes…")
        .build();
    let status = group.clone();

    // Rows are built with what settings.json says, then corrected — and
    // locked where the plan does not include them — once `config list`
    // answers.
    let rows: Rc<std::cell::RefCell<Vec<(&'static str, gtk::Widget)>>> = Rc::default();
    // Changes made by this code, not the user, must not run a command.
    let quiet = Rc::new(std::cell::Cell::new(true));
    for setting in &SETTINGS {
        let value = initial.get(setting.key).cloned().unwrap_or_default();
        let run = {
            let a = app.clone();
            move |argv: Vec<String>| {
                let a = a.clone();
                glib::spawn_future_local(async move {
                    let out = runner::capture(argv.clone()).await;
                    a.log_output(&argv, &out);
                    a.toast(&out.headline());
                });
            }
        };
        let widget: gtk::Widget = match setting.kind {
            Kind::Choice(choices) => {
                let labels: Vec<&str> = choices.iter().map(|c| c.0).collect();
                let row = adw::ComboRow::builder().title(setting.title).subtitle(setting.subtitle).model(&gtk::StringList::new(&labels)).build();
                row.set_selected(choices.iter().position(|c| c.1 == value).unwrap_or(0) as u32);
                let key = setting.key;
                let q = quiet.clone();
                row.connect_selected_notify(move |r| {
                    if !q.get() {
                        run(proton::set_command(key, choices[r.selected() as usize].1, None));
                    }
                });
                row.upcast()
            }
            Kind::Toggle => {
                let row = adw::SwitchRow::builder().title(setting.title).subtitle(setting.subtitle).active(proton::is_on(&value)).build();
                let key = setting.key;
                let q = quiet.clone();
                row.connect_active_notify(move |r| {
                    if !q.get() {
                        run(proton::set_command(key, if r.is_active() { "on" } else { "off" }, None));
                    }
                });
                row.upcast()
            }
            Kind::Dns => {
                let row = adw::ExpanderRow::builder()
                    .title(setting.title)
                    .subtitle(setting.subtitle)
                    .show_enable_switch(true)
                    .enable_expansion(proton::is_on(&value))
                    .build();
                let entry = adw::EntryRow::builder().title("Servers, e.g. 1.1.1.1, 9.9.9.9").text(proton::custom_dns_ips(&json).join(", ")).show_apply_button(true).build();
                row.add_row(&entry);
                let q = quiet.clone();
                let e = entry.clone();
                let run2 = run.clone();
                row.connect_enable_expansion_notify(move |r| {
                    if q.get() {
                        return;
                    }
                    if r.enables_expansion() {
                        let list = e.text().to_string();
                        if !list.trim().is_empty() {
                            run2(proton::set_command("custom-dns", "on", Some(&list)));
                        }
                    } else {
                        run2(proton::set_command("custom-dns", "off", None));
                    }
                });
                entry.connect_apply(move |e| run(proton::set_command("custom-dns", "on", Some(&e.text()))));
                row.upcast()
            }
        };
        group.add(&widget);
        rows.borrow_mut().push((setting.key, widget));
    }
    page.add(&group);

    let (rows2, q) = (rows.clone(), quiet.clone());
    glib::spawn_future_local(async move {
        let out = runner::capture(vec!["protonvpn".into(), "config".into(), "list".into()]).await;
        let listed = proton::parse_config_list(&out.stdout);
        if listed.is_empty() {
            status.set_description(Some("The settings Proton's own app has, applied with `protonvpn config set`. Could not read `protonvpn config list`; showing settings.json."));
            q.set(false);
            return;
        }
        let mut locked = 0;
        for (key, widget) in rows2.borrow().iter() {
            let Some(value) = listed.get(*key) else { continue };
            if value == proton::UPGRADE {
                widget.set_sensitive(false);
                widget.set_tooltip_text(Some("Not on your plan"));
                locked += 1;
                continue;
            }
            let setting = SETTINGS.iter().find(|s| s.key == *key).unwrap();
            match setting.kind {
                Kind::Choice(choices) => {
                    if let (Ok(row), Some(i)) = (widget.clone().downcast::<adw::ComboRow>(), choices.iter().position(|c| c.1 == value)) {
                        row.set_selected(i as u32);
                    }
                }
                Kind::Toggle => {
                    if let Ok(row) = widget.clone().downcast::<adw::SwitchRow>() {
                        row.set_active(proton::is_on(value));
                    }
                }
                Kind::Dns => {
                    if let Ok(row) = widget.clone().downcast::<adw::ExpanderRow>() {
                        row.set_enable_expansion(proton::is_on(value));
                    }
                }
            }
        }
        status.set_description(Some(&if locked > 0 {
            format!("The settings Proton's own app has, applied with `protonvpn config set`. They take effect on the next connect. {locked} need a paid plan.")
        } else {
            "The settings Proton's own app has, applied with `protonvpn config set`. They take effect on the next connect.".to_string()
        }));
        q.set(false);
    });

    let account = adw::PreferencesGroup::builder().title("Proton account").build();
    let info = adw::ActionRow::builder().title("Account and plan").subtitle("protonvpn info").activatable(true).build();
    info.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    {
        let a = app.clone();
        info.connect_activated(move |_| {
            let a = a.clone();
            glib::spawn_future_local(async move {
                let out = runner::capture(vec!["protonvpn".into(), "info".into()]).await;
                a.show_text("Proton account", &out.combined());
            });
        });
    }
    account.add(&info);
    let upgrade = adw::ActionRow::builder().title("Plans").subtitle("account.protonvpn.com/pricing — sign out and in again afterwards").activatable(true).build();
    upgrade.add_suffix(&gtk::Image::from_icon_name("adw-external-link-symbolic"));
    {
        let w = app.window.clone();
        upgrade.connect_activated(move |_| {
            gtk::UriLauncher::new("https://account.protonvpn.com/pricing").launch(Some(&w), gio::Cancellable::NONE, |_| {});
        });
    }
    account.add(&upgrade);
    page.add(&account);
    page
}

fn connection(app: &Rc<App>) -> adw::PreferencesPage {
    let c = load_config();
    let page = adw::PreferencesPage::builder().title("Connection").icon_name("network-vpn-symbolic").name("connection").build();

    let overrides: Vec<String> = std::env::vars().map(|(k, _)| k).filter(|k| k.starts_with("PVPN_")).collect();
    let choosing = adw::PreferencesGroup::builder()
        .title("Choosing a server")
        .description(if overrides.is_empty() {
            "~/.config/pvpn/config.toml — read by every pvpn, the hooks and the timer included.".to_string()
        } else {
            format!("~/.config/pvpn/config.toml. Overridden in this session by: {}", overrides.join(", "))
        })
        .build();
    macro_rules! sw {
        ($group:expr, $title:expr, $sub:expr, $key:literal, $val:expr) => {{
            let a = app.clone();
            $group.add(&switch_row($title, $sub, $val, move |v| report(&a, set_value($key, v))));
        }};
    }
    macro_rules! sp {
        ($group:expr, $title:expr, $sub:expr, $key:literal, $min:expr, $max:expr, $val:expr) => {{
            let a = app.clone();
            $group.add(&spin_row($title, $sub, $min as f64, $max as f64, $val as f64, move |v| {
                report(&a, set_value($key, v as i64))
            }));
        }};
    }
    sw!(choosing, "Rank before connecting", "Connect down a freshly measured ranking rather than Proton's own pick", "auto_best_server", c.auto_best_server);
    {
        let a = app.clone();
        choosing.add(&entry_row("Country (e.g. JP; empty for any)", c.country.as_deref().unwrap_or(""), move |t| {
            let t = t.trim().to_uppercase();
            report(&a, edit_config(move |doc| {
                if t.is_empty() {
                    doc.remove("country");
                } else {
                    doc["country"] = toml_edit::value(t);
                }
            }));
        }));
    }
    sw!(choosing, "Free servers only", "", "free_only", c.free_only);
    sp!(choosing, "Blocked servers retried after", "Hours. Longer for a server that keeps failing.", "blocked_retry_after_hours", 1, 720, c.blocked_retry_after_hours);
    page.add(&choosing);

    let timing = adw::PreferencesGroup::builder().title("Timing").build();
    sp!(timing, "Connect timeout", "Seconds per attempt", "connect_timeout_secs", 5, 600, c.connect_timeout_secs);
    sp!(timing, "Settle", "Seconds a new tunnel gets to start carrying before it is written off", "settle_secs", 5, 600, c.settle_secs);
    sp!(timing, "Server list is stale after", "Hours", "stale_hours", 1, 720, c.stale_hours);
    sp!(timing, "Server list refresh timeout", "Seconds", "refresh_timeout_secs", 10, 3600, c.refresh_timeout_secs);
    sp!(timing, "Measure &amp; rank timeout", "Seconds", "best_timeout_secs", 10, 900, c.best_timeout_secs);
    page.add(&timing);

    let probing = adw::PreferencesGroup::builder().title("Measuring").build();
    sp!(probing, "Shortlist", "Servers given one measurement pass", "probe_shortlist", 1, 500, c.probe_shortlist);
    sp!(probing, "Finalists", "Re-timed without contention", "probe_refine", 1, 100, c.probe_refine);
    sp!(probing, "Rounds", "", "probe_rounds", 1, 20, c.probe_rounds);
    page.add(&probing);

    let after = adw::PreferencesGroup::builder().title("After connecting").build();
    sw!(after, "Put Flatpak apps back on the tunnel", "Remove proxy overrides after a successful connect", "fix_apps", c.fix_apps);
    page.add(&after);
    page
}

fn autoconnect(app: &Rc<App>, dialog: &adw::PreferencesDialog) -> adw::PreferencesPage {
    let c = load_config();
    let page = adw::PreferencesPage::builder().title("Autoconnect").icon_name("view-refresh-symbolic").name("autoconnect").build();
    let snap = app.snapshot.borrow().clone();
    let here = snap.as_ref().map(|s| s.network.clone()).unwrap_or_else(pvpn_core::proc::active_network_key);

    // This network first: the one question that matters most, answered in
    // one switch.
    let this = adw::PreferencesGroup::builder()
        .title(format!("This network: {}", bare(&here)))
        .description(match &snap {
            Some(s) => match &s.autoconnect_here {
                Ok(why) => format!("Autoconnect may rebuild the tunnel here — {why}."),
                Err(why) => format!("Autoconnect will not act here — {why}."),
            },
            None => String::new(),
        })
        .build();
    {
        let a = app.clone();
        let net = here.clone();
        let d = dialog.clone();
        this.add(&switch_row(
            "Never reconnect automatically here",
            "Adds this network to autoconnect_never, which wins over every other rule. Connecting by hand still works.",
            in_list(&c.autoconnect_never, &here),
            move |on| {
                let mut list = load_config().autoconnect_never;
                list.retain(|e| e.trim() != net && e.trim() != bare(&net));
                if on {
                    list.push(bare(&net));
                }
                report(&a, set_list("autoconnect_never", &list));
                a.poll_now(false);
                let _ = &d;
            },
        ));
    }
    page.add(&this);

    let master = adw::PreferencesGroup::builder()
        .title("Automatic reconnects")
        .description("After a resume or a link change, pvpn-autoconnect rebuilds the tunnel; every two minutes the watch timer checks a tunnel that is up. Both only act where the rules below allow.")
        .build();
    {
        let a = app.clone();
        master.add(&switch_row(
            "Autoconnect",
            "Off everywhere writes ~/.config/pvpn/autoconnect-off (pvpn-autoconnect --off)",
            !intent::autoconnect_is_off(&Config::config_dir()),
            move |on| {
                let marker = intent::autoconnect_off_marker(&Config::config_dir());
                let r = if on {
                    std::fs::remove_file(&marker).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
                } else {
                    std::fs::create_dir_all(Config::config_dir()).and_then(|_| std::fs::write(&marker, ""))
                };
                report(&a, r.map_err(|e| e.to_string()));
                a.poll_now(false);
            },
        ));
    }
    let hold = adw::ActionRow::builder()
        .title("Hold after disconnect")
        .subtitle(if intent::is_down_by_user(&Config::data_dir()) {
            "Holding: you disconnected, so a resume leaves the tunnel down until you connect again."
        } else {
            "Not holding."
        })
        .build();
    let release = gtk::Button::builder().label("Release").valign(gtk::Align::Center).sensitive(intent::is_down_by_user(&Config::data_dir())).build();
    {
        let a = app.clone();
        let hold = hold.clone();
        release.connect_clicked(move |b| {
            intent::clear_down(&Config::data_dir());
            hold.set_subtitle("Not holding.");
            b.set_sensitive(false);
            a.poll_now(false);
        });
    }
    hold.add_suffix(&release);
    master.add(&hold);
    // The watch timer.
    let timer = adw::SwitchRow::builder().title("Health timer").subtitle("pvpn-watch.timer — checks a tunnel that is up every two minutes").build();
    {
        let t = timer.clone();
        glib::spawn_future_local(async move {
            let unit = UNITS.iter().find(|u| u.name == "pvpn-watch.timer").unwrap();
            let st = services::state(unit).await;
            t.set_sensitive(st.loaded);
            t.set_active(st.file_state == "enabled");
        });
        let a = app.clone();
        timer.connect_active_notify(move |t| {
            let on = t.is_active();
            let a = a.clone();
            glib::spawn_future_local(async move {
                let argv: Vec<String> = ["systemctl", "--user", if on { "enable" } else { "disable" }, "--now", "pvpn-watch.timer"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                let out = runner::capture(argv.clone()).await;
                a.log_output(&argv, &out);
                if !out.ok {
                    a.toast(&out.headline());
                }
            });
        });
    }
    master.add(&timer);
    page.add(&master);

    // Where.
    let scope = adw::PreferencesGroup::builder().title("Where").build();
    let mode = adw::ComboRow::builder()
        .title("Reconnect on")
        .model(&gtk::StringList::new(&["The network where you last connected", "Any network", "Only the networks listed below"]))
        .build();
    let (mode_idx, listed) = match &c.autoconnect_networks {
        AutoconnectNetworks::Mode(Mode::Started) => (0, Vec::new()),
        AutoconnectNetworks::Mode(Mode::All) => (1, Vec::new()),
        AutoconnectNetworks::List(l) => (2, l.clone()),
    };
    mode.set_selected(mode_idx);
    scope.add(&mode);
    let known: Vec<String> = crate::data::networks(&crate::data::load_state());
    let allow = list_editor(app, "autoconnect_networks", "Allowed networks", "SSID or network key, e.g. detnsw or wired:eth0", listed, &known);
    allow.set_visible(mode_idx == 2);
    {
        let a = app.clone();
        let allow = allow.clone();
        mode.connect_selected_notify(move |r| {
            let i = r.selected();
            allow.set_visible(i == 2);
            let result = match i {
                0 => set_value("autoconnect_networks", "started"),
                1 => set_value("autoconnect_networks", "all"),
                _ => {
                    let current = match load_config().autoconnect_networks {
                        AutoconnectNetworks::List(l) => l,
                        _ => Vec::new(),
                    };
                    set_list("autoconnect_networks", &current)
                }
            };
            report(&a, result);
            a.poll_now(false);
        });
    }
    page.add(&scope);
    page.add(&allow);
    page.add(&list_editor(
        app,
        "autoconnect_never",
        "Never on these networks",
        "Wins over everything above",
        c.autoconnect_never.clone(),
        &known,
    ));
    page
}

/// An editable list of networks backed by one config key.
fn list_editor(app: &Rc<App>, key: &'static str, title: &str, description: &str, initial: Vec<String>, known: &[String]) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).description(description).build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    list.add_css_class("boxed-list");
    group.add(&list);
    let items = Rc::new(std::cell::RefCell::new(initial));

    let rebuild: Rc<std::cell::RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(std::cell::RefCell::new(None));
    let add_row = adw::EntryRow::builder().title("Add a network").show_apply_button(true).build();
    // Suggestions: networks this machine has records for.
    let suggest = gtk::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text("Known networks").valign(gtk::Align::Center).build();
    suggest.add_css_class("flat");
    let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let pop = gtk::Popover::builder().child(&pop_box).build();
    suggest.set_popover(Some(&pop));
    add_row.add_suffix(&suggest);

    let save = {
        let a = app.clone();
        let items = items.clone();
        move || {
            report(&a, set_list(key, &items.borrow()));
            a.poll_now(false);
        }
    };
    let save: Rc<dyn Fn()> = Rc::new(save);

    let draw: Rc<dyn Fn()> = {
        let list = list.clone();
        let items = items.clone();
        let save = save.clone();
        let rebuild = rebuild.clone();
        let add_row = add_row.clone();
        Rc::new(move || {
            while let Some(c) = list.first_child() {
                list.remove(&c);
            }
            for (i, entry) in items.borrow().iter().enumerate() {
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(entry)).build();
                let rm = gtk::Button::builder().icon_name("user-trash-symbolic").valign(gtk::Align::Center).tooltip_text("Remove").build();
                rm.add_css_class("flat");
                let items = items.clone();
                let save = save.clone();
                let rebuild = rebuild.clone();
                rm.connect_clicked(move |_| {
                    items.borrow_mut().remove(i);
                    save();
                    if let Some(r) = rebuild.borrow().as_ref() {
                        r();
                    }
                });
                row.add_suffix(&rm);
                list.append(&row);
            }
            list.append(&add_row);
        })
    };
    *rebuild.borrow_mut() = Some(draw.clone());

    let add = {
        let items = items.clone();
        let save = save.clone();
        let draw = draw.clone();
        Rc::new(move |value: String| {
            let value = bare(value.trim());
            if value.is_empty() || items.borrow().iter().any(|e| *e == value) {
                return;
            }
            items.borrow_mut().push(value);
            save();
            draw();
        })
    };
    {
        let add = add.clone();
        add_row.connect_apply(move |r| {
            add(r.text().to_string());
            r.set_text("");
        });
    }
    for net in known {
        let b = gtk::Button::with_label(&bare(net));
        b.add_css_class("flat");
        let add = add.clone();
        let net = net.clone();
        let pop = pop.clone();
        b.connect_clicked(move |_| {
            pop.popdown();
            add(net.clone());
        });
        pop_box.append(&b);
    }
    draw();
    group
}

// ---------- files ----------

struct FileSpec {
    title: &'static str,
    subtitle: &'static str,
    path: fn() -> PathBuf,
    privileged: bool,
    kind: Check,
}

#[derive(Clone, Copy)]
enum Check {
    PvpnConfig,
    GuiConfig,
    Json,
    Torrc,
    Plain,
}

fn proton_settings() -> PathBuf {
    settings::dirs_home().join(".config/Proton/VPN/settings.json")
}

const FILES: [FileSpec; 5] = [
    FileSpec {
        title: "pvpn config",
        subtitle: "~/.config/pvpn/config.toml — everything on the Connection and Autoconnect pages, with comments kept",
        path: Config::default_path,
        privileged: false,
        kind: Check::PvpnConfig,
    },
    FileSpec {
        title: "Window settings",
        subtitle: "~/.config/pvpn/gui.toml",
        path: GuiSettings::path,
        privileged: false,
        kind: Check::GuiConfig,
    },
    FileSpec {
        title: "Proton VPN client settings",
        subtitle: "~/.config/Proton/VPN/settings.json — protocol, kill switch, NetShield, as Proton's client reads them",
        path: proton_settings,
        privileged: false,
        kind: Check::Json,
    },
    FileSpec {
        title: "Tor",
        subtitle: "/etc/tor/torrc — checked with tor --verify-config before saving",
        path: || PathBuf::from("/etc/tor/torrc"),
        privileged: true,
        kind: Check::Torrc,
    },
    FileSpec {
        title: "torsocks",
        subtitle: "/etc/tor/torsocks.conf",
        path: || PathBuf::from("/etc/tor/torsocks.conf"),
        privileged: true,
        kind: Check::Plain,
    },
];

fn files(app: &Rc<App>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().title("Files").icon_name("document-edit-symbolic").name("files").build();
    let group = adw::PreferencesGroup::builder()
        .title("Edit directly")
        .description("A copy of the previous version is kept in ~/.cache/pvpn-gui/backups before every save.")
        .build();
    for spec in &FILES {
        let row = adw::ActionRow::builder().title(spec.title).subtitle(spec.subtitle).activatable(true).build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let a = app.clone();
        row.connect_activated(move |_| editor(&a, spec));
        group.add(&row);
    }
    page.add(&group);

    let tor = adw::PreferencesGroup::builder().title("Tor service").build();
    let tor_row = adw::ActionRow::builder().title("tor.service").subtitle("Restart after editing torrc").build();
    for verb in [services::Verb::Restart, services::Verb::Stop, services::Verb::Start] {
        let b = gtk::Button::builder().label(verb.label()).valign(gtk::Align::Center).build();
        let a = app.clone();
        b.connect_clicked(move |_| {
            let a = a.clone();
            glib::spawn_future_local(async move {
                let unit = UNITS.iter().find(|u| u.name == "tor.service").unwrap();
                let out = services::run(unit, verb).await;
                a.log_output(&services::command(unit, verb), &out);
                a.toast(&if out.ok { format!("Tor: {}", verb.word()) } else { out.headline() });
            });
        });
        tor_row.add_suffix(&b);
    }
    tor.add(&tor_row);
    page.add(&tor);

    let reset = adw::PreferencesGroup::builder().title("Start over").build();
    let reset_row = adw::ActionRow::builder()
        .title("Reset pvpn config")
        .subtitle("Back up config.toml and replace it with the defaults. Your autoconnect_never list is kept.")
        .build();
    let reset_b = gtk::Button::builder().label("Reset").valign(gtk::Align::Center).build();
    reset_b.add_css_class("destructive-action");
    {
        let a = app.clone();
        reset_b.connect_clicked(move |_| {
            let a2 = a.clone();
            a.confirm("Reset pvpn config?", "The current file is backed up first.", "Reset", true, move || {
                let path = Config::default_path();
                backup(&path);
                let mut fresh = Config::default();
                fresh.autoconnect_never = load_config().autoconnect_never;
                match fresh.save(&path) {
                    Ok(()) => a2.toast("Config reset to defaults"),
                    Err(e) => a2.toast(&format!("Not reset: {e}")),
                }
            });
        });
    }
    reset_row.add_suffix(&reset_b);
    reset.add(&reset_row);
    page.add(&reset);
    page
}

fn backup_dir() -> PathBuf {
    crate::data::last_best_path().with_file_name("backups")
}

fn backup(path: &Path) {
    let Ok(text) = std::fs::read(path) else { return };
    let dir = backup_dir();
    let _ = std::fs::create_dir_all(&dir);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let _ = std::fs::write(dir.join(format!("{name}.{stamp}")), text);
}

async fn check(kind: Check, text: &str) -> Result<(), String> {
    match kind {
        Check::PvpnConfig => validate_config(text),
        Check::GuiConfig => toml::from_str::<GuiSettings>(text).map(|_| ()).map_err(|e| e.to_string()),
        Check::Json => serde_json::from_str::<serde_json::Value>(text).map(|_| ()).map_err(|e| e.to_string()),
        Check::Plain => Ok(()),
        Check::Torrc => {
            if glib::find_program_in_path("tor").is_none() {
                return Ok(());
            }
            let tmp = std::env::temp_dir().join(format!("pvpn-gui-torrc-{}", std::process::id()));
            std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
            let out = runner::capture(vec![
                "tor".into(),
                "--verify-config".into(),
                "-f".into(),
                tmp.to_string_lossy().into_owned(),
            ])
            .await;
            let _ = std::fs::remove_file(&tmp);
            if out.ok {
                Ok(())
            } else {
                let why = out.combined().lines().filter(|l| l.contains("[warn]") || l.contains("[err]")).collect::<Vec<_>>().join("\n");
                Err(if why.is_empty() { out.headline() } else { why })
            }
        }
    }
}

async fn write_privileged(path: &Path, text: &str) -> Result<(), String> {
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
    );
    let path = path.to_string_lossy().into_owned();
    let proc = launcher
        .spawn(&[std::ffi::OsStr::new("pkexec"), std::ffi::OsStr::new("tee"), std::ffi::OsStr::new(&path)])
        .map_err(|e| e.to_string())?;
    let (_, err) = proc.communicate_utf8_future(Some(text.to_string())).await.map_err(|e| e.to_string())?;
    if proc.has_exited() && proc.exit_status() == 0 {
        Ok(())
    } else {
        Err(err.map(|e| e.to_string()).filter(|e| !e.trim().is_empty()).unwrap_or_else(|| "cancelled".into()))
    }
}

fn editor(app: &Rc<App>, spec: &'static FileSpec) {
    let path = (spec.path)();
    let original = std::fs::read_to_string(&path).unwrap_or_default();
    let dialog = adw::Dialog::builder().title(spec.title).content_width(760).content_height(620).build();
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&original);
    let view = gtk::TextView::builder()
        .buffer(&buffer)
        .monospace(true)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
    let status = gtk::Label::builder().xalign(0.0).wrap(true).margin_start(12).margin_end(12).margin_bottom(8).build();
    status.add_css_class("caption");
    status.set_label(&path.display().to_string());
    let save = gtk::Button::builder().label("Save").build();
    save.add_css_class("suggested-action");
    let revert = gtk::Button::builder().icon_name("edit-undo-symbolic").tooltip_text("Revert to what is on disk").build();
    let header = adw::HeaderBar::new();
    header.pack_end(&save);
    header.pack_start(&revert);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&scroll);
    content.append(&status);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    dialog.set_child(Some(&tv));

    {
        let buffer = buffer.clone();
        let original = original.clone();
        revert.connect_clicked(move |_| buffer.set_text(&original));
    }
    {
        let a = app.clone();
        let buffer = buffer.clone();
        let status = status.clone();
        let dialog = dialog.clone();
        save.connect_clicked(move |b| {
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
            let a = a.clone();
            let status = status.clone();
            let dialog = dialog.clone();
            let path = path.clone();
            let b = b.clone();
            b.set_sensitive(false);
            glib::spawn_future_local(async move {
                let result = match check(spec.kind, &text).await {
                    Err(e) => Err(format!("Not saved — this would not load:\n{e}")),
                    Ok(()) => {
                        backup(&path);
                        if spec.privileged {
                            write_privileged(&path, &text).await
                        } else {
                            path.parent().map(std::fs::create_dir_all);
                            std::fs::write(&path, &text).map_err(|e| e.to_string())
                        }
                    }
                };
                b.set_sensitive(true);
                match result {
                    Ok(()) => {
                        if matches!(spec.kind, Check::GuiConfig) {
                            *a.settings.borrow_mut() = GuiSettings::load();
                            a.apply_tray();
                            a.restart_polling();
                        }
                        a.toast(&format!("Saved {}", path.display()));
                        if matches!(spec.kind, Check::Torrc) {
                            a.toast("Restart Tor for it to take effect");
                        }
                        dialog.close();
                    }
                    Err(e) => {
                        status.set_label(&e);
                        status.add_css_class("error");
                    }
                }
            });
        });
    }
    dialog.present(Some(&app.window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_edits_keep_comments_and_unknown_keys() {
        let text = "# mine\nauto_reconnect = true\nsettle_secs = 90\n";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        doc["settle_secs"] = toml_edit::value(120i64);
        let arr: toml_edit::Array = ["ratradinternet"].into_iter().collect();
        doc["autoconnect_never"] = toml_edit::value(arr);
        let out = doc.to_string();
        assert!(out.contains("# mine"));
        assert!(out.contains("auto_reconnect = true"));
        validate_config(&out).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.settle_secs, 120);
        assert_eq!(cfg.autoconnect_never, vec!["ratradinternet".to_string()]);
    }

    #[test]
    fn a_config_pvpn_would_refuse_is_refused() {
        assert!(validate_config("settle_secs = \"soon\"\n").is_err());
        assert!(validate_config("autoconnect_networks = \"sometimes\"\n").is_err());
    }

    #[test]
    fn membership_accepts_either_form() {
        let list = vec!["ratradinternet".to_string()];
        assert!(in_list(&list, "wifi:ratradinternet"));
        assert!(!in_list(&list, "wifi:detnsw"));
        assert_eq!(bare("wifi:home"), "home");
        assert_eq!(bare("wired:eth0"), "wired:eth0");
    }
}
