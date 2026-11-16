//! `status --json` field `local_tool_permission` (always | ask | never), read through getLocalToolPermission
//! and left out when it cannot be read. Never set.
mod common;
use botport_grok::lifecycle::Status;
use botport_grok::parse;
use botport_grok::policy::{floors, Caller, Policy};
use botport_grok::transport::fake::FakeTransport;
use botport_grok::transport::{Cap, TransportError};
use common::*;
use serde_json::json;

fn status(ltp: Option<String>) -> serde_json::Value {
    serde_json::to_value(Status {
        installed: true,
        version: Some("1.1.0".into()),
        tested: true,
        running: true,
        pid: None,
        port_open: false,
        port_by_botport: false,
        port_opened_at: None,
        port_expires_at: None,
        preset: "fast".into(),
        local_tool_permission: ltp,
    })
    .unwrap()
}

#[test]
fn fake_reads_the_fixture_setting() {
    let mut e = eng(&[]);
    assert_eq!(e.local_tool_permission().as_deref(), Some("ask"));
    assert_eq!(status(Some("ask".into()))["local_tool_permission"], "ask");
}

#[test]
fn unreadable_leaves_the_field_out() {
    for f in [
        FakeTransport::load(&fx(), clock())
            .unwrap()
            .with_local_tool_permission(None),
        FakeTransport::load(&fx(), clock())
            .unwrap()
            .with_local_tool_permission(Some(json!("sometimes"))),
        FakeTransport::load(&fx(), clock()).unwrap().fail_next(
            Cap::LocalToolPermission,
            TransportError::Floor("floor 6".into()),
        ),
    ] {
        let mut e = engine_from(f, clock(), Policy::default(), Caller::Cli { tty: false });
        assert_eq!(e.local_tool_permission(), None);
    }
    assert!(status(None).get("local_tool_permission").is_none());
}

#[test]
fn answer_shapes_normalise_and_the_setter_is_never_allowed() {
    assert_eq!(
        parse::local_tool_permission(&json!("Always")).as_deref(),
        Some("always")
    );
    assert_eq!(
        parse::local_tool_permission(&json!({"value": "never"})).as_deref(),
        Some("never")
    );
    assert_eq!(
        parse::local_tool_permission(&json!({"permission": "ask"})).as_deref(),
        Some("ask")
    );
    assert_eq!(parse::local_tool_permission(&json!(null)), None);
    assert!(floors::ipc_allowed("getLocalToolPermission"));
    assert!(!floors::ipc_allowed("setLocalToolPermission"));
}

#[test]
fn cli_status_with_the_fake_exits_0() {
    let s = Scratch::new("ltp-cli");
    std::env::set_var("BOTPORT_HOME", &s.0);
    let args: Vec<String> = ["status", "--json", "--transport", "fake", "--fixtures"]
        .iter()
        .map(|x| x.to_string())
        .chain([fx().to_string_lossy().into_owned()])
        .collect();
    assert_eq!(botport_grok::cli::main(&args), 0);
}
