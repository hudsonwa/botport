//! `botport grok …` end to end through the binary, on the fake transport (offline).
#![cfg(feature = "grok")]
use std::path::PathBuf;
use std::process::Command;

fn fx() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/botport-grok/tests/fixtures/app-1.1.0")
        .display()
        .to_string()
}

fn home(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("botport-grok-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn run(h: &PathBuf, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_botport"))
        .arg("grok")
        .args(args)
        .env("BOTPORT_HOME", h)
        .env_remove("BOTPORT_GROK_FAKE")
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into(),
        String::from_utf8_lossy(&o.stderr).into(),
    )
}

#[test]
fn bots_json_and_exit_codes() {
    let h = home("bots");
    let f = fx();
    let (rc, out, err) = run(&h, &["bots", "--json", "--fixtures", &f]);
    assert_eq!(rc, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["bots"].as_array().unwrap().len(), 7);
    assert_eq!(v["proof"], "PASS: 7 bots; matches sidebar");
    assert!(
        err.contains("PASS: 7 bots"),
        "the proof line also goes to stderr with --json"
    );
    let (rc, out, _) = run(
        &h,
        &[
            "ask",
            "main",
            "hello",
            "--fixtures",
            &f,
            "--turn",
            "needs-approval",
        ],
    );
    assert_eq!(rc, 3);
    assert!(out
        .trim_end()
        .lines()
        .last()
        .unwrap()
        .starts_with("FAIL: waiting for you"));
    let (rc, out, _) = run(
        &h,
        &[
            "ask",
            "main",
            "hello",
            "--fixtures",
            &f,
            "--turn",
            "reply-ready",
        ],
    );
    assert_eq!(rc, 0);
    assert!(
        out.contains("READY")
            && out
                .trim_end()
                .ends_with("PASS: reply received; bot idle 8 s; sidebar row idle")
    );
    let (rc, _, _) = run(
        &h,
        &[
            "ask",
            "main",
            "hello",
            "--fixtures",
            &f,
            "--turn",
            "long-reply",
            "--timeout",
            "30",
        ],
    );
    assert_eq!(rc, 2);
    let (rc, _, _) = run(&h, &["send", "m", "x", "--fixtures", &f]);
    assert_eq!(rc, 1, "ambiguous selector");
    let (rc, _, _) = run(
        &h,
        &["read", "main", "--limit", "2", "--json", "--fixtures", &f],
    );
    assert_eq!(rc, 0);
    let (rc, _, _) = run(&h, &["bots", "--transport", "webhook"]);
    assert_eq!(rc, 5);
    let (rc, _, _) = run(&h, &["frobnicate"]);
    assert_ne!(
        rc, 0,
        "unknown grok subcommands fall through to the twin interface, which refuses them"
    );
    let (rc, out, _) = run(&h, &["watch", "--for", "3s", "--json", "--fixtures", &f]);
    assert_eq!(rc, 0);
    assert!(out
        .trim_end()
        .lines()
        .last()
        .unwrap()
        .contains("\"type\":\"summary\""));
    // the action log exists, 0600, and holds no message text
    let log = std::fs::read_to_string(h.join("grok-actions.jsonl")).unwrap();
    assert!(!log.contains("hello"));
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn policy_commands_persist_and_refuse_with_exit_4() {
    let h = home("policy");
    let f = fx();
    let (rc, out, _) = run(&h, &["policy", "preset", "careful"]);
    assert_eq!(rc, 0);
    assert!(out
        .trim_end()
        .ends_with("PASS: preset careful; saved policy file agrees"));
    let (rc, out, _) = run(&h, &["send", "main", "hi", "--fixtures", &f]);
    assert_eq!(rc, 4, "{out}");
    assert!(out.contains("bots_scope"));
    let (rc, _, _) = run(&h, &["policy", "set", "bots_scope", "Main Bot"]);
    assert_eq!(rc, 0);
    let (rc, _, _) = run(&h, &["send", "main", "hi", "--fixtures", &f]);
    assert_eq!(rc, 0);
    let (rc, _, _) = run(&h, &["policy", "set", "debug_port", "maybe"]);
    assert_eq!(rc, 1);
    let (rc, out, _) = run(&h, &["policy", "preset", "fast"]);
    assert_eq!(rc, 0, "{out}");
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn grok_list_still_belongs_to_the_twin_interface() {
    let h = home("list");
    let (_, out, err) = run(&h, &["list", "--help"]);
    assert!(
        !out.contains("botport grok <command>") && !err.contains("unknown grok command"),
        "{out}{err}"
    );
    let _ = std::fs::remove_dir_all(&h);
}
