//! The front page: one orb to connect, a globe of servers, the fastest few.

use crate::app::App;
use crate::data::{self, Kind};
use crate::globe::{Globe, Marker};
use crate::status::Phase;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

pub const PROTOCOLS: [&str; 7] = [
    "protun-tls",
    "protun-tcp",
    "protun-udp",
    "protun-smart",
    "openvpn-tcp",
    "openvpn-udp",
    "wireguard",
];

pub fn build(app: &Rc<App>, bp: &adw::Breakpoint) -> gtk::Widget {
    // ---------- left: the orb ----------
    let orb_icon = gtk::Image::builder().icon_name("system-shutdown-symbolic").pixel_size(58).build();
    let orb_label = gtk::Label::new(Some("Connect"));
    orb_label.add_css_class("orb-label");
    let orb_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    orb_box.set_valign(gtk::Align::Center);
    orb_box.append(&orb_icon);
    orb_box.append(&orb_label);
    let orb = gtk::Button::builder()
        .child(&orb_box)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .tooltip_text("Connect or disconnect (Ctrl+Enter)")
        .build();
    orb.add_css_class("orb");
    orb.add_css_class("off");
    {
        let a = app.clone();
        orb.connect_clicked(move |_| a.toggle_connection());
    }

    let title = gtk::Label::new(Some("Disconnected"));
    title.add_css_class("phase-title");
    let detail = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
    detail.add_css_class("title-4");
    let sub = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
    sub.add_css_class("dim-label");
    let progress = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(48)
        .build();
    progress.add_css_class("progress-line");
    let cancel = gtk::Button::builder().label("Cancel").halign(gtk::Align::Center).visible(false).build();
    cancel.add_css_class("pill");
    cancel.add_css_class("destructive-action");
    {
        let a = app.clone();
        cancel.connect_clicked(move |_| a.runner.cancel());
    }

    // Protocol for the orb, the one choice worth having next to it.
    let mut labels = vec!["Automatic"];
    labels.extend(PROTOCOLS);
    let protocol = gtk::DropDown::from_strings(&labels);
    protocol.set_tooltip_text(Some("Protocol for Connect and Switch. Automatic is pvpn's own choice (Stealth, or whatever has worked here)."));
    {
        let current = app.settings.borrow().protocol.clone();
        let idx = PROTOCOLS.iter().position(|p| *p == current).map(|i| i + 1).unwrap_or(0);
        protocol.set_selected(idx as u32);
        let a = app.clone();
        protocol.connect_selected_notify(move |d| {
            let i = d.selected() as usize;
            a.settings.borrow_mut().protocol = if i == 0 { String::new() } else { PROTOCOLS[i - 1].to_string() };
            a.save_settings();
        });
    }

    let hop = gtk::Button::builder().label("Switch server").tooltip_text("pvpn hop (Ctrl+N)").build();
    {
        let a = app.clone();
        hop.connect_clicked(move |_| a.hop(None));
    }
    let best = gtk::Button::builder().label("Measure & connect").tooltip_text("pvpn best --connect: measure, then connect to the winner").build();
    {
        let a = app.clone();
        best.connect_clicked(move |_| {
            let s = a.settings.borrow().clone();
            let mut args = vec!["best".to_string(), "--connect".into(), "--limit".into(), s.rank_limit.to_string()];
            if !s.rank_country.is_empty() {
                args.extend(["--country".into(), s.rank_country.clone()]);
            }
            if s.rank_free_only {
                args.push("--free".into());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            a.run_net("Measuring, then connecting", &refs);
        });
    }
    let check = gtk::Button::builder().label("Check").tooltip_text("pvpn watch --check: send a packet through the tunnel and see if one comes back").build();
    {
        let a = app.clone();
        check.connect_clicked(move |_| {
            a.run_capture(&["watch", "--check"], |a, out| a.toast(&out.headline()));
        });
    }
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::Center);
    actions.append(&hop);
    actions.append(&best);
    actions.append(&check);

    let proto_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    proto_row.set_halign(gtk::Align::Center);
    let proto_label = gtk::Label::new(Some("Protocol"));
    proto_label.add_css_class("dim-label");
    proto_row.append(&proto_label);
    proto_row.append(&protocol);

    let autoconnect = gtk::Button::builder().halign(gtk::Align::Center).build();
    autoconnect.add_css_class("flat");
    autoconnect.add_css_class("caption");
    {
        let a = app.clone();
        autoconnect.connect_clicked(move |_| a.open_preferences(Some("autoconnect")));
    }

    let left = gtk::Box::new(gtk::Orientation::Vertical, 14);
    left.set_valign(gtk::Align::Center);
    left.set_margin_top(24);
    left.set_margin_bottom(24);
    left.set_margin_start(24);
    left.set_margin_end(24);
    left.set_width_request(340);
    left.append(&orb);
    left.append(&title);
    left.append(&detail);
    left.append(&sub);
    left.append(&progress);
    left.append(&cancel);
    left.append(&actions);
    left.append(&proto_row);
    left.append(&autoconnect);

    // ---------- right: globe + fastest ----------
    let globe = Globe::new();
    let globe_frame = gtk::Overlay::new();
    globe_frame.set_child(Some(&globe.area));
    globe_frame.add_css_class("card");
    globe_frame.add_css_class("globe-card");
    globe_frame.set_overflow(gtk::Overflow::Hidden);
    let globe_tools = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    globe_tools.set_halign(gtk::Align::End);
    globe_tools.set_valign(gtk::Align::Start);
    globe_tools.set_margin_top(8);
    globe_tools.set_margin_end(8);
    let home_btn = gtk::Button::builder().icon_name("find-location-symbolic").tooltip_text("Turn to where you are").build();
    home_btn.add_css_class("osd");
    home_btn.add_css_class("circular");
    let server_btn = gtk::Button::builder().icon_name("network-server-symbolic").tooltip_text("Turn to the server you are on").build();
    server_btn.add_css_class("osd");
    server_btn.add_css_class("circular");
    globe_tools.append(&home_btn);
    globe_tools.append(&server_btn);
    globe_frame.add_overlay(&globe_tools);
    let legend = gtk::Label::builder()
        .use_markup(true)
        .label("<span foreground='#2ec27e'>●</span> working   <span foreground='#3584e4'>●</span> fast   <span foreground='#9a9996'>●</span> measured   <span foreground='#e01b24'>●</span> blocked   drag to turn · scroll to zoom · click a server")
        .halign(gtk::Align::Start)
        .valign(gtk::Align::End)
        .margin_start(12)
        .margin_bottom(8)
        .wrap(true)
        .build();
    legend.add_css_class("caption");
    legend.add_css_class("dim-label");
    globe_frame.add_overlay(&legend);
    {
        let g = globe.clone();
        home_btn.connect_clicked(move |_| g.fly_home());
    }
    {
        let g = globe.clone();
        let a = app.clone();
        server_btn.connect_clicked(move |_| {
            let server = a.snapshot.borrow().as_ref().and_then(|s| s.server.clone());
            if let Some(p) = server.and_then(|n| a.places.borrow().get(&n).cloned()) {
                g.fly_to(p.lat, p.lon);
            }
        });
    }
    {
        let a = app.clone();
        let area = globe.area.clone();
        globe.connect_pick(move |name, x, y| server_popover(&a, &area, name, x, y));
    }

    let fastest_title = gtk::Label::builder().label("Fastest here").xalign(0.0).build();
    fastest_title.add_css_class("heading");
    let chips = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(8)
        .column_spacing(6)
        .row_spacing(6)
        .homogeneous(false)
        .build();

    let right = gtk::Box::new(gtk::Orientation::Vertical, 10);
    right.set_hexpand(true);
    right.set_margin_top(18);
    right.set_margin_bottom(18);
    right.set_margin_end(18);
    right.set_margin_start(6);
    right.append(&globe_frame);
    right.append(&fastest_title);
    right.append(&chips);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&left);
    body.append(&right);
    bp.add_setter(&body, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    bp.add_setter(&right, "margin-start", Some(&18.to_value()));
    bp.add_setter(&globe.area, "content-height", Some(&320.to_value()));

    // Warnings that change what the orb means.
    let cert_banner = adw::Banner::builder().revealed(false).build();
    cert_banner.set_button_label(Some("Renew"));
    {
        let a = app.clone();
        cert_banner.connect_button_clicked(move |_| a.run_net("Renewing certificate", &["cert", "--renew"]));
    }
    let leak_banner = adw::Banner::builder().revealed(false).build();
    leak_banner.set_button_label(Some("Restore"));
    {
        let a = app.clone();
        leak_banner.connect_button_clicked(move |_| a.disconnect());
    }

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&cert_banner);
    page.append(&leak_banner);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    page.append(&scroll);

    // ---------- live updates ----------
    {
        let orb = orb.clone();
        let (orb_icon, orb_label, title, detail, sub, progress, cancel, hop, best) =
            (orb_icon.clone(), orb_label.clone(), title.clone(), detail.clone(), sub.clone(), progress.clone(), cancel.clone(), hop.clone(), best.clone());
        let globe = globe.clone();
        let autoconnect = autoconnect.clone();
        let leak_banner = leak_banner.clone();
        let last_server: std::cell::RefCell<Option<String>> = Default::default();
        app.on_status(move |a| {
            let phase = a.phase.borrow().clone();
            let snap = a.snapshot.borrow().clone();
            let ours = a.runner.busy();
            for c in ["off", "busy", "on", "dead"] {
                orb.remove_css_class(c);
                title.remove_css_class(c);
            }
            let css = if ours { "busy" } else { phase.css() };
            orb.add_css_class(css);
            title.add_css_class(css);
            let (icon, word) = match (&phase, ours) {
                (_, true) => ("process-stop-symbolic", "Cancel"),
                (Phase::Connected { .. } | Phase::Broken { .. }, _) => ("system-shutdown-symbolic", "Disconnect"),
                (Phase::Connecting { .. } | Phase::Disconnecting, _) => ("content-loading-symbolic", "Working"),
                (Phase::Disconnected, _) => ("system-shutdown-symbolic", "Connect"),
            };
            orb_icon.set_icon_name(Some(icon));
            orb_label.set_label(word);
            let heading = if ours { a.runner.label() } else { phase.title().to_string() };
            title.set_label(&heading);
            globe.set_busy(ours || phase.is_busy());

            let Some(s) = snap else { return };
            let net = s.network.trim_start_matches("wifi:");
            match &phase {
                Phase::Connected { server } => {
                    let place = a.places.borrow().get(server).cloned();
                    let at = place.map(|p| format!(" — {}, {}", p.city, p.country)).unwrap_or_default();
                    detail.set_label(&format!("{server}{at}"));
                    let traffic = match a.carrying() {
                        Some(true) => " · carrying traffic",
                        Some(false) => " · NOT carrying",
                        None => "",
                    };
                    sub.set_label(&format!("{} on {net}{traffic}", s.protocol));
                }
                Phase::Broken { reason } => {
                    detail.set_label(reason);
                    sub.set_label(&format!("on {net} — switch server or disconnect"));
                }
                Phase::Connecting { automatic } => {
                    detail.set_label(if *automatic { "autoconnect is rebuilding the tunnel" } else { "" });
                    sub.set_label(&format!("on {net}"));
                }
                _ => {
                    detail.set_label("");
                    sub.set_label(&format!("on {net} · normal internet"));
                }
            }
            if let Some(stray) = &s.stray_route {
                leak_banner.set_title(&format!("{stray} still holds a default route with no tunnel behind it — some traffic is blackholed."));
                leak_banner.set_revealed(true);
            } else {
                leak_banner.set_revealed(false);
            }
            let line = a.progress.borrow().clone();
            progress.set_label(&line);
            progress.set_visible(!line.is_empty());
            cancel.set_visible(ours);
            hop.set_sensitive(!ours);
            best.set_sensitive(!ours);

            let auto_text = if s.autoconnect_off {
                "Autoconnect is off everywhere".to_string()
            } else {
                match &s.autoconnect_here {
                    Ok(_) if s.down_by_user => format!("Autoconnect on {net}: holding, you disconnected"),
                    Ok(_) => format!("Autoconnect will rebuild the tunnel on {net}"),
                    Err(why) => format!("No autoconnect here: {}", why.trim_start_matches(&format!("{}: ", s.network))),
                }
            };
            autoconnect.set_label(&auto_text);
            autoconnect.set_tooltip_text(Some("Change where the tunnel is rebuilt for you"));

            // Turn to a new server once, not on every poll.
            let server = s.server.clone().filter(|_| s.tunneled);
            if *last_server.borrow() != server {
                if let Some(p) = server.as_ref().and_then(|n| a.places.borrow().get(n).cloned()) {
                    globe.fly_to(p.lat, p.lon);
                }
                *last_server.borrow_mut() = server;
                refresh_markers(a, &globe);
            }
        });
    }
    {
        let globe = globe.clone();
        let chips = chips.clone();
        let cert_banner = cert_banner.clone();
        app.on_data(move |a| {
            refresh_markers(a, &globe);
            refresh_chips(a, &chips);
            refresh_cert(&cert_banner);
        });
    }
    // Certificate state changes on a clock, not with any file we watch.
    {
        let cert_banner = cert_banner.clone();
        refresh_cert(&cert_banner);
        glib::timeout_add_seconds_local(600, move || {
            refresh_cert(&cert_banner);
            glib::ControlFlow::Continue
        });
    }

    page.upcast()
}

