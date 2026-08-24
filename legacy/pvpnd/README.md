# pvpnd — removed

This was a long-running user daemon: a supervisor loop that polled
`protonvpn status` every five seconds and rebuilt the tunnel whenever it
decided one was missing, a background prober that maintained the fast
list, and a Unix-socket RPC server the `pvpn` CLI talked to.

It is not built and not installed. See
[docs/architecture.md](../../docs/architecture.md) for why it was removed
and where each piece of its behaviour went.

Kept here because the connect logic in `connect.rs` was expensive to learn
and the comments record *why* each rule exists — most of that file now
lives on as `crates/pvpn/src/connect.rs`.
