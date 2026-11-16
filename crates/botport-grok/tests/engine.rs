//! Engine commands against the fake transport replaying the app-1.1.0 fixtures (virtual time, offline).
mod common;
use botport_grok::engine::{ExitCode, WaitOpts};
use botport_grok::model::*;
use botport_grok::policy::{Caller, Policy};
use botport_grok::transport::fake::FakeTransport;
use botport_grok::transport::{Cap, TransportError};
use botport_grok::Engine;
use common::*;

fn w() -> WaitOpts {
    WaitOpts::default()
}

// ---------------------------------------------------------------- bots + selectors

#[test]
fn bots_lists_seven_with_main_and_passes_against_the_sidebar_rows() {
    let mut e = eng(&[]);
    let o = e.bots();
    assert_eq!(o.exit, ExitCode::Done);
    let b = o.value.unwrap();
    assert_eq!(b.len(), 7);
    assert!(b[0].main && b[0].name == "Main Bot");
    assert_eq!(b.iter().filter(|b| b.main).count(), 1);
    assert!(b.iter().all(|b| b.state == BotState::Idle));
    assert_eq!(o.proof.line(), "PASS: 7 bots; matches sidebar");
    assert_eq!(o.proof.route, "dom-rows");
}

#[test]
fn bots_fails_when_the_second_route_disagrees() {
    let s = Scratch::new("hidden");
    copy_fixtures(&s.0);
    let p = s.0.join("roster.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    v["agents"][2]["isHiddenFromSidebar"] = true.into();
    std::fs::write(&p, v.to_string()).unwrap();
    let c = clock();
    let mut e = engine_from(
        FakeTransport::load(&s.0, c.clone()).unwrap(),
        c,
        Policy::default(),
        Caller::Cli { tty: false },
    );
    let o = e.bots();
    assert_eq!(o.exit, ExitCode::Other);
    assert!(
        o.proof
            .line()
            .starts_with("FAIL: 7 bots; the sidebar shows 7 rows"),
        "{}",
        o.proof.line()
    );
}

#[test]
fn selectors_exact_id_name_prefix_ambiguous_none() {
    let mut e = eng(&[]);
    let bots = e.bots().value.unwrap();
    assert_eq!(Engine::resolve(&bots, MAIN).unwrap().name, "Main Bot");
    assert_eq!(Engine::resolve(&bots, "main bot").unwrap().id, MAIN);
    assert_eq!(Engine::resolve(&bots, "led").unwrap().name, "Ledger Bot");
    let amb = Engine::resolve(&bots, "m").unwrap_err();
    assert_eq!(amb.exit, ExitCode::Other);
    assert!(
        amb.why.contains("ambiguous")
            && amb.why.contains("Main Bot")
            && amb.why.contains("Mirror Bot")
    );
    assert_eq!(
        Engine::resolve(&bots, "nobody").unwrap_err().exit,
        ExitCode::Other
    );
    // through a command: ambiguous selector is exit 1 and sends nothing
    let o = e.send("m", "hi", &[]);
    assert_eq!(o.exit, ExitCode::Other);
}

// ---------------------------------------------------------------- ask over every turn script

#[test]
fn ask_reply_ready_returns_reply_and_idle() {
    let mut e = eng(&["reply-ready"]);
    let o = e.ask("main", "Reply READY", &[], &w());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let r = o.value.unwrap();
    assert_eq!(r.state, BotState::Idle);
    assert_eq!(r.messages.len(), 1);
    assert_eq!(r.messages[0].text, "READY");
    assert_eq!(r.messages[0].role, Role::Bot);
    assert!(r.waited_s >= 8.0 && r.waited_s < 20.0, "{}", r.waited_s);
    assert_eq!(
        o.proof.line(),
        "PASS: reply received; bot idle 8 s; sidebar row idle"
    );
    assert_eq!(o.proof.route, "dom-rows");
}

#[test]
fn ask_long_reply_still_composing_is_exit_2() {
    let mut e = eng(&["long-reply"]);
    let o = e.ask(
        "main",
        "write a lot",
        &[],
        &WaitOpts {
            timeout_s: 60.0,
            settle_s: 8.0,
        },
    );
    assert_eq!(o.exit, ExitCode::StillWorking);
    let r = o.value.unwrap();
    assert_eq!(r.state, BotState::Working);
    assert!(o.proof.line().contains("bot still working"));
    assert!(o.hint.unwrap().contains("botport grok wait"));
}

#[test]
fn ask_tool_run_with_escape_ignored_times_out_working() {
    let mut e = eng(&["tool-run-escape-ignored"]);
    let o = e.ask(
        "main",
        "run a long tool",
        &[],
        &WaitOpts {
            timeout_s: 30.0,
            settle_s: 8.0,
        },
    );
    assert_eq!(o.exit, ExitCode::StillWorking);
    assert_eq!(o.value.unwrap().bot.state, BotState::Working);
}

#[test]
fn ask_needs_approval_is_exit_3_and_never_answers() {
    let mut e = eng(&["needs-approval"]);
    let o = e.ask("main", "run echo", &[], &w());
    assert_eq!(o.exit, ExitCode::NeedsUser, "{}", o.proof.line());
    let r = o.value.unwrap();
    assert_eq!(r.state, BotState::NeedsInput);
    let n = r.bot.needs_input.unwrap();
    assert_eq!(n.kind, NeedsKind::Approval);
    assert!(n.reason.starts_with("Permission needed"));
    assert!(r
        .messages
        .iter()
        .any(|m| m.kind == MsgKind::Approval && m.answered.is_none()));
    assert!(o
        .proof
        .line()
        .starts_with("FAIL: waiting for you; Permission needed"));
    assert!(o.hint.unwrap().contains("floor 2"));
    // returned straight away (no settle wait for needs-input)
    assert!(r.waited_s < 8.0, "{}", r.waited_s);
    // floor 2: nothing else was sent; the only user message after ours is none
    let msgs = e.read("main", None, 0).value.unwrap();
    let ours = msgs
        .iter()
        .filter(|m| m.role == Role::User && m.text == "run echo")
        .count();
    assert_eq!(ours, 1);
    let last_user = msgs.iter().rev().find(|m| m.role == Role::User).unwrap();
    assert_eq!(last_user.text, "run echo");
}

#[test]
fn ask_routine_created_sees_the_event_and_ignores_the_stale_card() {
    let mut e = eng(&["routine-created"]);
    let o = e.ask("main", "add a routine", &[], &w());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let r = o.value.unwrap();
    assert_eq!(r.messages.last().unwrap().text, "ROUTINE OK");
}

#[test]
fn ask_steer_stop_turn_finishes() {
    let mut e = eng(&["steer-stop"]);
    let o = e.ask("main", "stop now", &[], &w());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.value.unwrap().messages.last().unwrap().text, "STOPPED");
}