fn refresh_cert(banner: &adw::Banner) {
    let now = chrono::Utc::now();
    match pvpn_core::cert::status_fast() {
        Some(c) if c.unusable(now) => {
            banner.set_title(&format!("Certificate: {} — no server can connect until it is renewed.", c.describe(now)));
            banner.set_revealed(true);
        }
        _ => banner.set_revealed(false),
    }
}

fn refresh_chips(app: &Rc<App>, chips: &gtk::FlowBox) {
    while let Some(child) = chips.first_child() {
        chips.remove(&child);
    }
    let state = data::load_state();
    let network = app.snapshot.borrow().as_ref().map(|s| s.network.clone()).unwrap_or_default();
    let config = pvpn_core::config::Config::load(&pvpn_core::config::Config::default_path()).unwrap_or_default();
    let view = data::view(&state, &network, &config);
    let names = app.fastest_names(8);
    if names.is_empty() {
        let hint = gtk::Label::new(Some("Nothing measured on this network yet — Measure & connect, or rank on the Servers page."));
        hint.add_css_class("dim-label");
        hint.set_wrap(true);
        chips.append(&hint);
        return;
    }
    for name in names {
        let server = view.servers.iter().find(|s| s.name == name);
        let kind = server.map(|s| s.kind).unwrap_or(Kind::Known);
        let text = match server.and_then(|s| s.stat.ema_ready_ms.or(s.stat.ema_latency_ms)) {
            Some(ms) if server.is_some_and(|s| s.stat.ema_ready_ms.is_some()) => format!("{name}  ·  ready {:.1}s", ms / 1000.0),
            Some(ms) => format!("{name}  ·  {ms:.0} ms"),
            None => name.clone(),
        };
        let button = gtk::Button::builder().label(&text).tooltip_text(format!("Connect to {name} ({})", kind.label())).build();
        button.add_css_class("server-chip");
        button.add_css_class(kind.label());
        let a = app.clone();
        button.connect_clicked(move |_| a.hop(Some(&name)));
        chips.append(&button);
    }
}

