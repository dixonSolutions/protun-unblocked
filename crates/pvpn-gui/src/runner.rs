//! Running `pvpn` and friends as child processes.
//!
//! The window drives the installed `pvpn` rather than linking the connect
//! code: the connect lock, the Ctrl-C teardown, the "started" network and
//! every fix that lands in the CLI then apply here with nothing to keep in
//! step. The one rule carried over from the CLI's hard lessons: a connect is
//! only ever stopped with SIGINT, which `pvpn` turns into a teardown that
//! puts routing and DNS back. Never SIGTERM, never SIGKILL — either one
//! leaves the kill switch holding the default route.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::rc::Rc;

pub const SIGINT: i32 = 2;

#[derive(Debug, Clone)]
pub struct Output {
    pub ok: bool,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Everything, for a dialog.
    pub fn combined(&self) -> String {
        let mut text = self.stdout.trim_end().to_string();
        let err = self.stderr.trim_end();
        if !err.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(err);
        }
        if text.is_empty() {
            text = if self.ok { "Done.".into() } else { format!("Failed (exit {}).", self.code) };
        }
        text
    }

    /// The line worth a toast: the last non-empty line of whichever stream
    /// the result went to.
    pub fn headline(&self) -> String {
        let pick = if self.ok { &self.stdout } else { &self.stderr };
        let fallback = if self.ok { &self.stderr } else { &self.stdout };
        last_line(pick)
            .or_else(|| last_line(fallback))
            .unwrap_or_else(|| if self.ok { "Done".into() } else { format!("Failed (exit {})", self.code) })
    }
}

