//! `pvpn` CLI — Proton VPN control for filtered networks.
//!
//! One process, no daemon. Every command does its own work and exits:
//! nothing keeps running afterwards, nothing reconnects on its own, and
//! nothing has an opinion about what should be up while you are not
//! looking. What *is* remembered is what this network taught us about
//! servers — the fast and blocked lists in `~/.local/share/pvpn/state.json`
//! — which every connect reads before choosing and writes after trying.

mod apps_cmd;
mod apps_hook;
mod blocklist;
mod connect;
mod login;
mod narrate;
mod session;

use clap::{Parser, Subcommand};
use pvpn_core::display;
use pvpn_core::paths;
use pvpn_core::pipeline;
use session::Session;

#[derive(Parser)]
#[command(
    name = "pvpn",
    about = "Proton VPN control for filtered networks",
    long_about = USAGE,
    disable_help_subcommand = true,
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the fastest measured server
    #[command(alias = "connect")]
    Up { protocol: Option<String> },
    /// Rank the fastest servers your account can use
    #[command(after_help = "\
pvpn best — find the fastest servers your account can use

  pvpn best                 measure and rank them
  pvpn best --connect       measure, then connect to the best one
  pvpn best --country JP    only one country
  pvpn best --limit 20      show more results
  pvpn best --quick         skip measuring; rank by distance and load
  pvpn best --json          machine-readable output
")]
    Best {
        /// Measure, then connect to the best one
        #[arg(long, short = 'c')]
        connect: bool,
        /// Skip measuring; rank by distance and load
        #[arg(long, short = 'q')]
        quick: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Only one country
        #[arg(long)]
        country: Option<String>,
        /// How many results to show
        #[arg(long, default_value_t = 10)]
        limit: u32,
        /// Only free-tier servers
        #[arg(long)]
        free: bool,
        /// Rank a specific serverlist.json instead of the cached one
        #[arg(long, hide = true)]
        serverlist: Option<std::path::PathBuf>,
        /// Rank from the cache without refreshing it (used by tests)
        #[arg(long, hide = true)]
        offline: bool,
    },
    /// Switch server; optionally one matching e.g. JP, SG-FREE#12
    #[command(alias = "next")]
    Hop { pattern: Option<String> },
    /// Disconnect and restore normal internet
    #[command(alias = "disconnect")]
    Down,
    /// Show connection state and protocol
    #[command(alias = "st")]
    Status,
    /// Show your current public IP
    Ip,
    /// Sign in to your Proton account
    #[command(alias = "signin")]
    Login {
        email: Option<String>,
        /// Browser bridge: CAPTCHA in browser → CLI session
        #[arg(long, short = 'b')]
        browser: bool,
    },
    /// Account helpers
    #[command(alias = "acct")]
    Account {
        #[arg(long, short = 'v')]
        view: bool,
        #[arg(long, short = 'l')]
        logout: bool,
        action: Option<String>,
    },
    /// Sign out
    #[command(alias = "signout")]
    Logout,
    /// Find Flatpak apps whose traffic skips the tunnel
    #[command(after_help = "\
pvpn apps — find Flatpak apps whose traffic skips the tunnel

  pvpn apps                  list apps that are routed around the VPN
  pvpn apps --fix            remove the proxy settings that do that
  pvpn apps --verify         launch each flagged app, compare its exit IP
")]
    Apps {
        /// Remove the proxy settings that do that
        #[arg(long, short = 'f')]
        fix: bool,
        /// Launch each flagged app, compare its exit IP
        #[arg(long, short = 'v')]
        verify: bool,
        apps: Vec<String>,
    },
    /// Try every protocol, stop at the first that works
    Try,
    /// Which backends are actually available here
    #[command(alias = "protos")]
    Protocols,
    /// Privileged cleanup that needs sudo
    Fix {
        /// Blackhole Proton API hosts in /etc/hosts
        #[arg(long)]
        hosts: bool,
        /// Remove the /etc/hosts blackhole
        #[arg(long)]
        unhosts: bool,
    },
    /// Servers measured fast on this network
    Fast,
    /// Servers that failed a real connect on this network
    Blocked,
    /// Read Proton's own log
    #[command(after_help = "\
