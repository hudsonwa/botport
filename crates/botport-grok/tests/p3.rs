//! P3 offline tests (fake transport, scrubbed synthetic fixtures): files, routines, usage, the MCP mirror.
mod common;
use botport_grok::engine::WaitOpts;
use botport_grok::mcp;
use botport_grok::policy::{Caller, Policy, Preset, UsageCheck};
use botport_grok::routines::RoutineSpec;
use botport_grok::transport::fake::FakeTransport;
use botport_grok::{Engine, ExitCode};
use common::*;
use serde_json::json;

const BOT2: &str = "00000000-0000-4000-8000-000000000002";

fn fake() -> (FakeTransport, botport_grok::clock::SharedClock) {
    let c = clock();
    (FakeTransport::load(&fx(), c.clone()).unwrap(), c)
}

fn engine(f: FakeTransport, c: botport_grok::clock::SharedClock) -> Engine {
    engine_from(f, c, Policy::default(), Caller::Cli { tty: false })
}

fn opts() -> WaitOpts {
    WaitOpts {
        timeout_s: 60.0,
        settle_s: 2.0,
    }
}

// ------------------------------------------------------------------ files

#[test]
fn files_list_reads_the_bots_files() {
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.files(BOT2);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let v = o.value.unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].name, "export.json");
    assert_eq!(v[0].bytes, 4096);
    assert_eq!(
        o.proof.line(),
        "PASS: 2 files, 4216 bytes; self-check (two reads agree)"
    );
}

