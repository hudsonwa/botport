//! Platform adapters (spokes) around the core model (hub). See docs/ARCHITECTURE.md.
#![allow(dead_code)] // library seam; CLI commands still call the moved modules directly
pub mod grok_bot;
pub mod hermes;