/// Every server this network has a record of, plus the last ranking.
pub fn refresh_markers(app: &Rc<App>, globe: &Globe) {
    let places = app.places.borrow();
    if places.is_empty() {
        return;
    }
    let state = data::load_state();
    let snap = app.snapshot.borrow().clone();
    let network = snap.as_ref().map(|s| s.network.clone()).unwrap_or_else(|| state.network().to_string());
    let current = snap.as_ref().filter(|s| s.tunneled).and_then(|s| s.server.clone());
    let config = pvpn_core::config::Config::load(&pvpn_core::config::Config::default_path()).unwrap_or_default();
    let view = data::view(&state, &network, &config);
    let settings = app.settings.borrow();
    let label_count = settings.globe_labels as usize;

    // The ranking to number: the last measure from this window if there is
    // one, else the one pvpn remembers for this network.
    let ranked: Vec<String> = match app.best.borrow().as_ref() {
        Some(b) if !b.results.is_empty() => b.results.iter().map(|r| r.name.clone()).collect(),
        _ => {
            let mut v: Vec<(usize, String)> = view.servers.iter().filter_map(|s| s.rank.map(|r| (r, s.name.clone()))).collect();
            v.sort();
            v.into_iter().map(|(_, n)| n).collect()
        }
    };

    let mut markers: Vec<Marker> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |name: &str, kind: Kind, stat: Option<&pvpn_core::state::ServerStat>| {
        if !seen.insert(name.to_string()) {
            return;
        }
        let Some(p) = places.get(name) else { return };
        let rank = ranked.iter().position(|n| n == name).map(|i| i + 1);
        let mut tip = format!("{name} — {}, {}", p.city, p.country);
        if let Some(r) = rank {
            tip.push_str(&format!("\nRanked #{r}"));
        }
        tip.push_str(&format!("\n{}", kind.label()));
        if let Some(s) = stat {
            if let Some(ms) = s.ema_latency_ms {
                tip.push_str(&format!(" · {ms:.0} ms"));
            }
            if s.connect_attempts > 0 {
                tip.push_str(&format!(" · {}/{} connects", s.connect_successes, s.connect_attempts));
            }
            if let Some(reason) = &s.blocked_reason {
                tip.push_str(&format!("\nblocked: {reason}"));
            }
        }
        if let Some(load) = p.load {
            tip.push_str(&format!(" · load {load}%"));
        }
        markers.push(Marker {
            name: name.to_string(),
            lat: p.lat,
            lon: p.lon,
            kind,
            rank,
            label: rank.is_some_and(|r| r <= label_count),
            current: current.as_deref() == Some(name),
            tooltip: tip,
        });
    };
    if let Some(c) = &current {
        let s = view.servers.iter().find(|s| &s.name == c);
        push(c, s.map(|s| s.kind).unwrap_or(Kind::Working), s.map(|s| &s.stat));
    }
    for name in &ranked {
        let s = view.servers.iter().find(|s| &s.name == name);
        push(name, s.map(|s| s.kind).unwrap_or(Kind::Known), s.map(|s| &s.stat));
    }
    for s in &view.servers {
        if s.kind == Kind::Blocked && !settings.globe_show_blocked {
            continue;
        }
        push(&s.name, s.kind, Some(&s.stat));
    }
    drop(places);
    drop(settings);
    globe.set_markers(markers);
}

