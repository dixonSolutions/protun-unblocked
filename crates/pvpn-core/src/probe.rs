//! Time a TLS handshake to a server's entry IP. Port of the "probing"
//! section of `lib/best-server.py`.
//!
//! The handshake is TLS, not just TCP, because a transparent proxy answers
//! the TCP handshake locally — every server on earth "replies" in about
//! 2 ms behind one of those, which is how a Sydney client got handed
//! Amsterdam over Singapore. A TLS handshake has to reach the real server.
//! See `docs/best-server.md` and `docs/transparent-proxy.md`.

use crate::rank::Candidate;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// Proton servers terminate Stealth, OpenVPN-TCP and WireGuard-TLS on 443.
pub const PROBE_PORT: u16 = 443;
/// A middlebox that inspects SNI will drop a ClientHello with none.
pub const PROBE_SNI: &str = "www.google.com";

pub const DEFAULT_PROBE_ROUNDS: usize = 2;
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_SHORTLIST: usize = 40;
pub const DEFAULT_REFINE: usize = 8;

/// A wide burst was observed collapsing every server onto the same ~200 ms
/// figure — probes compete with each other for the same uplink. So: a wide
/// sweep to find what answers, then a gentle re-time of the finalists.
pub const SWEEP_CONCURRENCY: usize = 16;
pub const REFINE_CONCURRENCY: usize = 4;

/// Reserved for documentation (RFC 5737), so nothing is behind them
/// anywhere. A TCP connection that succeeds proves a local box answers for
/// them.
pub const TEST_NET_ADDRESSES: [&str; 3] = ["198.51.100.77", "203.0.113.9", "192.0.2.55"];

/// Time a single handshake, in milliseconds. `None` if unreachable.
///
/// With `tls` the timing covers the full TLS handshake, which is what
/// makes the number reflect the real path rather than whatever answered
/// the SYN. `tls=false` times the TCP handshake alone.
pub async fn probe_once(host: &str, port: u16, timeout: Duration, tls: bool) -> Option<f64> {
    let started = Instant::now();
    let addr = format!("{host}:{port}");

    let result = tokio::time::timeout(timeout, async {
        let tcp = TcpStream::connect(&addr).await.ok()?;
        if !tls {
            return Some(());
        }
        // A context that completes a handshake with anything and verifies
        // nothing. We are timing the path, not authenticating the peer —
        // the tunnel does that itself later — and Proton entry servers
        // present a generic certificate that would fail hostname checks
        // anyway.
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .ok()?;
        let connector = tokio_native_tls::TlsConnector::from(connector);
        connector.connect(PROBE_SNI, tcp).await.ok()?;
        Some(())
    })
    .await;

    match result {
        Ok(Some(())) => Some(started.elapsed().as_secs_f64() * 1000.0),
        _ => None,
    }
}

/// Probe one server several times and keep the best result.
///
/// The minimum, not the mean: a slow sample means something queued
/// somewhere, while the fastest handshake is the closest thing we have to
/// the true path latency. Best across all passes, not just this one:
/// `measure()` probes twice (a wide sweep, then a refine on the leaders),
/// and letting a slower refine sample overwrite unconditionally buried a
/// good sweep result — or, worse, a refine that timed out entirely reset
/// this to `None`, marking a server we had already reached as unreachable.
async fn probe_candidate(
    candidate: &mut Candidate,
    port: u16,
    rounds: usize,
    timeout: Duration,
    semaphore: &Semaphore,
    tls: bool,
) {
    let _permit = semaphore.acquire().await.expect("semaphore not closed");
    let mut best: Option<f64> = None;
    for _ in 0..rounds {
        if let Some(elapsed) = probe_once(&candidate.entry_ip, port, timeout, tls).await {
            best = Some(match best {
                Some(current) => current.min(elapsed),
                None => elapsed,
            });
        }
    }
    if let Some(best) = best {
        if candidate.latency_ms.is_none() || best < candidate.latency_ms.unwrap() {
            candidate.latency_ms = Some(best);
        }
    }
}

/// Measure every candidate concurrently, in place.
pub async fn probe_all(
    candidates: &mut [Candidate],
    port: u16,
    rounds: usize,
    timeout: Duration,
    concurrency: usize,
    tls: bool,
) {
    let semaphore = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut tasks = Vec::new();
    // Probe concurrently via owned clones, then write results back — avoids
    // holding multiple mutable borrows of `candidates` across an await.
    for candidate in candidates.iter() {
        let mut owned = candidate.clone();
        let semaphore = semaphore.clone();
        tasks.push(tokio::spawn(async move {
            probe_candidate(&mut owned, port, rounds, timeout, &semaphore, tls).await;
            owned
        }));
    }
    for (slot, task) in candidates.iter_mut().zip(tasks) {
        if let Ok(result) = task.await {
            *slot = result;
        }
    }
}

