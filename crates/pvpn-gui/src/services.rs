//! The systemd units around `pvpn`, and the system services it leans on.
//!
//! User units run as-is; system units go through `pkexec`, which asks for a
//! password in the desktop's own dialog. `systemd-logind` is deliberately
//! absent: restarting it ends the graphical session.

use crate::runner::{capture, Output};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
}

impl Verb {
    pub fn word(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
            Verb::Enable => "enable",
            Verb::Disable => "disable",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Verb::Start => "Start",
            Verb::Stop => "Stop",
            Verb::Restart => "Restart",
            Verb::Enable => "Enable",
            Verb::Disable => "Disable",
        }
    }
}

pub struct Unit {
    pub name: &'static str,
    pub user: bool,
    pub title: &'static str,
    pub blurb: &'static str,
    pub verbs: &'static [Verb],
    /// Shown in a confirmation before any verb runs.
    pub caution: Option<&'static str>,
}

use Verb::*;

pub const UNITS: &[Unit] = &[
    Unit {
        name: "pvpn-watch.timer",
        user: true,
        title: "Tunnel health timer",
        blurb: "Every two minutes, asks whether the tunnel still carries; a dead one is recorded and rebuilt where autoconnect allows.",
        verbs: &[Enable, Disable, Start, Stop],
        caution: None,
    },
    Unit {
        name: "pvpn-watch.service",
        user: true,
        title: "Health check",
        blurb: "One run of `pvpn watch`. Start runs a check now.",
        verbs: &[Start, Stop],
        caution: None,
    },
    Unit {
        name: "pvpn-autoconnect.service",
        user: true,
        title: "Autoconnect",
        blurb: "Rebuilds the tunnel after a resume or a link change, on networks autoconnect allows. Stop cancels a reconnect cleanly (SIGINT).",
        verbs: &[Start, Stop],
        caution: None,
    },
    Unit {
        name: "pvpn-recover.service",
        user: false,
        title: "Resume recovery",
        blurb: "Runs on resume: repairs stranded VPN DNS, then asks autoconnect to reconnect.",
        verbs: &[Enable, Disable, Start],
        caution: None,
    },
    Unit {
        name: "tor.service",
        user: false,
        title: "Tor",
        blurb: "The path certificate renewals take when this network blocks Proton's API.",
        verbs: &[Start, Stop, Restart, Enable, Disable],
        caution: None,
    },
    Unit {
        name: "NetworkManager.service",
        user: false,
        title: "NetworkManager",
        blurb: "Owns every connection, the tunnel included.",
        verbs: &[Restart],
        caution: Some("Restarting NetworkManager drops every connection for a few seconds, the tunnel included."),
    },
    Unit {
        name: "systemd-resolved.service",
        user: false,
        title: "DNS resolver",
        blurb: "systemd-resolved. A restart is the fix for a resolver left pointing into a dead tunnel.",
        verbs: &[Restart],
        caution: Some("Name resolution stops for a moment while it restarts."),
    },
    Unit {
        name: "tailscaled.service",
        user: false,
        title: "Tailscale",
        blurb: "A second tunnel that rebinds routes and DNS on every tunnel change. Stop it to rule it out.",
        verbs: &[Start, Stop, Restart],
        caution: None,
    },
];

#[derive(Debug, Clone, Default)]
pub struct UnitState {
    pub loaded: bool,
    pub active: String,
    pub sub: String,
    pub file_state: String,
    /// Timers only: when it fires next.
    pub next: String,
}

impl UnitState {
    pub fn summary(&self) -> String {
        if !self.loaded {
            return "not installed".into();
        }
        let mut s = format!("{} ({})", self.active, self.sub);
        if !self.file_state.is_empty() {
            s.push_str(&format!(" · {}", self.file_state));
        }
        if !self.next.is_empty() && self.next != "n/a" {
            s.push_str(&format!(" · next {}", self.next));
        }
        s
    }
}

pub fn parse_show(text: &str) -> UnitState {
    let map: HashMap<&str, &str> = text
        .lines()
        .filter_map(|l| l.split_once('='))
        .collect();
    let get = |k: &str| map.get(k).copied().unwrap_or("").to_string();
    UnitState {
        loaded: get("LoadState") == "loaded",
        active: get("ActiveState"),
        sub: get("SubState"),
        file_state: get("UnitFileState"),
        next: get("NextElapseUSecRealtime"),
    }
}

/// `pvpn service …` for this unit: the CLI owns which units may be touched
/// and how (`pkexec` for system units, `--no-block` for starts).
fn pvpn_service(unit: &Unit, args: &[&str]) -> Vec<String> {
    let mut full = vec!["service"];
    full.extend_from_slice(args);
    if unit.user {
        full.push("--user");
    }
    crate::cli::argv(&full)
}

pub async fn state(unit: &Unit) -> UnitState {
    parse_show(&capture(pvpn_service(unit, &["show", unit.name])).await.stdout)
}

pub fn command(unit: &Unit, verb: Verb) -> Vec<String> {
    pvpn_service(unit, &["run", verb.word(), unit.name])
}

pub async fn run(unit: &Unit, verb: Verb) -> Output {
    capture(command(unit, verb)).await
}

/// `pvpn service journal` for a unit's recent lines.
pub fn journal_command(unit: &str, user: bool, lines: u32) -> Vec<String> {
    let lines = lines.to_string();
    let mut args = vec!["service", "journal", unit, "-n", lines.as_str()];
    if user {
        args.push("--user");
    }
    crate::cli::argv(&args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_output_parses() {
        let s = parse_show(
            "LoadState=loaded\nActiveState=active\nSubState=waiting\nUnitFileState=enabled\n\
             NextElapseUSecRealtime=Tue 2026-09-29 10:50:12 AEST\n",
        );
        assert!(s.loaded);
        assert_eq!(s.summary(), "active (waiting) · enabled · next Tue 2026-09-29 10:50:12 AEST");
        assert_eq!(parse_show("LoadState=not-found\n").summary(), "not installed");
    }

    #[test]
    fn service_actions_are_asked_of_pvpn() {
        // `pvpn service` decides pkexec and --no-block; see the CLI's tests.
        let tor = UNITS.iter().find(|u| u.name == "tor.service").unwrap();
        assert_eq!(command(tor, Verb::Restart)[1..], ["service", "run", "restart", "tor.service"]);
        let watch = UNITS.iter().find(|u| u.name == "pvpn-watch.timer").unwrap();
        assert_eq!(command(watch, Verb::Enable)[1..], ["service", "run", "enable", "pvpn-watch.timer", "--user"]);
        assert_eq!(
            journal_command("pvpn-watch.service", true, 50)[1..],
            ["service", "journal", "pvpn-watch.service", "-n", "50", "--user"]
        );
    }

    #[test]
    fn logind_is_never_offered() {
        assert!(UNITS.iter().all(|u| !u.name.contains("logind")));
    }
}
