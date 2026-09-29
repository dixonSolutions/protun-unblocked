//! Everything a network has taught us about servers, and the ranking.

use crate::app::App;
use crate::data::{self, Kind};
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    Ranked,
    Working,
    Fast,
    Blocked,
    All,
}

const FILTERS: [(&str, &str, Filter); 5] = [
    ("ranked", "Ranked", Filter::Ranked),
    ("working", "Working", Filter::Working),
    ("fast", "Fast", Filter::Fast),
    ("blocked", "Blocked", Filter::Blocked),
    ("all", "All", Filter::All),
];

struct Ui {
    network: gtk::DropDown,
    networks: RefCell<Vec<String>>,
    filter: adw::ToggleGroup,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    summary: gtk::Label,
    empty: adw::StatusPage,
    stack: gtk::Stack,
}

pub fn build(app: &Rc<App>) -> gtk::Widget {
    let network = gtk::DropDown::from_strings(&[]);
    network.set_tooltip_text(Some("Records are kept per network"));
    let filter = adw::ToggleGroup::new();
    for (name, label, _) in FILTERS {
        filter.add(adw::Toggle::builder().name(name).label(label).build());
    }
    filter.set_active_name(Some("ranked"));
    let search = gtk::SearchEntry::builder().placeholder_text("Filter: JP, SG-FREE, #13…").hexpand(true).build();

    let measure = adw::SplitButton::builder().label("Measure & rank").tooltip_text("pvpn best: probe the shortlist and rank what this account can use").build();
    measure.add_css_class("suggested-action");
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Quick rank (no measuring)"), Some("servers.quick"));
    menu.append(Some("Measure, then connect to the best"), Some("servers.connect"));
    menu.append(Some("Ranking options…"), Some("servers.options"));
    menu.append(Some("Lift every block on this network"), Some("servers.forget-all"));
    measure.set_menu_model(Some(&menu));

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.set_margin_top(12);
    toolbar.set_margin_start(12);
    toolbar.set_margin_end(12);
    toolbar.append(&network);
    toolbar.append(&filter);
    toolbar.append(&search);
    toolbar.append(&measure);

    let summary = gtk::Label::builder().xalign(0.0).margin_start(14).margin_top(6).wrap(true).build();
    summary.add_css_class("dim-label");
    summary.add_css_class("caption");

    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).valign(gtk::Align::Start).build();
    list.add_css_class("boxed-list");
    let clamp = adw::Clamp::builder().maximum_size(1100).child(&list).margin_top(8).margin_bottom(18).margin_start(12).margin_end(12).build();
    let scroll = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    let empty = adw::StatusPage::builder().icon_name("network-server-symbolic").title("Nothing here yet").build();
    let stack = gtk::Stack::new();
    stack.add_named(&scroll, Some("list"));
    stack.add_named(&empty, Some("empty"));

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&toolbar);
    page.append(&summary);
    page.append(&stack);

    let ui = Rc::new(Ui {
        network,
        networks: RefCell::new(Vec::new()),
        filter,
        search,
        list,
        summary,
        empty,
        stack,
    });

    // Actions for the split button's menu.
    let group = gtk::gio::SimpleActionGroup::new();
    let add = |name: &str, f: Box<dyn Fn()>| {
        let action = gtk::gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| f());
        group.add_action(&action);
    };
    {
        let a = app.clone();
        let m = measure.clone();
        measure.connect_clicked(move |_| rank(&a, &m, false));
    }
    {
        let a = app.clone();
        let m = measure.clone();
        add("quick", Box::new(move || rank(&a, &m, true)));
    }
    {
        let a = app.clone();
        add("connect", Box::new(move || {
            let args = rank_args(&a, false, true);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            a.run_net("Measuring, then connecting", &refs);
        }));
    }
    {
        let a = app.clone();
        add("options", Box::new(move || a.open_preferences(Some("general"))));
    }
    {
        let a = app.clone();
        let u = ui.clone();
        add("forget-all", Box::new(move || {
            let net = a.snapshot.borrow().as_ref().map(|s| s.network.clone()).unwrap_or_default();
            let _ = &u;
            let a2 = a.clone();
            a.confirm(
                "Lift every block?",
                &format!("Every server blocked on {} will be tried again by the next connect.", net.trim_start_matches("wifi:")),
                "Lift blocks",
                true,
                move || {
                    a2.run_capture(&["forget", "--all"], |a, out| {
                        a.toast(&out.headline());
                        a.reload_data();
                    })
                },
            );
        }));
    }
    page.insert_action_group("servers", Some(&group));

    {
        let a = app.clone();
        let u = ui.clone();
        ui.filter.connect_active_name_notify(move |_| fill(&a, &u));
    }
    {
        let a = app.clone();
        let u = ui.clone();
        ui.search.connect_search_changed(move |_| fill(&a, &u));
    }
    {
        let a = app.clone();
        let u = ui.clone();
        ui.network.connect_selected_notify(move |_| fill(&a, &u));
    }
    {
        let u = ui.clone();
        app.on_data(move |a| {
            refresh_networks(a, &u);
            fill(a, &u);
        });
    }
    page.upcast()
}

