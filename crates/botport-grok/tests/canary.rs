//! P5: tested-versions gate, the read-only canary and its reports, all offline on the fake transport.
mod common;
use botport_grok::canary::{self, CanaryReport};
use botport_grok::engine::{ExitCode, WaitOpts};
use botport_grok::policy::{Caller, Policy};
use botport_grok::transport::fake::FakeTransport;
use botport_grok::transport::{Cap, TransportError};
use botport_grok::{Engine, Preset};
use common::*;
use std::path::Path;

fn untested(policy: Policy, dir: &Path) -> Engine {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_version("9.9.9")
        .with_turn("reply-ready");
    let mut e = engine_from(f, c, policy, Caller::Cli { tty: false });
    e.canary_dir = Some(dir.to_path_buf());
    e
}

fn reports(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn tested_table_knows_1_1_0_only() {
    assert_eq!(botport_grok::version::lookup("1.1.0"), Some((true, true)));
    assert!(botport_grok::version::lookup("9.9.9").is_none());
    let mut e = eng(&[]);
    let c = e.capabilities().unwrap();
    assert_eq!(c.app_version.as_deref(), Some("1.1.0"));
    assert!(c.tested);
    assert!(c.caps.contains(&Cap::PortCheck));
}

#[test]
fn canary_on_the_fixtures_passes_every_read_probe_and_files_a_private_report() {
    let s = Scratch::new("canary-pass");
    let mut e = eng(&[]);
    let o = canary::run(&mut e, &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let r = o.value.unwrap();
    let names: Vec<&str> = r.steps.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["port", "status", "bots", "state", "read", "send", "stop"]
    );
    assert_eq!((r.read_passed, r.read_total), (7, 7));
    assert_eq!(r.transport, "fake");
    assert!(o
        .proof
        .line()
        .starts_with("PASS: canary 1.1.0 read 7/7; report canary/canary-1.1.0-"));
    let files = reports(&s.0);
    assert_eq!(files.len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&files[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let body = std::fs::read_to_string(&files[0]).unwrap();
    let back: CanaryReport = serde_json::from_str(&body).unwrap();
    assert!(back.pass);
    // counts only: no bot ids or names in the report
    assert!(!body.contains(MAIN), "report carries a bot id");
    for b in e.bots().value.unwrap() {
        assert!(!body.contains(&format!("\"{}\"", b.name)));
    }
}

#[test]
fn canary_writes_nothing_to_the_app() {
    let s = Scratch::new("canary-ro");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone()).unwrap();
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let before = e.read("main", None, 0).value.unwrap().len();
    assert_eq!(canary::run(&mut e, &s.0).exit, ExitCode::Done);
    assert_eq!(e.read("main", None, 0).value.unwrap().len(), before);
}

#[test]
fn unknown_version_reads_warn_writes_exit_4_until_the_canary_passes() {
    let s = Scratch::new("canary-gate");
    let mut e = untested(Policy::default(), &s.0);
    let b = e.bots();
    assert_eq!(b.exit, ExitCode::Done);
    assert!(b.warnings.iter().any(|w| w.contains("9.9.9")));
    let r = e.read("main", None, 3);
    assert_eq!(r.exit, ExitCode::Done);
    assert!(!r.warnings.is_empty());
    let sent = e.send("main", "hi", &[]);
    assert_eq!(sent.exit, ExitCode::Refused, "{}", sent.proof.line());
    assert!(sent.hint.unwrap().contains("botport grok canary"));
    // canary on 9.9.9 passes against the fixtures, with a warning that it unlocks writes
    let o = canary::run(&mut e, &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(o.proof.line().contains("canary 9.9.9 read 7/7"));
    assert!(o.warnings.iter().any(|w| w.contains("unlocks writes")));
    assert!(!o.value.unwrap().tested);
    // now writes run, naming the canary
    let a = e.ask("main", "hi", &[], &WaitOpts::default());
    assert_eq!(a.exit, ExitCode::Done, "{}", a.proof.line());
    assert!(a.warnings.iter().any(|w| w.contains("canary that passed")));
}

#[test]
fn balanced_needs_the_canary_and_careful_stays_blocked() {
    let s = Scratch::new("canary-presets");
    let mut e = untested(Policy::preset(Preset::Balanced), &s.0);
    e.trust_version = true;
    assert_eq!(e.send("main", "hi", &[]).exit, ExitCode::Refused);
    assert_eq!(canary::run(&mut e, &s.0).exit, ExitCode::Done);
    assert_eq!(
        e.ask("main", "hi", &[], &WaitOpts::default()).exit,
        ExitCode::Done
    );

    // careful with every bot in scope, so the version gate is what decides
    let mut careful = Policy::preset(Preset::Careful);
    careful.bots_scope = botport_grok::policy::BotsScope::All;
    let mut e = untested(careful, &s.0);
    e.trust_version = true;
    // a passing 9.9.9 canary is already on file: careful still refuses
    assert!(canary::passed(&s.0, "9.9.9", "fake").is_some());
    let o = e.send("main", "hi", &[]);
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("careful"));
}

#[test]
fn only_a_passing_report_for_the_same_version_and_transport_unlocks() {
    let s = Scratch::new("canary-passed");
    assert!(canary::passed(&s.0, "9.9.9", "fake").is_none());
    let mut e = untested(Policy::default(), &s.0);
    assert_eq!(canary::run(&mut e, &s.0).exit, ExitCode::Done);
    assert!(canary::passed(&s.0, "9.9.9", "fake").is_some());
    assert!(canary::passed(&s.0, "9.9.9", "debug-port").is_none());
    assert!(canary::passed(&s.0, "9.9.10", "fake").is_none());
    // a missing directory is simply "no canary"
    assert!(canary::passed(&s.0.join("nope"), "9.9.9", "fake").is_none());
}

#[test]
fn port_gate_closed_stops_the_canary_at_step_one_with_exit_5() {
    let s = Scratch::new("canary-port");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_version("9.9.9")
        .fail_next(
            Cap::PortCheck,
            TransportError::Unavailable("devtools gate closed the port".into()),
        );
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    e.canary_dir = Some(s.0.clone());
    let o = canary::run(&mut e, &s.0);
    assert_eq!(o.exit, ExitCode::Unavailable);
    assert!(o
        .proof
        .line()
        .starts_with("FAIL: canary 9.9.9 read 0/1; port failed"));
    let r = o.value.unwrap();
    assert_eq!(r.steps.len(), 1);
    assert!(!r.pass);
    assert_eq!(reports(&s.0).len(), 1, "a failed canary is filed too");
    // and it unlocks nothing
    assert!(canary::passed(&s.0, "9.9.9", "fake").is_none());
    assert_eq!(e.send("main", "hi", &[]).exit, ExitCode::Refused);
}

#[test]
fn shape_drift_fails_the_canary_with_exit_1_naming_the_field() {
    let s = Scratch::new("canary-shape");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_version("9.9.9")
        .fail_next(
            Cap::StateFlags,
            TransportError::Shape {
                field: "agent.isRunningTurn".into(),
                version: "9.9.9".into(),
            },
        );
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let o = canary::run(&mut e, &s.0);
    assert_eq!(o.exit, ExitCode::Other, "{}", o.proof.line());
    assert!(o.proof.line().contains("state failed"));
    assert!(o.proof.line().contains("agent.isRunningTurn"));
    assert!(canary::passed(&s.0, "9.9.9", "fake").is_none());
}

#[test]
fn careful_allowlist_skips_the_read_probe_instead_of_failing() {
    let s = Scratch::new("canary-careful");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone()).unwrap();
    let mut e = engine_from(
        f,
        c,
        Policy::preset(Preset::Careful),
        Caller::Cli { tty: false },
    );
    let o = canary::run(&mut e, &s.0);
    let r = o.value.as_ref().unwrap();
    let read = r.steps.iter().find(|s| s.name == "read").unwrap();
    if read.ran {
        assert!(read.pass);
    } else {
        assert!(read.detail.starts_with("skipped"));
        assert_eq!(r.read_total, 6);
    }
}
