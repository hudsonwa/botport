//! botport-grok: the engine behind `botport grok …` (bots, ask, send, wait, read, stop, watch, status, launch,
//! restore, policy) and its stdio MCP mirror.
//!
//! Layout (frozen at the end of P1, see docs/GROK-ENGINE-INTERFACES.md in the botport repo):
//! - [`model`]: the JSON shapes of the spec (Bot, Message, AskResult, ...);
//! - [`parse`]: the version-keyed parsers that turn the app's raw props into the model (shared by every transport);
//! - [`transport`]: the `Transport` trait, the debug-port transport and the fixture-replaying fake;
//! - [`policy`]: presets and the hard floors;
//! - [`engine`]: command logic, settle loop, PASS lines;
//! - [`canary`]: the P5 read-only canary and its local reports (unlocks writes on an untested version);
//! - [`lifecycle`]: launch / restore / port file / TTL guard;
//! - [`mcp`]: stdio MCP over the engine; [`cli`]: argument parsing and output.
pub mod canary;
pub mod cli;
pub mod clock;
pub mod engine;
pub mod files;
pub mod lifecycle;
pub mod log;
pub mod mcp;
pub mod model;
pub mod parse;
pub mod policy;
pub mod routines;
pub mod transport;
pub mod util;
pub mod version;

pub use engine::{Engine, ExitCode, Outcome, Proof};
pub use policy::{Policy, Preset};
pub use transport::{Transport, TransportError};
