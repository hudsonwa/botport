//! Conformance checks every `Adapter` must pass (see docs/ADAPTERS.md). Each check takes an adapter and a fixture
//! directory holding a native layout seeded with `FAKE_SECRET` (somewhere a real credential would live) and
//! `UNKNOWN_MARKER` (in data the model has no field for), with at least one schedule with a cron, one without,
//! one connector with an http url, one without, and one credential name.
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

/// read -> write -> read, and both models.
fn round_trip<A: Adapter>(a: &A, fixture: &Path, tag: &str) -> (BotModel, BotModel) {
    let m = read(a, fixture);
    let d = scratch(a, tag);
    a.write(&m, &d).unwrap();
    let back = read(a, &d);
    std::fs::remove_dir_all(&d).unwrap();
    (m, back)
}

/// `name` survives as a schedule/connector of that name or as a manual item `<kind> <name>`.
fn kept_by_name(m: &BotModel, kind: &str, name: &str) -> bool {
    let named = match kind {
        "schedule" => m.schedules.iter().any(|s| s.name == name),
        _ => m.connectors.iter().any(|c| c.name == name),
    };
    named || m.manual.iter().any(|x| x.item == format!("{kind} {name}"))
}

/// Manual items of one kind (`schedule`, `connector`): the names they carry.
fn manual_names<'a>(m: &'a BotModel, kind: &str) -> Vec<&'a str> {
    let pre = format!("{kind} ");
    m.manual
        .iter()
        .filter_map(|x| x.item.strip_prefix(pre.as_str()))
        .collect()
}

/// read -> write -> read keeps every schedule: one with a cron by cron + action (a target may rename it); one
/// without (an event), or one only listed for a human, at least by name.
pub fn round_trips_schedules<A: Adapter>(a: &A, fixture: &Path) {
    let (m, back) = round_trip(a, fixture, "sched");
    assert!(
        m.schedules.iter().any(|s| s.cron.is_some()),
        "{}: the fixture needs a schedule with a cron",
        a.id()
    );
    for s in &m.schedules {
        let kept = match &s.cron {
            Some(c) => back
                .schedules
                .iter()
                .any(|b| b.cron.as_deref() == Some(c) && b.action.trim() == s.action.trim()),
            None => kept_by_name(&back, "schedule", &s.name),
        };
        assert!(kept, "{}: schedule {:?} dropped", a.id(), s.name);
    }
    for n in manual_names(&m, "schedule") {
        assert!(
            kept_by_name(&back, "schedule", n),
            "{}: schedule {n:?} (manual) dropped",
            a.id()
        );
    }
}

/// read -> write -> read keeps every connector: one with a url by url; one without, or one only listed for a
/// human, at least by name.
pub fn round_trips_connectors<A: Adapter>(a: &A, fixture: &Path) {
    let (m, back) = round_trip(a, fixture, "conn");
    assert!(
        m.connectors.iter().any(|c| c.url.is_some()),
        "{}: the fixture needs a connector with a url",
        a.id()
    );
    for c in &m.connectors {
        let kept = match &c.url {
            Some(u) => back.connectors.iter().any(|b| b.url.as_ref() == Some(u)),
            None => kept_by_name(&back, "connector", &c.name),
        };
        assert!(kept, "{}: connector {:?} dropped", a.id(), c.name);
    }
    for n in manual_names(&m, "connector") {
        assert!(
            kept_by_name(&back, "connector", n),
            "{}: connector {n:?} (manual) dropped",
            a.id()
        );
    }
}

/// read -> write -> read keeps every credential name (names only: values never reach the model).
pub fn round_trips_credential_names<A: Adapter>(a: &A, fixture: &Path) {
    let (m, back) = round_trip(a, fixture, "cred");
    assert!(
        !m.credentials.is_empty(),
        "{}: the fixture needs a credential name",
        a.id()
    );
    for c in &m.credentials {
        assert!(
            back.credentials.iter().any(|b| b.name == c.name),
            "{}: credential {:?} dropped",
            a.id(),
            c.name
        );
    }
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
    round_trips_schedules(a, fixture);
    round_trips_connectors(a, fixture);
    round_trips_credential_names(a, fixture);
    never_leaks_credentials(a, fixture);
    has_no_absolute_paths(a, fixture);
    keeps_unknown_data_in_extras(a, fixture);
    has_schema_version(a, fixture);
}