pvpn logs — Proton's own client log

  pvpn logs             the last 50 lines
  pvpn logs -f          follow as it happens
  pvpn logs -n 200      more history

pvpn narrates its own work as it happens, so there is nothing of its own
to read back. This is Proton's log, which is where an expired certificate
or a session the far end tore down actually shows up.
")]
    Logs {
        /// Follow the log as it happens
        #[arg(long, short = 'f')]
        follow: bool,
        /// How many past lines to show
        #[arg(long, short = 'n', default_value_t = 50)]
        lines: u32,
    },
    /// This message
    Help,
}

const USAGE: &str = "\
pvpn - simple Proton VPN control

  pvpn up [protocol]   connect to the fastest measured server
  pvpn best            rank the fastest servers you can use
  pvpn best --connect  rank them, then connect to the best
  pvpn hop [match]     switch server; optionally one matching e.g. JP, SG-FREE#12
  pvpn down            disconnect and restore normal internet
  pvpn status          show connection state and protocol
  pvpn ip              show your current public IP
  pvpn login [email]   sign in to your Proton account
  pvpn login --browser browser bridge (CAPTCHA in browser → CLI session)
  pvpn account         account helpers (see below)
  pvpn logout          sign out (same as: pvpn account --logout)
  pvpn apps            find Flatpak apps whose traffic skips the tunnel
  pvpn try             try every protocol, stop at the first that works
  pvpn protocols       which backends are actually available here
  pvpn fix             privileged cleanup (sudo)
  pvpn fast            servers this network measured as fast
  pvpn blocked         servers that failed a real connect here
  pvpn logs            read Proton's own log (-f to follow)
  pvpn help            this message

Account:
  pvpn account --view     show signed-in account + VPN state
  pvpn account --logout   sign out and clear local credentials

Choosing a server:
  pvpn best --country JP  rank one country only
  pvpn best --quick       skip measuring (distance and load only)
  pvpn best --json        machine-readable output
  `pvpn up` measures first and connects to the best result, then works
  down the list if one fails.

Nothing runs in the background. A tunnel stays up until you run
`pvpn down`, and nothing brings one back on its own.

Default protocol: protun-tls (Stealth) — required on DPI/filtered wifi.
Others: protun-udp, protun-tcp, protun-smart, openvpn-tcp, openvpn-udp, wireguard
";

fn main() {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(err) => {
            let rendered = err.to_string();
            if rendered.contains("unrecognized subcommand") {
                let name = std::env::args().nth(1).unwrap_or_default();
                eprintln!("Unknown command: {name}");
                eprintln!();
                print!("{USAGE}");
                std::process::exit(1);
            }
            if rendered.contains("unexpected argument") {
                eprintln!("Unknown option: {}", unexpected_option(&rendered));
                std::process::exit(1);
            }
            err.exit();
        }
    };

    narrate::init();
    let code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    };
    std::process::exit(code);
}

fn unexpected_option(rendered: &str) -> String {
    rendered
        .lines()
        .next()
        .and_then(|l| l.split('\'').nth(1))
        .unwrap_or("?")
        .to_string()
}

