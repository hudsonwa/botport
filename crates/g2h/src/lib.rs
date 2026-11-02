//! g2h core library: paths, message envelope, safety guard, framing,
//! secret redaction, logging, persistent state, IPC types and config.
//! Networking is confined to `hermes` (the peer backend) and `wake` (the
//! outbound wake webhook); `daemon` listens only on a Unix socket.

pub mod cli;
pub mod client;
pub mod config;
pub mod daemon;
pub mod envelope;
pub mod framing;
pub mod guard;
pub mod hermes;
pub mod ipc;
pub mod log;
pub mod ops;
pub mod paths;
pub mod secrets;
pub mod setup;
pub mod store;
pub mod wake;
