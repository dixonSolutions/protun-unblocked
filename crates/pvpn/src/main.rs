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
mod verify;

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
    /// Servers that have actually carried traffic on this network
    #[command(alias = "proven")]
    Working,
    /// Servers that failed a real connect on this network
    Blocked,
    /// Everything this network has taught us, in one table
    #[command(after_help = "\
pvpn servers — what this network has taught us

  pvpn servers              every server we have observed here
  pvpn servers --limit 0    all of them, not just the top 25
  pvpn servers --all        every network, not just this one
  pvpn servers --json       machine-readable output

STATUS is earned two different ways. `fast` is a TLS handshake time from a
measurement pass — cheap, and never proof that a server works. `working`
means a real connect carried real traffic here. `blocked` means one did not,
and says when it will be tried again.
")]
    Servers {
        /// Every network, not just this one
        #[arg(long, short = 'a')]
        all: bool,
        /// How many to show per network (0 for all of them)
        #[arg(long, default_value_t = 25)]
        limit: usize,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Connect attempts made on this network, newest first
    #[command(
        alias = "hist",
        after_help = "\
pvpn history — what happened, and when

  pvpn history              the last 20 attempts on this network
  pvpn history -n 100       more of them
  pvpn history --all        every network, tagged with which
  pvpn history --json       machine-readable output

Each line is one connect attempt: which server, how long until the outcome
was known, and what the outcome was. `link-down` and `cert-expired` are the
two nobody is blamed for — an evening of those is why nothing worked, and
no server was written off for it.
"
    )]
    History {
        /// How many attempts to show
        #[arg(long, short = 'n', default_value_t = 20)]
        lines: u32,
        /// Every network, tagged with which
        #[arg(long, short = 'a')]
        all: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Lift a block so the server is tried again
    #[command(after_help = "\
pvpn forget — lift a block by hand

  pvpn forget SG-FREE#13    try this one again
  pvpn forget --all         try everything on this network again

Blocks expire on their own — 24h, longer for a server that keeps failing.
This is for when you know something the record does not: the network
changed, the school lifted a filter, you moved room.
")]
    Forget {
        /// Every blocked server on this network
        #[arg(long)]
        all: bool,
        server: Option<String>,
    },
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
  pvpn servers         everything this network has taught us
  pvpn history         connect attempts here, newest first
  pvpn fast            servers this network measured as fast
  pvpn working         servers that actually carried traffic here
  pvpn blocked         servers that failed a real connect here
  pvpn forget [server] lift a block so it is tried again
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
  down the list if one fails. `pvpn hop` with nothing named does the same
  from the server you are on, trying servers proven to work here before
  ones that merely measured quickly.

What this network taught us:
  pvpn servers            every server, its status and its record here
  pvpn history            every connect attempt, and how long it took
  pvpn forget --all       clear the blocks and start over
  These are per-network and maintained by every connect you run — nothing
  is measured in the background, so what is written down is what the
  commands you typed already paid for.

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
            Ok(report(
                run_interruptible(connect::up(&mut session, protocol)).await,
            ))
        }),
        Command::Down => block_on(async move { Ok(report(connect::down().await)) }),
        Command::Hop { pattern } => block_on(async move {
            let mut session = Session::load()?;
            Ok(report(
                run_interruptible(connect::hop(&mut session, pattern)).await,
            ))
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
        Command::Working => block_on(cmd_working()),
        Command::Blocked => block_on(cmd_blocked()),
        Command::Servers { all, limit, json } => block_on(cmd_servers(all, limit, json)),
        Command::History { lines, all, json } => block_on(cmd_history(lines, all, json)),
        Command::Forget { all, server } => block_on(cmd_forget(all, server)),
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

/// A connect rewrites routing and DNS before it starts carrying traffic, so
/// Ctrl-C is a request to abort safely, not permission to leave immediately.
///
/// Polling the signal beside the operation installs Tokio's SIGINT handler
/// before any network work begins. Dropping the operation stops the failover
/// loop; `restore` then kills a still-running Proton child and puts the normal
/// connection back before this process exits.
async fn run_interruptible<F>(operation: F) -> connect::UpReport
where
    F: std::future::Future<Output = connect::UpReport>,
{
    tokio::select! {
        report = operation => report,
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("interrupted — disconnecting and restoring normal internet");
            let restored = connect::restore().await;
            connect::UpReport {
                ok: false,
                message: if restored {
                    "Cancelled. Internet restored.".to_string()
                } else {
                    "Cancelled, but the network is still down. Run `pvpn fix`; \
                     if only DNS fails, restart your local DNS service."
                        .to_string()
                },
                server: None,
            }
        }
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

/// "3h ago", "just now" — a duration a person can read without arithmetic.
///
/// Every one of these tables is answering "is this still true?", and an
/// RFC-3339 timestamp makes the reader do the subtraction themselves.
fn ago(then: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    humanise(now - then, "ago")
}

/// The same, forwards: "in 19h".
fn until(then: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let delta = then - now;
    if delta <= chrono::Duration::zero() {
        return "any moment".to_string();
    }
    format!("in {}", humanise(delta, ""))
}

fn humanise(delta: chrono::Duration, suffix: &str) -> String {
    let minutes = delta.num_minutes();
    let text = if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 48 {
        format!("{}h", delta.num_hours())
    } else {
        format!("{}d", delta.num_days())
    };
    match (text.as_str(), suffix) {
        ("just now", _) => text,
        (_, "") => text,
        _ => format!("{text} {suffix}"),
    }
}

fn latency(stat: &pvpn_core::state::ServerStat) -> String {
    stat.ema_latency_ms
        .map(|v| format!("{v:.0}ms"))
        .unwrap_or_else(|| "-".to_string())
}

fn ready_time(stat: &pvpn_core::state::ServerStat) -> String {
    stat.ema_ready_ms
        .map(|milliseconds| format!("{:.2}s", milliseconds / 1000.0))
        .unwrap_or_else(|| "-".to_string())
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
    let now = chrono::Utc::now();
    println!(
        "Fast servers (TLS handshake and verified readiness, measured on {}):",
        session.network()
    );
    for (name, stat) in servers {
        // A handshake time is not a verdict — it is how quickly *something*
        // answered, and on a network with a transparent proxy that
        // something is the proxy. Say plainly which of these have since
        // been proven, so the number is not read as more than it is.
        let proven = match (stat.connect_successes, stat.last_connect_ok) {
            (0, _) => "  (never carried traffic here)".to_string(),
            (_, Some(at)) => format!("  worked {}", ago(at, now)),
            (_, None) => "  has worked here".to_string(),
        };
        println!(
            "  {name:<16} TLS {:>7}  READY {:>7}{proven}",
            latency(&stat),
            ready_time(&stat)
        );
    }
    println!();
    println!("`pvpn working` lists only the ones that carried real traffic.");
    Ok(0)
}

/// Servers that have actually carried traffic here — the list `pvpn hop`
/// reaches for first, and the one worth trusting.
async fn cmd_working() -> anyhow::Result<i32> {
    let session = Session::load()?;
    let servers = session.state.working_list();
    if servers.is_empty() {
        println!(
            "No server has carried traffic on {} yet.",
            session.network()
        );
        println!("`pvpn up` fills this in the first time one does.");
        return Ok(0);
    }
    let now = chrono::Utc::now();
    println!(
        "Servers that carried traffic on {} (most recently proven first):",
        session.network()
    );
    for (name, stat) in servers {
        let last = stat
            .last_connect_ok
            .map(|at| ago(at, now))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {name:<16} TLS {:>7}  READY {:>7}  {}/{} connects worked, last {last}",
            latency(&stat),
            ready_time(&stat),
            stat.connect_successes,
            stat.connect_attempts
        );
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
    let now = chrono::Utc::now();
    let retry_after = session.config.blocked_retry_after();
    for (name, stat) in servers {
        let reason = stat.blocked_reason.as_deref().unwrap_or("unknown");
        // The count is what decides how long the block is held, so it
        // belongs on screen next to the reason — and so does the horizon.
        // "Blocked" with no end date reads as permanent, and none of these
        // are: networks and server pools change, which is the whole reason
        // blocks expire at all.
        let repeats = match stat.consecutive_connect_failures {
            0 | 1 => String::new(),
            n => format!(", {n} in a row"),
        };
        let retry = session
            .state
            .block_expires_at(&name, retry_after)
            .map(|at| until(at, now))
            .unwrap_or_else(|| "on the next run".to_string());
        println!("  {name:<16} {reason}{repeats} — retried {retry}");
    }
    println!();
    println!("Nothing here is permanent. `pvpn forget <server>` tries one again now.");
    Ok(0)
}

/// One table for everything this network has taught us.
async fn cmd_servers(all: bool, limit: usize, json: bool) -> anyhow::Result<i32> {
    let mut session = Session::load()?;
    let now = chrono::Utc::now();
    let retry_after = session.config.blocked_retry_after();
    let networks = if all {
        session.state.known_networks()
    } else {
        vec![session.network().to_string()]
    };

    if json {
        let mut out = serde_json::Map::new();
        for net in &networks {
            session.state.set_network(net.clone());
            out.insert(net.clone(), serde_json::to_value(session.state.servers())?);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    let mut printed = false;
    for net in &networks {
        session.state.set_network(net.clone());
        let mut servers: Vec<_> = session
            .state
            .servers()
            .iter()
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect();
        if servers.is_empty() {
            continue;
        }
        // Proven first, then blocked last, then by latency: the reading
        // order is "what can I use", not the alphabet.
        servers.sort_by(|a, b| {
            let rank = |s: &pvpn_core::state::ServerStat| match s.status {
                pvpn_core::state::ServerStatus::Blocked => 2,
                _ if s.connect_successes > 0 => 0,
                _ => 1,
            };
            rank(&a.1).cmp(&rank(&b.1)).then(
                a.1.ema_latency_ms
                    .unwrap_or(f64::MAX)
                    .partial_cmp(&b.1.ema_latency_ms.unwrap_or(f64::MAX))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });

        // Anything with a record is worth the line whatever the limit
        // says; the tail is servers we have only ever timed, and a
        // hundred of those push the ones that matter off the screen.
        let has_record = |s: &pvpn_core::state::ServerStat| {
            s.connect_attempts > 0 || s.status == pvpn_core::state::ServerStatus::Blocked
        };
        let kept = if limit == 0 {
            servers.len()
        } else {
            let interesting = servers.iter().filter(|(_, s)| has_record(s)).count();
            limit.max(interesting).min(servers.len())
        };
        let hidden = servers.len() - kept;
        if hidden > 0 {
            servers = servers
                .into_iter()
                .enumerate()
                .filter(|(i, (_, s))| *i < kept || has_record(s))
                .map(|(_, row)| row)
                .collect();
        }

        if printed {
            println!();
        }
        printed = true;
        println!("{net}");
        println!("  SERVER           STATUS    TLS      READY    CONNECTS NOTE");
        for (name, stat) in servers {
            let status = match stat.status {
                pvpn_core::state::ServerStatus::Blocked => "blocked",
                _ if stat.connect_successes > 0 => "working",
                pvpn_core::state::ServerStatus::Fast => "fast",
                pvpn_core::state::ServerStatus::Known => "known",
            };
            let connects = if stat.connect_attempts == 0 {
                "-".to_string()
            } else {
                format!("{}/{}", stat.connect_successes, stat.connect_attempts)
            };
            let note = if stat.status == pvpn_core::state::ServerStatus::Blocked {
                let retry = session
                    .state
                    .block_expires_at(&name, retry_after)
                    .map(|at| until(at, now))
                    .unwrap_or_else(|| "on the next run".to_string());
                format!(
                    "{} — retried {retry}",
                    stat.blocked_reason.as_deref().unwrap_or("unknown")
                )
            } else if let Some(at) = stat.last_connect_ok {
                format!("worked {}", ago(at, now))
            } else if let Some(at) = stat.last_tried {
                format!("tried {}", ago(at, now))
            } else {
                "measured only".to_string()
            };
            println!(
                "  {name:<16} {status:<9} {:>7}  {:>7}  {connects:<7} {note}",
                latency(&stat),
                ready_time(&stat)
            );
        }
        if hidden > 0 {
            println!("  … and {hidden} more measured but never tried (--limit 0 for all)");
        }
    }

    if !printed {
        println!(
            "Nothing observed on {} yet. `pvpn best` measures, `pvpn up` proves.",
            session.network()
        );
    }
    Ok(0)
}

/// What happened, and when. The copy of the narration that outlives the
/// terminal it was printed to.
async fn cmd_history(lines: u32, all: bool, json: bool) -> anyhow::Result<i32> {
    let session = Session::load()?;
    let entries: Vec<(String, pvpn_core::state::Event)> = if all {
        session.state.all_events()
    } else {
        session
            .state
            .events()
            .into_iter()
            .map(|e| (session.network().to_string(), e))
            .collect()
    };
    let entries: Vec<_> = entries.into_iter().take(lines as usize).collect();

    if json {
        let rendered: Vec<serde_json::Value> = entries
            .iter()
            .map(|(net, e)| {
                let mut v = serde_json::to_value(e).unwrap_or(serde_json::Value::Null);
                if let Some(map) = v.as_object_mut() {
                    map.insert("network".into(), serde_json::Value::String(net.clone()));
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rendered)?);
        return Ok(0);
    }

    if entries.is_empty() {
        println!("No connect attempts recorded on {} yet.", session.network());
        println!("Every `pvpn up` and `pvpn hop` writes one.");
        return Ok(0);
    }

    if !all {
        println!("Connect attempts on {} (newest first):", session.network());
    }
    for (net, e) in &entries {
        let when = e.at.with_timezone(&chrono::Local).format("%b %d %H:%M");
        let took = e
            .ready_ms
            .map(|milliseconds| format!("{:.2}s", milliseconds as f64 / 1000.0))
            .or_else(|| e.seconds.map(|seconds| format!("{seconds}s")))
            .unwrap_or_else(|| "-".to_string());
        let where_ = if all {
            format!("  [{net}]")
        } else {
            String::new()
        };
        println!(
            "  {when}  {:<16} {:<14} {:>5}{where_}",
            e.server, e.outcome, took
        );
        if let Some(detail) = &e.detail {
            println!("      {detail}");
        }
    }
    println!();
    println!(
        "  {} attempts shown, {} of them carried traffic.",
        entries.len(),
        entries.iter().filter(|(_, e)| e.outcome == "ok").count()
    );
    // The distinction the rest of this tool is built around, said once
    // where someone reading a bad evening will see it.
    let unblamed = entries
        .iter()
        .filter(|(_, e)| e.outcome == "link-down" || e.outcome == "cert-expired")
        .count();
    if unblamed > 0 {
        println!(
            "  {unblamed} were our own network or certificate — no server was written off for those."
        );
    }
    Ok(0)
}

/// Lift a block by hand.
async fn cmd_forget(all: bool, server: Option<String>) -> anyhow::Result<i32> {
    let mut session = Session::load()?;
    if all {
        let n = session.state.unblock_all();
        session.save();
        println!("Lifted {n} block(s) on {}.", session.network());
        return Ok(0);
    }
    let Some(name) = server else {
        eprintln!("Name a server, or use --all.");
        eprintln!("  pvpn forget SG-FREE#13");
        eprintln!("  pvpn forget --all");
        eprintln!();
        eprintln!("`pvpn blocked` lists what is currently held here.");
        return Ok(1);
    };
    if session.state.unblock(&name) {
        session.save();
        println!("{name} will be tried again on {}.", session.network());
        Ok(0)
    } else {
        eprintln!("{name} is not blocked on {}.", session.network());
        Ok(1)
    }
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
            return Ok(report(
                run_interruptible(connect::up(&mut session, None)).await,
            ));
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
        tracing::warn!(
            "latency looked like a local middlebox; ranked by distance and load instead"
        );
    }
    print_rank(&result, json)?;

    if do_connect {
        return Ok(report(
            run_interruptible(connect::up(&mut session, None)).await,
        ));
    }
    Ok(0)
}

fn print_rank(result: &pipeline::RankResult, json: bool) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            display::render_json(&result.candidates, result.origin)?
        );
    } else {
        println!(
            "{}",
            display::render_table(&result.candidates, result.origin)
        );
    }
    Ok(())
}