#[test]
fn download_saves_checks_and_never_overwrites() {
    let s = Scratch::new("dl");
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.file_download(BOT2, &["export.json".into()], &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let first = o.value.unwrap().remove(0);
    let p1 = s.0.join("export.json");
    assert_eq!(first.saved_to, p1.display().to_string());
    let bytes = std::fs::read(&p1).unwrap();
    assert_eq!(bytes.len(), 4096);
    assert_eq!(first.sha256, botport_grok::files::sha256_hex(&bytes));
    assert_eq!(first.app_sha256.as_deref(), Some(first.sha256.as_str()));
    assert_eq!(o.proof.route, "files-list");
    assert!(
        o.proof
            .line()
            .starts_with("PASS: export.json saved, 4096 bytes; sha256 "),
        "{}",
        o.proof.line()
    );
    // a changed local copy is never replaced: the next download gets a new name
    std::fs::write(&p1, b"mine").unwrap();
    let o = e.file_download(BOT2, &["export".into()], &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(std::fs::read(&p1).unwrap(), b"mine");
    assert!(s.0.join("export (1).json").exists());
}

#[test]
fn download_all_when_no_name_is_given() {
    let s = Scratch::new("dlall");
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.file_download(BOT2, &[], &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.value.unwrap().len(), 2);
    assert!(s.0.join("notes.md").exists());
    assert!(o
        .proof
        .line()
        .starts_with("PASS: 2 files saved, 4216 bytes"));
}

#[test]
fn a_large_file_comes_through_in_chunks_intact() {
    let s = Scratch::new("dlbig");
    let (f, c) = fake();
    let big: Vec<u8> = (0..(30 * 1024 * 1024 + 17))
        .map(|i: u32| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let want = botport_grok::files::sha256_hex(&big);
    let f = f.with_file(BOT2, "big-export.bin", big);
    let mut e = engine(f, c);
    let o = e.file_download(BOT2, &["big-export.bin".into()], &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let got = std::fs::read(s.0.join("big-export.bin")).unwrap();
    assert_eq!(got.len(), 30 * 1024 * 1024 + 17);
    assert_eq!(botport_grok::files::sha256_hex(&got), want);
}

#[test]
fn a_digest_mismatch_fails_and_leaves_no_file() {
    let s = Scratch::new("dlbad");
    let (f, c) = fake();
    let mut e = engine(f.with_bad_app_digest(), c);
    let o = e.file_download(BOT2, &["export.json".into()], &s.0);
    assert_eq!(o.exit, ExitCode::Other);
    assert!(
        o.proof.line().contains("differs from the app's sha256"),
        "{}",
        o.proof.line()
    );
    assert_eq!(std::fs::read_dir(&s.0).unwrap().count(), 0);
}

#[test]
fn an_unsafe_name_is_saved_inside_the_folder() {
    let s = Scratch::new("dlname");
    let out = s.0.join("out");
    let (f, c) = fake();
    let f = f.with_file(BOT2, "../../escape.json", b"{}".to_vec());
    let mut e = engine(f, c);
    let o = e.file_download(BOT2, &["../../escape.json".into()], &out);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(out.join("escape.json").exists());
    assert!(!s.0.join("escape.json").exists());
}

#[test]
fn download_of_an_unknown_name_is_exit_1() {
    let s = Scratch::new("dlnone");
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.file_download(BOT2, &["nope.txt".into()], &s.0);
    assert_eq!(o.exit, ExitCode::Other);
    assert!(o.proof.line().contains("no file matches"));
}

#[test]
fn upload_lists_the_file_and_a_too_large_file_is_refused() {
    let s = Scratch::new("up");
    let p = s.0.join("hello.txt");
    std::fs::write(&p, b"hello from a test").unwrap();
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.file_upload(BOT2, &[p]);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(
        o.proof.line(),
        "PASS: uploaded hello.txt; file list shows each with the same size"
    );
    let big = s.0.join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(26 * 1024 * 1024)
        .unwrap();
    let o = e.file_upload(BOT2, &[big]);
    assert_eq!(o.exit, ExitCode::Other);
    assert!(o.proof.line().contains("25 MB"));
}

#[test]
fn delete_needs_the_exact_name() {
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.file_delete(BOT2, "export", None, false);
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("floor 4"), "{}", o.proof.line());
    let o = e.file_delete(BOT2, "export.json", None, false);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(
        o.proof.line(),
        "PASS: export.json deleted; absent from the file list"
    );
    assert_eq!(e.files(BOT2).value.unwrap().len(), 1);
}

#[test]
fn balanced_delete_needs_yes() {
    let (f, c) = fake();
    let mut e = engine_from(
        f,
        c,
        Policy::preset(Preset::Balanced),
        Caller::Cli { tty: false },
    );
    let o = e.file_delete(BOT2, "export.json", None, false);
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("confirm_destructive"));
    assert_eq!(
        e.file_delete(BOT2, "export.json", None, true).exit,
        ExitCode::Done
    );
}

// ------------------------------------------------------------------ routines

#[test]
fn routines_list_toggle_and_check_by_a_fresh_list() {
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.routines(MAIN);
    assert_eq!(o.exit, ExitCode::Done);
    let r = o.value.unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].schedule, "0 8 * * 1");
    assert_eq!(r[0].tz.as_deref(), Some("UTC"));
    assert!(r[0].enabled);
    let o = e.routine_toggle(MAIN, "weekly-cost-log", false);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.proof.route, "routines-list");
    assert!(o
        .proof
        .line()
        .ends_with("disabled; list shows enabled=false"));
    assert!(!e.routines(MAIN).value.unwrap()[0].enabled);
    let o = e.routine_toggle(MAIN, "weekly-cost-log", true);
    assert_eq!(o.exit, ExitCode::Done);
}

fn spec(name: &str) -> RoutineSpec {
    RoutineSpec {
        name: name.into(),
        schedule: "0 9 * * 1".into(),
        tz: Some("UTC".into()),
        prompt: "Write SYNTHETIC OK.".into(),
        disabled: false,
    }
}

#[test]
fn routine_add_asks_the_bot_and_checks_the_list() {
    let (f, c) = fake();
    let mut e = engine(f.with_turn("reply-ready"), c);
    let o = e.routine_add(BOT2, &spec("p3 probe"), &opts());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(
        o.proof.line(),
        "PASS: p3 probe listed with schedule 0 9 * * 1; routine list"
    );
    let v = o.value.unwrap();
    assert_eq!(v.via, "bot");
    assert!(!v.messages.is_empty());
}

#[test]
fn routine_add_disabled_turns_it_off() {
    let (f, c) = fake();
    let mut e = engine(f.with_turn("reply-ready"), c);
    let mut s = spec("p3 off");
    s.disabled = true;
    let o = e.routine_add(BOT2, &s, &opts());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(!o.value.unwrap().routine.unwrap().enabled);
}

