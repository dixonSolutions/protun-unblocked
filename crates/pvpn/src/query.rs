//! The machine-readable side of `pvpn`: what the window asks for instead of
//! reading files and running other programs itself.
//!
//! The window used to read `state.json`, Proton's inventory and the config
//! directly, run `protonvpn` and `systemctl` itself, and re-derive the
//! tunnel's state with its own copy of the logic. Two readers of the same
//! facts drift; this keeps one. Everything here prints JSON on stdout (or
//! does one thing and says so), so the window — or a script — only ever
//! talks to `pvpn`.

use clap::Subcommand;
use pvpn_core::config::Config;
use std::io::Read;
use std::process::Command;

#[derive(Subcommand, Debug)]
pub enum What {
    /// The tunnel, who is connecting, and whether a reconnect is allowed here
    Status {
        /// Also send a request through the tunnel to see if it carries
        #[arg(long)]
        probe: bool,
    },
    /// Only who holds the connect lock — cheap enough to ask every second
    Busy,
    /// Everything every network has taught pvpn (`state.json`)
    State,
    /// Proton's server inventory (`serverlist.json`), steer-healed
    Inventory,
    /// The client certificate's expiry and renewal point
    Cert,
    /// The effective configuration, defaults filled in
    Config,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the effective configuration as JSON
    Get,
    /// Set one key; VALUE is a TOML value (`true`, `30`, `["a", "b"]`) or a bare string
    Set { key: String, value: String },
    /// Replace the whole file with stdin, if pvpn can read the result
    Write,
}

#[derive(Subcommand, Debug)]
pub enum ServiceAction {
    /// start, stop, restart, enable or disable a unit
    Run {
        verb: String,
        unit: String,
        /// A user unit rather than a system one
        #[arg(long)]
        user: bool,
    },
    /// `systemctl show` for a unit, key=value per line
    Show {
        unit: String,
        #[arg(long)]
        user: bool,
    },
    /// The unit's recent journal
    Journal {
        unit: String,
        #[arg(long)]
        user: bool,
        #[arg(long, short = 'n', default_value_t = 200)]
        lines: u32,
    },
}

fn print_json(value: &impl serde::Serialize) -> i32 {
    match serde_json::to_string(value) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(err) => {
            eprintln!("could not encode: {err}");
            1
        }
    }
}

pub fn query(what: What) -> i32 {
    match what {
        What::Status { probe } => print_json(&pvpn_core::snapshot::read(probe, None)),
        What::Busy => {
            let (busy, autoconnect_running) = pvpn_core::snapshot::read_busy(None);
            print_json(&serde_json::json!({
                "busy": busy,
                "autoconnect_running": autoconnect_running,
            }))
        }
        What::State => {
            let state =
                pvpn_core::state::State::load(&Config::state_path()).unwrap_or_default();
            print_json(&state)
        }
        What::Inventory => {
            let path = pvpn_core::paths::serverlist_path();
            let _ = pvpn_core::cache::heal_stranded_steer(&path);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    println!("{}", text.trim());
                    0
                }
                Err(err) => {
                    eprintln!("no server inventory at {}: {err}", path.display());
                    1
                }
            }
        }
        What::Cert => match pvpn_core::cert::status_fast() {
            Some(status) => print_json(&serde_json::json!({
                "expires_at": status.expires_at,
                "refresh_at": status.refresh_at,
                "describe": status.describe(chrono::Utc::now()),
                "unusable": status.unusable(chrono::Utc::now()),
            })),
            None => {
                eprintln!("could not read the certificate from the keyring");
                1
            }
        },
        What::Config => config(ConfigAction::Get),
    }
}

fn effective_config() -> Config {
    Config::load(&Config::default_path()).unwrap_or_default()
}

/// Validate and write the config file in one step, so nothing pvpn cannot
/// read ever lands on disk.
fn write_config(text: &str) -> Result<(), String> {
    toml::from_str::<Config>(text).map_err(|e| e.to_string())?;
    let path = Config::default_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

pub fn config(action: ConfigAction) -> i32 {
    let result = match action {
        ConfigAction::Get => return print_json(&effective_config()),
        ConfigAction::Set { key, value } => {
            let path = Config::default_path();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            text.parse::<toml_edit::DocumentMut>()
                .map_err(|e| e.to_string())
                .and_then(|mut doc| {
                    // A TOML literal where it parses as one, else a string:
                    // `pvpn config set country JP` should not need quotes.
                    let parsed = format!("v = {value}")
                        .parse::<toml_edit::DocumentMut>()
                        .ok()
                        .and_then(|d| d.get("v").and_then(|i| i.as_value().cloned()));
                    let value = parsed.unwrap_or_else(|| toml_edit::Value::from(value.as_str()));
                    doc[key.as_str()] = toml_edit::Item::Value(value);
                    write_config(&doc.to_string())
                })
        }
        ConfigAction::Write => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())
                .and_then(|_| write_config(&text))
        }
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("config not changed: {err}");
            1
        }
    }
}

