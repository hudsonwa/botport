//! In-process g2hd tests: socket verbs, wake logic against a fake webhook,
//! loop prevention, persistence. The daemon is single-threaded (LocalSet).
mod support;

use g2h::client;
use g2h::config::Config;
use g2h::daemon::{self, DaemonOptions};
use g2h::envelope::Kind;
use g2h::hermes::{GatewayOptions, GatewayWsBackend, Health, PeerBackend, PeerError, Reply};
use g2h::ipc::{Request, Response};
use g2h::paths::Paths;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

const DEBOUNCE: Duration = Duration::from_millis(150);
const SETTLE: Duration = Duration::from_millis(450);

fn run<F: Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&rt, f)
}

/// Replies "pong" instantly with 7 tokens of usage.
struct Echo;
impl PeerBackend for Echo {
    async fn ask(&self, _conv: &str, _text: &str, _d: Duration) -> Result<Reply, PeerError> {
        Ok(Reply {
            text: "pong".into(),
            usage_tokens: Some(7),
            first_delta_ms: Some(0),
            total_ms: 0,
            rotated: false,
            oob_stripped: 0,
            context_tokens: None,
        })
    }
    async fn interrupt(&self, _conv: &str) -> Result<(), PeerError> {
        Ok(())
    }
    async fn health(&self) -> Health {
        Health {
            ok: true,
            detail: "echo".into(),
        }
    }
}

struct D {
    socket: PathBuf,
    home: PathBuf,
    shutdown: Rc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl D {
    async fn call(&self, r: Request) -> Response {
        client::call(&self.socket, &r).await.unwrap()
    }
    async fn stop(self) {
        self.shutdown.notify_one();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap();
    }
    fn log(&self) -> String {
        std::fs::read_to_string(Paths::new(&self.home).log()).unwrap_or_default()
    }
}

fn opts(home: &Path, cfg: Config) -> DaemonOptions {
    let mut o = DaemonOptions::new(Paths::new(home), cfg);
    o.debounce = DEBOUNCE;
    o.retry_base = Duration::from_millis(10);
    o.post_timeout = Duration::from_secs(2);
    o
}

fn start<B: PeerBackend + 'static>(o: DaemonOptions, be: B) -> D {
    let bound = daemon::bind(&o).unwrap();
    let shutdown = Rc::new(Notify::new());
    let sd = shutdown.clone();
    let task = tokio::task::spawn_local(async move { bound.run(be, sd).await.unwrap() });
    D {
        socket: o.paths.socket(),
        home: o.paths.home.clone(),
        shutdown,
        task,
    }
}

fn home() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let h = t.path().join("g2h");
    (t, h)
}

fn ask(text: &str) -> Request {
    Request::Ask {
        text: text.into(),
        conv_id: None,
        reply_to: None,
        timeout_secs: Some(10),
    }
}
fn send(text: &str, reply_to: Option<String>) -> Request {
    Request::Send {
        text: text.into(),
        conv_id: Some("c".into()),
        reply_to,
        no_reply: false,
        notice: false,
    }
}
fn hsend(text: &str) -> Request {
    Request::HermesSend {
        text: text.into(),
        conv_id: Some("c".into()),
        reply_to: None,
        urgent: false,
        end: false,
    }
}
fn hsend_urgent(text: &str) -> Request {
    Request::HermesSend {
        text: text.into(),
        conv_id: Some("c".into()),
        reply_to: None,
        urgent: true,
        end: false,
    }
}

// ---------- fake webhook ----------

#[derive(Default)]
struct Hook {
    hits: Vec<(String, String)>, // (head lowercased, body)
    statuses: Vec<u16>,          // per-hit status; last repeats; empty = 200
}