/// Multi-threaded on purpose: a connect blocks a whole thread in
/// `protonvpn connect` for up to thirty seconds while the probe futures
/// still have to make progress.
fn block_on<F>(fut: F) -> anyhow::Result<i32>
where
    F: std::future::Future<Output = anyhow::Result<i32>>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    match cli.cmd.unwrap_or(Command::Help) {
        Command::Help => {
            print!("{USAGE}");
            Ok(0)
        }
        Command::Up { protocol } => block_on(async move {
            let mut session = Session::load()?;
            Ok(report(connect::up(&mut session, protocol).await))
        }),
        Command::Down => block_on(async move { Ok(report(connect::down().await)) }),
        Command::Hop { pattern } => block_on(async move {
            let mut session = Session::load()?;
            Ok(report(connect::hop(&mut session, pattern).await))
        }),
        Command::Status => block_on(cmd_status()),
        Command::Ip => block_on(cmd_ip()),
        Command::Best {
            connect,
            quick,
            json,
            country,
            limit,
            free,
            serverlist,
            offline,
        } => block_on(cmd_best(
            connect, quick, json, country, limit, free, serverlist, offline,
        )),
        Command::Login { email, browser } => login::cmd_login(email, browser),
        Command::Logout => login::cmd_logout(),
        Command::Account {
            view,
            logout,
            action,
        } => {
            let action = action.unwrap_or_default();
            if logout || matches!(action.as_str(), "--logout" | "-l" | "logout" | "signout") {
                login::cmd_logout()
            } else if action == "help" || action == "--help" {
                print!(
                    "pvpn account — Proton account\n\n\
                       pvpn account              show account (same as --view)\n\
                       pvpn account --view       show signed-in account + VPN state\n\
                       pvpn account --logout     sign out and clear local credentials\n"
                );
                Ok(0)
            } else {
                let _ = view;
                login::cmd_account_view()
            }
        }
        Command::Apps { fix, verify, apps } => apps_cmd::cmd_apps(fix, verify, &apps),
        Command::Try => block_on(login::cmd_try()),
        Command::Protocols => login::cmd_protocols(),
        Command::Fix { hosts, unhosts } => login::cmd_fix(hosts, unhosts),
        Command::Fast => block_on(cmd_fast()),
        Command::Blocked => block_on(cmd_blocked()),
        Command::Logs { follow, lines } => cmd_logs(follow, lines),
    }
}

/// Print the outcome of a connect/disconnect and turn it into an exit code.
fn report(report: connect::UpReport) -> i32 {
    if report.ok {
        println!("{}", report.message);
        0
    } else {
        eprintln!("{}", report.message);
        1
    }
}

/// Report what is actually true, which is not always what Proton believes.
///
/// `protonvpn status` reports what the client *thinks*. On the networks
/// this tool exists for, a middlebox can end the session and leave the
/// status reading `Connected` with every packet going out in the clear —
/// so this checks the routing table too, and says so loudly when the two
/// disagree.
async fn cmd_status() -> anyhow::Result<i32> {
    let session = Session::load()?;
    let status = session.status().await;

    if status.connected {
        println!("Status: Connected");
        if let Some(desc) = &status.server_desc {
            println!("Server: {desc}");
        }
    } else {
        println!("Status: Disconnected");
    }
    println!("Protocol: {}", status.protocol);
    println!("Network: {}", session.network());

    let mut code = 0;
    if status.connected && !status.tunneled {
        eprintln!();
        eprintln!(
            "Nothing is actually tunneled — your traffic is going out in the clear.\n\
             Proton still reports a server. Run: pvpn up   (or: pvpn down)"
        );
        code = 1;
    }
    if let Some(dev) = &status.stray_route {
        eprintln!();
        eprintln!(
            "{dev} is holding a default route and blackholing traffic that way.\n\
             Run: pvpn fix"
        );
        code = 1;
    }
    Ok(code)
}

async fn cmd_ip() -> anyhow::Result<i32> {
    match session::blocking(pvpn_core::net::public_ip).await {
        Some(ip) => {
            println!("Public IP: {ip}");
            Ok(0)
        }
        None => {
            eprintln!("public IP unavailable");
            Ok(1)
        }
    }
}

