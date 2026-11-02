//! `GatewayWsBackend`: persistent WebSocket JSON-RPC client to one profile's gateway.

use super::endpoint::{discover, mint_ticket};
use super::{ms, Health, PeerBackend, PeerError, Reply};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

const WS_PROTOCOL: &str = "hermes-gateway-v1";
const TICKET_PREFIX: &str = "hermes-gateway-ticket.";
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(2);
/// Recap of the last exchanges seeded into a rotated session.
const RECAP_EXCHANGES: usize = 2;
const RECAP_MAX_CHARS: usize = 1500;
const RECAP_PART_CHARS: usize = 360;

#[derive(Debug, Clone)]
pub struct GatewayOptions {
    pub profile_home: PathBuf,
    /// Toolsets for peer sessions, always sent as `session.create` `toolsets`.
    /// Default empty = no tools (peer text is untrusted); name safe ones (e.g. a knowledge MCP) to allow them.
    pub peer_toolsets: Vec<String>,
    /// Where conv_id -> stored session id is persisted (None: memory only).
    pub sessions_file: Option<PathBuf>,
    /// Bounded context: after this many turns on one peer session, the next turn starts a new
    /// one seeded with a framed recap of the last exchanges. 0 = unlimited.
    pub max_turns: u64,
    /// Same, once a turn's context size reaches this (`usage.context_used`, else the turn's share
    /// of the session's running `usage.total`). 0 = unlimited.
    pub max_tokens: u64,
    /// Total time a (re)connect may take before `Unavailable`.
    pub connect_wait: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl GatewayOptions {
    pub fn new(profile_home: PathBuf) -> GatewayOptions {
        GatewayOptions {
            profile_home,
            peer_toolsets: Vec::new(),
            sessions_file: None,
            max_turns: 0,
            max_tokens: 0,
            connect_wait: Duration::from_secs(10),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_secs(2),
        }
    }
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, PeerError>>>;

/// One live WebSocket. A reader task routes responses by id and events by runtime session id.
struct Conn {
    generation: u64,
    out: mpsc::UnboundedSender<Message>,
    pending: Mutex<Pending>,
    subs: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
}

impl Conn {
    fn close(&self, why: &str) {
        self.alive.store(false, Ordering::SeqCst);
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(PeerError::Disconnected(why.into())));
        }
        self.subs.lock().unwrap().clear();
    }