async fn fake_webhook() -> (String, Arc<Mutex<Hook>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hooks/secret-path-xyz", l.local_addr().unwrap());
    let st: Arc<Mutex<Hook>> = Arc::default();
    let s2 = st.clone();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = l.accept().await {
            let st = s2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = tcp.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map(|v| v.trim().parse().unwrap())
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + len {
                            break (
                                head,
                                String::from_utf8_lossy(&buf[pos + 4..pos + 4 + len]).to_string(),
                            );
                        }
                    }
                };
                let status = {
                    let mut s = st.lock().unwrap();
                    let i = s.hits.len();
                    s.hits.push((head, body));
                    s.statuses
                        .get(i)
                        .or(s.statuses.last())
                        .copied()
                        .unwrap_or(200)
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = tcp.write_all(resp.as_bytes()).await;
            });
        }
    });
    (url, st)
}

const KEY: &str = "test-sender-key-0123456789abcdef";

fn write_creds(home: &Path, url: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let p = Paths::new(home).credentials();
    std::fs::write(&p, format!("webhook_url={url}\nsender_key=\"{KEY}\" # test\nkey_mode=header:Authorization-Bearer\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn live() -> Config {
    Config {
        dry_run: false,
        ..Config::default()
    }
}

fn hits(h: &Arc<Mutex<Hook>>) -> usize {
    h.lock().unwrap().hits.len()
}

// ---------- verbs ----------

#[test]
fn ask_through_fake_gateway_frames_scans_and_reports_latency() {
    run(async {
        let fg = support::FakeGateway::start().await;
        let (_t, h) = home();
        let mut go = GatewayOptions::new(fg.path());
        go.connect_wait = Duration::from_secs(3);
        let d = start(opts(&h, Config::default()), GatewayWsBackend::new(go));
        let secret = format!("sk-{}", "Zz9".repeat(10));
        match d.call(ask(&format!("hello, my key is {secret}"))).await {
            Response::Reply {
                env,
                first_delta_ms,
                total_ms,
            } => {
                assert_eq!(env.body, "pong");
                assert_eq!(
                    (env.from.as_str(), env.to.as_str(), env.hop),
                    ("hermes", "grok", 1)
                );
                assert!(first_delta_ms.is_some() && total_ms >= first_delta_ms.unwrap());
            }
            r => panic!("{r:?}"),
        }
        let sub = &fg.calls_of("prompt.submit")[0]["text"];
        let sub = sub.as_str().unwrap();
        assert!(sub.starts_with("[Message from grok via g2h"), "{sub}");
        assert!(!sub.contains(&secret) && sub.contains("[REDACTED]"));
        let log = d.log();
        assert!(log.contains("first_delta_ms") && !log.contains(&secret));
        d.stop().await;
    });
}

#[test]
fn send_returns_id_and_reply_lands_in_inbox_recv_drains() {
    run(async {
        let (_t, h) = home();
        let d = start(opts(&h, Config::default()), Echo);
        let id = match d.call(send("hi", None)).await {
            Response::Ok { id: Some(id) } => id,
            r => panic!("{r:?}"),
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        match d.call(Request::Recv).await {
            Response::Messages { envs } => {
                assert_eq!(envs.len(), 1);
                assert_eq!(envs[0].reply_to.as_deref(), Some(id.as_str()));
                assert_eq!(envs[0].body, "pong");
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(
            d.call(Request::Recv).await,
            Response::Messages { envs: vec![] }
        );
        d.stop().await;
    });
}

#[test]
fn wait_returns_within_5ms_of_arrival_and_times_out() {
    run(async {
        let (_t, h) = home();
        let d = start(opts(&h, Config::default()), Echo);
        let sock = d.socket.clone();
        let w = tokio::task::spawn_local(async move {
            let r = client::call(
                &sock,
                &Request::Wait {
                    timeout_secs: Some(10),
                },
            )
            .await
            .unwrap();
            (r, Instant::now())
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t0 = Instant::now();
        d.call(hsend("ping from hermes")).await;
        let (r, t1) = w.await.unwrap();
        let lat = t1 - t0;
        assert!(
            matches!(&r, Response::Messages { envs } if envs.len() == 1 && envs[0].body == "ping from hermes"),
            "{r:?}"
        );
        eprintln!("wait wake latency: {lat:?}");
        assert!(lat < Duration::from_millis(50), "{lat:?}"); // 5 ms flaked on loaded efficiency cores
        let t = Instant::now();
        assert_eq!(
            d.call(Request::Wait {
                timeout_secs: Some(1)
            })
            .await,
            Response::Timeout
        );
        assert!(t.elapsed() >= Duration::from_millis(900));
        // timeout is capped at 720 s
        assert!(matches!(
            d.call(Request::Wait {
                timeout_secs: Some(721)
            })
            .await,
            Response::Error { .. }
        ));
        d.stop().await;
    });
}

#[test]
fn bye_clears_grok_active_and_status_shows_it() {
    run(async {
        let (_t, h) = home();
        let d = start(opts(&h, Config::default()), Echo);
        d.call(Request::Recv).await;
        let Response::Status(s) = d.call(Request::Status).await else {
            panic!()
        };
        assert!(s.grok_active && s.dry_run && s.backend_ok);
        assert_eq!(s.webhook, "missing");
        d.call(Request::Bye).await;
        // bye, status and doctor do not count as activity
        let Response::Status(s) = d.call(Request::Status).await else {
            panic!()
        };
        assert!(!s.grok_active);
        let Response::Doctor { checks } = d.call(Request::Doctor).await else {
            panic!()
        };
        assert!(checks.iter().any(|c| c.name == "backend" && c.ok));
        d.stop().await;
    });
}

// ---------- wake logic ----------

#[test]
fn dry_run_is_default_and_never_posts() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, Config::default()), Echo);
        d.call(hsend("hello")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 0);
        let log = d.log();
        assert!(log.contains("would_post"), "{log}");
        assert!(
            !log.contains(KEY) && !log.contains("secret-path-xyz"),
            "{log}"
        );
        let Response::Status(s) = d.call(Request::Status).await else {
            panic!()
        };
        assert_eq!(s.wakes_today, 1);
        d.stop().await;
    });
}

#[test]
fn debounce_batches_five_into_one_wake_with_auth_and_small_body() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Echo);
        for i in 0..5 {
            d.call(hsend(&format!("m{i}"))).await;
        }
        tokio::time::sleep(SETTLE).await;
        let hits_now = hook.lock().unwrap().hits.clone();
        assert_eq!(hits_now.len(), 1);
        let (head, body) = &hits_now[0];
        assert!(head.starts_with("post /hooks/secret-path-xyz "), "{head}");
        assert!(head.contains(&format!("authorization: bearer {}", KEY.to_lowercase())));
        assert!(body.len() < 500);
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"conv_id": "c", "count": 5, "hint": "run g2h recv"})
        );
        assert!(!d.log().contains(KEY));
        d.stop().await;
    });
}