fn rank_args(app: &Rc<App>, quick: bool, connect: bool) -> Vec<String> {
    let s = app.settings.borrow().clone();
    let mut args = vec!["best".to_string()];
    if connect {
        args.push("--connect".into());
    } else {
        args.push("--json".into());
    }
    if quick {
        args.push("--quick".into());
    }
    args.extend(["--limit".into(), s.rank_limit.max(1).to_string()]);
    if !s.rank_country.trim().is_empty() {
        args.extend(["--country".into(), s.rank_country.trim().to_uppercase()]);
    }
    if s.rank_free_only {
        args.push("--free".into());
    }
    args
}

fn rank(app: &Rc<App>, button: &adw::SplitButton, quick: bool) {
    let args = rank_args(app, quick, false);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    button.set_sensitive(false);
    button.set_label(if quick { "Ranking…" } else { "Measuring…" });
    app.toast(if quick { "Ranking by distance and load" } else { "Measuring servers — this takes up to a minute and a half" });
    let b = button.clone();
    app.run_capture(&refs, move |a, out| {
        b.set_sensitive(true);
        b.set_label("Measure & rank");
        match data::parse_best(&out.stdout) {
            Some(best) if out.ok => {
                data::save_last_best(&out.stdout);
                let n = best.results.len();
                *a.best.borrow_mut() = Some(best);
                a.toast(&format!("Ranked {n} servers"));
                a.reload_data();
            }
            _ => a.show_text("Ranking failed", &out.combined()),
        }
    });
}

fn selected_network(ui: &Ui) -> String {
    let nets = ui.networks.borrow();
    nets.get(ui.network.selected() as usize).cloned().unwrap_or_default()
}

fn refresh_networks(app: &Rc<App>, ui: &Ui) {
    let state = data::load_state();
    let current = app.snapshot.borrow().as_ref().map(|s| s.network.clone()).unwrap_or_else(|| state.network().to_string());
    let mut nets = data::networks(&state);
    nets.retain(|n| n != &current);
    nets.insert(0, current.clone());
    if *ui.networks.borrow() == nets {
        return;
    }
    let previous = selected_network(ui);
    let labels: Vec<String> = nets
        .iter()
        .map(|n| if *n == current { format!("{} (here)", n.trim_start_matches("wifi:")) } else { n.trim_start_matches("wifi:").to_string() })
        .collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let model = gtk::StringList::new(&refs);
    *ui.networks.borrow_mut() = nets.clone();
    ui.network.set_model(Some(&model));
    let idx = nets.iter().position(|n| *n == previous).unwrap_or(0);
    ui.network.set_selected(idx as u32);
}