/// Sweep every candidate, then re-time the finalists without contention.
/// Returns all candidates, ranked best first.
pub async fn measure(
    candidates: &[Candidate],
    port: u16,
    rounds: usize,
    timeout: Duration,
    refine: usize,
    tls: bool,
) -> Vec<Candidate> {
    let mut working: Vec<Candidate> = candidates.to_vec();
    // `rounds`, not 1. The sweep used to take a single sample per server
    // and silently drop the caller's setting, which mattered because the
    // sweep is also the cut: only its top `refine` get re-timed. One
    // unlucky sample — and same-city servers vary by 2.8x on a filtered
    // link — dropped a real contender below the cut, where nothing ever
    // looked at it again. Taking the best of `rounds` costs a second or
    // two and stops the cut being a lottery.
    probe_all(
        &mut working,
        port,
        rounds.max(1),
        timeout,
        SWEEP_CONCURRENCY,
        tls,
    )
    .await;
    let mut ranked = crate::rank::rank(&working);

    let finalist_names: Vec<String> = ranked
        .iter()
        .filter(|c| c.reachable())
        .take(refine)
        .map(|c| c.name.clone())
        .collect();

    if !finalist_names.is_empty() && rounds > 0 {
        let mut finalists: Vec<Candidate> = ranked
            .iter()
            .filter(|c| finalist_names.contains(&c.name))
            .cloned()
            .collect();
        probe_all(
            &mut finalists,
            port,
            rounds,
            timeout,
            REFINE_CONCURRENCY,
            tls,
        )
        .await;
        for finalist in finalists {
            if let Some(slot) = ranked.iter_mut().find(|c| c.name == finalist.name) {
                // Keep the best of sweep vs refine (probe_candidate already
                // does this per-call; here we're merging the refined value
                // for a name that appears once in `ranked`).
                slot.latency_ms = match (slot.latency_ms, finalist.latency_ms) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, None) => a,
                    (None, b) => b,
                };
            }
        }
        ranked = crate::rank::rank(&ranked);
    }

    ranked
}

/// Is something answering TCP/443 for addresses that do not exist?
pub async fn transparent_proxy_present(timeout: Duration) -> bool {
    for address in TEST_NET_ADDRESSES {
        if probe_once(address, PROBE_PORT, timeout, false)
            .await
            .is_some()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn start_closing_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[tokio::test]
    async fn reports_a_time_for_a_listening_port() {
        let (listener, port) = start_closing_listener().await;
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let elapsed = probe_once("127.0.0.1", port, Duration::from_secs(2), false).await;
        assert!(elapsed.is_some());
        assert!(elapsed.unwrap() >= 0.0);
    }

    #[tokio::test]
    async fn returns_none_for_a_closed_port() {
        // Bind and immediately drop to free the port, giving us one nothing
        // is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(probe_once("127.0.0.1", port, Duration::from_secs(1), false)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn returns_none_for_an_unroutable_address() {
        // 203.0.113.0/24 is TEST-NET-3: reserved for documentation, never
        // routed, so this exercises the timeout path without real traffic.
        // Not port 443 — see docs/transparent-proxy.md.
        assert!(
            probe_once("203.0.113.1", 47001, Duration::from_millis(200), false)
                .await
                .is_none()
        );
    }

    fn candidate(name: &str, entry_ip: &str) -> Candidate {
        Candidate {
            name: name.to_string(),
            entry_ip: entry_ip.to_string(),
            country: "XX".to_string(),
            city: "Nowhere".to_string(),
            tier: 0,
            load: 50,
            proton_score: 1.0,
            distance_km: None,
            latency_ms: None,
            rating: None,
            carry: None,
        }
    }

    #[tokio::test]
    async fn measure_marks_reachable_and_unreachable() {
        let (listener, port) = start_closing_listener().await;
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => drop(stream),
                    Err(_) => break,
                }
            }
        });
        let mut up = candidate("up", "127.0.0.1");
        up.entry_ip = "127.0.0.1".to_string();
        let down = candidate("down", "203.0.113.1");

        let mut working = vec![up, down];
        probe_candidate(
            &mut working[0],
            port,
            1,
            Duration::from_millis(300),
            &Semaphore::new(4),
            false,
        )
        .await;
        probe_candidate(
            &mut working[1],
            47001,
            1,
            Duration::from_millis(300),
            &Semaphore::new(4),
            false,
        )
        .await;
        assert!(working[0].reachable());
        assert!(!working[1].reachable());
    }

    #[tokio::test]
    async fn tls_probe_fails_against_a_plain_tcp_server() {
        let (listener, port) = start_closing_listener().await;
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        assert!(probe_once("127.0.0.1", port, Duration::from_secs(2), true)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_probe_never_worsens_a_measurement() {
        let mut target = candidate("target", "203.0.113.1");
        target.latency_ms = Some(99.0);
        let semaphore = Semaphore::new(1);

        // A failed pass (unroutable, tiny timeout) must not erase a good one.
        probe_candidate(
            &mut target,
            47001,
            1,
            Duration::from_millis(50),
            &semaphore,
            false,
        )
        .await;
        assert_eq!(
            target.latency_ms,
            Some(99.0),
            "a failed pass erased a good one"
        );
    }
}