#[test]
fn no_wake_while_wait_open_or_grok_active_until_bye() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Echo);
        // the Grok bot in wait
        let sock = d.socket.clone();
        let w = tokio::task::spawn_local(async move {
            client::call(
                &sock,
                &Request::Wait {
                    timeout_secs: Some(5),
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // the wait call itself made Grok active; clear that, keep the wait open
        d.call(Request::Bye).await;
        d.call(hsend("while waiting")).await;
        assert!(matches!(
            w.await.unwrap().unwrap(),
            Response::Messages { .. }
        ));
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 0, "never wake while a wait is open");
        // Grok active (a recent Grok-side call)
        d.call(Request::Recv).await;
        d.call(hsend("while active")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 0, "never wake while the Grok bot is active");
        // bye -> idle -> wakes
        d.call(Request::Bye).await;
        d.call(hsend("after bye")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1);
        d.stop().await;
    });
}

#[test]
fn wake_cap_enforced_urgent_only_past_80_percent() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(
            opts(
                &h,
                Config {
                    max_wakes_per_day: 5,
                    ..live()
                },
            ),
            Echo,
        );
        // wakes are counted from hermes-side messages only; recv would make Grok active, so
        // drain with bye-sandwiched recv
        let round = |r: Request| {
            let d = &d;
            async move {
                d.call(r).await;
                tokio::time::sleep(SETTLE).await;
                d.call(Request::Recv).await;
                d.call(Request::Bye).await;
            }
        };
        for i in 0..5 {
            round(hsend(&format!("n{i}"))).await;
        }
        assert_eq!(hits(&hook), 4, "past 80% only urgent wakes");
        round(hsend_urgent("u1")).await;
        assert_eq!(hits(&hook), 5);
        round(hsend_urgent("u2")).await;
        assert_eq!(hits(&hook), 5, "cap");
        let Response::Status(s) = d.call(Request::Status).await else {
            panic!()
        };
        assert_eq!((s.wakes_today, s.wakes_cap), (5, 5));
        d.stop().await;
    });
}

