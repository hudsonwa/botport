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
