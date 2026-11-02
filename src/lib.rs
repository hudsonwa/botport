//! botport as a library: the neutral bot model and the adapter seam (`core`), the platform adapters around it
//! (`adapters`), and the helpers the CLI (src/main.rs) is built from. A new platform implements
//! `core::adapter::Adapter` (and `Target` if it can sync a live install); see docs/ADAPTERS.md.
#[cfg(test)]
mod adapter_tests;
pub mod adapters;
pub mod battery;
#[cfg(test)]
mod conformance_tests;
pub mod core;
pub mod doctor;

// Old module paths kept as aliases so moved code compiles unchanged (see docs/ARCHITECTURE.md).
use crate::core::{util, yaml11};
use adapters::grok_bot::{bundle, describe};
use adapters::hermes::{spec, sync as hermes};