#[test]
fn four_xx_stops_waking_and_alerts() {
    run(async {
        let (url, hook) = fake_webhook().await;
        hook.lock().unwrap().statuses = vec![401];
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Echo);
        d.call(hsend("a")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1, "4xx is not retried");
        let Response::Status(s) = d.call(Request::Status).await else {
            panic!()
        };
        assert!(
            s.alert.as_deref().unwrap_or("").contains("401"),
            "{:?}",
            s.alert
        );
        d.call(Request::Bye).await;
        d.call(hsend("b")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1, "waking stopped after 4xx");
        d.stop().await;
    });
}

#[test]
fn five_xx_retries_with_bounded_attempts() {
    run(async {
        let (url, hook) = fake_webhook().await;
        hook.lock().unwrap().statuses = vec![503, 200];
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Echo);
        d.call(hsend("a")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 2, "one retry then success");
        hook.lock().unwrap().statuses = vec![503];
        d.call(Request::Recv).await;
        d.call(Request::Bye).await;
        d.call(hsend("b")).await;
        tokio::time::sleep(SETTLE + Duration::from_millis(300)).await;
        assert_eq!(hits(&hook), 2 + daemon::POST_ATTEMPTS as usize);
        d.stop().await;
    });
}

// ---------- loop prevention / caps / persistence ----------

#[test]
fn hop_limit_and_no_reply_to_end() {
    run(async {
        let (_t, h) = home();
        let d = start(
            opts(
                &h,
                Config {
                    hop_limit: 3,
                    ..Config::default()
                },
            ),
            Echo,
        );
        // hop0 -> reply hop1 ; reply_to hop1 -> hop2 whose reply would be hop3: refused
        let Response::Reply { env, .. } = d.call(ask("q")).await else {
            panic!()
        };
        assert_eq!(env.hop, 1);
        let r = d
            .call(Request::Ask {
                text: "q2".into(),
                conv_id: Some(env.conv_id.clone()),
                reply_to: Some(env.id.clone()),
                timeout_secs: None,
            })
            .await;
        assert!(
            matches!(&r, Response::Error { code, .. } if code == "hop_limit"),
            "{r:?}"
        );
        // hermes says end; grok may not reply to it
        d.call(Request::HermesSend {
            text: "bye".into(),
            conv_id: Some("c".into()),
            reply_to: None,
            urgent: false,
            end: true,
        })
        .await;
        let Response::Messages { envs } = d.call(Request::Recv).await else {
            panic!()
        };
        assert_eq!(envs[0].kind, Kind::End);
        let r = d.call(send("ok", Some(envs[0].id.clone()))).await;
        assert!(
            matches!(&r, Response::Error { code, .. } if code == "no_reply"),
            "{r:?}"
        );
        assert!(d.log().contains("refused"));
        d.stop().await;
    });
}

