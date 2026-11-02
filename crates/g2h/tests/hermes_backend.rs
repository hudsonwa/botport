mod support;

use g2h::hermes::{
    A2aBackend, A2aOptions, GatewayOptions, GatewayWsBackend, PeerBackend, PeerError,
};
use std::time::{Duration, Instant};
use support::FakeGateway;

const T: Duration = Duration::from_secs(5);

fn opts(fg: &FakeGateway) -> GatewayOptions {
    let mut o = GatewayOptions::new(fg.path());
    o.connect_wait = Duration::from_secs(3);
    o
}

#[tokio::test]
async fn ask_round_trip_streams_deltas_and_reports_usage() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    let r = be.ask("c1", "ping", T).await.unwrap();
    assert_eq!(r.text, "pong");
    assert_eq!(r.usage_tokens, Some(42));
    assert!(r.first_delta_ms.is_some());
    assert!(r.total_ms >= r.first_delta_ms.unwrap());
    // dedicated, titled peer session; prompt sent to it
    let creates = fg.calls_of("session.create");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0]["title"], "g2h peer c1");
    let submits = fg.calls_of("prompt.submit");
    assert_eq!(submits[0]["text"], "ping");
    // second turn on same conv reuses the session, no new create
    be.ask("c1", "again", T).await.unwrap();
    assert_eq!(fg.calls_of("session.create").len(), 1);
    // a different conv gets its own session
    be.ask("c2", "x", T).await.unwrap();
    assert_eq!(fg.calls_of("session.create").len(), 2);
}

#[tokio::test]
async fn drop_mid_turn_errors_then_reconnects_and_resumes() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    be.ask("c1", "one", T).await.unwrap();
    let stored = fg
        .state
        .lock()
        .unwrap()
        .stored
        .keys()
        .next()
        .unwrap()
        .clone();
    fg.state.lock().unwrap().drop_mid_turn_once = true;
    let e = be.ask("c1", "two", T).await.unwrap_err();
    assert!(matches!(e, PeerError::Disconnected(_)), "{e:?}");
    let r = be.ask("c1", "three", T).await.unwrap();
    assert_eq!(r.text, "pong");
    let resumes = fg.calls_of("session.resume");
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["session_id"], stored.as_str());
    assert_eq!(
        fg.calls_of("session.create").len(),
        1,
        "never a second session"
    );
    // two connections, each with its own freshly minted ticket
    let s = fg.state.lock().unwrap();
    assert_eq!(s.connections.len(), 2);
    assert_ne!(s.connections[0].1, s.connections[1].1);
}

#[tokio::test]
async fn idle_drop_is_transparent() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    be.ask("c1", "one", T).await.unwrap();
    fg.drop_connections();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(be.ask("c1", "two", T).await.unwrap().text, "pong");
    assert_eq!(fg.calls_of("session.resume").len(), 1);
}

#[tokio::test]
async fn reconnect_re_resolves_endpoint_and_ticket() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    be.ask("c1", "one", T).await.unwrap();
    let port1 = fg.state.lock().unwrap().port;
    fg.rotate_endpoint().await;
    let port2 = fg.state.lock().unwrap().port;
    assert_ne!(port1, port2);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(be.ask("c1", "two", T).await.unwrap().text, "pong");
    let s = fg.state.lock().unwrap();
    assert_eq!(s.identify_calls, 2, "endpoint re-resolved on reconnect");
    assert_eq!(s.minted.len(), 2, "ticket re-minted on reconnect");
    assert_eq!(s.connections.last().unwrap().0, port2);
    assert!(s.connections.last().unwrap().1.starts_with("tkt-2-"));
}

