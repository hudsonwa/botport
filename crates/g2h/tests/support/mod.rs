//! Fake Hermes gateway: the control socket (`identify`, `session-ticket`) and the
//! ticket-gated WebSocket JSON-RPC (`session.create`/`resume`, `prompt.submit`,
//! `session.interrupt`, `message.delta`/`message.complete` events).
#![allow(dead_code, clippy::type_complexity, clippy::result_large_err)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;

pub const PROTOCOL: &str = "hermes-gateway-v1";

/// `gateway/session_policy.py` CREATE_FIELDS, plus `title` (popped by `session_controls.create`).
const CREATE_FIELDS: &[&str] = &[
    "request_id",
    "source",
    "cwd",
    "model",
    "toolsets",
    "provider",
    "base_url",
    "reasoning",
    "max_turns",
    "ignore_rules",
    "api_key",
    "editor",
    "yolo",
    "safe_mode",
    "ignore_user_config",
    "skills",
    "checkpoints",
    "accept_hooks",
    "pass_session_id",
    "follow_profile_config",
    "service_tier",
    "cwd_explicit",
    "run_budget",
    "verbose",
    "row_cwd",
    "client_env",
    "title",
];
/// `session_policy.py` SURFACES keys.
const SURFACES: &[&str] = &["cli", "tui", "gui", "acp", "tool", "oneshot"];
/// `session_controls.py` submission() allowed keys.
const SUBMIT_FIELDS: &[&str] = &[
    "session_id",
    "text",
    "submission_id",
    "input_id",
    "queued",
    "attachments",
    "finite",
    "unattended",
    "surface",
    "voice_context",
    "interrupted",
    "voice_turn",
    "display_kind",
    "title_preview",
    "turn_author",
    "api_text",
    "model_override",
    "clarify_batch",
];

fn keys(p: &Value) -> Vec<&str> {
    p.as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

fn refusal(id: &Value, reason: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": 4001, "message": reason, "data": {"reason": reason}}}).to_string()
}

#[derive(Default)]
pub struct State {
    pub instance_id: String,
    pub epoch: i64,
    pub port: u16,
    pub valid_tickets: HashSet<String>,
    pub minted: Vec<String>,
    pub identify_calls: usize,
    /// (port, ticket) per accepted WS connection.
    pub connections: Vec<(u16, String)>,
    /// (method, params) of every RPC received.
    pub calls: Vec<(String, Value)>,
    /// stored session id -> title
    pub stored: HashMap<String, String>,
    pub deltas: Vec<String>,
    pub delta_delay_ms: u64,
    pub drop_mid_turn_once: bool,
    pub hang: bool,
    /// Never answer `session.create` / `session.resume`.
    pub stall_setup: bool,
    /// Refuse `session.resume` with this 4001 reason instead of resolving it.
    pub resume_refusal: Option<String>,
    /// request_id -> stored id: the gateway's create idempotency registry
    /// (it outlives the session, like the real one).
    pub by_request: HashMap<String, String>,
    /// `usage.total` reported per turn (None: 42).
    pub usage_total: Option<u64>,
    /// `usage.context_used` reported per turn (None: omitted).
    pub context_used: Option<u64>,
    /// Report RUNNING session totals (+30 input, +12 output per turn), like Hermes.
    pub running_usage: bool,
    /// With `running_usage`: leave `usage.total` out (input/output only).
    pub usage_no_total: bool,
    /// session id -> running (input, output)
    pub running: HashMap<String, (u64, u64)>,
    /// Prepend this Hermes out-of-band (steer) block to the turn's text.
    pub oob: Option<String>,
    next: u64,
}

pub struct FakeGateway {
    pub home: tempfile::TempDir,
    pub state: Arc<Mutex<State>>,
    control: JoinHandle<()>,
    ws: Mutex<Option<(JoinHandle<()>, Arc<Mutex<Vec<JoinHandle<()>>>>)>>,
}

impl FakeGateway {
    pub async fn start() -> FakeGateway {
        let home = tempfile::Builder::new()
            .prefix("g2hfk")
            .tempdir_in("/tmp")
            .unwrap();
        let state = Arc::new(Mutex::new(State {
            instance_id: "inst-1".into(),
            epoch: 1,
            deltas: vec!["po".into(), "ng".into()],
            ..Default::default()
        }));
        let canon = std::fs::canonicalize(home.path()).unwrap();
        let control = spawn_control(&canon, state.clone());
        let fg = FakeGateway {
            home,
            state,
            control,
            ws: Mutex::new(None),
        };
        fg.start_ws().await;
        fg
    }

    pub fn path(&self) -> PathBuf {
        self.home.path().to_path_buf()
    }

    async fn start_ws(&self) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        self.state.lock().unwrap().port = port;
        let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let (st, cs) = (self.state.clone(), conns.clone());
        let accept = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let st = st.clone();
                cs.lock()
                    .unwrap()
                    .push(tokio::spawn(serve_ws(tcp, port, st)));
            }
        });
        *self.ws.lock().unwrap() = Some((accept, conns));
    }

    fn stop_ws(&self) {
        if let Some((accept, conns)) = self.ws.lock().unwrap().take() {
            accept.abort();
            for c in conns.lock().unwrap().drain(..) {
                c.abort();
            }
        }
    }

    /// Drop every live WS connection (listener stays).
    pub fn drop_connections(&self) {
        if let Some((_, conns)) = self.ws.lock().unwrap().as_ref() {
            for c in conns.lock().unwrap().drain(..) {
                c.abort();
            }
        }
    }

    /// Restart the "owner": new WS port, new instance id, old tickets invalid.
    pub async fn rotate_endpoint(&self) {
        self.stop_ws();
        {
            let mut s = self.state.lock().unwrap();
            s.epoch += 1;
            s.instance_id = format!("inst-{}", s.epoch);
            s.valid_tickets.clear();
        }
        self.start_ws().await;
    }

    /// Owner gone entirely: no control socket, no WS.
    pub fn shutdown(&self) {
        self.stop_ws();
        self.control.abort();
        let _ = std::fs::remove_file(self.home.path().join("gateway.sock"));
    }

    pub fn methods(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(|c| c.0.clone())
            .collect()
    }

    pub fn calls_of(&self, m: &str) -> Vec<Value> {
        let s = self.state.lock().unwrap();
        s.calls
            .iter()
            .filter(|c| c.0 == m)
            .map(|c| c.1.clone())
            .collect()
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.stop_ws();
        self.control.abort();
    }
}