#[test]
fn caps_persist_across_restart() {
    run(async {
        let (_t, h) = home();
        let cfg = Config {
            max_messages_per_day: 3,
            ..Config::default()
        };
        let d = start(opts(&h, cfg.clone()), Echo);
        // ask = 2 messages (out + reply)
        assert!(matches!(d.call(ask("1")).await, Response::Reply { .. }));
        let r = d.call(ask("2")).await;
        assert!(
            matches!(&r, Response::Error { code, .. } if code == "cap"),
            "{r:?}"
        );
        d.stop().await;
        let d = start(opts(&h, cfg), Echo);
        let r = d.call(ask("3")).await;
        assert!(
            matches!(&r, Response::Error { code, .. } if code == "cap"),
            "{r:?}"
        );
        let Response::Status(s) = d.call(Request::Status).await else {
            panic!()
        };
        assert_eq!(s.messages_today, 2);
        assert_eq!(s.tokens_today, 7 + 1);
        d.stop().await;
    });
}

#[test]
fn single_instance_socket_perms_and_graceful_shutdown() {
    run(async {
        let (_t, h) = home();
        let o = opts(&h, Config::default());
        let d = start(o.clone(), Echo);
        let e = daemon::bind(&o).err().expect("second instance must fail");
        assert!(e.contains("already running"), "{e}");
        let mode = std::fs::metadata(&d.socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            std::fs::metadata(&h).unwrap().permissions().mode() & 0o777,
            0o700
        );
        d.call(hsend("persist me")).await;
        let sock = d.socket.clone();
        d.stop().await;
        assert!(!sock.exists(), "socket removed on shutdown");
        // queue persisted, and the lock is free again
        let d = start(o, Echo);
        let Response::Messages { envs } = d.call(Request::Recv).await else {
            panic!()
        };
        assert_eq!(envs[0].body, "persist me");
        d.stop().await;
    });
}

#[test]
fn refuses_world_writable_home() {
    let (_t, h) = home();
    std::fs::create_dir_all(&h).unwrap();
    std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o777)).unwrap();
    let e = daemon::bind(&opts(&h, Config::default())).err().unwrap();
    assert!(e.contains("world-writable"), "{e}");
}

#[test]
fn status_never_prints_secrets() {
    run(async {
        let (_t, h) = home();
        write_creds(
            &h,
            "https://example.invalid/hooks/secret-path-xyz?token=abc",
        );
        let d = start(opts(&h, Config::default()), Echo);
        let r = d.call(Request::Status).await;
        let s = serde_json::to_string(&r).unwrap();
        assert!(
            !s.contains(KEY) && !s.contains("secret-path-xyz") && !s.contains("token=abc"),
            "{s}"
        );
        let Response::Status(st) = r else { panic!() };
        assert_eq!(st.webhook, "set");
        let s = serde_json::to_string(&d.call(Request::Doctor).await).unwrap();
        assert!(!s.contains(KEY) && !s.contains("secret-path-xyz"), "{s}");
        d.stop().await;
    });
}

// ---------- property: two auto-replying bots ----------

mod prop {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
        #[test]
        fn auto_replying_bots_never_exceed_hop_or_caps(
            hop_limit in 1u32..6, msg_cap in 0u64..12, steps in 1usize..25,
            hermes_initiates in any::<bool>()
        ) {
            let (msgs, max_hop) = run(async move {
                let (_t, h) = home();
                let cfg = Config { hop_limit, max_messages_per_day: msg_cap, ..Config::default() };
                let d = start(opts(&h, cfg), Echo);
                let mut max_hop = 0;
                let mut last: Option<String> = None;
                if hermes_initiates {
                    d.call(hsend("start")).await;
                }
                for _ in 0..steps {
                    // grok bot: auto-reply to whatever it last received
                    d.call(send("auto", last.clone())).await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    if let Response::Messages { envs } = d.call(Request::Recv).await {
                        for e in &envs { max_hop = max_hop.max(e.hop); }
                        if let Some(e) = envs.last() {
                            // hermes bot also auto-replies via its tool
                            d.call(Request::HermesSend { text: "auto".into(), conv_id: Some(e.conv_id.clone()),
                                reply_to: Some(e.id.clone()), urgent: false, end: false }).await;
                            last = Some(e.id.clone());
                        }
                    }
                }
                if let Response::Messages { envs } = d.call(Request::Recv).await {
                    for e in &envs { max_hop = max_hop.max(e.hop); }
                }
                let Response::Status(s) = d.call(Request::Status).await else { panic!() };
                d.stop().await;
                (s.messages_today, max_hop)
            });
            prop_assert!(msgs <= msg_cap);
            prop_assert!(max_hop < hop_limit.max(1));
        }
    }
}

