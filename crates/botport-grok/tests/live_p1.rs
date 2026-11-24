//! Regressions from the P1 live acceptance run on app 1.1.0 (TEST account). Each test replays a scrubbed fixture
//! of the shape the real app produced; ids and names are synthetic.
mod common;
use botport_grok::parse;
use common::*;
use serde_json::Value;

fn load(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fx().join(name)).unwrap()).unwrap()
}

/// Live: `bots` failed its second route on every call ("the sidebar shows 9 rows that differ") because the pinned
/// main bot's row is labelled "<name>, Main Bot" while the props say "<name>".
#[test]
fn sidebar_row_labels_match_the_roster_names_including_the_pinned_main_bot() {
    let rows = load("sidebar-rows.json");
    let roster = load("roster.json");
    let mut from_rows: Vec<(String, String, bool)> = rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(parse::row)
        .map(|r| (r.id, r.name, r.main))
        .collect();
    let mut from_props: Vec<(String, String, bool)> = roster["agents"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, a)| {
            (
                a["id"].as_str().unwrap().to_string(),
                a["name"].as_str().unwrap().to_string(),
                i == 0,
            )
        })
        .collect();
    from_rows.sort();
    from_props.sort();
    assert_eq!(from_rows, from_props);
    // both suffixes on the main row, in either order
    for label in [
        "Main Bot, Main Bot, Unread activity",
        "Main Bot, Unread activity, Main Bot",
    ] {
        let r = parse::row(&serde_json::json!({"id": MAIN, "label": label, "layout": "pinned"}))
            .unwrap();
        assert_eq!(r.name, "Main Bot");
    }
    // a row awaiting approval says ", Needs attention" (live, chat-turn card)
    let r = parse::row(
        &serde_json::json!({"id": TEMP, "label": "Temp Probe Bot, Needs attention, Unread activity",
        "awaiting": "true", "layout": "expanded"}),
    )
    .unwrap();
    assert_eq!(r.name, "Temp Probe Bot");
    assert!(r.awaiting);
    // a non-pinned bot keeps its name as typed
    let r = parse::row(
        &serde_json::json!({"id": TEMP, "label": "Ops, Main Bot", "layout": "expanded"}),
    )
    .unwrap();
    assert_eq!(r.name, "Ops, Main Bot");
}

#[test]
fn bots_passes_the_second_route_with_the_live_main_row_label() {
    // the fake now labels its pinned row the way the app does
    let mut e = eng(&[]);
    let o = e.bots();
    assert_eq!(o.proof.line(), "PASS: 7 bots; matches sidebar");
}

/// Live: `send` returned the `optimistic:<nonce>` key; about 2 s later the app replaced it with `t1u`, so ask's
/// "after our message" slice found nothing and returned the whole transcript (the welcome text included), and
/// `wait --since <key>` could never match.
#[test]
fn a_sent_message_key_is_the_server_key_not_the_optimistic_one() {
    let v = load("send-confirm.json");
    let s = v["samples"].as_array().unwrap();
    let opt = s[0]["entries"].as_array().unwrap().last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(opt.starts_with(parse::OPTIMISTIC_PREFIX));
    // still optimistic: not confirmed yet
    assert_eq!(parse::confirmed_key(&s[0]["entries"], &opt), None);
    // confirmed: the same clientNonce now has the server key
    assert_eq!(
        parse::confirmed_key(&s[1]["entries"], &opt).as_deref(),
        Some("t1u")
    );
    // a server key is returned as is
    assert_eq!(
        parse::confirmed_key(&s[1]["entries"], "t0u").as_deref(),
        Some("t0u")
    );
    // and slicing after the confirmed key leaves only what came after it
    let chat = parse::chat(&s[1], "1.1.0").unwrap();
    let after = botport_grok::engine::slice_since(chat.messages, Some("t1u"), 0);
    assert!(after.is_empty());
}

/// Live: `bots` right after an automatic launch failed with "shape: sidebar props (agents, listRows) missing or
/// changed": the page was loaded but had not rendered its sidebar yet. The first read now waits for the render.
#[test]
fn the_first_read_after_launch_waits_for_the_sidebar_to_render() {
    use botport_grok::clock::{FakeClock, SharedClock};
    use botport_grok::transport::{wait_until_rendered, TransportError};
    let v = load("launch-warmup.json");
    let samples = v["samples"].as_array().unwrap().clone();
    let fc = FakeClock::new(1_800_000_000_000);
    let clock: SharedClock = fc.clone();
    let mut i = 0;
    let t0 = clock.now_ms();
    let got = wait_until_rendered(&clock, 20_000, || {
        let s = samples[i.min(samples.len() - 1)].clone();
        i += 1;
        parse::sidebar_props(s, "1.1.0")
    })
    .unwrap();
    assert_eq!(got["agents"].as_array().unwrap().len(), 2);
    assert_eq!(i, 2);
    assert!(clock.now_ms() - t0 <= 500);
    // a sidebar that never renders is still the shape error, after the timeout
    let r = wait_until_rendered(&clock, 2_000, || {
        parse::sidebar_props(samples[0].clone(), "1.1.0")
    });
    assert!(matches!(r, Err(TransportError::Shape { .. })));
    // other errors are not retried
    let mut n = 0;
    let r: Result<(), _> = wait_until_rendered(&clock, 20_000, || {
        n += 1;
        Err(TransportError::Unavailable("synthetic".into()))
    });
    assert!(r.is_err() && n == 1);
}

