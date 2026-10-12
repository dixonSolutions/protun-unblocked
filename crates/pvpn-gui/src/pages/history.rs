//! Connect attempts, newest first — `pvpn history` with filters.

use crate::app::App;
use crate::data;
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

struct Ui {
    network: gtk::DropDown,
    networks: RefCell<Vec<Option<String>>>,
    outcome: adw::ToggleGroup,
    list: gtk::ListBox,
    summary: gtk::Label,
    stack: gtk::Stack,
}

pub fn build(app: &Rc<App>) -> gtk::Widget {
    let network = gtk::DropDown::from_strings(&[]);
    let outcome = adw::ToggleGroup::new();
    for (name, label) in [("all", "All"), ("ok", "Carried"), ("failed", "Failed")] {
        outcome.add(adw::Toggle::builder().name(name).label(label).build());
    }
    outcome.set_active_name(Some("all"));
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.set_margin_top(12);
    toolbar.set_margin_start(12);
    toolbar.set_margin_end(12);
    toolbar.append(&network);
    toolbar.append(&outcome);

    let summary = gtk::Label::builder().xalign(0.0).margin_start(14).margin_top(6).wrap(true).build();
    summary.add_css_class("dim-label");
    summary.add_css_class("caption");
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).valign(gtk::Align::Start).build();
    list.add_css_class("boxed-list");
    let clamp = adw::Clamp::builder().maximum_size(1100).child(&list).margin_top(8).margin_bottom(18).margin_start(12).margin_end(12).build();
    let scroll = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    let empty = adw::StatusPage::builder()
        .icon_name("document-open-recent-symbolic")
        .title("No attempts yet")
        .description("Every connect is recorded here with how long it took and what happened.")
        .build();
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
        outcome,
        list,
        summary,
        stack,
    });
    {
        let a = app.clone();
        let u = ui.clone();
        ui.network.connect_selected_notify(move |_| fill(&a, &u));
    }
    {
        let a = app.clone();
        let u = ui.clone();
        ui.outcome.connect_active_name_notify(move |_| fill(&a, &u));
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

fn refresh_networks(app: &Rc<App>, ui: &Ui) {
    let state = data::load_state();
    let current = app.snapshot.borrow().as_ref().map(|s| s.network.clone()).unwrap_or_else(|| state.network().to_string());
    let mut nets: Vec<Option<String>> = vec![Some(current.clone()), None];
    for n in data::networks(&state) {
        if n != current {
            nets.push(Some(n));
        }
    }
    if *ui.networks.borrow() == nets {
        return;
    }
    let labels: Vec<String> = nets
        .iter()
        .map(|n| match n {
            Some(n) if *n == current => format!("{} (here)", n.trim_start_matches("wifi:")),
            Some(n) => n.trim_start_matches("wifi:").to_string(),
            None => "All networks".into(),
        })
        .collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    *ui.networks.borrow_mut() = nets;
    ui.network.set_model(Some(&gtk::StringList::new(&refs)));
    ui.network.set_selected(0);
}

fn fill(_app: &Rc<App>, ui: &Ui) {
    while let Some(row) = ui.list.first_child() {
        ui.list.remove(&row);
    }
    let network = ui.networks.borrow().get(ui.network.selected() as usize).cloned().flatten();
    let state = data::load_state();
    let events = data::history(&state, network.as_deref());
    let want = ui.outcome.active_name().map(|s| s.to_string()).unwrap_or_else(|| "all".into());
    let total = events.len();
    let ok = events.iter().filter(|(_, e)| e.outcome == "ok").count();
    ui.summary.set_label(&format!(
        "{total} attempts, {ok} carried traffic. `link-down` and `cert-expired` blame no server."
    ));
    let mut shown = 0;
    for (net, e) in events.into_iter().take(400) {
        let good = e.outcome == "ok";
        if (want == "ok" && !good) || (want == "failed" && good) {
            continue;
        }
        let local = e.at.with_timezone(&chrono::Local);
        let mut bits = vec![local.format("%a %d %b %H:%M").to_string()];
        if let Some(s) = e.seconds {
            bits.push(format!("{s}s"));
        }
        if let Some(r) = e.ready_ms {
            bits.push(format!("ready {:.1}s", r as f64 / 1000.0));
        }
        bits.push(e.outcome.clone());
        if network.is_none() {
            bits.push(net.trim_start_matches("wifi:").to_string());
        }
        let mut subtitle = bits.join(" · ");
        if let Some(d) = &e.detail {
            subtitle.push('\n');
            subtitle.push_str(d);
        }
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&format!("{} · {}", e.server, e.protocol)))
            .subtitle(glib::markup_escape_text(&subtitle))
            .subtitle_lines(4)
            .build();
        let icon = gtk::Image::from_icon_name(if good { "object-select-symbolic" } else { "dialog-warning-symbolic" });
        icon.add_css_class(if good { "success" } else { "warning" });
        row.add_prefix(&icon);
        ui.list.append(&row);
        shown += 1;
    }
    ui.stack.set_visible_child_name(if shown == 0 { "empty" } else { "list" });
}
