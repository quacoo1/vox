//! # vox-bridge
//!
//! The localhost HTTP bridge for the browser extension (integrations.md §1, contracts.md §1.10). Wire-compatible with
//! the unchanged `browser-extension/`: same routes, same check order, same status codes, bodies and headers (test
//! vectors: integrations.md §1.10).
//!
//! | Module | Spec |
//! |---|---|
//! | [`pipeline`] | §1.4–§1.6: pure request checks, body parsing, responses (unit-tested without sockets) |
//! | [`server`] | §1.2, §1.7: listener lifecycle on `127.0.0.1:<port>` (socket2 + `SO_EXCLUSIVEADDRUSE`), its own accept loop and per-connection threads (httparse heads, header/body/keep-alive deadlines, 431/408/400 raw answers, 64-connection cap, closes that keep the port re-bindable at once; D-INT-16), serialized start/stop |
//! | `wire` (private) | §1.4 step 0, §1.7: request heads (Node's method list, 16 KiB cap, framing, keep-alive, `Expect`), chunked bodies, response bytes |
//! | `net` (private) | WinSock: exclusive bind, listen-error copy, event-driven non-blocking sockets with a shared stop event |
//!
//! The server never blocks the core: each connection runs on its own thread; `/read` waits ≤ 15 s for the core's
//! `ReadResult` (`BridgeHandler::read` returns `None` → 504 `Timed out waiting for the app`). `/read` bodies in tests are
//! compared as parsed JSON, not bytes (the `ReadResult` key order follows vox-core; clients ignore it). Owner: the
//! vox-bridge engineer.

#[cfg(windows)]
mod net;
#[cfg(unix)]
#[path = "net_unix.rs"]
mod net;
pub mod pipeline;
pub mod server;
mod wire;

#[cfg(test)]
mod socket_tests;

pub use server::{BridgeHandler, BridgeLimits, BridgeServer, BridgeStatus, StartResult};
