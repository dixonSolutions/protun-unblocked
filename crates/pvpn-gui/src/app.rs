//! The window, the status clock, and everything the pages share.

use crate::data;
use crate::notify;
use crate::runner::{self, Output, RunEvent, Runner};
use crate::settings::{self, GuiSettings};
use crate::status::{self, Note, Phase, Snapshot, Tracker};
use crate::tray::{self, PvpnTray, TrayMsg};
use adw::prelude::*;
use gtk::{gio, glib};
use ksni::blocking::Handle;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

type Listener = Rc<dyn Fn(&Rc<App>)>;

pub struct App {
    pub gapp: adw::Application,
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    pub stack: adw::ViewStack,
    pub settings: RefCell<GuiSettings>,
    pub runner: Rc<Runner>,
    /// Everything this window ran, and what it said.
    pub activity: gtk::TextBuffer,
    pub snapshot: RefCell<Option<Snapshot>>,
    pub phase: RefCell<Phase>,
    pub places: RefCell<HashMap<String, data::Place>>,
    pub best: RefCell<Option<data::BestResult>>,
    /// Latest line of the running operation, for the orb page.
    pub progress: RefCell<String>,
    last_carrying: Cell<Option<bool>>,
    last_probe: Cell<Option<Instant>>,
    tracker: RefCell<Tracker>,
    tray: RefCell<Option<Handle<PvpnTray>>>,
    tray_shown: RefCell<(String, String, String, bool, bool, Vec<String>, bool)>,
    status_listeners: RefCell<Vec<Listener>>,
    data_listeners: RefCell<Vec<Listener>>,
    polling: Cell<bool>,
    /// Bumped when a connect finishes, so readings begun before it are
    /// dropped instead of reported.
    generation: Cell<u64>,
    /// A failure this window already announced, so the poll that sees the
    /// same failure a moment later does not announce it twice.
    announced_failure_at: Cell<Option<Instant>>,
    monitors: RefCell<Vec<gio::FileMonitor>>,
    poll_source: RefCell<Option<glib::SourceId>>,
    busy_source: RefCell<Option<glib::SourceId>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    /// Filled once the pages exist.
    prefs_opener: RefCell<Option<Rc<dyn Fn(Option<&str>)>>>,
}

impl App {
    pub fn new(gapp: &adw::Application) -> Rc<Self> {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("../data/style.css"));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().expect("a display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        add_icon_search_path();

        let window = adw::ApplicationWindow::builder()
            .application(gapp)
            .title("Protun Unblocked")
            .icon_name(settings::APP_ID)
            .default_width(1180)
            .default_height(760)
            .width_request(360)
            .height_request(480)
            .build();
        let stack = adw::ViewStack::new();
        let toasts = adw::ToastOverlay::new();

        let app = Rc::new(Self {
            gapp: gapp.clone(),
            window,
            toasts,
            stack,
            settings: RefCell::new(GuiSettings::load()),
            runner: Runner::new(),
            activity: gtk::TextBuffer::new(None),
            snapshot: RefCell::new(None),
            phase: RefCell::new(Phase::Disconnected),
            places: RefCell::new(HashMap::new()),
            best: RefCell::new(data::load_last_best()),
            progress: RefCell::new(String::new()),
            last_carrying: Cell::new(None),
            last_probe: Cell::new(None),
            tracker: RefCell::new(Tracker::default()),
            tray: RefCell::new(None),
            tray_shown: RefCell::new(Default::default()),
            status_listeners: RefCell::new(Vec::new()),
            data_listeners: RefCell::new(Vec::new()),
            polling: Cell::new(false),
            generation: Cell::new(0),
            announced_failure_at: Cell::new(None),
            monitors: RefCell::new(Vec::new()),
            poll_source: RefCell::new(None),
            busy_source: RefCell::new(None),
            hold: RefCell::new(None),
            prefs_opener: RefCell::new(None),
        });
        app.build_ui();
        app.wire_runner();
        app.wire_actions();
        app.watch_files();
        app.load_places();
        app.apply_tray();
        app.restart_polling();
        app.maybe_snapshot();
        {
            let a = Rc::downgrade(&app);
            notify::on_activated(move || {
                if let Some(a) = a.upgrade() {
                    a.present();
                }
            });
        }
        app
    }