#[test]
fn routine_add_fails_when_the_list_does_not_show_it() {
    let (f, c) = fake();
    let mut e = engine(f.bot_ignores_routine_requests().with_turn("reply-ready"), c);
    let o = e.routine_add(BOT2, &spec("p3 ghost"), &opts());
    assert_eq!(o.exit, ExitCode::Other);
    assert!(
        o.proof.line().contains("not in the routine list"),
        "{}",
        o.proof.line()
    );
}

#[test]
fn routine_add_needing_approval_is_exit_3() {
    let (f, c) = fake();
    let mut e = engine(f.with_turn("needs-approval"), c);
    let o = e.routine_add(BOT2, &spec("p3 card"), &opts());
    assert_eq!(o.exit, ExitCode::NeedsUser, "{}", o.proof.line());
}

#[test]
fn routine_remove_is_exact_only_and_checked_by_the_list() {
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.routine_remove(MAIN, "weekly", None, false, &opts());
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("floor 4"));
    let o = e.routine_remove(MAIN, "weekly-cost-log", None, false, &opts());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.value.as_ref().unwrap().via, "ui");
    assert!(o
        .proof
        .line()
        .ends_with("removed; absent from the routine list"));
    assert!(e.routines(MAIN).value.unwrap().is_empty());
}

#[test]
fn routine_remove_without_the_menu_disables_then_asks_the_bot() {
    let (f, c) = fake();
    let mut e = engine(f.without_ui_remove().with_turn("reply-ready"), c);
    let o = e.routine_remove(MAIN, "weekly-cost-log", None, false, &opts());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.value.unwrap().via, "bot");
}

#[test]
fn routine_run_needs_a_new_run_in_the_list() {
    let (f, c) = fake();
    let mut e = engine(f.with_turn("reply-ready"), c);
    let o = e.routine_run(MAIN, "weekly-cost-log", &opts());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(o.value.unwrap().routine.unwrap().running);
    let (f, c) = fake();
    let mut e = engine(f.bot_ignores_routine_requests().with_turn("reply-ready"), c);
    let o = e.routine_run(MAIN, "weekly-cost-log", &opts());
    assert_eq!(o.exit, ExitCode::Other);
    assert!(o.proof.line().contains("no new run"));
}

// ------------------------------------------------------------------ usage

#[test]
fn usage_reads_and_never_shows_the_dashboard_url() {
    let (f, c) = fake();
    let mut e = engine(f, c);
    let o = e.usage();
    assert_eq!(o.exit, ExitCode::Done);
    let u = o.value.clone().unwrap();
    assert_eq!(u.percent_used, 18.7);
    assert_eq!(u.plan, "Premium");
    assert!(u.available);
    assert!(o.proof.line().starts_with("PASS: 18.7% used; resets "));
    let j = mcp::outcome_json(&o, None).to_string();
    assert!(!j.contains("dashboard"));
}

#[test]
fn fast_warns_over_90_percent_before_a_paid_send() {
    let (f, c) = fake();
    let mut e = engine(f.with_usage(93.0, true).with_turn("reply-ready"), c);
    let o = e.send(BOT2, "hello", &[]);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(
        o.warnings
            .iter()
            .any(|w| w.contains("93% of the weekly Grok usage")),
        "{:?}",
        o.warnings
    );
    let (f, c) = fake();
    let mut e = engine(f.with_usage(50.0, true).with_turn("reply-ready"), c);
    assert!(e.send(BOT2, "hello", &[]).warnings.is_empty());
}

#[test]
fn a_refuse_setting_stops_the_send_with_exit_4_and_sends_nothing() {
    let (f, c) = fake();
    let p = Policy {
        usage_check: UsageCheck::Refuse(90.0),
        ..Policy::default()
    };
    let mut e = engine_from(f.with_usage(95.0, true), c, p, Caller::Cli { tty: false });
    let o = e.ask(BOT2, "hello", &[], &opts());
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("usage_check"), "{}", o.proof.line());
}

#[test]
fn no_usage_left_is_warned_even_under_the_threshold() {
    let (f, c) = fake();
    let mut e = engine(f.with_usage(10.0, false).with_turn("reply-ready"), c);
    let o = e.send(BOT2, "hello", &[]);
    assert!(o.warnings.iter().any(|w| w.contains("no usage left")));
}

// ------------------------------------------------------------------ MCP