#[test]
fn ask_with_files_is_unavailable_in_p1() {
    let mut e = eng(&["reply-ready"]);
    let o = e.ask(
        "main",
        "see file",
        &[std::path::PathBuf::from("/nonexistent")],
        &w(),
    );
    assert_eq!(o.exit, ExitCode::Unavailable);
}

// ---------------------------------------------------------------- send / wait / read

#[test]
fn send_passes_on_the_transcript_route() {
    let mut e = eng(&[]);
    let o = e.send("ledger", "hello", &[]);
    assert_eq!(o.exit, ExitCode::Done);
    let v = o.value.unwrap();
    assert!(v.key.ends_with('u'));
    assert_eq!(
        o.proof.line(),
        format!("PASS: message {} sent; seen in transcript", v.key)
    );
    assert_eq!(o.proof.route, "transcript");
    assert_eq!(e.send("ledger", "   ", &[]).exit, ExitCode::Other);
}

/// Live 1.1.0: the app stores "a\nb" as "a\n\nb"; send still finds and confirms our message (zero-touch run).
#[test]
fn send_passes_for_a_multi_line_message_the_app_stores_with_doubled_line_breaks() {
    let mut e = eng(&[]);
    let o = e.send(
        "ledger",
        "Export yourself.\nRules: none.\n{\"a\": 1,\n \"b\": []}",
        &[],
    );
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
}