    fn route(&self, v: Value) {
        if v.get("method").and_then(Value::as_str) == Some("event") {
            let sid = v["params"]["session_id"].as_str().unwrap_or("");
            if let Some(tx) = self.subs.lock().unwrap().get(sid) {
                let _ = tx.send(v["params"].clone());
            }
            return;
        }
        let Some(id) = v.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(tx) = self.pending.lock().unwrap().remove(&id) else {
            return;
        };
        let r = match v.get("error") {
            Some(e) => Err(PeerError::Rpc {
                code: e["code"].as_i64().unwrap_or(0),
                message: e["message"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect(),
            }),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = tx.send(r);
    }

    async fn rpc(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, PeerError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(PeerError::Disconnected("closed".into()));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.out.send(Message::text(frame.to_string())).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(PeerError::Disconnected("writer gone".into()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(PeerError::Disconnected("closed".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(PeerError::Protocol(format!("{method} timed out")))
            }
        }
    }
}

/// On-disk session map. Older files are a bare `conv -> stored id` object.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct Persisted {
    stored: HashMap<String, String>,
    /// conv_id -> create generation, bumped when the gateway forgets the session
    /// or the bounded-context policy rotates it
    #[serde(default)]
    gens: HashMap<String, u64>,
    /// conv_id -> turns completed on the current session
    #[serde(default)]
    turns: HashMap<String, u64>,
    /// conv_id -> context size of the last turn on the current session
    #[serde(default)]
    tokens: HashMap<String, u64>,
    /// conv_id -> last `usage.total` (the session's running total)
    #[serde(default)]
    totals: HashMap<String, u64>,
    /// conv_id -> last exchanges (peer text, reply), truncated
    #[serde(default)]
    recent: HashMap<String, Vec<(String, String)>>,
    /// conv_id -> recap still to seed into the rotated session's first turn
    #[serde(default)]
    seed: HashMap<String, String>,
}

fn load_persisted(b: &[u8]) -> Option<Persisted> {
    serde_json::from_slice::<Persisted>(b).ok().or_else(|| {
        serde_json::from_slice(b).ok().map(|stored| Persisted {
            stored,
            ..Persisted::default()
        })
    })
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut c: String = s.chars().take(n).collect();
    c.push('…');
    c
}

/// Framed recap of the last exchanges, at most about [`RECAP_MAX_CHARS`] of content.
fn recap(recent: &[(String, String)]) -> String {
    let mut body = String::from(
        "Recap of your earlier exchanges with this peer (older history was dropped to bound context):",
    );
    for (peer, reply) in recent {
        body.push_str(&format!(
            "\nPeer: {}\nYou: {}",
            clip(peer, RECAP_PART_CHARS),
            clip(reply, RECAP_PART_CHARS)
        ));
    }
    crate::framing::frame("g2h recap", &clip(&body, RECAP_MAX_CHARS))
}

#[derive(Default)]
struct Sessions {
    /// Durable part (stored ids, generations, policy counters, recap).
    p: Persisted,
    /// conv_id -> (connection generation, runtime session id)
    runtime: HashMap<String, (u64, String)>,
}

pub struct GatewayWsBackend {
    opts: GatewayOptions,
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
    generation: AtomicU64,
    sessions: Mutex<Sessions>,
}

impl GatewayWsBackend {
    pub fn new(opts: GatewayOptions) -> GatewayWsBackend {
        let p = opts
            .sessions_file
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| load_persisted(&b))
            .unwrap_or_default();
        GatewayWsBackend {
            opts,
            conn: tokio::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
            sessions: Mutex::new(Sessions {
                p,
                runtime: HashMap::new(),
            }),
        }
    }

    fn persist(&self, p: &Persisted) {
        let Some(path) = &self.opts.sessions_file else {
            return;
        };
        if let Ok(b) = serde_json::to_vec_pretty(p) {
            let _ = crate::store::save_atomic(path, &b);
        }
    }

    /// One attempt: re-resolve the endpoint, mint a fresh ticket, dial.
    async fn dial(&self) -> Result<Arc<Conn>, PeerError> {
        let ep = discover(&self.opts.profile_home).await?;
        let ticket = mint_ticket(&ep).await?;
        let mut req = ep
            .ws_url()
            .into_client_request()
            .map_err(|_| PeerError::Unavailable("bad ws url".into()))?;
        let protos = format!("{WS_PROTOCOL}, {TICKET_PREFIX}{ticket}");
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&protos)
                .map_err(|_| PeerError::Unavailable("bad ticket".into()))?,
        );
        let addr = ep.api_origin.trim_start_matches("http://").to_string();
        let tcp =
            tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
                .await
                .map_err(|_| PeerError::Unavailable("ws connect timeout".into()))?
                .map_err(|e| PeerError::Unavailable(format!("ws connect: {}", e.kind())))?;
        let _ = tcp.set_nodelay(true);
        let (ws, resp) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::client_async(req, tcp),
        )
        .await
        .map_err(|_| PeerError::Unavailable("ws upgrade timeout".into()))?
        .map_err(|_| PeerError::Unavailable("ws upgrade refused".into()))?;
        if resp
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|v| v.to_str().ok())
            != Some(WS_PROTOCOL)
        {
            return Err(PeerError::Unavailable("gateway protocol mismatch".into()));
        }
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let conn = Arc::new(Conn {
            generation: self.generation.fetch_add(1, Ordering::SeqCst) + 1,
            out,
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
        });
        let w = conn.clone();
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            w.close("write failed");
        });
        let r = conn.clone();
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                match m {
                    Message::Text(t) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            r.route(v);
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            r.close("connection closed");
        });
        Ok(conn)
    }

    /// Live connection, reconnecting with bounded exponential backoff.
    async fn connection(&self) -> Result<Arc<Conn>, PeerError> {
        let mut guard = self.conn.lock().await;
        if let Some(c) = guard.as_ref().filter(|c| c.alive.load(Ordering::SeqCst)) {
            return Ok(c.clone());
        }
        *guard = None;
        let start = Instant::now();
        let mut backoff = self.opts.initial_backoff;
        loop {
            let left = self.opts.connect_wait.saturating_sub(start.elapsed());
            let err = match tokio::time::timeout(left, self.dial()).await {
                Ok(Ok(c)) => {
                    *guard = Some(c.clone());
                    return Ok(c);
                }
                // no profile home: retrying cannot help, so say why now instead of at the deadline
                Ok(Err(e)) if !self.opts.profile_home.is_dir() => return Err(e),
                Ok(Err(e)) => e.to_string(),
                Err(_) => "connect timed out".into(),
            };
            let left = self.opts.connect_wait.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Err(PeerError::Unavailable(format!(
                    "no ready gateway after {} ms: {err}",
                    ms(self.opts.connect_wait)
                )));
            }
            tokio::time::sleep(backoff.min(left)).await;
            backoff = (backoff * 2).min(self.opts.max_backoff);
        }
    }

    /// Runtime session id for `conv` on `conn`: reuse, resume the stored one, or create it.
    async fn session(&self, conv: &str, conn: &Conn) -> Result<String, PeerError> {
        let stored = {
            let s = self.sessions.lock().unwrap();
            if let Some((g, sid)) = s.runtime.get(conv) {
                if *g == conn.generation {
                    return Ok(sid.clone());
                }
            }
            s.p.stored.get(conv).cloned()
        };
        if let Some(stored_id) = stored {
            match conn
                .rpc(
                    "session.resume",
                    json!({"session_id": stored_id}),
                    RPC_TIMEOUT,
                )
                .await
            {
                Ok(r) => {
                    let sid = r["session_id"]
                        .as_str()
                        .ok_or_else(|| PeerError::Protocol("resume: no session_id".into()))?;
                    self.sessions
                        .lock()
                        .unwrap()
                        .runtime
                        .insert(conv.into(), (conn.generation, sid.into()));
                    return Ok(sid.into());
                }
                // gone on the gateway side: fall through to a fresh dedicated session. Any other
                // refusal (permission, draining) keeps the stored session and its history.
                // A fresh request_id (next generation, persisted before use) keeps the
                // gateway's idempotency registry from handing back the dead session.
                Err(PeerError::Rpc { message, .. }) if message == "not_found" => {
                    let snapshot = {
                        let mut s = self.sessions.lock().unwrap();
                        *s.p.gens.entry(conv.into()).or_insert(0) += 1;
                        s.p.stored.remove(conv);
                        s.p.clone()
                    };
                    self.persist(&snapshot);
                }
                Err(e) => return Err(e),
            }
        }
        let gen = self
            .sessions
            .lock()
            .unwrap()
            .p
            .gens
            .get(conv)
            .copied()
            .unwrap_or(0);
        // Hermes resolves a titled create to a live session with that title, so each
        // generation needs its own title or the dropped session comes back.
        let title = if gen == 0 {
            format!("g2h peer {conv}")
        } else {
            format!("g2h peer {conv} #{gen}")
        };
        let mut params = json!({
            "title": title,
            "source": "oneshot",
            "request_id": peer_request_id(conv, gen),
        });
        params["toolsets"] = json!(self.opts.peer_toolsets);
        let r = conn.rpc("session.create", params, RPC_TIMEOUT).await?;
        let sid = r["session_id"]
            .as_str()
            .ok_or_else(|| PeerError::Protocol("create: no session_id".into()))?;
        let stored_id = r["stored_session_id"].as_str().unwrap_or(sid);
        let snapshot = {
            let mut s = self.sessions.lock().unwrap();
            s.p.stored.insert(conv.into(), stored_id.into());
            s.runtime.insert(conv.into(), (conn.generation, sid.into()));
            s.p.clone()
        };
        self.persist(&snapshot);
        Ok(sid.into())
    }

    /// Bounded context: if the conv's session hit a limit, drop it (a new generation, so the
    /// old one is never resumed or handed back) and queue a recap for the new session.
    /// Returns the text to submit and whether it starts a rotated session.
    fn bound_context(&self, conv: &str, text: &str) -> (String, bool) {
        let (max_turns, max_tokens) = (self.opts.max_turns, self.opts.max_tokens);
        let snapshot = {
            let mut s = self.sessions.lock().unwrap();
            let turns = s.p.turns.get(conv).copied().unwrap_or(0);
            let tokens = s.p.tokens.get(conv).copied().unwrap_or(0);
            let over =
                (max_turns > 0 && turns >= max_turns) || (max_tokens > 0 && tokens >= max_tokens);
            if over && s.p.stored.contains_key(conv) {
                *s.p.gens.entry(conv.into()).or_insert(0) += 1;
                s.p.stored.remove(conv);
                s.runtime.remove(conv);
                s.p.turns.remove(conv);
                s.p.tokens.remove(conv);
                s.p.totals.remove(conv);
                let r = recap(s.p.recent.get(conv).map_or(&[][..], |v| v.as_slice()));
                s.p.seed.insert(conv.into(), r);
                Some(s.p.clone())
            } else {
                None
            }
        };
        if let Some(p) = snapshot {
            self.persist(&p);
        }
        match self.sessions.lock().unwrap().p.seed.get(conv) {
            Some(r) => (format!("{r}\n\n{text}"), true),
            None => (text.to_string(), false),
        }
    }

    /// Count a completed turn toward the policy and remember the exchange for a recap.
    /// Returns this turn's tokens: the growth of the session's running total.
    fn record_turn(&self, conv: &str, text: &str, reply: &Reply) -> u64 {
        let (snapshot, delta) = {
            let mut s = self.sessions.lock().unwrap();
            s.p.seed.remove(conv);
            *s.p.turns.entry(conv.into()).or_insert(0) += 1;
            // `usage.total` is the session's running total: this turn's share is its growth
            // (all of it when the total restarted, e.g. a fresh session or agent)
            let total = reply.usage_tokens.unwrap_or(0);
            let last = s.p.totals.insert(conv.into(), total).unwrap_or(0);
            let delta = if total >= last { total - last } else { total };
            // the rotation policy prefers the reported context occupancy
            let ctx = reply.context_tokens.unwrap_or(delta);
            s.p.tokens.insert(conv.into(), ctx);
            let r = s.p.recent.entry(conv.into()).or_default();
            r.push((clip(text, 600), clip(&reply.text, 600)));
            let excess = r.len().saturating_sub(RECAP_EXCHANGES);
            r.drain(..excess);
            (s.p.clone(), delta)
        };
        self.persist(&snapshot);
        delta
    }

    async fn turn(&self, conv: &str, text: &str, deadline: Instant) -> Result<Reply, PeerError> {
        let (submit, rotated) = self.bound_context(conv, text);
        let mut reply = self.turn_once(conv, &submit, deadline).await?;
        reply.rotated = rotated;
        let delta = self.record_turn(conv, text, &reply);
        // callers (the daily cap) see this turn's tokens, never the running total
        reply.usage_tokens = reply.usage_tokens.map(|_| delta);
        Ok(reply)
    }

    async fn turn_once(
        &self,
        conv: &str,
        text: &str,
        deadline: Instant,
    ) -> Result<Reply, PeerError> {
        // Setup is safe to retry once after a drop (resume/create are idempotent here).
        // The ask deadline caps connect + resume/create + that retry too.
        let setup = async {
            let mut attempt = 0;
            loop {
                let conn = self.connection().await?;
                match self.session(conv, &conn).await {
                    Ok(sid) => return Ok((conn, sid)),
                    Err(PeerError::Disconnected(_)) if attempt == 0 => attempt += 1,
                    Err(e) => return Err(e),
                }
            }
        };
        let (conn, sid) = match tokio::time::timeout_at(deadline.into(), setup).await {
            Ok(r) => r?,
            Err(_) => return Err(PeerError::Deadline),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        conn.subs.lock().unwrap().insert(sid.clone(), tx);
        let res = self.run_turn(&conn, &sid, text, deadline, &mut rx).await;
        conn.subs.lock().unwrap().remove(&sid);
        if matches!(res, Err(PeerError::Disconnected(_))) {
            self.sessions.lock().unwrap().runtime.remove(conv);
        }
        res
    }

    async fn run_turn(
        &self,
        conn: &Conn,
        sid: &str,
        text: &str,
        deadline: Instant,
        rx: &mut mpsc::UnboundedReceiver<Value>,
    ) -> Result<Reply, PeerError> {
        let t0 = Instant::now();
        let params = json!({"session_id": sid, "input_id": uuid::Uuid::now_v7().simple().to_string(), "text": text, "finite": true, "unattended": true});
        let left = deadline.saturating_duration_since(t0);
        match conn
            .rpc("prompt.submit", params, left.min(RPC_TIMEOUT))
            .await
        {
            Ok(_) => {}
            Err(PeerError::Protocol(_)) if Instant::now() >= deadline => {
                return self.expire(conn, sid).await
            }
            Err(e) => return Err(e),
        }
        let mut first_delta = None;
        let mut acc = String::new();
        loop {
            let ev = match tokio::time::timeout_at(deadline.into(), rx.recv()).await {
                Err(_) => return self.expire(conn, sid).await,
                Ok(None) => return Err(PeerError::Disconnected("connection lost mid-turn".into())),
                Ok(Some(ev)) => ev,
            };
            let p = &ev["payload"];
            match ev["type"].as_str() {
                Some("message.delta") => {
                    first_delta.get_or_insert_with(|| ms(t0.elapsed()));
                    acc.push_str(p["text"].as_str().unwrap_or(""));
                }
                Some("message.complete") => {
                    let status = p["status"].as_str().unwrap_or("complete");
                    if status == "error" {
                        return Err(PeerError::Rpc {
                            code: 0,
                            message: p["text"]
                                .as_str()
                                .unwrap_or("turn failed")
                                .chars()
                                .take(200)
                                .collect(),
                        });
                    }
                    let text = p["text"].as_str().map(String::from).unwrap_or(acc);
                    let (text, oob_stripped) = super::strip_oob(&text);
                    return Ok(Reply {
                        text,
                        oob_stripped,
                        usage_tokens: running_total(&p["usage"]),
                        context_tokens: p["usage"]["context_used"].as_u64(),
                        first_delta_ms: first_delta,
                        total_ms: ms(t0.elapsed()),
                        rotated: false,
                    });
                }
                _ => {}
            }
        }
    }

    async fn expire(&self, conn: &Conn, sid: &str) -> Result<Reply, PeerError> {
        let _ = conn
            .rpc(
                "session.interrupt",
                json!({"session_id": sid}),
                INTERRUPT_TIMEOUT,
            )
            .await;
        Err(PeerError::Deadline)
    }
}

