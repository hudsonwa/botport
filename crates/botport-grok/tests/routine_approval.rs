//! Routine approval cards (control/ROUTINE-TRUTH.md): `awaitingUserResponse` stays null and the row never says
//! "Waiting for you". Signals: the row / lastEntry text "Permission required: …", a pending permission card in
//! the transcript (later "expired"), and a routine run "running" with no finish time. All exit 3.
mod common;
use botport_grok::engine::{ExitCode, WaitOpts};
use botport_grok::model::*;
use botport_grok::parse;
use botport_grok::policy::{Caller, Policy};
use botport_grok::transport::fake::FakeTransport;
use common::*;
use serde_json::{json, Value};

const FILE: &str = "transcript-routine-approval.json";

fn fixture() -> Value {
    serde_json::from_str(&std::fs::read_to_string(fx().join(FILE)).unwrap()).unwrap()
}

fn agent(v: &Value) -> Value {
    let cur = v["currentAgentId"].as_str().unwrap();
    v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == cur)
        .unwrap()
        .clone()
}

fn eng_routine() -> botport_grok::Engine {
    let c = clock();
    let f = FakeTransport::load_routine_awaiting(&fx(), c.clone()).unwrap();
    engine_from(f, c, Policy::default(), Caller::Cli { tty: false })
}

#[test]
fn the_fixture_has_no_awaiting_field_and_a_running_unfinished_run() {
    let v = fixture();
    let a = agent(&v);
    assert!(a["awaitingUserResponse"].is_null());
    assert!(parse::awaiting(&a).is_none());
    assert!(parse::routine_run_open(&v["routineRuns"][TEMP]));
}

#[test]
fn row_text_permission_required_is_needs_input() {
    let mut a = agent(&fixture());
    a["isRunning"] = json!(false);
    a["lastEntry"]["text"] = json!("Permission required: echo synthetic");
    let b = parse::agent(&a, "1.1.0").unwrap();
    assert_eq!(b.state, BotState::NeedsInput);
    let n = b.needs_input.unwrap();
    assert_eq!(n.kind, NeedsKind::Approval);
    assert!(n.reason.starts_with("Permission required:"));
    // the DOM row carries the same text in aria-description: awaiting on the second route too
    let r = parse::row(&json!({"id": TEMP, "label": "Temp Probe Bot",
        "desc": "Permission required: echo synthetic"}))
    .unwrap();
    assert!(r.awaiting);
    // ordinary last text is not
    a["lastEntry"]["text"] = json!("Synthetic text 1.");
    assert_eq!(parse::agent(&a, "1.1.0").unwrap().state, BotState::Idle);
}

#[test]
fn a_pending_card_while_the_working_flags_are_set_is_needs_input() {
    // runs 2-3: isRunning true while the card was pending; no open run known (e.g. debug port in P1)
    let v = fixture();
    let a = agent(&v);
    let mut entries = v["chat"]["entries"].as_array().unwrap().clone();
    entries.truncate(entries.len() - 2); // the card is the newest entry
    let chat = parse::chat(
        &json!({"entries": entries, "isAgentRunning": false}),
        "1.1.0",
    )
    .unwrap();
    let bot = parse::agent(&a, "1.1.0").unwrap();
    assert_eq!(bot.state, BotState::Working);
    let s = parse::snapshot(&bot, Some(&chat));
    assert_eq!(s.state, BotState::NeedsInput);
    assert_eq!(s.needs_input.unwrap().kind, NeedsKind::Approval);
}

#[test]
fn an_open_run_finds_the_card_under_a_later_chat_message() {
    let v = fixture();
    let bot = parse::agent(&agent(&v), "1.1.0").unwrap();
    let chat = parse::chat(&v["chat"], "1.1.0").unwrap();
    // without the run signal, the newest bot entry is a reply: just working
    assert_eq!(parse::snapshot(&bot, Some(&chat)).state, BotState::Working);
    let s = parse::snapshot_with(&bot, Some(&chat), true);
    assert_eq!(s.state, BotState::NeedsInput);
    assert!(s
        .needs_input
        .unwrap()
        .reason
        .starts_with("Permission needed"));
}

#[test]
fn an_expired_card_and_a_finished_run_are_not_needs_input() {
    let v = fixture();
    let mut a = agent(&v);
    a["isRunning"] = json!(false);
    let mut chat_v = v["chat"].clone();
    let n = chat_v["entries"].as_array().unwrap().len();
    chat_v["entries"][n - 3]["message"]["ask"]["status"] = json!("expired");
    let mut runs = v["routineRuns"][TEMP].clone();
    runs[0]["status"] = json!("error");
    runs[0]["finishedAt"] = json!(1_794_802_455_000u64);
    assert!(!parse::routine_run_open(&runs));
    let bot = parse::agent(&a, "1.1.0").unwrap();
    let chat = parse::chat(&chat_v, "1.1.0").unwrap();
    let s = parse::snapshot_with(&bot, Some(&chat), parse::routine_run_open(&runs));
    assert_eq!(s.state, BotState::Idle);
    // the run list shapes: {automations:[{runs}]}, [{runs}], [run]
    let open = json!({"status": "running", "finishedAt": null});
    assert!(parse::routine_run_open(
        &json!({"automations": [{"runs": [open.clone()]}]})
    ));
    assert!(parse::routine_run_open(&json!([{"runs": [open.clone()]}])));
    assert!(parse::routine_run_open(&json!([open])));
    assert!(!parse::routine_run_open(&json!({"kind": "off"})));
}

#[test]
fn engine_wait_ask_send_and_bots_all_say_exit_3() {
    let mut e = eng_routine();
    let b = e.bots().value.unwrap();
    let t = b.iter().find(|b| b.id == TEMP).unwrap();
    assert_eq!(t.state, BotState::NeedsInput);
    let w = e.wait(TEMP, None, &WaitOpts::default());
    assert_eq!(w.exit, ExitCode::NeedsUser, "{}", w.proof.line());
    assert!(w.hint.unwrap().contains("floor 2"));
    // not stale: the routine card is older than the chat message, but a chat message does not cancel it
    let mut e = eng_routine();
    let a = e.ask(TEMP, "hello", &[], &WaitOpts::default());
    assert_eq!(a.exit, ExitCode::NeedsUser, "{}", a.proof.line());
}

#[test]
fn cli_scenario_routine_approval_runs_offline() {
    let s = Scratch::new("routine-cli");
    std::env::set_var("BOTPORT_HOME", &s.0);
    let args: Vec<String> = [
        "wait",
        TEMP,
        "--transport",
        "fake",
        "--scenario",
        "routine-approval",
        "--no-log",
        "--json",
        "--fixtures",
    ]
    .iter()
    .map(|x| x.to_string())
    .chain([fx().to_string_lossy().into_owned()])
    .collect();
    assert_eq!(botport_grok::cli::main(&args), 3);
}