    fn build_ui(self: &Rc<Self>) {
        let header = adw::HeaderBar::new();
        let switcher = adw::ViewSwitcher::builder()
            .stack(&self.stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        header.set_title_widget(Some(&switcher));

        let menu = gio::Menu::new();
        menu.append(Some("_Preferences"), Some("app.preferences"));
        menu.append(Some("_Refresh"), Some("app.refresh"));
        menu.append(Some("_Keyboard Shortcuts"), Some("app.shortcuts"));
        menu.append(Some("_About Protun Unblocked"), Some("app.about"));
        menu.append(Some("_Quit"), Some("app.quit"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .primary(true)
            .tooltip_text("Main menu")
            .build();
        header.pack_end(&menu_button);
        let prefs_button = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Preferences")
            .action_name("app.preferences")
            .build();
        header.pack_end(&prefs_button);

        let bottom = adw::ViewSwitcherBar::builder().stack(&self.stack).build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.add_bottom_bar(&bottom);
        view.set_content(Some(&self.stack));
        self.toasts.set_child(Some(&view));
        self.window.set_content(Some(&self.toasts));

        // Narrow windows swap the header switcher for the bottom bar.
        let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Sp,
        ));
        bp.add_setter(&switcher, "visible", Some(&false.to_value()));
        bp.add_setter(&bottom, "reveal", Some(&true.to_value()));
        self.window.add_breakpoint(bp.clone());

        let pages: [(&str, &str, &str, gtk::Widget); 5] = [
            ("connect", "Connect", "network-vpn-symbolic", crate::pages::connect::build(self, &bp)),
            ("servers", "Servers", "view-list-bullet-symbolic", crate::pages::servers::build(self)),
            ("history", "History", "document-open-recent-symbolic", crate::pages::history::build(self)),
            ("logs", "Logs", "utilities-terminal-symbolic", crate::pages::logs::build(self)),
            ("system", "System", "preferences-system-symbolic", crate::pages::system::build(self)),
        ];
        for (name, title, icon, widget) in pages {
            let page = self.stack.add_titled(&widget, Some(name), title);
            page.set_icon_name(Some(icon));
        }

        let a = Rc::downgrade(self);
        self.window.connect_close_request(move |w| {
            let Some(a) = a.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if a.settings.borrow().close_to_tray && a.tray.borrow().is_some() {
                w.set_visible(false);
                return glib::Propagation::Stop;
            }
            a.quit();
            glib::Propagation::Proceed
        });
    }

    /// `PVPN_GUI_SNAPSHOT_DIR=dir pvpn-gui`: render every page to
    /// `dir/<page>.png` once the first readings are in, then quit. For
    /// looking at the window without a screen to look at it on.
    fn maybe_snapshot(self: &Rc<Self>) {
        let Some(dir) = std::env::var_os("PVPN_GUI_SNAPSHOT_DIR").map(std::path::PathBuf::from) else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let pages = ["connect", "servers", "history", "logs", "system"];
        let a = self.clone();
        glib::spawn_future_local(async move {
            a.present();
            glib::timeout_future(Duration::from_secs(4)).await;
            for page in pages {
                a.stack.set_visible_child_name(page);
                glib::timeout_future(Duration::from_millis(1500)).await;
                a.save_png(&dir.join(format!("{page}.png")));
                eprintln!("snapshot: {page}");
            }
            a.stack.set_visible_child_name("connect");
            a.open_preferences(std::env::var("PVPN_GUI_SNAPSHOT_PREFS").ok().as_deref().or(Some("general")));
            glib::timeout_future(Duration::from_millis(1500)).await;
            a.save_png(&dir.join("preferences.png"));
            a.quit();
        });
    }

    fn save_png(&self, path: &std::path::Path) {
        let (w, h) = (self.window.width() as f64, self.window.height() as f64);
        let paintable = gtk::WidgetPaintable::new(Some(&self.window)).current_image();
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, w, h);
        let Some(node) = snapshot.to_node() else {
            eprintln!("snapshot: nothing drawn for {}", path.display());
            return;
        };
        let Some(renderer) = self.window.renderer() else { return };
        let texture = renderer.render_texture(&node, None);
        if let Err(err) = texture.save_to_png(path) {
            eprintln!("snapshot: {}: {err}", path.display());
        }
    }