#[tokio::test]
async fn deadline_interrupts_the_turn() {
    let fg = FakeGateway::start().await;
    fg.state.lock().unwrap().hang = true;
    let be = GatewayWsBackend::new(opts(&fg));
    let t0 = Instant::now();
    let e = be
        .ask("c1", "slow", Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(matches!(e, PeerError::Deadline), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ints = fg.calls_of("session.interrupt");
    assert_eq!(ints.len(), 1);
    assert_eq!(
        ints[0]["session_id"],
        fg.calls_of("prompt.submit")[0]["session_id"]
    );
}

#[tokio::test]
async fn bounded_wait_gives_unavailable() {
    let fg = FakeGateway::start().await;
    fg.shutdown();
    let mut o = opts(&fg);
    o.connect_wait = Duration::from_millis(400);
    let be = GatewayWsBackend::new(o);
    let t0 = Instant::now();
    let e = be.ask("c1", "hi", T).await.unwrap_err();
    assert!(matches!(e, PeerError::Unavailable(_)), "{e:?}");
    let el = t0.elapsed();
    assert!(
        el >= Duration::from_millis(300) && el < Duration::from_secs(2),
        "{el:?}"
    );
    assert!(!be.health().await.ok);
}

#[test]
fn backend_never_spawns_processes() {
    for src in [
        include_str!("../src/hermes/mod.rs"),
        include_str!("../src/hermes/endpoint.rs"),
        include_str!("../src/hermes/gateway.rs"),
        include_str!("../src/hermes/a2a.rs"),
    ] {
        assert!(!src.contains("Command::new") && !src.contains("process::Command"));
    }
}

#[tokio::test]
async fn sessions_persist_across_backend_restarts() {
    let fg = FakeGateway::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(&fg);
    o.sessions_file = Some(dir.path().join("peer-sessions.json"));
    GatewayWsBackend::new(o.clone())
        .ask("c1", "one", T)
        .await
        .unwrap();
    GatewayWsBackend::new(o).ask("c1", "two", T).await.unwrap();
    assert_eq!(fg.calls_of("session.create").len(), 1);
    assert_eq!(fg.calls_of("session.resume").len(), 1);
}

#[tokio::test]
async fn health_ok_against_ready_owner() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    assert!(be.health().await.ok);
}

#[tokio::test]
async fn a2a_round_trip_and_deadline() {
    let (url, seen) = support::fake_a2a("hello back", 0).await;
    let be = A2aBackend::new(A2aOptions::new(&url));
    let r = be.ask("conv-9", "hello", T).await.unwrap();
    assert_eq!(r.text, "hello back");
    let req = seen.lock().unwrap()[0].clone();
    assert_eq!(req["method"], "SendMessage");
    assert_eq!(req["params"]["message"]["contextId"], "conv-9");
    assert_eq!(req["params"]["message"]["parts"][0]["text"], "hello");

    let (url, _) = support::fake_a2a("late", 2000).await;
    let be = A2aBackend::new(A2aOptions::new(&url));
    let e = be
        .ask("c", "x", Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(e, PeerError::Deadline), "{e:?}");
}

/// Client overhead per message vs the fake (no model time). Run with
/// `cargo test --release -p g2h --test hermes_backend -- --ignored bench_ --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn bench_client_overhead() {
    let fg = FakeGateway::start().await;
    fg.state.lock().unwrap().deltas = vec!["pong".into()];
    let be = GatewayWsBackend::new(opts(&fg));
    be.ask("b", "warm", T).await.unwrap();
    let n = 500;
    let t0 = Instant::now();
    for _ in 0..n {
        be.ask("b", "ping", T).await.unwrap();
    }
    let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("client+fake round trip: {per:.3} ms/message over {n} messages");
    assert!(per < 1.0, "{per} ms");
}

/// Live check, manual, needs a live Hermes:
/// `G2H_LIVE_HERMES_HOME=<profile home> cargo test -p g2h --test hermes_backend -- --ignored live_gateway_pong --nocapture`
#[tokio::test]
#[ignore]
async fn live_gateway_pong() {
    let Ok(home) = std::env::var("G2H_LIVE_HERMES_HOME") else {
        eprintln!("G2H_LIVE_HERMES_HOME not set; skipping");
        return;
    };
    let be = GatewayWsBackend::new(GatewayOptions::new(home.into()));
    let conv = format!("live-pong-{}", std::process::id());
    for i in 0..5 {
        let r = be
            .ask(&conv, "Reply with exactly: pong", Duration::from_secs(120))
            .await
            .unwrap();
        println!(
            "turn {i}: ttfd={:?}ms total={}ms tokens={:?} text={:?}",
            r.first_delta_ms, r.total_ms, r.usage_tokens, r.text
        );
    }
}

#[tokio::test]
async fn create_and_submit_params_pass_gateway_validation() {
    let fg = FakeGateway::start().await;
    let mut o = opts(&fg);
    o.peer_toolsets = vec!["web".into()];
    let be = GatewayWsBackend::new(o);
    be.ask("c1", "hi", T).await.unwrap();
    let c = &fg.calls_of("session.create")[0];
    assert_eq!(c["source"], "oneshot");
    assert_eq!(c["toolsets"], serde_json::json!(["web"]));
    assert!(c["request_id"].as_str().unwrap().len() <= 256);
    assert!(fg.calls_of("prompt.submit")[0]["input_id"].is_string());
}