impl PeerBackend for GatewayWsBackend {
    async fn ask(
        &self,
        conv_id: &str,
        framed_text: &str,
        deadline: Duration,
    ) -> Result<Reply, PeerError> {
        let end = Instant::now() + deadline;
        self.turn(conv_id, framed_text, end).await
    }

    async fn interrupt(&self, conv_id: &str) -> Result<(), PeerError> {
        let Some(conn) = self
            .conn
            .lock()
            .await
            .clone()
            .filter(|c| c.alive.load(Ordering::SeqCst))
        else {
            return Ok(());
        };
        let sid = match self.sessions.lock().unwrap().runtime.get(conv_id) {
            Some((g, sid)) if *g == conn.generation => sid.clone(),
            _ => return Ok(()),
        };
        conn.rpc(
            "session.interrupt",
            json!({"session_id": sid}),
            INTERRUPT_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    async fn health(&self) -> Health {
        let toolsets = if self.opts.peer_toolsets.is_empty() {
            "; peer sessions have no tools".to_string()
        } else {
            format!("; peer toolsets {:?}", self.opts.peer_toolsets)
        };
        match discover(&self.opts.profile_home).await {
            Ok(ep) => Health {
                ok: true,
                detail: format!("gateway ready (epoch {}){toolsets}", ep.authority_epoch),
            },
            Err(e) => Health {
                ok: false,
                detail: format!("{e}{toolsets}"),
            },
        }
    }
}

/// Stable hex request id for a conv's `session.create`: the gateway's idempotency registry is
/// keyed by `request_id`, so a retried create after a drop returns the same session.
/// Generation 0 keeps the original id; later generations are salted with it.
/// The session's running token total from a `message.complete` usage:
/// `total`, else `input + output` (Hermes reports both as session totals).
fn running_total(usage: &Value) -> Option<u64> {
    usage["total"].as_u64().or_else(
        || match (usage["input"].as_u64(), usage["output"].as_u64()) {
            (None, None) => None,
            (i, o) => Some(i.unwrap_or(0) + o.unwrap_or(0)),
        },
    )
}

fn peer_request_id(conv: &str, gen: u64) -> String {
    use sha2::{Digest, Sha256};
    let seed = if gen == 0 {
        format!("g2h-peer-{conv}")
    } else {
        format!("g2h-peer-{conv}#{gen}")
    };
    let d = Sha256::digest(seed.as_bytes());
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}