async fn cmd_fast() -> anyhow::Result<i32> {
    let session = Session::load()?;
    let servers = session.state.fast_list();
    if servers.is_empty() {
        println!(
            "No servers on the fast list for {} yet. `pvpn best` and `pvpn up` fill it in.",
            session.network()
        );
        return Ok(0);
    }
    println!(
        "Fast servers (TLS handshake, measured on {}):",
        session.network()
    );
    for (name, stat) in servers {
        let ms = stat
            .ema_latency_ms
            .map(|v| format!("{v:.0} ms"))
            .unwrap_or_else(|| "?".to_string());
        println!("  {name:<16} {ms}");
    }
    Ok(0)
}

async fn cmd_blocked() -> anyhow::Result<i32> {
    let session = Session::load()?;
    let servers = session.state.blocked_list();
    if servers.is_empty() {
        println!("No blocked servers on {}.", session.network());
        return Ok(0);
    }
    println!(
        "Blocked servers (failed a real connect on {}):",
        session.network()
    );
    for (name, stat) in servers {
        let reason = stat.blocked_reason.as_deref().unwrap_or("unknown");
        // The count is what decides how long the block is held, so it
        // belongs on screen next to the reason.
        let repeats = match stat.consecutive_connect_failures {
            0 | 1 => String::new(),
            n => format!("  ({n} failures in a row)"),
        };
        println!("  {name:<16} {reason}{repeats}");
    }
    Ok(0)
}

/// Proton's own log. There is no pvpn log to read back — the narration
/// already went to the terminal that asked for the work — but Proton's is
/// where `ExpiredCertificate` and a torn-down session actually surface.
fn cmd_logs(follow: bool, lines: u32) -> anyhow::Result<i32> {
    let path = paths::proton_log_path();
    if !path.is_file() {
        eprintln!("No Proton log at {}", path.display());
        eprintln!("It appears once the client has run at least once.");
        return Ok(1);
    }
    let mut cmd = std::process::Command::new("tail");
    cmd.arg("-n").arg(lines.to_string());
    if follow {
        cmd.arg("-F");
    }
    cmd.arg(&path);
    match cmd.status() {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("could not read {}: {err}", path.display());
            Ok(1)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_best(
    do_connect: bool,
    quick: bool,
    json: bool,
    country: Option<String>,
    limit: u32,
    free: bool,
    serverlist: Option<std::path::PathBuf>,
    offline: bool,
) -> anyhow::Result<i32> {
    let fixture =
        serverlist.or_else(|| std::env::var_os("PVPN_SERVERLIST").map(std::path::PathBuf::from));
    let offline = offline || fixture.is_some();

    // `--serverlist` / `--offline`: rank one file and nothing else. No
    // config, no state, no refresh — this is the path the tests use.
    if offline {
        let path = fixture.unwrap_or_else(paths::serverlist_path);
        if !json {
            // chatter stays off stdout so the document is pipeable
            eprintln!("Ranking locally from {}", path.display());
        }
        let req = pipeline::quick_request(&path, country.as_deref(), free, limit as usize);
        let result = pipeline::rank_servers(req).await?;
        if !result.measured && !quick && !json {
            eprintln!(
                "Latency looked like a local middlebox answering, not the real \
                 servers,\nso these are ranked by distance and load instead."
            );
        }
        print_rank(&result, json)?;
        if do_connect {
            let mut session = Session::load()?;
            return Ok(report(connect::up(&mut session, None).await));
        }
        return Ok(0);
    }

    let mut session = Session::load()?;
    let cfg = session.config.clone();
    if let Err(err) = connect::ensure_fresh_data(&cfg, true).await {
        tracing::warn!("best: {err}");
    }
    let result = connect::compute_full_rank(&mut session, country, quick, limit, free).await?;
    if !result.measured && !quick {
        tracing::warn!("latency looked like a local middlebox; ranked by distance and load instead");
    }
    print_rank(&result, json)?;

    if do_connect {
        return Ok(report(connect::up(&mut session, None).await));
    }
    Ok(0)
}

fn print_rank(result: &pipeline::RankResult, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", display::render_json(&result.candidates, result.origin)?);
    } else {
        println!("{}", display::render_table(&result.candidates, result.origin));
    }
    Ok(())
}