#[tokio::test]
async fn deadline_caps_setup() {
    let fg = FakeGateway::start().await;
    fg.state.lock().unwrap().stall_setup = true;
    let be = GatewayWsBackend::new(opts(&fg));
    let t0 = Instant::now();
    let e = be
        .ask("c1", "hi", Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(matches!(e, PeerError::Deadline), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn deadline_caps_connect() {
    let fg = FakeGateway::start().await;
    fg.shutdown();
    let be = GatewayWsBackend::new(opts(&fg));
    let t0 = Instant::now();
    let e = be
        .ask("c1", "hi", Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(
        matches!(e, PeerError::Deadline | PeerError::Unavailable(_)),
        "{e:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn resume_refusal_other_than_not_found_keeps_stored_session() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    be.ask("c1", "hi", T).await.unwrap();
    fg.drop_connections();
    tokio::time::sleep(Duration::from_millis(100)).await;
    fg.state.lock().unwrap().resume_refusal = Some("permission_denied".into());
    let e = be.ask("c1", "hi", T).await.unwrap_err();
    assert!(matches!(e, PeerError::Rpc { .. }), "{e:?}");
    assert_eq!(fg.calls_of("session.create").len(), 1);
}

#[tokio::test]
async fn default_options_send_empty_toolsets() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    be.ask("c-tools", "hi", T).await.unwrap();
    let c = &fg.calls_of("session.create")[0];
    assert_eq!(c["toolsets"], serde_json::json!([]));
}

#[tokio::test]
async fn resume_not_found_recreates_with_fresh_request_id_persisted() {
    let fg = FakeGateway::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(&fg);
    o.sessions_file = Some(dir.path().join("sessions.json"));
    let be = GatewayWsBackend::new(o.clone());
    be.ask("c1", "hi", T).await.unwrap();
    let old = fg
        .state
        .lock()
        .unwrap()
        .stored
        .keys()
        .next()
        .unwrap()
        .clone();
    // the gateway forgets the session but still remembers its request_id
    fg.state.lock().unwrap().stored.clear();
    fg.drop_connections();
    tokio::time::sleep(Duration::from_millis(100)).await;
    be.ask("c1", "hi", T).await.unwrap();
    let creates = fg.calls_of("session.create");
    assert_eq!(creates.len(), 2);
    assert_ne!(creates[0]["request_id"], creates[1]["request_id"]);
    let new = fg
        .state
        .lock()
        .unwrap()
        .stored
        .keys()
        .next()
        .unwrap()
        .clone();
    assert_ne!(new, old, "must not get the dead session back");
    // a restarted backend that loses the session again must not reuse generation 1's id
    drop(be);
    fg.state.lock().unwrap().stored.clear();
    fg.drop_connections();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let be2 = GatewayWsBackend::new(o);
    be2.ask("c1", "hi", T).await.unwrap();
    let creates = fg.calls_of("session.create");
    assert_eq!(creates.len(), 3);
    assert_ne!(creates[2]["request_id"], creates[1]["request_id"]);
    assert_ne!(creates[2]["request_id"], creates[0]["request_id"]);
}

fn stored_ids(fg: &FakeGateway) -> Vec<String> {
    fg.calls_of("session.resume")
        .iter()
        .map(|c| c["session_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn rotates_after_max_turns_with_framed_recap() {
    let fg = FakeGateway::start().await;
    let mut o = opts(&fg);
    o.max_turns = 2;
    let be = GatewayWsBackend::new(o);
    assert!(!be.ask("c1", "first question", T).await.unwrap().rotated);
    assert!(!be.ask("c1", "second question", T).await.unwrap().rotated);
    let r = be.ask("c1", "third question", T).await.unwrap();
    assert!(r.rotated);
    let creates = fg.calls_of("session.create");
    assert_eq!(creates.len(), 2);
    assert_ne!(creates[0]["request_id"], creates[1]["request_id"]);
    let submits = fg.calls_of("prompt.submit");
    let seeded = submits[2]["text"].as_str().unwrap();
    assert!(
        seeded.starts_with("[Message from g2h recap via g2h"),
        "{seeded}"
    );
    assert!(seeded.contains("second question") && seeded.contains("pong"));
    assert!(seeded.ends_with("third question"), "{seeded}");
    assert_eq!(seeded.matches(g2h::framing::CLOSE).count(), 1, "{seeded}");
    assert_ne!(submits[2]["session_id"], submits[1]["session_id"]);
    // a fresh session again counts from zero, and is not seeded twice
    let r = be.ask("c1", "fourth", T).await.unwrap();
    assert!(!r.rotated);
    assert_eq!(fg.calls_of("prompt.submit")[3]["text"], "fourth");
    assert_eq!(fg.calls_of("session.create").len(), 2);
}

#[tokio::test]
async fn rotates_after_token_limit_and_recap_is_bounded() {
    let fg = FakeGateway::start().await;
    fg.state.lock().unwrap().context_used = Some(30_000);
    let mut o = opts(&fg);
    o.max_turns = 0;
    o.max_tokens = 24_000;
    let be = GatewayWsBackend::new(o);
    let long = "q".repeat(5000);
    be.ask("c1", &long, T).await.unwrap();
    let r = be.ask("c1", "next", T).await.unwrap();
    assert!(r.rotated);
    assert_eq!(fg.calls_of("session.create").len(), 2);
    let seeded = fg.calls_of("prompt.submit")[1]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(seeded.len() < 2200, "recap too long: {}", seeded.len());
}

#[tokio::test]
async fn policy_off_never_rotates() {
    let fg = FakeGateway::start().await;
    fg.state.lock().unwrap().usage_total = Some(1_000_000);
    let be = GatewayWsBackend::new(opts(&fg));
    for _ in 0..10 {
        assert!(!be.ask("c1", "x", T).await.unwrap().rotated);
    }
    assert_eq!(fg.calls_of("session.create").len(), 1);
}

#[tokio::test]
async fn rotation_survives_restart_and_old_session_never_resumed() {
    let fg = FakeGateway::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(&fg);
    o.sessions_file = Some(dir.path().join("peer-sessions.json"));
    o.max_turns = 2;
    GatewayWsBackend::new(o.clone())
        .ask("c1", "one", T)
        .await
        .unwrap();
    let first: Vec<String> = fg.state.lock().unwrap().stored.keys().cloned().collect();
    // restart: turn 2 resumes the same session (count persisted)
    let r = GatewayWsBackend::new(o.clone())
        .ask("c1", "two", T)
        .await
        .unwrap();
    assert!(!r.rotated);
    assert_eq!(fg.calls_of("session.create").len(), 1);
    // restart: turn 3 rotates instead of resuming the full session
    let r = GatewayWsBackend::new(o.clone())
        .ask("c1", "three", T)
        .await
        .unwrap();
    assert!(r.rotated);
    let seeded = fg.calls_of("prompt.submit")[2]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(seeded.contains("two"), "recap persisted: {seeded}");
    assert_eq!(fg.calls_of("session.create").len(), 2);
    // later restarts resume only the new session
    GatewayWsBackend::new(o).ask("c1", "four", T).await.unwrap();
    let resumed = stored_ids(&fg);
    assert_eq!(resumed.len(), 2);
    assert!(resumed[0] == first[0]);
    assert!(resumed[1] != first[0], "old session resumed after rotation");
}

/// Live context growth, manual, needs a live Hermes (dedicated peer sessions, no webhook):
/// `G2H_LIVE_HERMES_HOME=<profile home> [G2H_LIVE_POLICY=off] cargo test -p g2h --test hermes_backend -- --ignored live_context_growth --nocapture`
/// Policy on uses the config defaults; `off` sets both limits to 0.
#[tokio::test]
#[ignore]
async fn live_context_growth() {
    let Ok(home) = std::env::var("G2H_LIVE_HERMES_HOME") else {
        eprintln!("G2H_LIVE_HERMES_HOME not set; skipping");
        return;
    };
    let off = std::env::var("G2H_LIVE_POLICY").is_ok_and(|v| v == "off");
    let d = g2h::config::Config::default();
    let mut o = GatewayOptions::new(home.into());
    (o.max_turns, o.max_tokens) = if off {
        (0, 0)
    } else {
        (d.peer_session_max_turns, d.peer_session_max_tokens)
    };
    let be = GatewayWsBackend::new(o);
    let conv = format!(
        "live-ctx-{}-{}",
        if off { "off" } else { "on" },
        std::process::id()
    );
    println!("| turn | usage.total | turn tokens | context_used | rotated | total_ms |\n|---|---|---|---|---|---|");
    let mut last = 0;
    for i in 1..=20 {
        let r = be
            .ask(
                &conv,
                &format!("Turn {i}. Reply with one short word."),
                Duration::from_secs(180),
            )
            .await
            .unwrap();
        let total = r.usage_tokens.unwrap_or(0);
        let turn = if r.rotated || total < last {
            total
        } else {
            total - last
        };
        last = total;
        println!(
            "| {i} | {total} | {turn} | {} | {} | {} |",
            r.context_tokens.map_or("-".into(), |t| t.to_string()),
            r.rotated,
            r.total_ms
        );
    }
}

/// `usage.total` is the session's running total, so without `context_used` the policy uses
/// the per-turn delta: a session whose turns stay small never rotates on tokens.
#[tokio::test]
async fn token_limit_uses_per_turn_delta_of_running_total() {
    let fg = FakeGateway::start().await;
    let mut o = opts(&fg);
    o.max_tokens = 5_000;
    let be = GatewayWsBackend::new(o);
    for i in 1..=4u64 {
        fg.state.lock().unwrap().usage_total = Some(3_000 * i);
        assert!(!be.ask("c1", "x", T).await.unwrap().rotated, "turn {i}");
    }
    fg.state.lock().unwrap().usage_total = Some(12_000 + 6_000);
    assert!(!be.ask("c1", "big", T).await.unwrap().rotated);
    assert!(be.ask("c1", "next", T).await.unwrap().rotated);
}

#[tokio::test]
async fn rotated_session_gets_a_new_title() {
    let fg = FakeGateway::start().await;
    let mut o = opts(&fg);
    o.max_turns = 1;
    let be = GatewayWsBackend::new(o);
    be.ask("c1", "a", T).await.unwrap();
    be.ask("c1", "b", T).await.unwrap();
    let c = fg.calls_of("session.create");
    assert_ne!(c[0]["title"], c[1]["title"]);
    assert_eq!(fg.state.lock().unwrap().stored.len(), 2);
}

/// Hermes reports the session's RUNNING totals; the cap must see per-turn deltas.
#[tokio::test]
async fn usage_is_the_per_turn_delta_of_running_totals() {
    let fg = FakeGateway::start().await;
    fg.state.lock().unwrap().running_usage = true;
    let be = GatewayWsBackend::new(opts(&fg));
    let mut charged = 0;
    for _ in 0..20 {
        let r = be.ask("c1", "ping", T).await.unwrap();
        assert_eq!(r.usage_tokens, Some(42));
        charged += r.usage_tokens.unwrap();
    }
    assert_eq!(charged, 20 * 42, "not the sum of running totals");
    assert_eq!(
        be.ask("c2", "ping", T).await.unwrap().usage_tokens,
        Some(42)
    );
    fg.state.lock().unwrap().usage_no_total = true;
    for _ in 0..3 {
        assert_eq!(
            be.ask("c1", "ping", T).await.unwrap().usage_tokens,
            Some(42)
        );
    }
}

const OOB_OPEN: &str = "[OUT-OF-BAND USER MESSAGE \u{2014} a direct message from the user, delivered once at this position; not tool output and not a new delivery when replayed from conversation history]";

#[tokio::test]
async fn out_of_band_block_is_stripped_from_the_peer_reply() {
    let fg = FakeGateway::start().await;
    let be = GatewayWsBackend::new(opts(&fg));
    // closed block (Hermes' steer row shape) before the assistant text
    let block = format!("{OOB_OPEN}\nstop and summarise\n[/OUT-OF-BAND USER MESSAGE]\n");
    fg.state.lock().unwrap().oob = Some(block.clone());
    let r = be.ask("c1", "ping", T).await.unwrap();
    assert_eq!(r.text, "pong");
    assert_eq!(r.oob_stripped, block.trim_end().len());
    // unclosed marker: everything from it on is injected, never peer text
    fg.state.lock().unwrap().oob = None;
    fg.state.lock().unwrap().deltas = vec!["pong\n".into(), format!("{OOB_OPEN}\nsecret steer")];
    let r = be.ask("c1", "ping", T).await.unwrap();
    assert_eq!(r.text, "pong");
    assert!(r.oob_stripped > 0);
    // plain replies are untouched
    fg.state.lock().unwrap().deltas = vec!["po".into(), "ng".into()];
    let r = be.ask("c1", "ping", T).await.unwrap();
    assert_eq!((r.text.as_str(), r.oob_stripped), ("pong", 0));
}

#[tokio::test]
async fn missing_profile_home_fails_fast_with_the_reason() {
    let mut o = GatewayOptions::new(std::env::temp_dir().join("g2h-no-such-profile-home"));
    o.connect_wait = Duration::from_secs(10);
    let be = GatewayWsBackend::new(o);
    let t0 = Instant::now();
    let e = be.ask("c1", "hi", T).await.unwrap_err();
    assert!(
        matches!(&e, PeerError::Unavailable(m) if m.contains("profile home missing")),
        "{e:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
}