fn spawn_control(home: &Path, state: Arc<Mutex<State>>) -> JoinHandle<()> {
    let sock = home.join("gateway.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let home_s = home.to_string_lossy().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (state, home_s) = (state.clone(), home_s.clone());
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(r).read_line(&mut line).await.is_err() {
                    return;
                }
                let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                let result = {
                    let mut s = state.lock().unwrap();
                    match req["verb"].as_str() {
                        Some("identify") => {
                            s.identify_calls += 1;
                            json!({
                                "runtime_protocol": 1, "state": "ready",
                                "served_profiles": [{"home": home_s, "profile_id": "fake-profile"}],
                                "capabilities": ["session-authority-v1"],
                                "authority_epoch": s.epoch, "instance_id": s.instance_id,
                                "api_origin": format!("http://127.0.0.1:{}", s.port),
                                "supervisor": "none",
                            })
                        }
                        Some("session-ticket") => {
                            let p = &req["params"];
                            if p["instance_id"] != json!(s.instance_id) {
                                Value::Null
                            } else {
                                s.next += 1;
                                let t = format!("tkt-{}-{}", s.epoch, s.next);
                                s.valid_tickets.insert(t.clone());
                                s.minted.push(t.clone());
                                json!({"profile_id": "fake-profile", "instance_id": s.instance_id,
                                       "runtime_protocol": 1, "ticket": t})
                            }
                        }
                        _ => Value::Null,
                    }
                };
                let resp = if result.is_null() {
                    json!({"ok": false, "protocol": 1, "id": 1, "error": "refused"})
                } else {
                    json!({"ok": true, "protocol": 1, "id": 1, "result": result})
                };
                let _ = w.write_all(format!("{resp}\n").as_bytes()).await;
            });
        }
    })
}