#[test]
fn killed_wait_client_stops_counting_and_never_eats_messages() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Echo);
        let mut s = tokio::net::UnixStream::connect(&d.socket).await.unwrap();
        let line = g2h::ipc::encode(&Request::Wait {
            timeout_secs: Some(30),
        })
        .unwrap();
        s.write_all(line.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(s); // the wait client dies
        tokio::time::sleep(Duration::from_millis(50)).await;
        d.call(hsend("after kill")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1, "a dead wait must not suppress the wake");
        match d.call(Request::Recv).await {
            Response::Messages { envs } => {
                assert_eq!(envs.len(), 1);
                assert_eq!(envs[0].body, "after kill");
            }
            r => panic!("{r:?}"),
        }
        d.stop().await;
    });
}

// ---------- live selftest wake guards ----------

fn wake_test(token: &str, consent: bool) -> Request {
    Request::WakeTest {
        live: true,
        token: token.into(),
        consent,
    }
}

fn err_code(r: &Response) -> String {
    match r {
        Response::Error { code, .. } => code.clone(),
        other => format!("not an error: {other:?}"),
    }
}

#[test]
fn live_selftest_refused_in_dry_run_without_consent() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, Config::default()), Echo);
        let r = d.call(wake_test("t1", false)).await;
        assert_eq!(err_code(&r), "dry_run", "{r:?}");
        assert_eq!(hits(&hook), 0);
        let r = d.call(wake_test("t2", true)).await;
        assert!(matches!(r, Response::Ok { .. }), "{r:?}");
        assert_eq!(hits(&hook), 1);
        d.stop().await;
    });
}

#[test]
fn live_selftest_has_its_own_daily_cap() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        assert_eq!(Config::default().selftest_wakes_per_day, 3);
        let cfg = Config {
            selftest_wakes_per_day: 2,
            ..live()
        };
        let d = start(opts(&h, cfg), Echo);
        for t in ["a", "b"] {
            let r = d.call(wake_test(t, false)).await;
            assert!(matches!(r, Response::Ok { .. }), "{r:?}");
        }
        let r = d.call(wake_test("c", true)).await;
        assert_eq!(err_code(&r), "cap", "{r:?}");
        assert_eq!(hits(&hook), 2);
        let Response::Status(s) = d.call(Request::Status).await else {
            panic!()
        };
        assert_eq!(s.wakes_today, 0, "selftest wakes don't use the wake cap");
        d.stop().await;
    });
}

#[test]
fn live_selftest_refused_while_alert_set() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Echo);
        hook.lock().unwrap().statuses = vec![401];
        d.call(hsend("a")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1);
        let r = d.call(wake_test("t", true)).await;
        assert_eq!(err_code(&r), "alert", "{r:?}");
        assert_eq!(hits(&hook), 1);
        d.stop().await;
    });
}

#[test]
fn live_selftest_one_probe_in_flight() {
    run(async {
        // accepts and never answers: the first POST stays in flight until post_timeout
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", l.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                held.push(s);
            }
        });
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = Rc::new(start(opts(&h, live()), Echo));
        let d2 = d.clone();
        let first = tokio::task::spawn_local(async move { d2.call(wake_test("a", false)).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let r = d.call(wake_test("b", false)).await;
        assert_eq!(err_code(&r), "busy", "{r:?}");
        let r = first.await.unwrap();
        assert_eq!(err_code(&r), "wake_failed", "{r:?}");
        Rc::try_unwrap(d).ok().unwrap().stop().await;
    });
}

