//! Conformance checks every `Adapter` must pass (see docs/ADAPTERS.md). Each check takes an adapter and a fixture
//! directory holding a native layout seeded with `FAKE_SECRET` (somewhere a real credential would live) and
//! `UNKNOWN_MARKER` (in data the model has no field for).
use super::adapter::Adapter;
use super::model::{check_schema, BotModel};
use std::path::{Path, PathBuf};

pub const FAKE_SECRET: &str = "CONFORMANCE-FAKE-SECRET-0000";
pub const UNKNOWN_MARKER: &str = "CONFORMANCE-UNKNOWN-MARKER";

fn scratch<A: Adapter>(a: &A, tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let d = std::env::temp_dir().join(format!("conformance-{}-{tag}-{pid}-{n}", a.id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn read<A: Adapter>(a: &A, fixture: &Path) -> BotModel {
    a.read(fixture)
        .unwrap_or_else(|e| panic!("{}: read failed: {e}", a.id()))
}

/// Two reads of the same input give byte-identical model JSON.
pub fn read_is_deterministic<A: Adapter>(a: &A, fixture: &Path) {
    assert_eq!(
        read(a, fixture).to_json(),
        read(a, fixture).to_json(),
        "{}",
        a.id()
    );
}

/// read -> write -> read keeps the portable fields (persona instructions, memory texts, skills).
pub fn round_trips_portable_fields<A: Adapter>(a: &A, fixture: &Path) {
    let m = read(a, fixture);
    let d = scratch(a, "rt");
    a.write(&m, &d).unwrap();
    let back = read(a, &d);
    std::fs::remove_dir_all(&d).unwrap();
    let texts = |x: &BotModel| x.memory.iter().map(|f| f.text.clone()).collect::<Vec<_>>();
    assert_eq!(
        back.persona.instructions.trim(),
        m.persona.instructions.trim()
    );
    assert_eq!(texts(&back), texts(&m));
    assert_eq!(back.skills, m.skills);
}

/// No credential value reaches the model or the written files.
pub fn never_leaks_credentials<A: Adapter>(a: &A, fixture: &Path) {
    let m = read(a, fixture);
    assert!(
        !m.to_json().contains(FAKE_SECRET),
        "{}: secret in model",
        a.id()
    );
    let d = scratch(a, "sec");
    for rel in a.write(&m, &d).unwrap() {
        let body = std::fs::read(d.join(&rel)).unwrap();
        assert!(
            !String::from_utf8_lossy(&body).contains(FAKE_SECRET),
            "{}: secret in {rel}",
            a.id()
        );
    }
    std::fs::remove_dir_all(&d).unwrap();
}

/// The model carries no absolute machine paths (fixture dir, home, temp dir).
pub fn has_no_absolute_paths<A: Adapter>(a: &A, fixture: &Path) {
    let j = read(a, fixture).to_json();
    let mut bad = vec![fixture.canonicalize().unwrap(), std::env::temp_dir()];
    bad.extend(std::env::var_os("HOME").map(PathBuf::from));
    for p in bad {
        let p = p.to_string_lossy().trim_end_matches('/').to_string();
        assert!(
            p.len() < 2 || !j.contains(&p),
            "{}: model contains {p}",
            a.id()
        );
    }
}

/// Data the model cannot express lands in `extras[adapter id]`, not dropped.
pub fn keeps_unknown_data_in_extras<A: Adapter>(a: &A, fixture: &Path) {
    let m = read(a, fixture);
    let e = m
        .extras
        .get(a.id())
        .map(|v| v.to_string())
        .unwrap_or_default();
    assert!(
        e.contains(UNKNOWN_MARKER),
        "{}: unknown data dropped",
        a.id()
    );
}

/// The model names a supported schema version.
pub fn has_schema_version<A: Adapter>(a: &A, fixture: &Path) {
    check_schema(&read(a, fixture).schema).unwrap();
}

/// All checks in one call, for new adapters with a clean fixture.
pub fn check<A: Adapter>(a: &A, fixture: &Path) {
    read_is_deterministic(a, fixture);
    round_trips_portable_fields(a, fixture);
    never_leaks_credentials(a, fixture);
    has_no_absolute_paths(a, fixture);
    keeps_unknown_data_in_extras(a, fixture);
    has_schema_version(a, fixture);
}