/// Proton's own CLI, with the same shim and API budget pvpn gives it, so a
/// blocked API fails in seconds rather than half a minute.
pub fn proton(args: &[String]) -> i32 {
    let _ = pvpn_core::paths::ensure_shim();
    let mut command = Command::new("protonvpn");
    command.args(args);
    for (key, value) in pvpn_core::proc::proton_api_envs(false) {
        command.env(key, value);
    }
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("could not run protonvpn: {err}");
            127
        }
    }
}

/// The units the window manages. Anything else is refused: this runs
/// `pkexec systemctl`, and `systemd-logind` in particular ends the session.
const USER_UNITS: [&str; 3] = [
    "pvpn-watch.timer",
    "pvpn-watch.service",
    "pvpn-autoconnect.service",
];
const SYSTEM_UNITS: [&str; 6] = [
    "tor@default.service",
    "pvpn-recover.service",
    "tor.service",
    "NetworkManager.service",
    "systemd-resolved.service",
    "tailscaled.service",
];
const VERBS: [&str; 5] = ["start", "stop", "restart", "enable", "disable"];

fn known_unit(unit: &str, user: bool) -> bool {
    if user {
        USER_UNITS.contains(&unit)
    } else {
        SYSTEM_UNITS.contains(&unit)
    }
}

/// The argv for a service action — split out so it can be tested without
/// running anything.
pub fn service_argv(action: &ServiceAction) -> Result<Vec<String>, String> {
    let (unit, user) = match action {
        ServiceAction::Run { unit, user, .. }
        | ServiceAction::Show { unit, user }
        | ServiceAction::Journal { unit, user, .. } => (unit.as_str(), *user),
    };
    if !known_unit(unit, user) {
        return Err(format!(
            "{unit} is not one of the {} units pvpn manages",
            if user { "user" } else { "system" }
        ));
    }
    let mut argv: Vec<String> = Vec::new();
    match action {
        ServiceAction::Run { verb, .. } => {
            if !VERBS.contains(&verb.as_str()) {
                return Err(format!("unknown action {verb} (one of: {})", VERBS.join(", ")));
            }
            if !user {
                argv.push("pkexec".into());
            }
            argv.push("systemctl".into());
            if user {
                argv.push("--user".into());
            }
            argv.push(verb.clone());
            // A restart of NetworkManager or Tor can take a while; the
            // window should hear back that it was asked, not wait it out.
            if matches!(verb.as_str(), "start" | "restart") {
                argv.push("--no-block".into());
            }
            argv.push(unit.to_string());
        }
        ServiceAction::Show { .. } => {
            argv.push("systemctl".into());
            if user {
                argv.push("--user".into());
            }
            argv.extend([
                "show".to_string(),
                unit.to_string(),
                "--property=LoadState,ActiveState,SubState,UnitFileState,NextElapseUSecRealtime".to_string(),
            ]);
        }
        ServiceAction::Journal { lines, .. } => {
            argv.push("journalctl".into());
            if user {
                argv.push("--user".into());
            }
            argv.extend([
                "-u".to_string(),
                unit.to_string(),
                "-n".to_string(),
                lines.to_string(),
                "--no-pager".to_string(),
                "-o".to_string(),
                "short-iso".to_string(),
            ]);
        }
    }
    Ok(argv)
}

pub fn service(action: ServiceAction) -> i32 {
    let argv = match service_argv(&action) {
        Ok(argv) => argv,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    match Command::new(&argv[0]).args(&argv[1..]).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("could not run {}: {err}", argv[0]);
            127
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_units_go_through_pkexec_and_starts_do_not_block() {
        let tor = ServiceAction::Run {
            verb: "restart".into(),
            unit: "tor.service".into(),
            user: false,
        };
        assert_eq!(
            service_argv(&tor).unwrap(),
            ["pkexec", "systemctl", "restart", "--no-block", "tor.service"]
        );
        let watch = ServiceAction::Run {
            verb: "enable".into(),
            unit: "pvpn-watch.timer".into(),
            user: true,
        };
        assert_eq!(
            service_argv(&watch).unwrap(),
            ["systemctl", "--user", "enable", "pvpn-watch.timer"]
        );
    }

    #[test]
    fn only_the_managed_units_and_verbs_are_accepted() {
        let logind = ServiceAction::Run {
            verb: "restart".into(),
            unit: "systemd-logind.service".into(),
            user: false,
        };
        assert!(service_argv(&logind).is_err());
        let wrong_scope = ServiceAction::Show {
            unit: "tor.service".into(),
            user: true,
        };
        assert!(service_argv(&wrong_scope).is_err());
        let mask = ServiceAction::Run {
            verb: "mask".into(),
            unit: "tor.service".into(),
            user: false,
        };
        assert!(service_argv(&mask).is_err());
    }
}