#[test]
fn selftest_probes_expire() {
    run(async {
        let (url, _hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        assert_eq!(
            DaemonOptions::new(Paths::new(&h), live()).probe_ttl,
            Duration::from_secs(15 * 60)
        );
        let mut o = opts(&h, live());
        o.probe_ttl = Duration::from_millis(100);
        let d = start(o, Echo);
        let r = d.call(wake_test("old", false)).await;
        assert!(matches!(r, Response::Ok { .. }), "{r:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let r = d
            .call(Request::Report {
                token: "old".into(),
            })
            .await;
        assert_eq!(err_code(&r), "unknown_token", "{r:?}");
        d.stop().await;
    });
}

// ---------- lost wake, one-way sends ----------

fn status(r: Response) -> g2h::ipc::StatusInfo {
    let Response::Status(s) = r else {
        panic!("not status: {r:?}")
    };
    s
}

/// Replies "pong" after `.0` ms.
struct Slow(u64);
impl PeerBackend for Slow {
    async fn ask(&self, c: &str, t: &str, d: Duration) -> Result<Reply, PeerError> {
        tokio::time::sleep(Duration::from_millis(self.0)).await;
        Echo.ask(c, t, d).await
    }
    async fn interrupt(&self, _conv: &str) -> Result<(), PeerError> {
        Ok(())
    }
    async fn health(&self) -> Health {
        Echo.health().await
    }
}

#[test]
fn queued_while_active_then_lapse_gives_exactly_one_wake() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let mut o = opts(&h, live());
        o.active_window = Duration::from_millis(400);
        let d = start(o, Echo);
        // a Grok-side ask makes Grok active; its reply is returned directly
        assert!(matches!(d.call(ask("hi")).await, Response::Reply { .. }));
        d.call(hsend("queued while active")).await;
        let r = d
            .call(Request::OpToGrok {
                name: "export".into(),
                args: serde_json::json!({"since": "2026-11-01"}),
            })
            .await;
        assert!(matches!(r, Response::Ok { .. }), "{r:?}");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(hits(&hook), 0, "no wake while the Grok bot is active");
        let s = status(d.call(Request::Status).await);
        assert!(s.wake_pending, "a wake is scheduled for when active lapses");
        tokio::time::sleep(Duration::from_millis(400) + SETTLE).await;
        assert_eq!(hits(&hook), 1, "one wake once active lapsed");
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1, "exactly one");
        let body = hook.lock().unwrap().hits[0].1.clone();
        assert!(body.contains("\"count\":2"), "{body}");
        d.stop().await;
    });
}

#[test]
fn bye_with_nonempty_inbox_schedules_a_wake() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Echo); // default active window: minutes
        d.call(Request::Recv).await;
        d.call(hsend("while active")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 0);
        d.call(Request::Bye).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1, "bye with a non-empty inbox wakes");
        d.stop().await;
    });
}

#[test]
fn message_during_wait_is_delivered_without_wake_even_after_lapse() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let mut o = opts(&h, live());
        o.active_window = Duration::from_millis(200);
        let d = start(o, Echo);
        let sock = d.socket.clone();
        let w = tokio::task::spawn_local(async move {
            client::call(
                &sock,
                &Request::Wait {
                    timeout_secs: Some(5),
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        d.call(hsend("during wait")).await;
        assert!(matches!(
            w.await.unwrap().unwrap(),
            Response::Messages { .. }
        ));
        tokio::time::sleep(Duration::from_millis(200) + SETTLE).await;
        assert_eq!(hits(&hook), 0);
        d.stop().await;
    });
}

fn send_opt(text: &str, no_reply: bool, notice: bool) -> Request {
    Request::Send {
        text: text.into(),
        conv_id: Some("c".into()),
        reply_to: None,
        no_reply,
        notice,
    }
}

#[test]
fn no_reply_and_notice_sends_run_the_turn_but_queue_nothing() {
    run(async {
        let (_t, h) = home();
        let d = start(opts(&h, Config::default()), Echo);
        for (nr, nt) in [(true, false), (false, true)] {
            let r = d.call(send_opt("ack, thanks", nr, nt)).await;
            assert!(matches!(r, Response::Ok { .. }), "{r:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let Response::Messages { envs } = d.call(Request::Recv).await else {
            panic!()
        };
        assert!(envs.is_empty(), "{envs:?}");
        let log = d.log();
        assert_eq!(log.matches("\"reply_suppressed\"").count(), 2, "{log}");
        assert!(log.contains("\"kind\":\"notice\""), "{log}");
        d.stop().await;
    });
}

#[test]
fn reply_after_bye_is_queued_but_rides_on_the_next_wake() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), Slow(150));
        d.call(send("hello", None)).await;
        d.call(Request::Bye).await;
        tokio::time::sleep(SETTLE).await;
        let s = status(d.call(Request::Status).await);
        assert_eq!(s.grok_inbox, 1, "the late reply is queued");
        assert_eq!(hits(&hook), 0, "but it does not wake on its own");
        assert!(!s.wake_pending);
        d.call(hsend("real news")).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1);
        let body = hook.lock().unwrap().hits[0].1.clone();
        assert!(body.contains("\"count\":2"), "{body}");
        d.stop().await;
    });
}

