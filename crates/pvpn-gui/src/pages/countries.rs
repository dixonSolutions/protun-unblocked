//! Every Proton server, by country and city — Proton's own app's browser,
//! with what this network has taught us marked on each row.

use crate::app::App;
use crate::countries::{self, Catalog, Feature};
use crate::data::{self, Kind};
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const FEATURES: [(&str, &str, Feature); 5] = [
    ("any", "All", Feature::Any),
    ("p2p", "P2P", Feature::P2p),
    ("sc", "Secure Core", Feature::SecureCore),
    ("tor", "Tor", Feature::Tor),
    ("stream", "Streaming", Feature::Streaming),
];

struct Ui {
    catalog: RefCell<Catalog>,
    /// What this network knows: server → kind.
    known: RefCell<HashMap<String, Kind>>,
    feature: adw::ToggleGroup,
    search: gtk::SearchEntry,
    sidebar: gtk::ListBox,
    codes: RefCell<Vec<String>>,
    selected: RefCell<Option<String>>,
    title: gtk::Label,
    summary: gtk::Label,
    city: gtk::DropDown,
    cities: RefCell<Vec<String>>,
    list: gtk::ListBox,
    plan: gtk::Label,
}

fn feature_of(ui: &Ui) -> Feature {
    FEATURES
        .iter()
        .find(|f| Some(f.0) == ui.feature.active_name().as_deref())
        .map(|f| f.2)
        .unwrap_or(Feature::Any)
}

