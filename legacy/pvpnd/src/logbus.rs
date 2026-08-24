//! Fan the daemon's own log events out to whichever CLI is waiting.
//!
//! `pvpnd` narrates a connect in detail — which servers it measured, which
//! one it picked and why, that the network is filtered, that the tunnel is
//! up and traffic is being checked. All of it went to stderr, which under
//! systemd means the journal and nowhere else, so `pvpn up` printed
//! nothing at all for up to two minutes.
//!
//! Rather than thread a progress channel through every call site in
//! `connect`, this is a `tracing` layer: it copies each event onto a
//! broadcast channel, and `rpc` forwards from there to the client. Every
//! existing `tracing::info!` becomes visible to the user for free, and the
//! journal keeps getting the same lines it always did.

use std::fmt::Write as _;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// One formatted log event, ready to hand to a client.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: String,
    pub message: String,
}

/// Broadcast hub for daemon log events.
///
/// Subscribers are transient — usually exactly one, for as long as a
/// `pvpn up` is running — so a send with nobody listening is the normal
/// case and is deliberately not an error.
#[derive(Clone)]
pub struct LogBus {
    tx: broadcast::Sender<LogLine>,
}

impl LogBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }
}

/// Pulls just the `message` field out of an event; the structured fields
/// are for the journal, not for a person watching a connect.
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `format_args!` Debug-formats without adding quotes, which is
            // what `tracing::info!("...")` produces.
            let _ = write!(self.0, "{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for LogBus {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        if visitor.0.is_empty() {
            return;
        }
        // Err means nobody is watching. That is the common case.
        let _ = self.tx.send(LogLine {
            level: event.metadata().level().to_string(),
            message: visitor.0,
        });
    }
}