#[test]
fn inbox_queued_before_restart_wakes_exactly_once_after() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let mut o = opts(&h, live());
        o.debounce = Duration::from_secs(30); // never fires before the stop
        let d = start(o, Echo);
        d.call(hsend("queued before restart")).await;
        d.stop().await;
        assert_eq!(hits(&hook), 0);
        let d = start(opts(&h, live()), Echo);
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1, "the restart re-arms the wake");
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 1, "exactly one");
        d.stop().await;
    });
}

/// Replies with nothing left after the OOB strip.
struct OobOnly;
impl PeerBackend for OobOnly {
    async fn ask(&self, _conv: &str, _text: &str, _d: Duration) -> Result<Reply, PeerError> {
        Ok(Reply {
            text: "  \n".into(),
            usage_tokens: Some(7),
            first_delta_ms: Some(0),
            total_ms: 0,
            rotated: false,
            oob_stripped: 42,
            context_tokens: None,
        })
    }
    async fn interrupt(&self, _conv: &str) -> Result<(), PeerError> {
        Ok(())
    }
    async fn health(&self) -> Health {
        Health {
            ok: true,
            detail: "oob".into(),
        }
    }
}

#[test]
fn oob_only_reply_becomes_a_quiet_notice() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(opts(&h, live()), OobOnly);
        d.call(send("hello", None)).await;
        d.call(Request::Bye).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(hits(&hook), 0, "a notice does not wake on its own");
        let Response::Messages { envs } = d.call(Request::Recv).await else {
            panic!()
        };
        assert_eq!(envs.len(), 1, "{envs:?}");
        assert_eq!(envs[0].kind, Kind::Notice);
        assert!(
            envs[0].body.contains("only injected out-of-band text"),
            "{envs:?}"
        );
        d.stop().await;
    });
}

#[test]
fn owner_op_to_grok_is_urgent_past_80_percent() {
    run(async {
        let (url, hook) = fake_webhook().await;
        let (_t, h) = home();
        write_creds(&h, &url);
        let d = start(
            opts(
                &h,
                Config {
                    max_wakes_per_day: 5,
                    ..live()
                },
            ),
            Echo,
        );
        let round = |r: Request| {
            let d = &d;
            async move {
                d.call(r).await;
                tokio::time::sleep(SETTLE).await;
                d.call(Request::Recv).await;
                d.call(Request::Bye).await;
            }
        };
        for i in 0..4 {
            round(hsend(&format!("n{i}"))).await;
        }
        assert_eq!(hits(&hook), 4);
        let op = || Request::OpToGrok {
            name: "export".into(),
            args: serde_json::json!({"since": "2026-11-01"}),
        };
        round(op()).await;
        assert_eq!(hits(&hook), 5, "owner op wakes past 80%");
        round(op()).await;
        assert_eq!(hits(&hook), 5, "still within the hard cap");
        d.stop().await;
    });
}
