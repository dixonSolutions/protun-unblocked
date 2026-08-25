//! The library behind `pvpn`.
//!
//! This crate is the Rust home for what used to be `lib/best-server.py`
//! (server modelling, geography, probing, ranking) plus the persisted
//! fast/blocked tracking and config. It has no opinion about *when* any of
//! this runs — that is the CLI's business, and the CLI only ever runs it
//! because someone typed a command.

pub mod apps;
pub mod cache;
pub mod config;
pub mod dbus;
pub mod display;
pub mod geo;
pub mod link;
pub mod net;
pub mod paths;
pub mod pipeline;
pub mod probe;
pub mod proc;
pub mod rank;
pub mod serverlist;
pub mod state;

pub use rank::Candidate;