#[test]
fn send_to_a_bot_awaiting_approval_is_refused_with_exit_3_and_sends_nothing() {
    let mut e = eng_awaiting();
    let before = e.read("temp", None, 0).value.unwrap().len();
    let o = e.send("temp", "anything", &[]);
    assert_eq!(o.exit, ExitCode::NeedsUser);
    assert!(o.hint.unwrap().contains("floor 2"));
    assert_eq!(e.read("temp", None, 0).value.unwrap().len(), before);
    let s = e.stop("temp", None);
    assert_eq!(s.exit, ExitCode::NeedsUser);
}

#[test]
fn bots_shows_needs_input_from_the_awaiting_field() {
    let mut e = eng_awaiting();
    let b = e.bots().value.unwrap();
    let t = b.iter().find(|b| b.id == TEMP).unwrap();
    assert_eq!(t.state, BotState::NeedsInput);
    assert_eq!(t.needs_input.as_ref().unwrap().kind, NeedsKind::Approval);
}

#[test]
fn wait_on_awaiting_bot_is_exit_3() {
    let mut e = eng_awaiting();
    let o = e.wait("temp", None, &w());
    assert_eq!(o.exit, ExitCode::NeedsUser);
}

#[test]
fn wait_with_nothing_happening_times_out_no_reply() {
    let mut e = eng(&[]);
    let k = e.send("scout", "hello", &[]).value.unwrap().key;
    let o = e.wait(
        "scout",
        Some(&k),
        &WaitOpts {
            timeout_s: 20.0,
            settle_s: 8.0,
        },
    );
    assert_eq!(o.exit, ExitCode::StillWorking);
    assert!(
        o.proof.line().contains("no reply yet"),
        "{}",
        o.proof.line()
    );
}

#[test]
fn wait_after_running_turn_defaults_to_our_last_message() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_turn("reply-ready");
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let k = e.send("main", "go", &[]).value.unwrap().key;
    let o = e.wait("main", None, &w());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.value.unwrap().messages[0].text, "READY");
    // and with an explicit --since
    let o = e.wait("main", Some(&k), &w());
    assert_eq!(o.exit, ExitCode::Done);
}

/// Live 1.1.0 (zero-touch acceptance): the debug-port websocket ended with EOF mid-wait and ask exited 5
/// ("failed to fill whole buffer") while the bot was still answering. wait reconnects and keeps polling.
#[test]
fn wait_survives_a_dropped_connection_by_reconnecting() {
    let c = clock();
    let lost = || TransportError::Unavailable("debug port: failed to fill whole buffer".into());
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_turn("reply-ready")
        .fail_next(Cap::StateFlags, lost())
        .fail_next(Cap::StateFlags, lost());
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    e.send("main", "go", &[]);
    let o = e.wait("main", None, &w());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
}

#[test]
fn wait_still_exits_5_when_the_connection_keeps_dropping() {
    let c = clock();
    let lost = || TransportError::Unavailable("debug port: failed to fill whole buffer".into());
    let mut f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_turn("reply-ready");
    for _ in 0..10 {
        f = f.fail_next(Cap::StateFlags, lost());
    }
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    e.send("main", "go", &[]);
    assert_eq!(e.wait("main", None, &w()).exit, ExitCode::Unavailable);
}