fn fill(app: &Rc<App>, ui: &Rc<Ui>) {
    while let Some(row) = ui.list.first_child() {
        ui.list.remove(&row);
    }
    let network = selected_network(ui);
    let here = app.snapshot.borrow().as_ref().map(|s| s.network.clone()).unwrap_or_default();
    let is_here = network == here;
    let state = data::load_state();
    let config = pvpn_core::config::Config::load(&pvpn_core::config::Config::default_path()).unwrap_or_default();
    let view = data::view(&state, &network, &config);
    let filter = FILTERS
        .iter()
        .find(|(n, _, _)| Some(*n) == ui.filter.active_name().as_deref())
        .map(|f| f.2)
        .unwrap_or(Filter::Ranked);
    let needle = ui.search.text().to_uppercase();
    let places = app.places.borrow();
    let current = app.snapshot.borrow().as_ref().filter(|s| s.tunneled).and_then(|s| s.server.clone());

    let mut shown = 0;
    if filter == Filter::Ranked {
        let best = app.best.borrow();
        let from_best = best.as_ref().filter(|b| !b.results.is_empty() && is_here);
        let ranked: Vec<(usize, String, Option<data::Ranked>)> = match from_best {
            Some(b) => b.results.iter().enumerate().map(|(i, r)| (i + 1, r.name.clone(), Some(r.clone()))).collect(),
            None => {
                let mut v: Vec<(usize, String, Option<data::Ranked>)> =
                    view.servers.iter().filter_map(|s| s.rank.map(|r| (r, s.name.clone(), None))).collect();
                v.sort_by_key(|x| x.0);
                v
            }
        };
        ui.summary.set_label(&match (from_best, view.ranked_at) {
            (Some(_), _) => format!("Your last Measure & rank from this window — {} servers. Numbers match the globe.", ranked.len()),
            (None, Some(at)) => format!("The ranking pvpn last connected down on {network}, {}.", data::ago(at)),
            (None, None) => "No ranking yet on this network. Measure & rank to make one.".into(),
        });
        for (rank, name, r) in ranked {
            if !needle.is_empty() && !name.to_uppercase().contains(&needle) {
                continue;
            }
            let rec = view.servers.iter().find(|s| s.name == name);
            ui.list.append(&row(app, &name, Some(rank), rec, r.as_ref(), places.get(&name), current.as_deref() == Some(&name), is_here));
            shown += 1;
        }
    } else {
        let mut n_working = 0;
        let mut n_blocked = 0;
        for s in &view.servers {
            match s.kind {
                Kind::Working => n_working += 1,
                Kind::Blocked => n_blocked += 1,
                _ => {}
            }
        }
        ui.summary.set_label(&format!(
            "{} servers observed on {network}: {n_working} have carried traffic here, {n_blocked} are blocked. `fast` is a handshake time, never proof a server works.",
            view.servers.len()
        ));
        for s in &view.servers {
            let keep = match filter {
                Filter::Working => s.kind == Kind::Working,
                Filter::Fast => s.kind == Kind::Fast || (s.kind == Kind::Working && s.stat.ema_latency_ms.is_some()),
                Filter::Blocked => s.kind == Kind::Blocked,
                _ => true,
            };
            if !keep || (!needle.is_empty() && !s.name.to_uppercase().contains(&needle)) {
                continue;
            }
            ui.list.append(&row(app, &s.name, s.rank, Some(s), None, places.get(&s.name), current.as_deref() == Some(&s.name), is_here));
            shown += 1;
        }
    }
    if shown == 0 {
        ui.empty.set_description(Some(match filter {
            Filter::Ranked => "Measure &amp; rank probes a shortlist and ranks what your account can use.",
            Filter::Blocked => "Nothing is blocked on this network.",
            Filter::Working => "No server has carried traffic here yet.",
            _ => "No records match.",
        }));
        ui.stack.set_visible_child_name("empty");
    } else {
        ui.stack.set_visible_child_name("list");
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    app: &Rc<App>,
    name: &str,
    rank: Option<usize>,
    rec: Option<&data::Server>,
    ranked: Option<&data::Ranked>,
    place: Option<&data::Place>,
    current: bool,
    is_here: bool,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(glib::markup_escape_text(name)).build();
    let badge = gtk::Label::new(Some(&rank.map(|r| format!("#{r}")).unwrap_or_else(|| "–".into())));
    badge.add_css_class("rank-badge");
    badge.set_valign(gtk::Align::Center);
    row.add_prefix(&badge);

    let mut bits: Vec<String> = Vec::new();
    if let Some(p) = place {
        bits.push(format!("{}, {}", p.city, p.country));
    } else if let Some(r) = ranked {
        bits.push(format!("{}, {}", r.city.clone().unwrap_or_default(), r.country.clone().unwrap_or_default()));
    }
    let latency = ranked.and_then(|r| r.latency_ms).or(rec.and_then(|s| s.stat.ema_latency_ms));
    if let Some(ms) = latency {
        bits.push(format!("{ms:.0} ms"));
    }
    if let Some(s) = rec {
        if let Some(ready) = s.stat.ema_ready_ms {
            bits.push(format!("ready {:.1}s", ready / 1000.0));
        }
        if s.stat.connect_attempts > 0 {
            bits.push(format!("{}/{} connects", s.stat.connect_successes, s.stat.connect_attempts));
        }
        if let Some(ok) = s.stat.last_connect_ok {
            bits.push(format!("last carried {}", data::ago(ok)));
        }
    }
    let load = ranked.and_then(|r| r.load).or(place.and_then(|p| p.load));
    if let Some(l) = load {
        bits.push(format!("load {l}%"));
    }
    if let Some(d) = ranked.and_then(|r| r.distance_km) {
        bits.push(format!("{d:.0} km"));
    }
    let mut subtitle = bits.join(" · ");
    if let Some(s) = rec.filter(|s| s.kind == Kind::Blocked) {
        let reason = s.stat.blocked_reason.clone().unwrap_or_else(|| "failed".into());
        let lifts = s.block_lifts.map(|t| format!(", tried again {}", data::ago(t))).unwrap_or_default();
        subtitle = format!("blocked: {reason}{lifts}\n{subtitle}");
    }
    row.set_subtitle(&glib::markup_escape_text(&subtitle));
    row.set_subtitle_lines(3);

    let kind = rec.map(|s| s.kind).unwrap_or(Kind::Known);
    let pill = gtk::Label::new(Some(if current { "connected" } else { kind.label() }));
    pill.add_css_class("caption");
    pill.add_css_class(&format!("kind-{}", if current { "working" } else { kind.label() }));
    pill.set_valign(gtk::Align::Center);
    row.add_suffix(&pill);

    if kind == Kind::Blocked && is_here {
        let lift = gtk::Button::builder().icon_name("edit-undo-symbolic").tooltip_text("Lift the block (pvpn forget)").valign(gtk::Align::Center).build();
        lift.add_css_class("flat");
        let a = app.clone();
        let n = name.to_string();
        lift.connect_clicked(move |_| {
            a.run_capture(&["forget", &n], |a, out| {
                a.toast(&out.headline());
                a.reload_data();
            })
        });
        row.add_suffix(&lift);
    }
    if !current {
        let go = gtk::Button::builder().icon_name("network-vpn-symbolic").tooltip_text(format!("Connect to {name}")).valign(gtk::Align::Center).build();
        go.add_css_class("flat");
        let a = app.clone();
        let n = name.to_string();
        go.connect_clicked(move |_| a.hop(Some(&n)));
        row.add_suffix(&go);
    }
    row
}
