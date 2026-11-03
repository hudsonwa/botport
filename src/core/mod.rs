//! Platform-free core: the neutral bot model, the `Adapter` trait and shared helpers. No platform names here.
pub mod adapter;
#[cfg(test)]
pub mod conformance;
pub mod model;
pub mod package;
pub mod secrets;
pub mod util;
pub mod yaml11;