#[test]
fn read_limit_and_since() {
    let mut e = eng(&[]);
    let all = e.read("main", None, 0).value.unwrap();
    assert!(all.len() > 40);
    assert!(all.iter().all(|m| !m.key.is_empty()));
    let last3 = e.read("main", None, 3);
    assert_eq!(last3.value.as_ref().unwrap().len(), 3);
    assert_eq!(
        last3.proof.line(),
        format!(
            "PASS: 3 messages up to {}; self-check",
            all.last().unwrap().key
        )
    );
    let k = &all[all.len() - 5].key;
    assert_eq!(e.read("main", Some(k), 0).value.unwrap().len(), 4);
    // kinds the app shows are mapped
    for k in [
        MsgKind::Text,
        MsgKind::Question,
        MsgKind::Approval,
        MsgKind::RoutineEvent,
        MsgKind::Feedback,
    ] {
        assert!(all.iter().any(|m| m.kind == k), "{k:?}");
    }
    // a bot with no transcript in the fixtures
    assert_eq!(e.read("trip", None, 0).value.unwrap().len(), 0);
}

// ---------------------------------------------------------------- stop

#[test]
fn stop_sends_a_steer_and_verifies_idle() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_running(MAIN, "tool-run-escape-ignored")
        .unwrap()
        .with_turn("steer-stop");
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    assert_eq!(e.bots().value.unwrap()[0].state, BotState::Working);
    let o = e.stop("main", None);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let v = o.value.unwrap();
    assert_eq!(v.method, StopMethod::Steer);
    assert!(
        v.idle_after_s > 3.0 && v.idle_after_s < 10.0,
        "{}",
        v.idle_after_s
    );
    assert!(
        o.proof.line().starts_with("PASS: bot idle ")
            && o.proof.line().ends_with("after stop; sidebar row idle")
    );
    let msgs = e.read("main", None, 0).value.unwrap();
    assert!(msgs
        .iter()
        .any(|m| m.role == Role::User && m.text.contains("stop the current task")));
}

#[test]
fn stop_that_does_not_take_is_exit_2() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_running(MAIN, "tool-run-escape-ignored")
        .unwrap()
        .with_turn("tool-run-escape-ignored");
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    assert_eq!(
        e.stop("main", Some("please stop")).exit,
        ExitCode::StillWorking
    );
}

#[test]
fn stop_on_an_idle_bot_does_nothing() {
    let mut e = eng(&[]);
    let before = e.read("main", None, 0).value.unwrap().len();
    let o = e.stop("main", None);
    assert_eq!(o.exit, ExitCode::Done);
    assert!(o.proof.line().starts_with("PASS: nothing to stop"));
    assert_eq!(e.read("main", None, 0).value.unwrap().len(), before);
}

#[test]
fn stop_is_refused_for_a_bot_callable_mcp_caller() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_running(MAIN, "tool-run-escape-ignored")
        .unwrap();
    let mut e = engine_from(f, c, Policy::default(), Caller::Mcp { bot_callable: true });
    let o = e.stop("main", None);
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("refused by mcp"));
}

// ---------------------------------------------------------------- watch

#[test]
fn watch_reports_working_idle_and_the_new_message() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_running(MAIN, "reply-ready")
        .unwrap();
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    assert_eq!(e.bots().value.unwrap()[0].state, BotState::Working);
    let mut ev = vec![];
    let o = e.watch(Some("main"), Some(60.0), &mut |x| ev.push(x.clone()));
    assert_eq!(o.exit, ExitCode::Done);
    let kinds: Vec<&str> = ev.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"idle") && kinds.contains(&"message"),
        "{kinds:?}"
    );
    let m = ev
        .iter()
        .find(|e| e.kind == "message" && e.message.is_some())
        .unwrap();
    assert_eq!(m.message.as_ref().unwrap().text, "READY");
    assert!(
        o.proof.line().starts_with("PASS: watched 1.0 min,"),
        "{}",
        o.proof.line()
    );
}

#[test]
fn watch_stops_at_the_port_ttl_and_on_interrupt() {
    let mut e = eng(&[]);
    let now = e.clock.now_ms();
    e.deadline_ms = Some(now + 10_000);
    let o = e.watch(None, None, &mut |_| {});
    assert!(
        o.proof.line().contains("port TTL reached"),
        "{}",
        o.proof.line()
    );
    let mut e = eng(&[]);
    e.interrupted
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(e
        .watch(None, None, &mut |_| {})
        .proof
        .line()
        .contains("interrupted"));
}

