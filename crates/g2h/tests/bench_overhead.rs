//! Per-message broker overhead through the socket with an instant fake backend.
//! Run: cargo test --release -p g2h --test bench_overhead -- --ignored --nocapture
use g2h::client;
use g2h::config::Config;
use g2h::daemon::{self, DaemonOptions};
use g2h::hermes::{Health, PeerBackend, PeerError, Reply};
use g2h::ipc::{Request, Response};
use g2h::paths::Paths;
use std::rc::Rc;
use std::time::{Duration, Instant};

struct Instant0;
impl PeerBackend for Instant0 {
    async fn ask(&self, _c: &str, _t: &str, _d: Duration) -> Result<Reply, PeerError> {
        Ok(Reply {
            text: "ok".into(),
            usage_tokens: Some(1),
            first_delta_ms: Some(0),
            total_ms: 0,
            rotated: false,
            oob_stripped: 0,
            context_tokens: None,
        })
    }
    async fn interrupt(&self, _c: &str) -> Result<(), PeerError> {
        Ok(())
    }
    async fn health(&self) -> Health {
        Health {
            ok: true,
            detail: String::new(),
        }
    }
}

#[test]
#[ignore]
fn broker_overhead_per_message() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        let t = tempfile::tempdir().unwrap();
        let cfg = Config {
            max_messages_per_day: 1_000_000,
            max_tokens_per_day: 10_000_000,
            ..Config::default()
        };
        let o = DaemonOptions::new(Paths::new(t.path().join("g2h")), cfg);
        let bound = daemon::bind(&o).unwrap();
        let sd = Rc::new(tokio::sync::Notify::new());
        let task = tokio::task::spawn_local(bound.run(Instant0, sd.clone()));
        let sock = o.paths.socket();
        let n = 2000;
        let mut ask = Vec::with_capacity(n);
        let mut hs = Vec::with_capacity(n);
        for i in 0..n {
            let t0 = Instant::now();
            let r = client::call(
                &sock,
                &Request::Ask {
                    text: format!("m{i}"),
                    conv_id: None,
                    reply_to: None,
                    timeout_secs: None,
                },
            )
            .await
            .unwrap();
            ask.push(t0.elapsed());
            assert!(matches!(r, Response::Reply { .. }), "{r:?}");
            let t0 = Instant::now();
            client::call(
                &sock,
                &Request::HermesSend {
                    text: format!("h{i}"),
                    conv_id: None,
                    reply_to: None,
                    urgent: false,
                    end: false,
                },
            )
            .await
            .unwrap();
            hs.push(t0.elapsed());
            if i % 100 == 0 {
                client::call(&sock, &Request::Recv).await.unwrap();
            }
        }
        for (name, v) in [("ask", &mut ask), ("hermes-send", &mut hs)] {
            v.sort();
            eprintln!(
                "{name}: p50 {:?} p99 {:?} (round trip incl. connect)",
                v[n / 2],
                v[n * 99 / 100]
            );
        }
        assert!(ask[n / 2] < Duration::from_millis(1));
        assert!(hs[n / 2] < Duration::from_millis(1));
        sd.notify_one();
        task.await.unwrap().unwrap();
    });
}
