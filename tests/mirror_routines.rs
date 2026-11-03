//! `botport mirror routines` end to end on invented fixtures (tests/fixtures/mirror): every status, the exit codes
//! and the rule that prompts are shown as digests, never as text.
mod common;
use common::{s, w, Tmp};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mirror")
}

fn run(args: &[&str]) -> (i32, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_botport"))
        .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR")) // not ~/.local/state
        .env_remove("HERMES_HOME")
        .args(args)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
    )
}

fn statuses(out: &str) -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(out).unwrap();
    v["routines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn every_status_from_the_fixtures_and_exit_1() {
    let f = fixtures();
    let bundle = f.join("grok-bot-export.json");
    let home = f.join("hermes");
    let (code, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&bundle),
        "--profile",
        "demo-bot",
        "--hermes-home",
        s(&home),
        "--json",
    ]);
    assert_eq!(code, 1, "{out}");
    let got = statuses(&out);
    let want = [
        ("demo-awaiting", "paused-differs"),
        ("demo-event", "event-only"),
        ("demo-grok-only", "only-grok"),
        ("demo-hermes-only", "only-hermes"),
        ("demo-interval", "same"),
        ("demo-owner-paused", "paused-differs"),
        ("demo-prompt", "prompt-differs"),
        ("demo-routine", "same"),
        ("demo-schedule", "schedule-differs"),
        ("demo-weekdays", "tz-differs"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(got, want);
    let v: Value = serde_json::from_str(&out).unwrap();
    let row = |n: &str| {
        v["routines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == n)
            .unwrap()
            .clone()
    };
    // botport's own import pause is expected; the owner's is not
    assert_eq!(row("demo-awaiting")["expected"], true);
    assert_eq!(row("demo-awaiting")["ok"], true);
    assert_eq!(row("demo-owner-paused")["ok"], false);
    assert_eq!(row("demo-schedule")["grok"], "0 8 * * *");
    assert_eq!(row("demo-hermes-only")["hermes"], "every 90m");
    // prompts never appear, only 8-digit digests
    assert!(!out.contains("Example prompt"), "{out}");
    let p = row("demo-prompt");
    assert_eq!(p["grok"].as_str().unwrap().len(), 8);
    assert_ne!(p["grok"], p["hermes"]);

    // the summary: one first line, then the differences before anything that is fine
    let (code, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&bundle),
        "--profile-dir",
        s(&home.join("profiles/demo-bot")),
    ]);
    assert_eq!(code, 1, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("mirror routines demo-bot: 1 event-only, 1 only-grok, 1 only-hermes"),
        "{out}"
    );
    assert!(lines[1].starts_with("  only-grok demo-grok-only"), "{out}");
    assert!(!out.contains("Example prompt"), "{out}");
    assert!(lines.len() <= 10, "{out}");
}

#[test]
fn the_default_timezone_decides_an_empty_one() {
    let f = fixtures();
    let (_, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&f.join("grok-bot-export.json")),
        "--profile-dir",
        s(&f.join("hermes/profiles/demo-bot")),
        "--tz",
        "Etc/GMT+3",
        "--json",
    ]);
    let got = statuses(&out);
    let st = |n: &str| got.iter().find(|(x, _)| x == n).unwrap().1.clone();
    // Hermes cron has no per-job zone: with --tz given, the weekday job matches and the UTC ones do not
    assert_eq!(st("demo-weekdays"), "same");
    assert_eq!(st("demo-routine"), "tz-differs");
}

#[test]
fn routines_in_step_exit_0() {
    let t = Tmp::new("mirror-routines-ok");
    let bundle = t.0.join("grok-bot-export.json");
    w(
        &bundle,
        r#"{"format": "grok-bot-export", "version": 1, "bots": [{"name": "demo-bot", "instructions": "",
        "routines": [
          {"name": "demo-routine", "instruction": "Example prompt: summarise the demo log.", "trigger": "schedule",
           "schedule_cron": "15 5 * * *", "timezone": "UTC", "paused": false},
          {"name": "demo-event", "instruction": "Example prompt: file the demo message.", "trigger": "webhook",
           "schedule_cron": "", "timezone": "UTC", "paused": false}]}]}"#,
    );
    let prof = t.0.join("demo-bot");
    w(
        &prof.join("cron/jobs.json"),
        r#"{"jobs": [{"name": "demo-routine", "prompt": "Example prompt: summarise the demo log.",
        "schedule": {"kind": "cron", "expr": "15 5 * * *"}, "enabled": true}]}"#,
    );
    let (code, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&bundle),
        "--profile-dir",
        s(&prof),
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.starts_with("mirror routines demo-bot: 1 event-only, 1 same"),
        "{out}"
    );
}