fn names(bot_callable: bool) -> Vec<String> {
    mcp::tools(bot_callable)
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn destructive_tools_are_never_on_a_bot_callable_server() {
    let all = names(false);
    for t in [
        "grok_routines",
        "grok_usage",
        "grok_files",
        "grok_routine_remove",
        "grok_file_delete",
    ] {
        assert!(all.contains(&t.to_string()), "{t}");
    }
    let bc = names(true);
    for t in ["grok_routine_remove", "grok_file_delete", "grok_stop"] {
        assert!(!bc.contains(&t.to_string()), "{t}");
    }
    for t in mcp::tools(true) {
        if let Some(en) = t.pointer("/inputSchema/properties/action/enum") {
            assert_eq!(en, &json!(["list"]), "{}", t["name"]);
        }
    }
}

#[test]
fn bot_callable_refuses_non_list_actions_and_destructive_calls() {
    let (f, c) = fake();
    let mut e = engine_from(f, c, Policy::default(), Caller::Mcp { bot_callable: true });
    let mut st = || json!({"exit": 0});
    let r = mcp::call(
        &mut e,
        &mut st,
        "grok_files",
        &json!({"bot": BOT2, "action": "download"}),
        true,
    );
    assert!(r.is_err());
    let r = mcp::call(
        &mut e,
        &mut st,
        "grok_file_delete",
        &json!({"bot": BOT2, "name": "export.json", "confirm": "export.json"}),
        true,
    );
    assert!(r.is_err());
    let (v, err) = mcp::call(&mut e, &mut st, "grok_files", &json!({"bot": BOT2}), true).unwrap();
    assert!(!err);
    assert_eq!(v["files"].as_array().unwrap().len(), 2);
}

#[test]
fn mcp_delete_needs_confirm_to_repeat_the_exact_name() {
    let (f, c) = fake();
    let mut e = engine_from(
        f,
        c,
        Policy::default(),
        Caller::Mcp {
            bot_callable: false,
        },
    );
    let mut st = || json!({"exit": 0});
    let (v, err) = mcp::call(
        &mut e,
        &mut st,
        "grok_file_delete",
        &json!({"bot": BOT2, "name": "export.json", "confirm": "export"}),
        false,
    )
    .unwrap();
    assert!(err);
    assert_eq!(v["exit"], json!(4));
    let (v, err) = mcp::call(
        &mut e,
        &mut st,
        "grok_routine_remove",
        &json!({"bot": MAIN, "routine": "weekly-cost-log", "confirm": "weekly-cost-log"}),
        false,
    )
    .unwrap();
    assert!(!err, "{v}");
    let (v, _) = mcp::call(&mut e, &mut st, "grok_usage", &json!({}), false).unwrap();
    assert_eq!(v["percent_used"], json!(18.7));
}

#[test]
fn fetch_saves_the_newest_file_the_bot_sent() {
    let s = Scratch::new("fetch");
    let (f, c) = fake();
    let f = f
        .with_attachment(MAIN, "old.json", b"{}".to_vec())
        .with_attachment(
            MAIN,
            "zt-export.json",
            br#"{"hello":"world","n":1}"#.to_vec(),
        );
    let mut e = engine(f, c);
    let o = e.file_fetch(MAIN, None, &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.proof.route, "app-preview");
    assert_eq!(
        std::fs::read(s.0.join("zt-export.json")).unwrap(),
        br#"{"hello":"world","n":1}"#
    );
    let o = e.file_fetch(MAIN, Some("old.json"), &s.0);
    assert_eq!(o.exit, ExitCode::Done);
    let o = e.file_fetch(MAIN, Some("zt-export.json"), &s.0);
    assert!(o.value.unwrap().saved_to.ends_with("zt-export (1).json"));
    assert_eq!(e.file_fetch(MAIN, Some("nope"), &s.0).exit, ExitCode::Other);
}

#[test]
fn fetch_with_a_bad_digest_leaves_no_file() {
    let s = Scratch::new("fetchbad");
    let (f, c) = fake();
    let f = f
        .with_attachment(MAIN, "x.json", b"{}".to_vec())
        .with_bad_app_digest();
    let mut e = engine(f, c);
    assert_eq!(e.file_fetch(MAIN, None, &s.0).exit, ExitCode::Other);
    assert_eq!(std::fs::read_dir(&s.0).unwrap().count(), 0);
}
