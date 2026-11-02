//! botport as a library: the neutral bot model and the adapter seam (`core`), the platform adapters around it
//! (`adapters`), and the helpers the CLI (src/main.rs) is built from. A new platform implements
//! [`core::adapter::Adapter`] (and [`core::adapter::Target`] if it can sync a live install); see docs/ADAPTERS.md.
//!
//! The model is [`core::model::BotModel`] (`bot-model/1`): spec in docs/MODEL.md, JSON Schema in
//! docs/schema/botmodel-v1.json. Not published to crates.io; depend on it by git path.
//!
//! Read a Grok Bot export bundle into the model and print it as canonical JSON:
//!
//! ```no_run
//! use botport::adapters::grok_bot::GrokBot;
//! use botport::core::adapter::Adapter;
//! use std::path::Path;
//!
//! let model = GrokBot.read(Path::new("my-bot-bundle")).expect("readable bundle");
//! println!("{} has {} skills", model.persona.name, model.skills.len());
//! print!("{}", model.to_json());
//! ```
#[cfg(test)]
mod adapter_tests;
pub mod adapters;
pub mod battery;
#[cfg(test)]
mod conformance_tests;
pub mod core;
pub mod doctor;
