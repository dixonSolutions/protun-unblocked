//! Every log that says something about the tunnel, in one place.

use crate::app::App;
use crate::runner;
use crate::services::journal_command;
use adw::prelude::*;
use gtk::glib;
use pvpn_core::config::Config;
use std::cell::Cell;
use std::rc::Rc;

enum Source {
    Activity,
    File(fn() -> std::path::PathBuf),
    Journal(&'static str, bool),
}

const SOURCES: [(&str, Source); 10] = [
    ("This window's activity", Source::Activity),
    ("Proton client log", Source::File(pvpn_core::paths::proton_log_path)),
    ("Autoconnect (journal)", Source::Journal("pvpn-autoconnect.service", true)),
    ("Health checks (journal)", Source::Journal("pvpn-watch.service", true)),
    ("Resume recovery (journal)", Source::Journal("pvpn-recover.service", false)),
    ("Certificate renewals", Source::File(cert_log)),
    ("After-connect chores", Source::File(after_log)),
    ("Tor (journal)", Source::Journal("tor@default.service", false)),
    ("NetworkManager (journal)", Source::Journal("NetworkManager.service", false)),
    ("DNS resolver (journal)", Source::Journal("systemd-resolved.service", false)),
];

fn cert_log() -> std::path::PathBuf {
    Config::data_dir().join("cert-renew.log")
}

fn after_log() -> std::path::PathBuf {
    Config::data_dir().join("after-connect.log")
}

/// The last `n` lines of a file.
pub fn tail(path: &std::path::Path, n: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(n)..].join("\n")
        }
        Err(err) => format!("{}: {err}", path.display()),
    }
}

pub fn build(app: &Rc<App>) -> gtk::Widget {
    let names: Vec<&str> = SOURCES.iter().map(|s| s.0).collect();
    let source = gtk::DropDown::from_strings(&names);
    let lines = gtk::SpinButton::with_range(50.0, 10_000.0, 50.0);
    lines.set_value(400.0);
    lines.set_tooltip_text(Some("Lines to show"));
    let search = gtk::SearchEntry::builder().placeholder_text("Only lines containing…").hexpand(true).build();
    let follow = gtk::ToggleButton::builder().icon_name("media-playback-start-symbolic").tooltip_text("Follow: refresh every two seconds").active(true).build();
    let refresh = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh").build();
    let copy = gtk::Button::builder().icon_name("edit-copy-symbolic").tooltip_text("Copy").build();
    let clear = gtk::Button::builder().icon_name("edit-clear-all-symbolic").tooltip_text("Clear this window's activity").build();
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.set_margin_top(12);
    toolbar.set_margin_start(12);
    toolbar.set_margin_end(12);
    toolbar.append(&source);
    toolbar.append(&search);
    toolbar.append(&lines);
    toolbar.append(&follow);
    toolbar.append(&refresh);
    toolbar.append(&copy);
    toolbar.append(&clear);

    let view = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(12)
        .right_margin(12)
        .build();
    view.add_css_class("log-view");
    let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
    let frame = gtk::Frame::builder().child(&scroll).margin_top(8).margin_bottom(12).margin_start(12).margin_end(12).build();

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&toolbar);
    page.append(&frame);

    let own = gtk::TextBuffer::new(None);
    let loading = Rc::new(Cell::new(false));

    let load: Rc<dyn Fn()> = {
        let a = app.clone();
        let (source, lines, search, view, scroll, own, loading) =
            (source.clone(), lines.clone(), search.clone(), view.clone(), scroll.clone(), own.clone(), loading.clone());
        Rc::new(move || {
            let n = lines.value() as usize;
            let needle = search.text().to_lowercase();
            let idx = source.selected() as usize;
            let show = {
                let view = view.clone();
                let scroll = scroll.clone();
                let own = own.clone();
                move |text: String| {
                    let filtered: String = if needle.is_empty() {
                        text
                    } else {
                        text.lines().filter(|l| l.to_lowercase().contains(&needle)).collect::<Vec<_>>().join("\n")
                    };
                    let at_bottom = {
                        let adj = scroll.vadjustment();
                        adj.value() + adj.page_size() >= adj.upper() - 40.0
                    };
                    if own.text(&own.start_iter(), &own.end_iter(), false) != filtered {
                        own.set_text(&filtered);
                        if view.buffer() != own {
                            view.set_buffer(Some(&own));
                        }
                        if at_bottom {
                            let adj = scroll.vadjustment();
                            glib::idle_add_local_once(move || adj.set_value(adj.upper()));
                        }
                    }
                }
            };
            match &SOURCES[idx].1 {
                Source::Activity => {
                    let buf = &a.activity;
                    show(buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string());
                }
                Source::File(path) => show(tail(&path(), n)),
                Source::Journal(unit, user) => {
                    if loading.replace(true) {
                        return;
                    }
                    let argv = journal_command(unit, *user, n as u32);
                    let loading = loading.clone();
                    glib::spawn_future_local(async move {
                        let out = runner::capture(argv).await;
                        loading.set(false);
                        let text = if out.stdout.trim().is_empty() { out.combined() } else { out.stdout };
                        show(text);
                    });
                }
            }
        })
    };

    for w in [&source] {
        let l = load.clone();
        w.connect_selected_notify(move |_| l());
    }
    {
        let l = load.clone();
        search.connect_search_changed(move |_| l());
    }
    {
        let l = load.clone();
        lines.connect_value_changed(move |_| l());
    }
    {
        let l = load.clone();
        refresh.connect_clicked(move |_| l());
    }
    {
        let own = own.clone();
        copy.connect_clicked(move |b| {
            b.clipboard().set_text(&own.text(&own.start_iter(), &own.end_iter(), false));
        });
    }
    {
        let a = app.clone();
        let l = load.clone();
        clear.connect_clicked(move |_| {
            a.activity.set_text("");
            l();
        });
    }
    {
        let l = load.clone();
        let (follow, page) = (follow.clone(), page.clone());
        glib::timeout_add_seconds_local(2, move || {
            if follow.is_active() && page.is_mapped() {
                l();
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let l = load.clone();
        page.connect_map(move |_| l());
    }
    page.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines() {
        let p = std::env::temp_dir().join(format!("pvpn-gui-tail-{}", std::process::id()));
        std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail(&p, 2), "c\nd");
        assert_eq!(tail(&p, 10), "a\nb\nc\nd");
        std::fs::remove_file(p).ok();
    }
}