pub fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(|l| l.trim().trim_start_matches(['✓', '✗', '→', '·', '!', '•']).trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

fn spawn(argv: &[String], flags: gio::SubprocessFlags) -> Result<gio::Subprocess, glib::Error> {
    let launcher = gio::SubprocessLauncher::new(flags);
    // The CLI's narration is for a terminal; nothing here renders colour.
    launcher.setenv("NO_COLOR", "1", true);
    launcher.unsetenv("RUST_LOG");
    let args: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
    launcher.spawn(&args)
}

fn exit_of(proc: &gio::Subprocess) -> (bool, i32) {
    if proc.has_exited() {
        let code = proc.exit_status();
        (code == 0, code)
    } else {
        // Killed by a signal.
        (false, 128 + proc.term_sig())
    }
}

/// Run to completion and collect both streams. For read-only commands.
pub async fn capture(argv: Vec<String>) -> Output {
    let proc = match spawn(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    ) {
        Ok(p) => p,
        Err(err) => {
            return Output {
                ok: false,
                code: 127,
                stdout: String::new(),
                stderr: format!("Could not run {}: {err}", argv.first().cloned().unwrap_or_default()),
            }
        }
    };
    let (stdout, stderr) = proc
        .communicate_utf8_future(None)
        .await
        .unwrap_or((None, None));
    let (ok, code) = exit_of(&proc);
    Output {
        ok,
        code,
        stdout: stdout.map(|s| s.to_string()).unwrap_or_default(),
        stderr: stderr.map(|s| s.to_string()).unwrap_or_default(),
    }
}

/// Spawn without waiting or reading, e.g. a terminal window.
pub fn detach(argv: &[String]) -> Result<(), glib::Error> {
    spawn(argv, gio::SubprocessFlags::NONE).map(|_| ())
}

/// What a running operation reports as it goes.
#[derive(Debug, Clone)]
pub enum RunEvent {
    Started { label: String },
    Line { line: String },
    Finished { label: String, output: Output },
}

type Listener = Rc<dyn Fn(&RunEvent)>;

/// The one network-moving operation the window may have in flight.
///
/// One, because `pvpn` would serialise a second behind its lock anyway and
/// a button that silently queues a connect behind another is worse than one
/// that says "busy".
#[derive(Default)]
pub struct Runner {
    current: RefCell<Option<gio::Subprocess>>,
    label: RefCell<String>,
    cancelled: Cell<bool>,
    listeners: RefCell<Vec<Listener>>,
}

impl Runner {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn listen(&self, f: impl Fn(&RunEvent) + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    fn emit(&self, event: RunEvent) {
        let listeners: Vec<Listener> = self.listeners.borrow().clone();
        for l in listeners {
            l(&event);
        }
    }

    pub fn busy(&self) -> bool {
        self.current.borrow().is_some()
    }

    pub fn label(&self) -> String {
        self.label.borrow().clone()
    }

    /// The pid of our own child, so the status reader can tell our connect
    /// from the autoconnect hook's.
    pub fn pid(&self) -> Option<u32> {
        self.current
            .borrow()
            .as_ref()
            .and_then(|p| p.identifier())
            .and_then(|id| id.parse().ok())
    }

    /// Ask the running operation to stop the way Ctrl-C would. `pvpn` tears
    /// the half-built tunnel down and restores normal routing before it
    /// exits.
    pub fn cancel(&self) {
        if let Some(proc) = self.current.borrow().as_ref() {
            self.cancelled.set(true);
            proc.send_signal(SIGINT);
        }
    }

    /// Start `argv`, streaming merged output line by line. Returns false
    /// (and starts nothing) if something is already running.
    pub fn start(self: &Rc<Self>, label: &str, argv: Vec<String>) -> bool {
        if self.busy() {
            return false;
        }
        let proc = match spawn(
            &argv,
            gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
        ) {
            Ok(p) => p,
            Err(err) => {
                self.emit(RunEvent::Finished {
                    label: label.to_string(),
                    output: Output {
                        ok: false,
                        code: 127,
                        stdout: String::new(),
                        stderr: format!("Could not run {}: {err}", argv.join(" ")),
                    },
                });
                return true;
            }
        };
        *self.current.borrow_mut() = Some(proc.clone());
        *self.label.borrow_mut() = label.to_string();
        self.cancelled.set(false);
        self.emit(RunEvent::Started {
            label: label.to_string(),
        });

        let this = Rc::clone(self);
        let label = label.to_string();
        glib::spawn_future_local(async move {
            let mut collected = String::new();
            if let Some(out) = proc.stdout_pipe() {
                let reader = gio::DataInputStream::new(&out);
                while let Ok(Some(line)) = reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
                    let line = line.to_string();
                    collected.push_str(&line);
                    collected.push('\n');
                    this.emit(RunEvent::Line { line });
                }
            }
            let _ = proc.wait_future().await;
            let (ok, code) = exit_of(&proc);
            this.current.borrow_mut().take();
            let mut output = Output {
                ok,
                code,
                stdout: if ok { collected.clone() } else { String::new() },
                stderr: if ok { String::new() } else { collected },
            };
            if this.cancelled.get() && !ok && output.stderr.trim().is_empty() {
                output.stderr = "Cancelled.".into();
            }
            this.emit(RunEvent::Finished { label, output });
        });
        true
    }
}

/// A terminal to run an interactive command in (a password prompt, sudo),
/// holding the window open afterwards so its output can be read.
pub fn in_terminal(command: &str) -> Vec<String> {
    let shell = format!("{command}; echo; read -r -p 'Press Enter to close…' _");
    for (term, flag) in [
        ("ptyxis", "--"),
        ("kgx", "--"),
        ("gnome-terminal", "--"),
        ("konsole", "-e"),
        ("xfce4-terminal", "-x"),
        ("x-terminal-emulator", "-e"),
        ("xterm", "-e"),
    ] {
        if glib::find_program_in_path(term).is_some() {
            return vec![
                term.to_string(),
                flag.to_string(),
                "bash".into(),
                "-lc".into(),
                shell,
            ];
        }
    }
    vec!["xterm".into(), "-e".into(), "bash".into(), "-lc".into(), shell]
}

/// Quote one argument for `bash -lc`.
pub fn sh_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./#:=@".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_prefers_the_stream_the_result_went_to() {
        let out = Output {
            ok: false,
            code: 1,
            stdout: "noise\n".into(),
            stderr: "  ✓ tried SG\n  ✗ All servers failed\n\n".into(),
        };
        assert_eq!(out.headline(), "All servers failed");
        let ok = Output {
            ok: true,
            code: 0,
            stdout: "Connected to JP#2\n".into(),
            stderr: "progress\n".into(),
        };
        assert_eq!(ok.headline(), "Connected to JP#2");
    }

    #[test]
    fn quoting_keeps_server_names_bare_and_wraps_the_rest() {
        assert_eq!(sh_quote("SG-FREE#2"), "SG-FREE#2");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }
}
