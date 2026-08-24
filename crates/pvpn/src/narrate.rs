//! Print the running commentary that used to go to the daemon's journal.
//!
//! `pvpnd` explained itself through `tracing`, and `rpc` forwarded those
//! lines over the socket so `pvpn up` could show them while it waited.
//! With the work back in this process there is no socket and no journal,
//! but the explanation is still the most useful thing on screen during a
//! two-minute connect — so the same `tracing` events are formatted
//! straight to stderr here.
//!
//! stderr, not stdout, so `pvpn best --json` stays pipeable.

use std::fmt::Write as _;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Layer};

/// Install the stderr narrator. `RUST_LOG` still overrides, which is the
/// only way to see the debug lines.
pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("pvpn=info,pvpn_core=info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(Narrator)
        .try_init();
}

struct Narrator;

/// Pulls just the `message` field out of an event; the structured fields
/// were for the journal, and there is no longer a journal.
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

impl<S: Subscriber> Layer<S> for Narrator {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // A marker per severity, so a warning still reads as one without a
        // level prefix shouting at a person watching a connect.
        let mark = match *event.metadata().level() {
            Level::ERROR => "✗",
            Level::WARN => "!",
            Level::INFO => "·",
            _ => "  ",
        };
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        if visitor.0.is_empty() {
            return;
        }
        eprintln!("  {mark} {}", visitor.0);
    }
}
