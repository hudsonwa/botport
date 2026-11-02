//! Runs the shared adapter conformance suite (src/core/conformance.rs) on every adapter.
use crate::adapters::{grok_bot::GrokBot, hermes::HermesProfile};
use crate::core::adapter::Adapter;
use crate::core::conformance as c;
use crate::core::model::BotModel;
use crate::core::util::Res;
use std::path::{Path, PathBuf};

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
        round_trips_schedules,
        round_trips_connectors,
        round_trips_credential_names,
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
        round_trips_schedules,
        round_trips_connectors,
        round_trips_credential_names,
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

#[test]
fn hermes_passes_full_check() {
    c::check(&HermesProfile, &fixture("hermes"));
}

/// The Hermes adapter with one part of the model dropped before writing: the suite must catch each loss.
struct Dropping(fn(&mut BotModel));

impl Adapter for Dropping {
    fn id(&self) -> &'static str {
        "dropping"
    }
    fn read(&self, src: &Path) -> Res<BotModel> {
        HermesProfile.read(src)
    }
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>> {
        let mut m = m.clone();
        (self.0)(&mut m);
        HermesProfile.write(&m, dest)
    }
}

#[test]
#[should_panic(expected = "dropped")]
fn an_adapter_that_drops_schedules_fails() {
    c::round_trips_schedules(&Dropping(|m| m.schedules.clear()), &fixture("hermes"));
}

#[test]
#[should_panic(expected = "dropped")]
fn an_adapter_that_drops_unscheduled_items_fails() {
    c::round_trips_schedules(&Dropping(|m| m.manual.clear()), &fixture("hermes"));
}

#[test]
#[should_panic(expected = "dropped")]
fn an_adapter_that_drops_connectors_fails() {
    // the profile's config.yaml (kept whole in `extras`) also holds the server, so drop both
    let drop = Dropping(|m| {
        m.connectors.clear();
        m.extras.clear();
    });
    c::round_trips_connectors(&drop, &fixture("hermes"));
}

#[test]
#[should_panic(expected = "dropped")]
fn an_adapter_that_drops_credential_names_fails() {
    c::round_trips_credential_names(&Dropping(|m| m.credentials.clear()), &fixture("hermes"));
}

/// The Grok adapter with one part of the model dropped before writing.
struct GrokDropping(fn(&mut BotModel));

impl Adapter for GrokDropping {
    fn id(&self) -> &'static str {
        "grok-dropping"
    }
    fn read(&self, src: &Path) -> Res<BotModel> {
        GrokBot.read(src)
    }
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>> {
        let mut m = m.clone();
        (self.0)(&mut m);
        GrokBot.write(&m, dest)
    }
}

#[test]
#[should_panic(expected = "dropped")]
fn the_grok_adapter_dropping_schedules_fails_too() {
    c::round_trips_schedules(&GrokDropping(|m| m.schedules.clear()), &fixture("grok-bot"));
}

#[test]
#[should_panic(expected = "dropped")]
fn the_grok_adapter_dropping_connectors_fails_too() {
    c::round_trips_connectors(
        &GrokDropping(|m| m.connectors.clear()),
        &fixture("grok-bot"),
    );
}

#[test]
#[should_panic(expected = "dropped")]
fn the_grok_adapter_dropping_credential_names_fails_too() {
    let drop = GrokDropping(|m| m.credentials.clear());
    c::round_trips_credential_names(&drop, &fixture("grok-bot"));
}

#[test]
fn adapter_write_refuses_unsafe_skill_names_and_paths() {
    let adapters: [(&dyn Adapter, &str); 2] = [(&GrokBot, "grok-bot"), (&HermesProfile, "hermes")];
    for (a, id) in adapters {
        let base =
            std::env::temp_dir().join(format!("botport-unsafe-write-{id}-{}", std::process::id()));
        let good = a.read(&fixture(id)).unwrap();
        let mut by_name = good.clone();
        let mut s = good.skills[0].clone();
        s.name = "../../evil".into();
        by_name.skills.push(s);
        let mut by_rel = good.clone();
        by_rel.skills[0]
            .files
            .insert("../../../escaped.md".into(), "x\n".into());
        for m in [by_name, by_rel] {
            let d = base.join("out/dest");
            assert!(a.write(&m, &d).is_err(), "{id}");
            assert!(
                !base.join("evil").exists() && !base.join("escaped.md").exists(),
                "{id}"
            );
        }
        std::fs::remove_dir_all(&base).ok();
    }
}
