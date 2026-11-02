//! The hub-and-spoke seam: every platform adapter reads its native layout into a `BotModel` and writes one back.
#![allow(dead_code)] // library seam; CLI commands still call the moved modules directly
use super::model::BotModel;
use super::util::Res;
use std::path::Path;

pub trait Adapter {
    /// Stable adapter id; also the adapter's key in `BotModel::extras`.
    fn id(&self) -> &'static str;
    /// Read the platform's native layout at `src` into the neutral model.
    fn read(&self, src: &Path) -> Res<BotModel>;
    /// Write the model into the platform's native layout under `dest`; returns the files written (relative).
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>>;
}