async fn serve_ws(tcp: tokio::net::TcpStream, port: u16, state: Arc<Mutex<State>>) {
    let st = state.clone();
    let cb = move |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        let offered = req
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let parts: Vec<&str> = offered.split(',').map(|p| p.trim()).collect();
        let ticket = parts
            .iter()
            .find_map(|p| p.strip_prefix("hermes-gateway-ticket."))
            .unwrap_or("")
            .to_string();
        let mut s = st.lock().unwrap();
        if req.uri().path() != "/api/ws"
            || !parts.contains(&PROTOCOL)
            || !s.valid_tickets.remove(&ticket)
        {
            let mut e = ErrorResponse::new(Some("forbidden".into()));
            *e.status_mut() = StatusCode::FORBIDDEN;
            return Err(e);
        }
        s.connections.push((port, ticket));
        resp.headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(PROTOCOL));
        Ok(resp)
    };
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else {
        return;
    };
    // runtime sid -> stored id, for this connection only
    let mut live: HashMap<String, String> = HashMap::new();
    while let Some(Ok(msg)) = ws.next().await {
        let Message::Text(t) = msg else { continue };
        let req: Value = serde_json::from_str(&t).unwrap();
        let (id, method, params) = (
            req["id"].clone(),
            req["method"].as_str().unwrap_or("").to_string(),
            req["params"].clone(),
        );
        state
            .lock()
            .unwrap()
            .calls
            .push((method.clone(), params.clone()));
        let reply =
            |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        if matches!(method.as_str(), "session.create" | "session.resume")
            && state.lock().unwrap().stall_setup
        {
            continue;
        }
        let invalid = match method.as_str() {
            "session.create" => {
                let src = params.get("source").map_or(Some("cli"), Value::as_str);
                keys(&params).iter().any(|k| !CREATE_FIELDS.contains(k))
                    || !src.is_some_and(|s| SURFACES.contains(&s))
            }
            "prompt.submit" => {
                let rid = params
                    .get("submission_id")
                    .or(params.get("input_id"))
                    .and_then(Value::as_str);
                keys(&params).iter().any(|k| !SUBMIT_FIELDS.contains(k))
                    || !rid.is_some_and(|r| !r.is_empty())
                    || !params["session_id"].is_string()
                    || !params["text"].is_string()
            }
            _ => false,
        };
        if invalid {
            let _ = ws.send(Message::text(refusal(&id, "invalid_params"))).await;
            continue;
        }
        match method.as_str() {
            "session.create" => {
                let (rt, stored) = {
                    let mut s = state.lock().unwrap();
                    s.next += 1;
                    let rid = params["request_id"].as_str().unwrap_or("").to_string();
                    let title = params["title"].as_str().unwrap_or("").to_string();
                    // a titled create resolves a live session with that title first
                    // (`session_local_title.resolve_titled_session`), like the real one
                    let by_title = s
                        .stored
                        .iter()
                        .find(|(_, t)| !title.is_empty() && **t == title)
                        .map(|(k, _)| k.clone());
                    let stored = match s.by_request.get(&rid).cloned().or(by_title) {
                        Some(old) => old,
                        None => {
                            let st = format!("st-{}", s.next);
                            s.stored
                                .insert(st.clone(), params["title"].as_str().unwrap_or("").into());
                            st
                        }
                    };
                    s.by_request.insert(rid, stored.clone());
                    (format!("rt-{}", s.next), stored)
                };
                live.insert(rt.clone(), stored.clone());
                let _ = ws
                    .send(Message::text(reply(
                        json!({"session_id": rt, "stored_session_id": stored}),
                    )))
                    .await;
            }
            "session.resume" => {
                let target = params["session_id"].as_str().unwrap_or("").to_string();
                let (known, refused) = {
                    let s = state.lock().unwrap();
                    (s.stored.contains_key(&target), s.resume_refusal.clone())
                };
                if let Some(r) = refused {
                    let _ = ws.send(Message::text(refusal(&id, &r))).await;
                    continue;
                }
                if !known {
                    let _ = ws.send(Message::text(refusal(&id, "not_found"))).await;
                    continue;
                }
                let rt = {
                    let mut s = state.lock().unwrap();
                    s.next += 1;
                    format!("rt-{}", s.next)
                };
                live.insert(rt.clone(), target.clone());
                let _ = ws
                    .send(Message::text(reply(
                        json!({"session_id": rt, "stored_session_id": target}),
                    )))
                    .await;
            }
            "session.interrupt" => {
                let sid = params["session_id"].as_str().unwrap_or("").to_string();
                let _ = ws
                    .send(Message::text(reply(json!({"status": "interrupted"}))))
                    .await;
                let ev = event(
                    "message.complete",
                    &sid,
                    json!({"text": "", "status": "interrupted"}),
                );
                let _ = ws.send(Message::text(ev)).await;
            }
            "prompt.submit" => {
                let sid = params["session_id"].as_str().unwrap_or("").to_string();
                if !live.contains_key(&sid) {
                    let e = json!({"jsonrpc": "2.0", "id": id, "error": {"code": 4001, "message": "session not found"}});
                    let _ = ws.send(Message::text(e.to_string())).await;
                    continue;
                }
                let _ = ws
                    .send(Message::text(reply(json!({"status": "streaming"}))))
                    .await;
                let (deltas, delay, drop_now, hang) = {
                    let mut s = state.lock().unwrap();
                    let d = s.drop_mid_turn_once;
                    s.drop_mid_turn_once = false;
                    let mut deltas = s.deltas.clone();
                    if let Some(o) = &s.oob {
                        deltas.insert(0, o.clone());
                    }
                    (deltas, s.delta_delay_ms, d, s.hang)
                };
                if hang {
                    continue;
                }
                for (i, d) in deltas.iter().enumerate() {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    let _ = ws
                        .send(Message::text(event(
                            "message.delta",
                            &sid,
                            json!({"text": d}),
                        )))
                        .await;
                    if drop_now && i == 0 {
                        return; // connection dropped mid-turn
                    }
                }
                let text: String = deltas.concat();
                let (mut usage, ctx) = {
                    let mut s = state.lock().unwrap();
                    let ctx = s.context_used;
                    if s.running_usage {
                        let no_total = s.usage_no_total;
                        let r = s.running.entry(sid.clone()).or_default();
                        r.0 += 30;
                        r.1 += 12;
                        let mut u = json!({"total": r.0 + r.1, "input": r.0, "output": r.1});
                        if no_total {
                            u.as_object_mut().unwrap().remove("total");
                        }
                        (u, ctx)
                    } else {
                        let total = s.usage_total.unwrap_or(42);
                        (json!({"total": total, "input": 30, "output": 12}), ctx)
                    }
                };
                if let Some(c) = ctx {
                    usage["context_used"] = json!(c);
                }
                let done = json!({"text": text, "status": "complete", "usage": usage});
                let _ = ws
                    .send(Message::text(event("message.complete", &sid, done)))
                    .await;
            }
            _ => {
                let e = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"}});
                let _ = ws.send(Message::text(e.to_string())).await;
            }
        }
    }
}