// ---------------------------------------------------------------- versions, drift, faults

#[test]
fn unknown_version_reads_warn_and_writes_need_trust() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_version("9.9.9")
        .with_turn("reply-ready");
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let b = e.bots();
    assert_eq!(b.exit, ExitCode::Done);
    assert!(b.warnings.iter().any(|w| w.contains("9.9.9")));
    let s = e.send("main", "hi", &[]);
    assert_eq!(s.exit, ExitCode::Refused);
    assert!(s.proof.line().contains("not a tested version"));
    assert!(s.proof.line().contains("unknown_version_writes"));
    e.trust_version = true;
    let a = e.ask("main", "hi", &[], &w());
    assert_eq!(a.exit, ExitCode::Done, "{}", a.proof.line());
    assert!(a.warnings.iter().any(|w| w.contains("--trust-version")));
    // balanced: trust is not enough (canary needed)
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_version("9.9.9");
    let mut e = engine_from(
        f,
        c,
        Policy::preset(botport_grok::Preset::Balanced),
        Caller::Cli { tty: false },
    );
    e.trust_version = true;
    assert_eq!(e.send("main", "hi", &[]).exit, ExitCode::Refused);
}

#[test]
fn schema_drift_fails_closed_naming_the_field() {
    let s = Scratch::new("drift");
    copy_fixtures(&s.0);
    let p = s.0.join("roster.json");
    let txt = std::fs::read_to_string(&p)
        .unwrap()
        .replace("\"isRunningTurn\"", "\"isRunningTurnRenamed\"");
    std::fs::write(&p, txt).unwrap();
    let c = clock();
    let mut e = engine_from(
        FakeTransport::load(&s.0, c.clone()).unwrap(),
        c,
        Policy::default(),
        Caller::Cli { tty: false },
    );
    let o = e.bots();
    assert_eq!(o.exit, ExitCode::Other);
    assert!(
        o.proof.line().contains("agents[].isRunningTurn") && o.proof.line().contains("1.1.0"),
        "{}",
        o.proof.line()
    );
    // chat drift
    let s2 = Scratch::new("drift2");
    copy_fixtures(&s2.0);
    let p = s2.0.join("transcript-idle.json");
    let txt = std::fs::read_to_string(&p)
        .unwrap()
        .replace("\"entries\"", "\"rows\"");
    std::fs::write(&p, txt).unwrap();
    let c = clock();
    let mut e = engine_from(
        FakeTransport::load(&s2.0, c.clone()).unwrap(),
        c,
        Policy::default(),
        Caller::Cli { tty: false },
    );
    let o = e.read("main", None, 0);
    assert_eq!(o.exit, ExitCode::Other);
    assert!(
        o.proof.line().contains("chat.entries"),
        "{}",
        o.proof.line()
    );
}

#[test]
fn transport_faults_map_to_exit_codes() {
    let cases = [
        (
            TransportError::Unavailable("app not running".into()),
            ExitCode::Unavailable,
        ),
        (
            TransportError::BotComputerAsleep("asleep".into()),
            ExitCode::Unavailable,
        ),
        (TransportError::App("boom".into()), ExitCode::Other),
        (TransportError::Floor("floor 6".into()), ExitCode::Refused),
        (
            TransportError::Unsupported(Cap::ListBots),
            ExitCode::Unavailable,
        ),
    ];
    for (err, code) in cases {
        let c = clock();
        let f = FakeTransport::load(&fx(), c.clone())
            .unwrap()
            .fail_next(Cap::ListBots, err.clone());
        let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
        assert_eq!(e.bots().exit, code, "{err:?}");
    }
}

#[test]
fn careful_allowlist_refuses_unlisted_bots_with_exit_4() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone()).unwrap();
    let mut p = Policy::preset(botport_grok::Preset::Careful);
    p.set("bots_scope", "Ledger Bot").unwrap();
    let mut e = engine_from(f, c, p, Caller::Cli { tty: false });
    let o = e.send("main", "hi", &[]);
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("bots_scope"));
    assert!(o.hint.unwrap().contains("policy set bots_scope"));
    assert_eq!(e.send("ledger", "hi", &[]).exit, ExitCode::Done);
    assert_eq!(e.bots().exit, ExitCode::Done);
}