pub fn build(app: &Rc<App>) -> gtk::Widget {
    let feature = adw::ToggleGroup::new();
    for (name, label, _) in FEATURES {
        feature.add(adw::Toggle::builder().name(name).label(label).build());
    }
    feature.set_active_name(Some("any"));
    let fastest_all = gtk::Button::builder().label("Fastest anywhere").tooltip_text("Proton's best-scored server your plan allows, with the filter above").build();
    fastest_all.add_css_class("suggested-action");
    let random_all = gtk::Button::builder().label("Random").tooltip_text("Any server your plan allows, with the filter above").build();
    let plan = gtk::Label::new(None);
    plan.add_css_class("dim-label");
    plan.add_css_class("caption");
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.set_margin_top(12);
    toolbar.set_margin_start(12);
    toolbar.set_margin_end(12);
    toolbar.append(&feature);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    toolbar.append(&spacer);
    toolbar.append(&plan);
    toolbar.append(&random_all);
    toolbar.append(&fastest_all);

    // Sidebar: countries.
    let search = gtk::SearchEntry::builder().placeholder_text("Country").build();
    let sidebar = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    sidebar.add_css_class("navigation-sidebar");
    let side_scroll = gtk::ScrolledWindow::builder().child(&sidebar).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    let side = gtk::Box::new(gtk::Orientation::Vertical, 6);
    side.set_width_request(260);
    side.set_margin_start(12);
    side.set_margin_bottom(12);
    side.append(&search);
    side.append(&side_scroll);

    // Main: one country's servers.
    let title = gtk::Label::builder().xalign(0.0).build();
    title.add_css_class("title-2");
    let summary = gtk::Label::builder().xalign(0.0).wrap(true).build();
    summary.add_css_class("dim-label");
    let city = gtk::DropDown::from_strings(&["All cities"]);
    city.set_valign(gtk::Align::Center);
    let fastest_here = gtk::Button::builder().label("Fastest here").valign(gtk::Align::Center).build();
    fastest_here.add_css_class("suggested-action");
    let random_here = gtk::Button::builder().label("Random here").valign(gtk::Align::Center).build();
    let head_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let head_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    head_text.set_hexpand(true);
    head_text.append(&title);
    head_text.append(&summary);
    head_row.append(&head_text);
    head_row.append(&city);
    head_row.append(&random_here);
    head_row.append(&fastest_here);
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).valign(gtk::Align::Start).build();
    list.add_css_class("boxed-list");
    let main_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    main_box.set_margin_end(12);
    main_box.set_margin_bottom(12);
    main_box.append(&head_row);
    main_box.append(&list);
    let main_scroll = gtk::ScrolledWindow::builder().child(&main_box).hexpand(true).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

    let body = gtk::Paned::builder().orientation(gtk::Orientation::Horizontal).start_child(&side).end_child(&main_scroll).position(280).shrink_start_child(false).build();
    body.set_margin_top(8);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&toolbar);
    page.append(&body);

    let ui = Rc::new(Ui {
        catalog: RefCell::new(Catalog::default()),
        known: RefCell::new(HashMap::new()),
        feature,
        search,
        sidebar,
        codes: RefCell::new(Vec::new()),
        selected: RefCell::new(None),
        title,
        summary,
        city,
        cities: RefCell::new(Vec::new()),
        list,
        plan,
    });

    {
        let (a, u) = (app.clone(), ui.clone());
        ui.feature.connect_active_name_notify(move |_| {
            fill_sidebar(&u);
            fill_country(&a, &u);
        });
    }
    {
        let u = ui.clone();
        ui.search.connect_search_changed(move |_| fill_sidebar(&u));
    }
    {
        let (a, u) = (app.clone(), ui.clone());
        ui.sidebar.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let code = u.codes.borrow().get(row.index() as usize).cloned();
            if code.is_some() && *u.selected.borrow() != code {
                *u.selected.borrow_mut() = code;
                u.city.set_selected(0);
                fill_country(&a, &u);
            }
        });
    }
    {
        let (a, u) = (app.clone(), ui.clone());
        ui.city.connect_selected_notify(move |_| fill_servers(&a, &u));
    }
    let pick = |a: &Rc<App>, u: &Rc<Ui>, here: bool, random: bool| {
        let catalog = u.catalog.borrow();
        let feature = feature_of(u);
        let country = u.selected.borrow().clone();
        let city = selected_city(u);
        let chosen = match (here, &country) {
            (true, Some(c)) => {
                let it = catalog.in_country(c, feature, city.as_deref());
                if random { catalog.random(it) } else { catalog.fastest(it) }
            }
            _ => {
                let it = catalog.servers.iter().filter(|s| feature.admits(s.features));
                if random { catalog.random(it) } else { catalog.fastest(it) }
            }
        };
        match chosen {
            Some(s) => a.hop(Some(&s.name)),
            None => a.toast("No server your plan allows matches that"),
        }
    };
    {
        let (a, u) = (app.clone(), ui.clone());
        fastest_all.connect_clicked(move |_| pick(&a, &u, false, false));
    }
    {
        let (a, u) = (app.clone(), ui.clone());
        random_all.connect_clicked(move |_| pick(&a, &u, false, true));
    }
    {
        let (a, u) = (app.clone(), ui.clone());
        fastest_here.connect_clicked(move |_| pick(&a, &u, true, false));
    }
    {
        let (a, u) = (app.clone(), ui.clone());
        random_here.connect_clicked(move |_| pick(&a, &u, true, true));
    }
    {
        let u = ui.clone();
        app.on_data(move |a| {
            *u.catalog.borrow_mut() = countries::load();
            let state = data::load_state();
            let network = a.snapshot.borrow().as_ref().map(|s| s.network.clone()).unwrap_or_default();
            let config = pvpn_core::config::Config::load(&pvpn_core::config::Config::default_path()).unwrap_or_default();
            *u.known.borrow_mut() = data::view(&state, &network, &config).servers.into_iter().map(|s| (s.name, s.kind)).collect();
            let c = u.catalog.borrow();
            u.plan.set_label(&format!(
                "{} servers · {}",
                c.servers.len(),
                match c.max_tier {
                    0 => "Free plan",
                    1 => "Basic plan",
                    _ => "Plus plan",
                }
            ));
            drop(c);
            fill_sidebar(&u);
            if u.selected.borrow().is_none() {
                // Start where you are exiting, or the first country.
                let here = a.snapshot.borrow().as_ref().and_then(|s| s.server.clone());
                let code = here
                    .and_then(|n| u.catalog.borrow().servers.iter().find(|s| s.name == n).map(|s| s.country.clone()))
                    .or_else(|| u.codes.borrow().first().cloned());
                *u.selected.borrow_mut() = code;
                select_in_sidebar(&u);
            }
            fill_country(a, &u);
        });
    }
    page.upcast()
}

fn selected_city(ui: &Ui) -> Option<String> {
    let i = ui.city.selected() as usize;
    if i == 0 {
        None
    } else {
        ui.cities.borrow().get(i - 1).cloned()
    }
}

fn fill_sidebar(ui: &Rc<Ui>) {
    while let Some(c) = ui.sidebar.first_child() {
        ui.sidebar.remove(&c);
    }
    let needle = ui.search.text().to_lowercase();
    let catalog = ui.catalog.borrow();
    let mut codes = Vec::new();
    for (code, count, usable) in catalog.countries(feature_of(ui)) {
        let name = countries::country_name(&code);
        if !needle.is_empty() && !name.to_lowercase().contains(&needle) && !code.to_lowercase().contains(&needle) {
            continue;
        }
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_top(4);
        row.set_margin_bottom(4);
        let flag = gtk::Label::new(Some(&countries::flag(&code)));
        let label = gtk::Label::builder().label(&name).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let n = gtk::Label::new(Some(&if usable == count { count.to_string() } else { format!("{usable}/{count}") }));
        n.add_css_class("dim-label");
        n.add_css_class("caption");
        n.set_tooltip_text(Some("usable on your plan / all"));
        row.append(&flag);
        row.append(&label);
        row.append(&n);
        if usable == 0 {
            row.add_css_class("dim-label");
        }
        ui.sidebar.append(&row);
        codes.push(code);
    }
    *ui.codes.borrow_mut() = codes;
    drop(catalog);
    select_in_sidebar(ui);
}