#[test]
fn a_name_listed_twice_is_reported_not_compared() {
    let t = Tmp::new("mirror-routines-dup");
    let bundle = t.0.join("grok-bot-export.json");
    w(
        &bundle,
        r#"{"format": "grok-bot-export", "version": 1, "bots": [{"name": "demo-bot", "instructions": "",
        "routines": [
          {"name": "demo-routine", "instruction": "Example prompt: summarise the demo log.", "trigger": "schedule",
           "schedule_cron": "15 5 * * *", "timezone": "UTC", "paused": false}]}]}"#,
    );
    let prof = t.0.join("demo-bot");
    // the same name twice once botport's "<Bot>: " prefix is removed
    w(
        &prof.join("cron/jobs.json"),
        r#"{"jobs": [{"name": "demo-routine", "prompt": "Example prompt: summarise the demo log.",
        "schedule": {"kind": "cron", "expr": "15 5 * * *"}, "enabled": true},
        {"name": "demo-bot: demo-routine", "prompt": "Example prompt: another text.",
        "schedule": {"kind": "cron", "expr": "0 1 * * *"}, "enabled": true}]}"#,
    );
    let (code, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&bundle),
        "--profile-dir",
        s(&prof),
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("duplicate-name demo-routine: entries with this name: grok 1, hermes 2"),
        "{out}"
    );
    assert!(!out.contains("same demo-routine"), "{out}");
}

#[test]
fn names_are_shown_escaped() {
    let t = Tmp::new("mirror-routines-bidi");
    let bundle = t.0.join("grok-bot-export.json");
    w(
        &bundle,
        "{\"format\": \"grok-bot-export\", \"version\": 1, \"bots\": [{\"name\": \"demo-bot\", \"instructions\": \"\",
        \"routines\": [{\"name\": \"demo\u{202e}enituor\", \"instruction\": \"Example prompt.\", \"trigger\": \"schedule\",
        \"schedule_cron\": \"15 5 * * *\", \"timezone\": \"UTC\", \"paused\": false}]}]}",
    );
    let prof = t.0.join("demo\u{2066}bot");
    w(&prof.join("cron/jobs.json"), r#"{"jobs": []}"#);
    let (_, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&bundle),
        "--profile-dir",
        s(&prof),
    ]);
    assert!(
        !out.contains('\u{202e}') && !out.contains('\u{2066}'),
        "{out:?}"
    );
    assert!(out.contains("demo\\u{202e}enituor"), "{out}");
    assert!(out.contains("mirror routines demo\\u{2066}bot:"), "{out}");
    assert!(
        out.contains("details: ") && out.contains("mirror-routines-demo\\u{2066}bot.json"),
        "{out}"
    );
}

