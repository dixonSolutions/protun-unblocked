//! Unix-socket server. One request, then zero or more `Progress` frames,
//! then exactly one terminal response, then the connection closes.
//! `Up`/`Down`/`Hop` take the connect lock; everything else is a cheap
//! read.

use crate::app::App;
use crate::connect;
use pvpn_core::config::Config;
use pvpn_core::display;
use pvpn_core::ipc::{self, Request, Response};
use pvpn_core::net;
use std::sync::atomic::Ordering;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

pub async fn run(app: App, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle(app, stream).await {
                        tracing::debug!("rpc client: {err}");
                    }
                });
            }
            Err(err) => {
                tracing::warn!("accept: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle(app: App, mut stream: UnixStream) -> anyhow::Result<()> {
    let payload = ipc::read_async(&mut stream).await?;
    let request: Request = ipc::decode_payload(&payload)?;

    if takes_real_time(&request) {
        return narrate(app, stream, request).await;
    }

    let response = dispatch(&app, request).await;
    let frame = ipc::encode(&response)?;
    ipc::write_async(&mut stream, &frame).await?;
    Ok(())
}

/// Requests worth narrating: the ones that measure servers or move the
/// routing table, and so leave the caller waiting tens of seconds. The
/// rest answer immediately and would only be noise.
fn takes_real_time(request: &Request) -> bool {
    matches!(
        request,
        Request::Up { .. } | Request::Down | Request::Hop { .. } | Request::Best { .. }
    )
}

/// Run the request while forwarding the daemon's log lines to the client,
/// then send the real response.
async fn narrate(app: App, mut stream: UnixStream, request: Request) -> anyhow::Result<()> {
    // Subscribe *before* starting the work, or the first lines are lost.
    let mut events = app.logbus.subscribe();
    let worker = {
        let app = app.clone();
        tokio::spawn(async move { dispatch(&app, request).await })
    };
    tokio::pin!(worker);

    loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Ok(line) => {
                    let frame = ipc::encode(&Response::Progress {
                        level: line.level,
                        message: line.message,
                    })?;
                    // A client that hangs up mid-connect must not cancel
                    // the connect — it is already rewriting the routing
                    // table, and a half-applied one is what `pvpn fix`
                    // exists to clean up. Stop narrating, keep working.
                    if ipc::write_async(&mut stream, &frame).await.is_err() {
                        break;
                    }
                }
                // Narration fell behind: skipping a line is fine, the
                // journal still has it. Losing the response is not.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            result = &mut worker => {
                let response = result.unwrap_or_else(|err| {
                    Response::Err(format!("daemon task failed: {err}"))
                });
                let frame = ipc::encode(&response)?;
                ipc::write_async(&mut stream, &frame).await?;
                return Ok(());
            }
        }
    }

    // Nobody left to tell, but the work still has to finish cleanly.
    let _ = worker.await;
    Ok(())
}

async fn dispatch(app: &App, request: Request) -> Response {
    match request {
        Request::Ping => Response::Pong,
        Request::Status => Response::Status(app.snapshot_status().await),
        Request::Ip => match tokio::task::spawn_blocking(net::public_ip).await {
            Ok(Some(ip)) => Response::Ip(ip),
            _ => Response::Err("public IP unavailable".to_string()),
        },
        Request::GetConfig => Response::Config(app.config.read().await.clone()),
        Request::SetConfig(patch) => {
            {
                let mut cfg = app.config.write().await;
                cfg.apply_patch(&patch);
                if let Err(err) = cfg.save(&Config::default_path()) {
                    return Response::Err(format!("could not save config: {err}"));
                }
            }
            Response::Ok
        }
        Request::ListFast => {
            app.sync_network().await;
            let persist = app.persist.read().await;
            Response::Fast {
                network: persist.network().to_string(),
                servers: persist.fast_list(),
            }
        }
        Request::ListBlocked => {
            app.sync_network().await;
            let persist = app.persist.read().await;
            Response::Blocked {
                network: persist.network().to_string(),
                servers: persist.blocked_list(),
            }
        }
        Request::Best {
            limit,
            country,
            quick,
            free_only,
        } => best(app, limit, country, quick, free_only).await,
        Request::Up { protocol } => {
            let _guard = app.connect_lock.lock().await;
            finish_report(app, connect::do_up(app, protocol).await).await
        }
        Request::Down => {
            let _guard = app.connect_lock.lock().await;
            finish_report(app, connect::do_down(app).await).await
        }
        Request::Hop { pattern } => {
            let _guard = app.connect_lock.lock().await;
            finish_report(app, connect::do_hop(app, pattern).await).await
        }
    }
}

async fn finish_report(app: &App, report: connect::UpReport) -> Response {
    if !report.ok {
        return Response::Err(report.message);
    }
    let mut status = app.snapshot_status().await;
    status.message = Some(report.message);
    Response::Status(status)
}

async fn best(
    app: &App,
    limit: u32,
    country: Option<String>,
    quick: bool,
    free_only: bool,
) -> Response {
    let cfg = app.config.read().await.clone();
    if let Err(err) = connect::ensure_fresh_data(&cfg, true).await {
        tracing::warn!("best: {err}");
    }

    if !quick && app.busy.load(Ordering::SeqCst) {
        return Response::Err(
            "a connect is in progress — try `pvpn best --quick`, or wait".to_string(),
        );
    }

    if !quick {
        app.busy.store(true, Ordering::SeqCst);
    }
    let result = connect::compute_full_rank(app, country, quick, limit, free_only).await;
    if !quick {
        app.busy.store(false, Ordering::SeqCst);
    }

    match result {
        Ok(ranked) => {
            if !ranked.measured && !quick {
                tracing::warn!(
                    "latency looked like a local middlebox; ranked by distance and load instead"
                );
            }
            let _ = display::render_table(&ranked.candidates, ranked.origin);
            Response::Ranked(ranked.candidates)
        }
        Err(err) => Response::Err(err.to_string()),
    }
}
