//! Platform-free core: the neutral bot model, the `Adapter` trait and shared helpers. No platform names here.
pub mod adapter;
pub mod auth;
#[cfg(test)]
pub mod conformance;
pub mod digest;
pub mod item;
pub mod local_records;
pub mod metrics;
pub mod mirror;
pub mod model;
pub mod package;
pub mod routines;
pub mod secrets;
pub mod util;
pub mod yaml11;