fn select_in_sidebar(ui: &Rc<Ui>) {
    let sel = ui.selected.borrow().clone();
    if let Some(idx) = sel.and_then(|c| ui.codes.borrow().iter().position(|x| *x == c)) {
        if let Some(row) = ui.sidebar.row_at_index(idx as i32) {
            ui.sidebar.select_row(Some(&row));
        }
    }
}

fn fill_country(app: &Rc<App>, ui: &Rc<Ui>) {
    let Some(code) = ui.selected.borrow().clone() else {
        ui.title.set_label("No server list yet");
        ui.summary.set_label("Proton's client has not cached a server list. Connect once, or refresh it from the Servers page.");
        return;
    };
    let catalog = ui.catalog.borrow();
    let feature = feature_of(ui);
    let cities = catalog.cities(&code, feature);
    let mut labels = vec!["All cities".to_string()];
    labels.extend(cities.iter().cloned());
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let previous = selected_city(ui);
    *ui.cities.borrow_mut() = cities.clone();
    ui.city.set_model(Some(&gtk::StringList::new(&refs)));
    ui.city.set_selected(previous.and_then(|p| cities.iter().position(|c| *c == p)).map(|i| i as u32 + 1).unwrap_or(0));
    ui.city.set_visible(cities.len() > 1);
    ui.title.set_label(&format!("{}  {}", countries::flag(&code), countries::country_name(&code)));
    drop(catalog);
    fill_servers(app, ui);
}

fn fill_servers(app: &Rc<App>, ui: &Rc<Ui>) {
    while let Some(c) = ui.list.first_child() {
        ui.list.remove(&c);
    }
    let Some(code) = ui.selected.borrow().clone() else { return };
    let catalog = ui.catalog.borrow();
    let feature = feature_of(ui);
    let city = selected_city(ui);
    let mut servers: Vec<&countries::Server> = catalog.in_country(&code, feature, city.as_deref()).collect();
    servers.sort_by(|a, b| {
        b.usable(catalog.max_tier)
            .cmp(&a.usable(catalog.max_tier))
            .then(a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
    });
    let usable = servers.iter().filter(|s| s.usable(catalog.max_tier)).count();
    ui.summary.set_label(&format!(
        "{} servers, {usable} on your plan{}. Sorted by Proton's score; connecting goes through pvpn, so what fails here is remembered.",
        servers.len(),
        city.as_deref().map(|c| format!(" in {c}")).unwrap_or_default()
    ));
    let current = app.snapshot.borrow().as_ref().filter(|s| s.tunneled).and_then(|s| s.server.clone());
    let known = ui.known.borrow();
    for s in servers.into_iter().take(300) {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&s.name)).build();
        let mut bits = vec![s.city.clone(), format!("load {}%", s.load)];
        bits.extend(s.feature_labels().into_iter().map(str::to_string));
        if !s.online {
            bits.push("maintenance".into());
        }
        row.set_subtitle(&glib::markup_escape_text(&bits.join(" · ")));
        let load = gtk::LevelBar::builder().min_value(0.0).max_value(100.0).value(s.load as f64).width_request(60).valign(gtk::Align::Center).build();
        row.add_prefix(&load);
        let status = if current.as_deref() == Some(&s.name) {
            Some(("connected", "kind-working"))
        } else {
            known.get(&s.name).and_then(|k| match k {
                Kind::Working => Some(("works here", "kind-working")),
                Kind::Blocked => Some(("blocked here", "kind-blocked")),
                Kind::Fast => Some(("fast here", "kind-fast")),
                Kind::Known => None,
            })
        };
        if let Some((text, css)) = status {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("caption");
            l.add_css_class(css);
            l.set_valign(gtk::Align::Center);
            row.add_suffix(&l);
        }
        if s.tier > catalog.max_tier {
            let lock = gtk::Label::new(Some("Plus"));
            lock.add_css_class("caption");
            lock.add_css_class("dim-label");
            lock.set_tooltip_text(Some("Not on your plan"));
            row.add_suffix(&lock);
            row.set_sensitive(false);
        } else if s.online && current.as_deref() != Some(&s.name) {
            let go = gtk::Button::builder().icon_name("network-vpn-symbolic").tooltip_text(format!("Connect to {}", s.name)).valign(gtk::Align::Center).build();
            go.add_css_class("flat");
            let a = app.clone();
            let n = s.name.clone();
            go.connect_clicked(move |_| a.hop(Some(&n)));
            row.add_suffix(&go);
        }
        ui.list.append(&row);
    }
}