    /// The last traffic answer for the tunnel that is up now; kept across
    /// the polls that do not ask.
    pub fn carrying(&self) -> Option<bool> {
        self.last_carrying.get()
    }

    pub fn set_prefs_opener(&self, f: impl Fn(Option<&str>) + 'static) {
        *self.prefs_opener.borrow_mut() = Some(Rc::new(f));
    }

    pub fn open_preferences(&self, page: Option<&str>) {
        let opener = self.prefs_opener.borrow().clone();
        if let Some(f) = opener {
            f(page);
        }
    }

    fn wire_actions(self: &Rc<Self>) {
        let add = |name: &str, f: Box<dyn Fn(&Rc<App>)>| {
            let action = gio::SimpleAction::new(name, None);
            let a = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(a) = a.upgrade() {
                    f(&a);
                }
            });
            self.gapp.add_action(&action);
        };
        add("preferences", Box::new(|a| {
            a.present();
            a.open_preferences(None);
        }));
        add("refresh", Box::new(|a| {
            a.poll_now(true);
            a.reload_data();
        }));
        add("about", Box::new(|a| a.show_about()));
        add("quit", Box::new(|a| a.quit()));
        add("shortcuts", Box::new(|a| a.show_shortcuts()));
        add("connect", Box::new(|a| a.toggle_connection()));
        add("hop", Box::new(|a| a.hop(None)));
        add("cancel", Box::new(|a| a.runner.cancel()));
        self.gapp.set_accels_for_action("app.quit", &["<Control>q"]);
        self.gapp.set_accels_for_action("app.preferences", &["<Control>comma"]);
        self.gapp.set_accels_for_action("app.refresh", &["<Control>r", "F5"]);
        self.gapp.set_accels_for_action("app.connect", &["<Control>Return"]);
        self.gapp.set_accels_for_action("app.hop", &["<Control>n"]);
        self.gapp.set_accels_for_action("app.cancel", &["Escape"]);
        self.gapp.set_accels_for_action("app.shortcuts", &["<Control>question"]);
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Protun Unblocked")
            .application_icon(settings::APP_ID)
            .developer_name("dixonSolutions")
            .version(env!("CARGO_PKG_VERSION"))
            .website("https://github.com/dixonSolutions/protun-unblocked")
            .issue_url("https://github.com/dixonSolutions/protun-unblocked/issues")
            .license_type(gtk::License::MitX11)
            .comments("Proton VPN for networks that filter it. A window over pvpn: it watches and asks, and never reconnects on its own.")
            .build();
        about.add_credit_section(Some("Map data"), &["Natural Earth (public domain)"]);
        about.present(Some(&self.window));
    }

    fn show_shortcuts(&self) {
        let text = "Ctrl+Enter\tConnect / disconnect\nCtrl+N\tSwitch server\nEscape\tCancel a running connect\n\
                    Ctrl+R, F5\tRefresh\nCtrl+,\tPreferences\nCtrl+Q\tQuit";
        let dialog = adw::AlertDialog::new(Some("Keyboard shortcuts"), None);
        let label = gtk::Label::builder().label(text).xalign(0.0).selectable(true).build();
        label.add_css_class("mono");
        dialog.set_extra_child(Some(&label));
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.window));
    }

    pub fn present(&self) {
        self.window.set_visible(true);
        self.window.present();
    }

    pub fn quit(&self) {
        if let Some(tray) = self.tray.borrow_mut().take() {
            let _ = tray.shutdown();
        }
        self.hold.borrow_mut().take();
        self.gapp.quit();
    }

    pub fn toast(&self, text: &str) {
        let toast = adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(4).build();
        self.toasts.add_toast(toast);
    }

    // ---------- running pvpn ----------

    pub fn pvpn(&self, args: &[&str]) -> Vec<String> {
        let mut argv = vec![self.settings.borrow().pvpn_binary()];
        argv.extend(args.iter().map(|s| s.to_string()));
        argv
    }

    /// Start a network-moving `pvpn` command, unless one is already running.
    pub fn run_net(self: &Rc<Self>, label: &str, args: &[&str]) {
        if self.runner.busy() {
            self.toast(&format!("Busy: {}. Cancel it first.", self.runner.label()));
            return;
        }
        let argv = self.pvpn(args);
        self.runner.start(label, argv);
        self.poll_soon();
    }

    pub fn connect(self: &Rc<Self>) {
        let protocol = self.settings.borrow().protocol.clone();
        if protocol.is_empty() {
            self.run_net("Connecting", &["up"]);
        } else {
            // `up` takes no protocol; `hop` does, with a pattern first. An
            // empty pattern matches every server, so this is "the best
            // server, over this protocol" — or the best in the ranking
            // country when one is set.
            let country = self.settings.borrow().rank_country.clone();
            self.run_net("Connecting", &["hop", &country, &protocol]);
        }
    }

    pub fn disconnect(self: &Rc<Self>) {
        self.run_net("Disconnecting", &["down"]);
    }

    pub fn hop(self: &Rc<Self>, to: Option<&str>) {
        let protocol = self.settings.borrow().protocol.clone();
        match (to, protocol.is_empty()) {
            (Some(name), true) => self.run_net(&format!("Switching to {name}"), &["hop", name]),
            (Some(name), false) => self.run_net(&format!("Switching to {name}"), &["hop", name, &protocol]),
            (None, _) => self.run_net("Switching server", &["hop"]),
        }
    }

    pub fn toggle_connection(self: &Rc<Self>) {
        if self.runner.busy() {
            self.runner.cancel();
            return;
        }
        let phase = self.phase.borrow().clone();
        match phase {
            Phase::Connected { .. } | Phase::Broken { .. } => self.disconnect(),
            Phase::Connecting { .. } | Phase::Disconnecting => {
                self.toast("Another pvpn is working — it will finish on its own")
            }
            Phase::Disconnected => self.connect(),
        }
    }

    /// Run a read-only command and show what it said.
    pub fn run_to_dialog(self: &Rc<Self>, title: &str, args: &[&str]) {
        let argv = self.pvpn(args);
        let a = self.clone();
        let title = title.to_string();
        self.toast(&format!("{title}…"));
        glib::spawn_future_local(async move {
            let out = runner::capture(argv.clone()).await;
            a.log_output(&argv, &out);
            a.show_text(&title, &out.combined());
        });
    }

    /// Run a read-only command; `then` gets the result.
    pub fn run_capture(self: &Rc<Self>, args: &[&str], then: impl FnOnce(&Rc<App>, Output) + 'static) {
        let argv = self.pvpn(args);
        let a = self.clone();
        glib::spawn_future_local(async move {
            let out = runner::capture(argv.clone()).await;
            a.log_output(&argv, &out);
            then(&a, out);
        });
    }

    /// Run anything in a terminal (sign-in prompts, sudo).
    pub fn run_in_terminal(&self, args: &[&str]) {
        let mut parts = vec![runner::sh_quote(&self.settings.borrow().pvpn_binary())];
        parts.extend(args.iter().map(|a| runner::sh_quote(a)));
        let argv = runner::in_terminal(&parts.join(" "));
        self.append_activity(&format!("$ {} (in a terminal)", parts.join(" ")));
        if let Err(err) = runner::detach(&argv) {
            self.toast(&format!("Could not open a terminal: {err}"));
        }
    }

    pub fn log_output(&self, argv: &[String], out: &Output) {
        self.append_activity(&format!("$ {}", argv.join(" ")));
        for line in out.combined().lines() {
            self.append_activity(&format!("  {line}"));
        }
    }

    pub fn append_activity(&self, line: &str) {
        let stamp = chrono::Local::now().format("%H:%M:%S");
        let mut end = self.activity.end_iter();
        self.activity.insert(&mut end, &format!("{stamp}  {line}\n"));
        // Bounded: a week in the tray should not grow without limit.
        let lines = self.activity.line_count();
        if lines > 5000 {
            let mut start = self.activity.start_iter();
            if let Some(mut cut) = self.activity.iter_at_line(lines - 4000) {
                self.activity.delete(&mut start, &mut cut);
            }
        }
    }

    pub fn show_text(&self, title: &str, text: &str) {
        let dialog = adw::Dialog::builder()
            .title(title)
            .content_width(720)
            .content_height(520)
            .build();
        let view = gtk::TextView::builder()
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(12)
            .bottom_margin(12)
            .left_margin(12)
            .right_margin(12)
            .build();
        view.buffer().set_text(text);
        let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
        let copy = gtk::Button::builder().icon_name("edit-copy-symbolic").tooltip_text("Copy").build();
        let text_owned = text.to_string();
        copy.connect_clicked(move |b| {
            b.clipboard().set_text(&text_owned);
        });
        let header = adw::HeaderBar::new();
        header.pack_start(&copy);
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&scroll));
        dialog.set_child(Some(&tv));
        dialog.present(Some(&self.window));
    }

    pub fn confirm(&self, title: &str, body: &str, verb: &str, destructive: bool, then: impl Fn() + 'static) {
        let dialog = adw::AlertDialog::new(Some(title), Some(body));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("go", verb);
        dialog.set_response_appearance(
            "go",
            if destructive { adw::ResponseAppearance::Destructive } else { adw::ResponseAppearance::Suggested },
        );
        dialog.set_default_response(Some("cancel"));
        dialog.connect_response(None, move |_, r| {
            if r == "go" {
                then();
            }
        });
        dialog.present(Some(&self.window));
    }

    fn wire_runner(self: &Rc<Self>) {
        let a = Rc::downgrade(self);
        self.runner.listen(move |event| {
            let Some(a) = a.upgrade() else { return };
            match event {
                RunEvent::Started { label } => {
                    a.append_activity(&format!("▶ {label}"));
                    *a.progress.borrow_mut() = format!("{label}…");
                    a.notify_status();
                }
                RunEvent::Line { line } => {
                    if std::env::var_os("PVPN_GUI_DEBUG").is_some() {
                        eprintln!("  | {}", line.trim_end());
                    }
                    a.append_activity(&format!("  {}", line.trim_end()));
                    if !line.trim().is_empty() {
                        *a.progress.borrow_mut() = runner::last_line(line).unwrap_or_default();
                        a.notify_status();
                    }
                }
                RunEvent::Finished { label, output } => {
                    let headline = output.headline();
                    a.append_activity(&format!(
                        "■ {label}: {} — {headline}",
                        if output.ok { "done" } else { "failed" }
                    ));
                    *a.progress.borrow_mut() = String::new();
                    a.toast(&headline);
                    let failed_connect = !output.ok && !label.starts_with("Disconnect");
                    if failed_connect {
                        let note = Note::Failure { what: headline.clone() };
                        if notify::wanted(&a.settings.borrow().notifications, &note) {
                            notify::send(&note);
                        }
                        a.announced_failure_at.set(Some(Instant::now()));
                    }
                    a.poll_now(true);
                    a.reload_data();
                }
            }
        });
    }

    // ---------- status ----------

    pub fn on_status(&self, f: impl Fn(&Rc<App>) + 'static) {
        self.status_listeners.borrow_mut().push(Rc::new(f));
    }

    pub fn on_data(&self, f: impl Fn(&Rc<App>) + 'static) {
        self.data_listeners.borrow_mut().push(Rc::new(f));
    }

    fn notify_status(self: &Rc<Self>) {
        let listeners: Vec<Listener> = self.status_listeners.borrow().clone();
        for l in listeners {
            l(self);
        }
        self.update_tray();
    }

    pub fn reload_data(self: &Rc<Self>) {
        let listeners: Vec<Listener> = self.data_listeners.borrow().clone();
        for l in listeners {
            l(self);
        }
    }

    pub fn restart_polling(self: &Rc<Self>) {
        if let Some(id) = self.poll_source.borrow_mut().take() {
            id.remove();
        }
        let secs = self.settings.borrow().poll_secs.clamp(1, 120);
        let a = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local(secs, move || {
            match a.upgrade() {
                Some(a) => {
                    a.poll_now(false);
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        *self.poll_source.borrow_mut() = Some(id);
        self.poll_now(false);

        // The fast half: the connect lock, every second, on this thread.
        if self.busy_source.borrow().is_none() {
            let a = Rc::downgrade(self);
            let id = glib::timeout_add_seconds_local(1, move || match a.upgrade() {
                Some(a) => {
                    a.refresh_busy();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
            *self.busy_source.borrow_mut() = Some(id);
        }
    }

    /// Re-read only who is connecting, and re-derive the phase from the
    /// last full reading. A change here is what turns the orb amber and
    /// what makes "Reconnecting" possible to notice at all on a connect that
    /// takes one second.
    fn refresh_busy(self: &Rc<Self>) {
        let (busy, autoconnect_running) = status::read_busy(self.runner.pid());
        let changed = {
            let snap = self.snapshot.borrow();
            match snap.as_ref() {
                Some(s) => s.busy != busy || s.autoconnect_running != autoconnect_running,
                None => false,
            }
        };
        if !changed {
            return;
        }
        let mut snap = self.snapshot.borrow().clone().unwrap_or_default();
        let finished = (snap.busy.is_some() || snap.autoconnect_running) && busy.is_none() && !autoconnect_running;
        if finished {
            // Whatever just finished changed the routes, and the last reading
            // predates that — deriving a phase from it would call a landed
            // connect a failure. Stay as we are until a fresh one is in, and
            // throw away any reading already under way.
            self.generation.set(self.generation.get() + 1);
            self.polling.set(false);
            self.poll_now(true);
            return;
        }
        snap.busy = busy;
        snap.autoconnect_running = autoconnect_running;
        self.absorb(snap, false);
    }

    /// A poll shortly after starting something, so the orb turns amber
    /// without waiting for the clock.
    fn poll_soon(self: &Rc<Self>) {
        let a = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(a) = a.upgrade() {
                a.poll_now(false);
            }
        });
    }

    /// Read the state now. `probe` forces a traffic check.
    pub fn poll_now(self: &Rc<Self>, probe: bool) {
        if self.polling.replace(true) {
            return;
        }
        let every = self.settings.borrow().traffic_check_secs;
        let due = every > 0
            && self
                .last_probe
                .get()
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(every as u64));
        // Never probe through a tunnel that is being built: the answer is
        // meaningless and the request competes with the connect's own.
        let probe = (probe || due) && !self.runner.busy();
        let own = self.runner.pid();
        let a = self.clone();
        let generation = self.generation.get();
        glib::spawn_future_local(async move {
            let snap = gio::spawn_blocking(move || status::read(probe, own)).await;
            if a.generation.get() != generation {
                return; // superseded by the reading started after a connect finished
            }
            a.polling.set(false);
            if let Ok(mut snap) = snap {
                // The lock may have changed while NetworkManager kept this
                // reading waiting; the fresh answer wins.
                let (busy, autoconnect_running) = status::read_busy(a.runner.pid());
                snap.busy = busy;
                snap.autoconnect_running = autoconnect_running;
                a.absorb(snap, probe);
            }
        });
    }

    fn absorb(self: &Rc<Self>, snap: Snapshot, probed: bool) {
        if probed && snap.tunneled {
            self.last_probe.set(Some(Instant::now()));
            self.last_carrying.set(snap.carrying);
        }
        if !snap.tunneled {
            self.last_carrying.set(None);
        }
        let phase = Phase::of(&snap, self.last_carrying.get());
        let note = self.tracker.borrow_mut().observe(&phase);
        if std::env::var_os("PVPN_GUI_DEBUG").is_some() && *self.phase.borrow() != phase {
            eprintln!("{} phase: {phase:?} carrying={:?} note={note:?}", chrono::Local::now().format("%T"), snap.carrying);
        }
        if let Some(mut note) = note {
            if let Note::Disconnected { expected } = &mut note {
                // `pvpn down` leaves this marker behind: the drop was asked for.
                *expected |= snap.down_by_user;
            }
            let recently_announced = self
                .announced_failure_at
                .get()
                .is_some_and(|t| t.elapsed() < Duration::from_secs(15));
            let duplicate = matches!(note, Note::Failure { .. }) && recently_announced;
            if !duplicate && notify::wanted(&self.settings.borrow().notifications, &note) {
                notify::send(&note);
            }
        }
        *self.phase.borrow_mut() = phase;
        let network_changed = self.snapshot.borrow().as_ref().map(|s| s.network.as_str()) != Some(snap.network.as_str());
        *self.snapshot.borrow_mut() = Some(snap);
        self.notify_status();
        // The pages file everything per network: a new one (or the first
        // reading) means a different set of records to show.
        if network_changed {
            self.reload_data();
        }
    }

    // ---------- tray ----------

    pub fn apply_tray(self: &Rc<Self>) {
        let want = self.settings.borrow().tray;
        let have = self.tray.borrow().is_some();
        if want && !have {
            let (tx, rx) = async_channel::unbounded::<TrayMsg>();
            let notifications = self.settings.borrow().notifications.enabled;
            if let Some(handle) = tray::spawn(tx, notifications) {
                *self.tray.borrow_mut() = Some(handle);
                *self.hold.borrow_mut() = Some(self.gapp.hold());
                let a = Rc::downgrade(self);
                glib::spawn_future_local(async move {
                    while let Ok(msg) = rx.recv().await {
                        let Some(a) = a.upgrade() else { break };
                        a.on_tray(msg);
                    }
                });
                *self.tray_shown.borrow_mut() = Default::default();
                self.update_tray();
            } else {
                self.toast("No system tray available");
            }
        } else if !want && have {
            if let Some(handle) = self.tray.borrow_mut().take() {
                let _ = handle.shutdown();
            }
            self.hold.borrow_mut().take();
        }
    }

    fn on_tray(self: &Rc<Self>, msg: TrayMsg) {
        match msg {
            TrayMsg::Show => {
                if self.window.is_visible() && self.window.is_active() {
                    self.window.set_visible(false);
                } else {
                    self.present();
                }
            }
            TrayMsg::Connect => self.connect(),
            TrayMsg::Disconnect => self.disconnect(),
            TrayMsg::Cancel => self.runner.cancel(),
            TrayMsg::Hop(to) => self.hop(to.as_deref()),
            TrayMsg::Check => self.run_to_dialog("Tunnel check", &["watch", "--check"]),
            TrayMsg::Preferences => {
                self.present();
                self.open_preferences(None);
            }
            TrayMsg::SetNotifications(on) => {
                self.settings.borrow_mut().notifications.enabled = on;
                let _ = self.settings.borrow().save();
                self.toast(if on { "Notifications on" } else { "Notifications off" });
            }
            TrayMsg::Quit => self.quit(),
        }
    }

    /// The five fastest servers to offer from the tray and the orb page.
    pub fn fastest_names(&self, n: usize) -> Vec<String> {
        let state = data::load_state();
        let network = self
            .snapshot
            .borrow()
            .as_ref()
            .map(|s| s.network.clone())
            .unwrap_or_else(|| state.network().to_string());
        let config = pvpn_core::config::Config::load(&pvpn_core::config::Config::default_path()).unwrap_or_default();
        let view = data::view(&state, &network, &config);
        let mut names: Vec<String> = Vec::new();
        let mut state = state;
        state.set_network(network);
        for (name, _) in state.fastest_working_list() {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        for s in view.servers.iter().filter(|s| s.kind != data::Kind::Blocked) {
            if !names.contains(&s.name) {
                names.push(s.name.clone());
            }
        }
        names.truncate(n);
        names
    }

    fn update_tray(self: &Rc<Self>) {
        let tray = self.tray.borrow();
        let Some(handle) = tray.as_ref() else { return };
        let phase = self.phase.borrow().clone();
        let snap = self.snapshot.borrow().clone();
        let busy = self.runner.busy() || phase.is_busy();
        let connected = matches!(phase, Phase::Connected { .. } | Phase::Broken { .. });
        let detail = match (&phase, &snap) {
            (Phase::Connected { server }, Some(s)) => format!("{server} · {}", s.protocol),
            (Phase::Broken { reason }, _) => reason.clone(),
            (_, Some(s)) => s.network.trim_start_matches("wifi:").to_string(),
            _ => String::new(),
        };
        let fastest = self.fastest_names(6);
        let notifications = self.settings.borrow().notifications.enabled;
        let shown = (
            phase.css().to_string(),
            phase.title().to_string(),
            detail.clone(),
            busy,
            connected,
            fastest.clone(),
            notifications,
        );
        if *self.tray_shown.borrow() == shown {
            return;
        }
        *self.tray_shown.borrow_mut() = shown;
        let css = phase.css().to_string();
        let title = phase.title().to_string();
        handle.update(move |t| {
            t.css = css;
            t.title = title;
            t.detail = detail;
            t.busy = busy;
            t.connected = connected;
            t.fastest = fastest;
            t.notifications = notifications;
        });
    }

    // ---------- files ----------

    pub fn load_places(self: &Rc<Self>) {
        let a = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(places) = gio::spawn_blocking(data::places).await {
                *a.places.borrow_mut() = places;
                a.reload_data();
            }
        });
    }

    /// Reload the pages when `pvpn` writes what it learned — from this
    /// window, a terminal, or the watch timer alike.
    fn watch_files(self: &Rc<Self>) {
        use pvpn_core::config::Config;
        let paths = [
            Config::state_path(),
            Config::default_path(),
            pvpn_core::paths::serverlist_path(),
        ];
        for path in paths {
            let file = gio::File::for_path(&path);
            let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) else {
                continue;
            };
            monitor.set_rate_limit(1000);
            let a = Rc::downgrade(self);
            let is_serverlist = path == pvpn_core::paths::serverlist_path();
            monitor.connect_changed(move |_, _, _, event| {
                if !matches!(event, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created) {
                    return;
                }
                if let Some(a) = a.upgrade() {
                    if is_serverlist {
                        a.load_places();
                    } else {
                        a.reload_data();
                    }
                }
            });
            self.monitors.borrow_mut().push(monitor);
        }
    }

    pub fn save_settings(self: &Rc<Self>) {
        if let Err(err) = self.settings.borrow().save() {
            self.toast(&format!("Could not save settings: {err}"));
        }
    }
}

/// So `icon_name(APP_ID)` finds the icon when the app runs from the build
/// tree rather than an install.
fn add_icon_search_path() {
    let Some(display) = gtk::gdk::Display::default() else { return };
    let theme = gtk::IconTheme::for_display(&display);
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/icons");
    if dev.is_dir() {
        theme.add_search_path(&dev);
    }
}