/// Live: `watch` message events carried `message: null` on the debug port, because it only read with `ReadAny`
/// and the debug port has `ReadOpen` (the bot on screen). The open bot's new message is now included.
#[test]
fn watch_message_events_carry_the_open_bots_message_with_read_open_only() {
    use botport_grok::model::BotState;
    use botport_grok::policy::{Caller, Policy};
    use botport_grok::transport::fake::FakeTransport;
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_running(MAIN, "reply-ready")
        .unwrap()
        .read_open_only(MAIN);
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    assert_eq!(e.bots().value.unwrap()[0].state, BotState::Working);
    let mut ev = vec![];
    e.watch(Some("main"), Some(60.0), &mut |x| ev.push(x.clone()));
    let m = ev
        .iter()
        .find(|e| e.kind == "message")
        .expect("a message event");
    assert_eq!(m.message.as_ref().expect("with its message").text, "READY");
}

#[test]
fn watch_never_reads_a_bot_that_is_not_open() {
    use botport_grok::policy::{Caller, Policy};
    use botport_grok::transport::fake::FakeTransport;
    let c = clock();
    // the running bot is MAIN, but the open one is another bot: the event comes, with no text
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_running(MAIN, "reply-ready")
        .unwrap()
        .read_open_only(TEMP);
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let mut ev = vec![];
    e.watch(Some("main"), Some(60.0), &mut |x| ev.push(x.clone()));
    let m = ev
        .iter()
        .find(|e| e.kind == "message")
        .expect("a message event");
    assert!(m.message.is_none());
}

/// Live 1.1.0 (zero-touch acceptance, 16 Nov): a multi-line message is stored with each line break as a paragraph
/// break, so `send` never matched it and exited 1 although the message was sent (twin's export request).
#[test]
fn a_multi_line_message_matches_with_its_line_breaks_doubled() {
    let sent = "Please export yourself.\nRules: none.\n\n{\"format\": \"x\",\n \"skills\": []}";
    let stored =
        "Please export yourself.\n\nRules: none.\n\n{\"format\": \"x\",\n\n \"skills\": []}";
    assert!(parse::same_sent_text(stored, sent));
    assert!(parse::same_sent_text(&format!("  {sent}\n"), sent));
    assert!(!parse::same_sent_text(
        "Please export yourself. Rules: some.",
        sent
    ));
    assert!(!parse::same_sent_text("", sent));
}

/// Large account (live 2026-11-16): with a chat open the sidebar is "collapsed" and the main row is not
/// "pinned"; its ", Main Bot" suffix still marks the main bot.
#[test]
fn a_collapsed_sidebar_main_row_is_still_the_main_bot() {
    let r = parse::row(
        &serde_json::json!({"id": TEMP, "label": "Ops · Desk, Main Bot", "layout": "collapsed"}),
    )
    .unwrap();
    assert_eq!(r.name, "Ops · Desk");
    assert!(r.main);
    let r = parse::row(
        &serde_json::json!({"id": TEMP, "label": "Ops, Unread activity", "layout": "collapsed"}),
    )
    .unwrap();
    assert_eq!(r.name, "Ops");
    assert!(!r.main);
}

/// Live on app 1.2.3 (24 Nov 2026): a busy bot's row label gains ", Working" after any other suffix
/// ("<name>, Working", "<name>, Unread activity, Working", "<name>, Main Bot, Unread activity"), so `bots` failed its
/// second route and the canary blocked writes. Names here are synthetic.
#[test]
fn app_1_2_3_working_suffix_is_stripped_in_any_order() {
    let cases = [
        ("Acme Support, Working", "Acme Support", false),
        (
            "Acme Support, Unread activity, Working",
            "Acme Support",
            false,
        ),
        (
            "Acme Support, Working, Unread activity",
            "Acme Support",
            false,
        ),
        ("Front Desk, Main Bot, Unread activity", "Front Desk", true),
        ("Front Desk, Main Bot, Working", "Front Desk", true),
        ("Plain Name", "Plain Name", false),
    ];
    for (label, name, main) in cases {
        let v = serde_json::json!({"id": "a1", "label": label, "layout": if main { "pinned" } else { "expanded" }});
        let r = parse::row(&v).unwrap();
        assert_eq!((r.name.as_str(), r.main), (name, main), "label {label:?}");
    }
}

/// Live on app 1.2.3 (24 Nov 2026): the bot's message box held a three-letter unsent draft; `Input.insertText`
/// typed after it, so the message went out with the draft in front and the send then failed its own check.
#[test]
fn a_send_refuses_while_the_message_box_holds_a_draft() {
    assert_eq!(parse::draft_refusal(0), None);
    let why = parse::draft_refusal(3).unwrap();
    assert!(why.contains("unsent draft (3 characters)"), "{why}");
    assert!(why.contains("does not send or clear it"), "{why}");
}