#[test]
fn timezones_and_duplicate_entries_are_shown_escaped() {
    let t = Tmp::new("mirror-routines-tz-dup");
    let bundle = t.0.join("grok-bot-export.json");
    w(
        &bundle,
        "{\"format\": \"grok-bot-export\", \"version\": 1, \"bots\": [{\"name\": \"demo-bot\", \"instructions\": \"\",
        \"routines\": [
          {\"name\": \"demo-routine\", \"instruction\": \"Example prompt.\", \"trigger\": \"schedule\",
           \"schedule_cron\": \"15 5 * * *\", \"timezone\": \"Etc/GMT+3\\nX\u{2066}\", \"paused\": false},
          {\"name\": \"demo\u{202e}dup\", \"instruction\": \"Example prompt.\", \"trigger\": \"schedule\",
           \"schedule_cron\": \"0 1 * * *\", \"timezone\": \"UTC\", \"paused\": false}]}]}",
    );
    let prof = t.0.join("demo-bot");
    w(
        &prof.join("cron/jobs.json"),
        "{\"jobs\": [{\"name\": \"demo-routine\", \"prompt\": \"Example prompt.\",
        \"schedule\": {\"kind\": \"cron\", \"expr\": \"15 5 * * *\"}, \"enabled\": true},
        {\"name\": \"demo\u{202e}dup\", \"prompt\": \"Example prompt.\",
        \"schedule\": {\"kind\": \"cron\", \"expr\": \"0 1 * * *\"}, \"enabled\": true},
        {\"name\": \"demo-bot: demo\u{202e}dup\", \"prompt\": \"Example prompt.\",
        \"schedule\": {\"kind\": \"cron\", \"expr\": \"0 2 * * *\"}, \"enabled\": true}]}",
    );
    let (code, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&bundle),
        "--profile-dir",
        s(&prof),
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(
        !out.contains('\u{202e}') && !out.contains('\u{2066}') && !out.contains("\nX"),
        "{out:?}"
    );
    // an odd zone is never shown as written
    assert!(
        out.contains(
            "tz-differs demo-routine: grok (a zone name with unexpected characters), hermes UTC"
        ),
        "{out}"
    );
    // listed twice on the Hermes side once the "<Bot>: " prefix is removed
    assert!(
        out.contains("duplicate-name demo\\u{202e}dup: entries with this name: grok 1, hermes 2"),
        "{out}"
    );
}

#[test]
fn a_name_the_export_lists_twice_is_reported_not_compared() {
    let t = Tmp::new("mirror-routines-dup-grok");
    let bundle = t.0.join("grok-bot-export.json");
    w(
        &bundle,
        r#"{"format": "grok-bot-export", "version": 1, "bots": [{"name": "demo-bot", "instructions": "",
        "routines": [
          {"name": "demo-routine", "instruction": "Example prompt.", "trigger": "schedule",
           "schedule_cron": "15 5 * * *", "timezone": "UTC", "paused": false},
          {"name": "demo-routine", "instruction": "Example prompt: another.", "trigger": "schedule",
           "schedule_cron": "0 1 * * *", "timezone": "UTC", "paused": false}]}]}"#,
    );
    let prof = t.0.join("demo-bot");
    w(
        &prof.join("cron/jobs.json"),
        r#"{"jobs": [{"name": "demo-routine", "prompt": "Example prompt.",
        "schedule": {"kind": "cron", "expr": "15 5 * * *"}, "enabled": true}]}"#,
    );
    let (code, out) = run(&[
        "mirror",
        "routines",
        "--bundle",
        s(&bundle),
        "--profile-dir",
        s(&prof),
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("duplicate-name demo-routine: entries with this name: grok 2, hermes 1"),
        "{out}"
    );
    assert!(!out.contains("same demo-routine"), "{out}");
}

#[test]
fn usage_errors_exit_2() {
    let f = fixtures();
    let b = f.join("grok-bot-export.json");
    let d = f.join("hermes/profiles/demo-bot");
    for args in [
        vec!["mirror"],
        vec!["mirror", "nope"],
        vec!["mirror", "routines", "--profile-dir", s(&d)],
        vec!["mirror", "routines", "--bundle", s(&b)],
        vec![
            "mirror",
            "routines",
            "--bundle",
            s(&b),
            "--profile-dir",
            s(&d),
            "--profile",
            "demo-bot",
        ],
        vec![
            "mirror",
            "routines",
            "--bundle",
            s(&b),
            "--profile-dir",
            s(&d),
            "--tz",
            "not a zone",
        ],
        vec![
            "mirror",
            "routines",
            "--bundle",
            s(&b),
            "--profile",
            "../x",
            "--hermes-home",
            s(&f),
        ],
        vec!["plan", "--bundle", s(&b), "--tz", "UTC"],
    ] {
        let (code, out) = run(&args);
        assert_eq!(code, 2, "{args:?}: {out}");
    }
}