#[test]
fn action_log_records_commands_without_message_text() {
    let s = Scratch::new("log");
    let mut e = eng(&["reply-ready"]);
    e.log = botport_grok::log::ActionLog::new(Some(s.0.join("grok-actions.jsonl")));
    e.ask("main", "PRIVATE-TEXT-MARKER", &[], &w());
    e.bots();
    let log = std::fs::read_to_string(s.0.join("grok-actions.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);
    assert!(!log.contains("PRIVATE-TEXT-MARKER"));
    assert!(log.contains("\"cmd\":\"ask\"") && log.contains("\"exit\":0"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(s.0.join("grok-actions.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }
    // opt-out
    let mut e = eng(&[]);
    e.policy.action_log = false;
    e.log = botport_grok::log::ActionLog::new(Some(s.0.join("off.jsonl")));
    e.bots();
    assert!(!s.0.join("off.jsonl").exists());
}

#[test]
fn proof_lines_never_carry_secret_shapes() {
    // built at run time so the source itself holds no key-shaped literal (the fixture gate scans it)
    let key = ["crsr", "abcdef0123456789"].join("_");
    let p = botport_grok::Proof::fail("x", format!("token {key} and more"));
    assert!(!p.line().contains("crsr_"));
    assert!(p.line().contains("[secret]"));
}

// ---------------------------------------------------------------- large account (virtualized sidebar)

fn eng_large(top: usize) -> (Engine, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_bots(64)
        .with_virtual_sidebar(14, top);
    let n = f.sidebar_scrolls();
    (
        engine_from(f, c, Policy::default(), Caller::Cli { tty: false }),
        n,
    )
}

#[test]
fn bots_passes_when_the_sidebar_renders_a_consistent_subset() {
    let (mut e, _) = eng_large(0);
    let o = e.bots();
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.value.unwrap().len(), 64);
    assert_eq!(
        o.proof.line(),
        "PASS: 64 bots; 15 of 64 rendered rows match (virtualized sidebar)"
    );
}

#[test]
fn read_scrolls_the_sidebar_to_an_off_screen_bot() {
    let (mut e, n) = eng_large(0);
    let o = e.read("Bulk Bot 50", None, 5);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(n.load(std::sync::atomic::Ordering::SeqCst) > 1);
}

#[test]
fn read_scrolls_back_up_to_a_bot_above_the_window() {
    let (mut e, n) = eng_large(49);
    let o = e.read("Ledger Bot", None, 5);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(n.load(std::sync::atomic::Ordering::SeqCst) >= 1);
}

// ------------------------------------------- large account, chat open: collapsed sidebar (botport#64)

fn eng_large_collapsed(top: usize) -> (Engine, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_turn("reply-ready")
        .with_bots(64)
        .with_virtual_sidebar(14, top)
        .with_collapsed_sidebar();
    let n = f.sidebar_scrolls();
    (
        engine_from(f, c, Policy::default(), Caller::Cli { tty: false }),
        n,
    )
}

#[test]
fn bots_on_64_bots_in_a_collapsed_sidebar_find_the_main_row() {
    let (mut e, _) = eng_large_collapsed(0);
    let o = e.bots();
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let bots = o.value.unwrap();
    assert_eq!(bots.len(), 64);
    assert_eq!(
        o.proof.line(),
        "PASS: 64 bots; 15 of 64 rendered rows match (virtualized sidebar)"
    );
}

#[test]
fn ask_and_read_on_64_bots_in_a_collapsed_sidebar() {
    let (mut e, n) = eng_large_collapsed(0);
    let o = e.read("Bulk Bot 50", None, 5);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert!(n.load(std::sync::atomic::Ordering::SeqCst) > 1);
    let o = e.ask("main", "Reply READY", &[], &w());
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
}