fn server_popover(app: &Rc<App>, area: &gtk::DrawingArea, name: &str, x: f64, y: f64) {
    let pop = gtk::Popover::new();
    pop.set_parent(area);
    pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 6);
    bx.set_margin_top(6);
    bx.set_margin_bottom(6);
    bx.set_margin_start(6);
    bx.set_margin_end(6);
    let title = gtk::Label::new(Some(name));
    title.add_css_class("heading");
    bx.append(&title);
    if let Some(p) = app.places.borrow().get(name) {
        let l = gtk::Label::new(Some(&format!("{}, {}{}", p.city, p.country, p.load.map(|l| format!(" · load {l}%")).unwrap_or_default())));
        l.add_css_class("dim-label");
        bx.append(&l);
    }
    let connect = gtk::Button::with_label("Connect here");
    connect.add_css_class("suggested-action");
    {
        let a = app.clone();
        let n = name.to_string();
        let p = pop.clone();
        connect.connect_clicked(move |_| {
            p.popdown();
            a.hop(Some(&n));
        });
    }
    bx.append(&connect);
    let state = data::load_state();
    let network = app.snapshot.borrow().as_ref().map(|s| s.network.clone()).unwrap_or_default();
    let config = pvpn_core::config::Config::load(&pvpn_core::config::Config::default_path()).unwrap_or_default();
    let view = data::view(&state, &network, &config);
    if view.servers.iter().any(|s| s.name == name && s.kind == Kind::Blocked) {
        let forget = gtk::Button::with_label("Lift block");
        let a = app.clone();
        let n = name.to_string();
        let p = pop.clone();
        forget.connect_clicked(move |_| {
            p.popdown();
            a.run_capture(&["forget", &n], |a, out| {
                a.toast(&out.headline());
                a.reload_data();
            });
        });
        bx.append(&forget);
    }
    pop.set_child(Some(&bx));
    pop.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    pop.popup();
}