fn event(kind: &str, sid: &str, payload: Value) -> String {
    json!({"jsonrpc": "2.0", "method": "event", "params": {"type": kind, "session_id": sid, "payload": payload}}).to_string()
}

/// Minimal loopback A2A JSON-RPC server. Returns (url, received request bodies).
pub async fn fake_a2a(reply: &'static str, delay_ms: u64) -> (String, Arc<Mutex<Vec<Value>>>) {
    use tokio::io::AsyncReadExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let s2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let seen = s2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
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
                            break buf[pos + 4..pos + 4 + len].to_vec();
                        }
                    }
                };
                let req: Value = serde_json::from_slice(&body).unwrap();
                seen.lock().unwrap().push(req.clone());
                let result = if req["method"] == "SendMessage" {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    json!({"task": {"id": "task-1", "contextId": req["params"]["message"]["contextId"],
                        "status": {"state": "TASK_STATE_COMPLETED"},
                        "artifacts": [{"parts": [{"text": reply}]}]}})
                } else {
                    json!({"id": "task-1", "status": {"state": "TASK_STATE_CANCELED"}})
                };
                let out = json!({"jsonrpc": "2.0", "id": req["id"], "result": result}).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    out.len(),
                    out
                );
                let _ = tcp.write_all(resp.as_bytes()).await;
            });
        }
    });
    (url, seen)
}
