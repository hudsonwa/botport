//! Runs the shared adapter conformance suite (src/core/conformance.rs) on every adapter.
use crate::adapters::{grok_bot::GrokBot, hermes::HermesProfile};
use crate::core::conformance as c;
use std::path::PathBuf;

fn fixture(id: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/conformance")
        .join(id)
}

macro_rules! suite {
    ($m:ident, $a:expr, $id:literal, [$($check:ident $(#[$attr:meta])?),*]) => {
        mod $m {
            use super::*;
            $( $(#[$attr])? #[test] fn $check() { c::$check(&$a, &fixture($id)); } )*
        }
    };
}

suite!(
    grok_bot,
    GrokBot,
    "grok-bot",
    [
        read_is_deterministic,
        round_trips_portable_fields,
        never_leaks_credentials,
        has_no_absolute_paths,
        keeps_unknown_data_in_extras,
        has_schema_version
    ]
);
suite!(
    hermes,
    HermesProfile,
    "hermes",
    [
        read_is_deterministic,
        round_trips_portable_fields,
        never_leaks_credentials,
        has_no_absolute_paths,
        keeps_unknown_data_in_extras,
        has_schema_version
    ]
);

#[test]
fn grok_bot_passes_full_check() {
    c::check(&GrokBot, &fixture("grok-bot"));
}
