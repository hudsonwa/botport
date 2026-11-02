//! `g2hd`: the broker. Listens only on the Unix socket in the g2h home (no TCP
//! listener exists anywhere in this crate), one instance per home (lock file),
//! newline-JSON requests per [`crate::ipc`]. Single-threaded: run [`Bound::run`]
//! inside a `tokio::task::LocalSet`.
//!
//! Grok side: `ask`/`send` deliver framed, secret-scanned text to the Hermes
//! bot through a [`PeerBackend`]; replies (and `hermes_send` messages) land in
//! Grok's inbox, which `wait` long-polls (woken by a `Notify`, no polling) and
//! `recv` drains.
//!
//! Loop prevention: a message's hop is 0 unless it names `reply_to`, then it is
//! that message's hop + 1; the Hermes reply to a Grok message is hop + 1 again.
//! Grok -> Hermes needs room for the reply (hop + 1 < limit); Hermes -> Grok
//! needs hop < limit; nothing may reply to `end`/`ack`. Message/token/wake
//! caps live in the persisted state.
//!
//! Wake: when a message reaches Grok's inbox with no `wait` open, a wake is
//! scheduled after the debounce (batching later messages); while the Grok bot is active
//! (a Grok-side call within the active window) it is scheduled for when the
//! window lapses instead. `bye` clears the window and reschedules if the inbox
//! is non-empty; a timer that finds Grok active again re-arms itself. A reply to
//! a `send` that arrives after a later `bye` is queued quietly (it rides on the
//! next wake); `send --no-reply`/`notice` replies are only logged. At fire time
//! the inbox/wait checks repeat, then the daily cap (urgent-only past 80 %) and
//! any stop alert apply; the webhook POST runs on its own task. `dry_run` (default) logs
//! the would-be POST (host only, no key). 4xx stops waking (alert in status);
//! 5xx/timeouts retry `POST_ATTEMPTS` times with doubling backoff.

// Handlers return the wire `Response` as their error; it is built once per request.
#![allow(clippy::result_large_err)]

use crate::config::{BackendOptions, Config};
use crate::envelope::{new_id, Envelope, Kind};
use crate::framing::frame;
use crate::guard::{Guard, LocalClock, OpOutcome, Refusal};
use crate::hermes::{A2aBackend, GatewayWsBackend, Health, PeerBackend, PeerError, Reply};
use crate::ipc::{self, Check, Request, Response, StatusInfo, MAX_TIMEOUT_SECS};
use crate::isolation::{self, Mode};
use crate::log::Logger;
use crate::ops::{
    parse_op_body, Allowlist, Decision, OpGuard, OpRequest, OpResult, OpStatus, Origin, PendingOp,
    SystemClock, Tier, TO_GROK_KEEP,
};
use crate::paths::{ensure_home_mode, Paths};
use crate::secrets::{redact, Fingerprints};
use crate::store::State;
use crate::wake::{self, PostOutcome};
use serde_json::json;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

pub const GROK: &str = "grok";
pub const HERMES: &str = "hermes";
/// Grok's inbox (queue key in the state file).
pub const INBOX: &str = "hermes->grok";
/// The owner's inbox: `op_result`s from Grok for ops the owner sent it.
pub const OWNER_INBOX: &str = "grok->owner";
pub const POST_ATTEMPTS: u32 = 3;
const DEFAULT_ASK_SECS: u64 = 300;
const DEFAULT_WAIT_SECS: u64 = 60;
const MAX_LINE: u64 = 1 << 20;
/// Sent to Grok instead of a reply that was nothing but stripped OOB blocks.
const OOB_ONLY_NOTICE: &str =
    "(the Grok bot's reply contained only injected out-of-band text; dropped)";
/// Remembered message ids for `reply_to` (hop/kind lookup).
const META_KEEP: usize = 10_000;

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub paths: Paths,
    pub config: Config,
    pub debounce: Duration,
    pub active_window: Duration,
    /// First retry delay after a 5xx/timeout; doubles per attempt.
    pub retry_base: Duration,
    pub post_timeout: Duration,
    /// Selftest probes (token -> wake time) are dropped after this.
    pub probe_ttl: Duration,
    /// A one-time op approval code expires after this (10 min).
    pub code_ttl: Duration,
    /// Tests only: treat every connection as coming from this uid, to
    /// simulate the bots' user without a second OS account. Exists only with
    /// the non-default `test-hooks` feature, so release g2hd cannot have it.
    #[cfg(feature = "test-hooks")]
    pub peer_uid_override: Option<u32>,
}

impl DaemonOptions {
    pub fn new(paths: Paths, config: Config) -> DaemonOptions {
        DaemonOptions {
            debounce: Duration::from_secs(config.debounce_secs),
            active_window: Duration::from_secs(config.active_window_minutes * 60),
            retry_base: Duration::from_secs(2),
            post_timeout: Duration::from_secs(10),
            probe_ttl: Duration::from_secs(15 * 60),
            code_ttl: Duration::from_secs(10 * 60),
            #[cfg(feature = "test-hooks")]
            peer_uid_override: None,
            paths,
            config,
        }
    }
}

/// The configured backend, as one concrete type.
pub enum AnyBackend {
    Gateway(Box<GatewayWsBackend>),
    A2a(A2aBackend),
}

impl AnyBackend {
    pub fn from_options(o: BackendOptions) -> AnyBackend {
        match o {
            BackendOptions::Gateway(g) => AnyBackend::Gateway(Box::new(GatewayWsBackend::new(g))),
            BackendOptions::A2a(a) => AnyBackend::A2a(A2aBackend::new(a)),
        }
    }
}

impl PeerBackend for AnyBackend {
    async fn ask(&self, conv: &str, text: &str, d: Duration) -> Result<Reply, PeerError> {
        match self {
            AnyBackend::Gateway(b) => b.ask(conv, text, d).await,
            AnyBackend::A2a(b) => b.ask(conv, text, d).await,
        }
    }
    async fn interrupt(&self, conv: &str) -> Result<(), PeerError> {
        match self {
            AnyBackend::Gateway(b) => b.interrupt(conv).await,
            AnyBackend::A2a(b) => b.interrupt(conv).await,
        }
    }
    async fn health(&self) -> Health {
        match self {
            AnyBackend::Gateway(b) => b.health().await,
            AnyBackend::A2a(b) => b.health().await,
        }
    }
}

/// Home checked, lock held, socket bound (0600). Nothing is served until `run`.
pub struct Bound {
    opts: DaemonOptions,
    listener: std::os::unix::net::UnixListener,
    _lock: File,
    /// g2hd's own uid (its effective uid).
    uid: u32,
}

pub fn bind(opts: &DaemonOptions) -> Result<Bound, String> {
    let p = &opts.paths;
    let sep = opts.config.separate_user.as_ref();
    let mode = Mode::of(sep);
    ensure_home_mode(&p.home, mode.home_mode()).map_err(|e| format!("g2h home: {e}"))?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode_0600()
        .open(p.lock())
        .map_err(|e| format!("{}: {e}", p.lock().display()))?;
    if lock.try_lock().is_err() {
        return Err(format!(
            "g2hd already running for {} (lock {} is held)",
            p.home.display(),
            p.lock().display()
        ));
    }
    // the real euid, not the lock file's owner: a lock left by an earlier run
    // would otherwise let g2hd under root pass as the g2h user
    let uid = isolation::euid();
    if let Some(s) = sep {
        let approver = opts
            .config
            .ops_approver
            .as_ref()
            .map(|a| a.command.as_slice());
        isolation::check_startup(s, uid, &p.home, &p.config(), approver)?;
        std::os::unix::fs::chown(&p.home, None, Some(s.socket_gid)).map_err(|e| {
            format!(
                "chgrp {} {}: {e} (g2hd's user must be in that group)",
                s.socket_gid,
                p.home.display()
            )
        })?;
    }
    let sock = p.socket();
    if std::fs::symlink_metadata(&sock).is_ok() {
        // we hold the lock, so any socket file left here is stale
        std::fs::remove_file(&sock).map_err(|e| format!("stale socket {}: {e}", sock.display()))?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&sock)
        .map_err(|e| format!("bind {}: {e}", sock.display()))?;
    if let Some(s) = sep {
        std::os::unix::fs::chown(&sock, None, Some(s.socket_gid))
            .map_err(|e| format!("chgrp {} {}: {e}", s.socket_gid, sock.display()))?;
    }
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(mode.socket_mode()))
        .map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(Bound {
        opts: opts.clone(),
        listener,
        _lock: lock,
        uid,
    })
}

trait Mode0600 {
    fn mode_0600(&mut self) -> &mut Self;
}
impl Mode0600 for std::fs::OpenOptions {
    fn mode_0600(&mut self) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::mode(self, 0o600)
    }
}

struct Inner {
    state: State,
    guard: Guard<LocalClock>,
    /// message id -> (hop, kind)
    meta: HashMap<String, (u32, Kind)>,
    meta_order: VecDeque<String>,
    last_grok: Option<Instant>,
    waiters: u64,
    in_flight: u64,
    wake_pending: bool,
    /// Bumped to cancel a scheduled wake timer.
    wake_gen: u64,
    last_bye: Option<Instant>,
    urgent_pending: bool,
    dirty: bool,
    /// selftest token -> (wake fired at, wake->report ms once reported)
    probes: HashMap<String, (Instant, Option<u64>)>,
    /// A live selftest POST is running.
    probe_in_flight: bool,
}

struct Broker<B> {
    opts: DaemonOptions,
    backend: B,
    inner: RefCell<Inner>,
    inbox: Notify,
    save: Notify,
    reports: Notify,
    log: Logger,
    fingerprints: Option<Fingerprints>,
    ops: OpGuard<SystemClock>,
    /// request_id -> one-time approval code. Memory only: never stored,
    /// logged (only a sha256 prefix) or returned; a restart drops them.
    codes: RefCell<HashMap<String, ApprovalCode>>,
    /// g2hd's own uid; see `isolation`.
    uid: u32,
}

struct ApprovalCode {
    code: String,
    expires: Instant,
    tries: u32,
}

/// Wrong codes before a pending op is dropped.
const MAX_CODE_TRIES: u32 = 5;

/// A one-time code: 10 chars from a 32-symbol alphabet (50 bits), from /dev/urandom.
fn new_code() -> std::io::Result<String> {
    use std::io::Read;
    const ALPHA: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut raw = [0u8; 10];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    Ok(raw
        .iter()
        .map(|b| ALPHA[(b & 31) as usize] as char)
        .collect())
}

fn code_hash(code: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(code.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Run the owner's approver with `msg` on stdin; it must exit 0 within 60 s.
async fn run_approver(argv: &[String], msg: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if let Some(mut w) = child.stdin.take() {
        w.write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("stdin: {e}"))?;
    }
    match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
        Ok(Ok(st)) if st.success() => Ok(()),
        Ok(Ok(st)) => Err(format!("exit {st}")),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn err(code: &str, message: impl Into<String>) -> Response {
    Response::Error {
        code: code.into(),
        message: message.into(),
    }
}

fn refusal(r: &Refusal) -> Response {
    match r {
        Refusal::HopLimit { hop, limit } => {
            err("hop_limit", format!("hop {hop} reaches the limit {limit}"))
        }
        Refusal::NoReplyToKind(k) => err("no_reply", format!("no reply to a {k:?} message")),
        Refusal::MessageCap => err("cap", "daily message cap reached"),
        Refusal::TokenCap => err("cap", "daily token cap reached"),
        Refusal::WakeCap => err("cap", "daily wake cap reached"),
        Refusal::TurnInFlight(d) => err("busy", format!("a turn is already in flight ({d})")),
    }
}

fn op_result_body(request_id: &str, status: OpStatus, detail: &str) -> String {
    serde_json::to_string(&OpResult {
        request_id: request_id.into(),
        status,
        detail: detail.into(),
    })
    .unwrap_or_default()
}

fn op_response(request_id: &str, status: OpStatus, detail: impl Into<String>) -> Response {
    Response::Op {
        result: OpResult {
            request_id: request_id.into(),
            status,
            detail: detail.into(),
        },
    }
}

fn est_tokens(s: &str) -> u64 {
    (s.chars().count() as u64).div_ceil(4)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Decrements the open-wait count when a `wait` ends or its client goes away.
struct Waiting<'a, B>(&'a Broker<B>);
impl<B> Drop for Waiting<'_, B> {
    fn drop(&mut self) {
        self.0.inner.borrow_mut().waiters -= 1;
    }
}

impl<B: PeerBackend + 'static> Broker<B> {
    fn logw(&self, mut rec: serde_json::Value) {
        rec["ts"] = json!(crate::envelope::now_ts());
        if let Err(e) = self.log.write(&rec) {
            eprintln!("g2hd: log write failed: {e}");
        }
    }

    fn changed(&self, inner: &mut Inner) {
        inner.dirty = true;
        self.save.notify_one();
    }

    fn save_now(&self) {
        let mut i = self.inner.borrow_mut();
        if !i.dirty {
            return;
        }
        i.state.counters = i.guard.counters.clone();
        i.state.purge_expired(now_unix());
        match i.state.save(&self.opts.paths.state()) {
            Ok(()) => i.dirty = false,
            Err(e) => eprintln!("g2hd: state save failed: {e}"),
        }
    }

    fn scrub(&self, s: &str) -> String {
        redact(s, self.fingerprints.as_ref()).0
    }

    fn touch_grok(&self) {
        self.inner.borrow_mut().last_grok = Some(Instant::now());
    }

    fn grok_active(&self, i: &Inner) -> bool {
        i.last_grok
            .is_some_and(|t| t.elapsed() < self.opts.active_window)
    }

    fn remember(i: &mut Inner, env: &Envelope) {
        i.meta.insert(env.id.clone(), (env.hop, env.kind));
        i.meta_order.push_back(env.id.clone());
        while i.meta_order.len() > META_KEEP {
            if let Some(old) = i.meta_order.pop_front() {
                i.meta.remove(&old);
            }
        }
    }

    /// Hop for a new message answering `reply_to` (0 when None).
    fn hop_for(i: &Inner, reply_to: Option<&str>) -> Result<u32, Response> {
        let Some(id) = reply_to else { return Ok(0) };
        match i.meta.get(id) {
            None => Err(err("unknown_reply_to", "reply_to names no known message")),
            Some((_, k @ (Kind::End | Kind::Ack | Kind::Notice))) => {
                Err(refusal(&Refusal::NoReplyToKind(*k)))
            }
            Some((hop, _)) => Ok(hop.saturating_add(1)),
        }
    }

    fn refuse(&self, what: &str, conv: &str, r: &Response) -> Response {
        if let Response::Error { code, message } = r {
            self.logw(json!({"event": "refused", "what": what, "conv_id": conv, "code": code, "reason": message}));
        }
        r.clone()
    }

    /// Checks and accounting for a Grok -> Hermes message; returns its envelope.
    fn admit_outbound(
        &self,
        text: &str,
        conv_id: Option<String>,
        reply_to: Option<&str>,
    ) -> Result<Envelope, Response> {
        self.admit(Kind::Msg, text, conv_id, reply_to)
    }

    /// Hermes -> Grok outcome of a `send`: one-way replies are only logged; a
    /// reply after a later `bye` is queued without scheduling a wake.
    fn send_reply(self: &Rc<Self>, renv: Envelope, one_way: bool, sent_at: Instant) {
        if one_way {
            self.logw(json!({"event": "reply_suppressed", "conv_id": renv.conv_id, "id": renv.id, "len": renv.body.len()}));
            return;
        }
        let after_bye = self.inner.borrow().last_bye.is_some_and(|b| b > sent_at);
        if renv.kind == Kind::Notice {
            self.enqueue(renv, false); // rides on the next wake
        } else if after_bye {
            self.logw(json!({"event": "reply_quiet", "reason": "conv had bye", "id": renv.id}));
            self.enqueue(renv, false);
        } else {
            self.deliver_to_grok(renv, false);
        }
    }

    fn admit(
        &self,
        kind: Kind,
        text: &str,
        conv_id: Option<String>,
        reply_to: Option<&str>,
    ) -> Result<Envelope, Response> {
        let conv = conv_id.unwrap_or_else(new_id);
        let mut i = self.inner.borrow_mut();
        let r = (|| {
            let hop = Self::hop_for(&i, reply_to)?;
            i.guard
                .check_hop(hop)
                .and_then(|_| i.guard.check_hop(hop.saturating_add(1)))
                .map_err(|e| refusal(&e))?;
            let body = self.scrub(text);
            let tokens = est_tokens(&body);
            // room for this message and the reply it will produce
            i.guard.check_room(2, tokens).map_err(|e| refusal(&e))?;
            i.guard
                .begin_turn(&format!("grok->hermes:{conv}"))
                .map_err(|e| refusal(&e))?;
            i.guard.record_message(tokens).map_err(|e| refusal(&e))?;
            let mut env = Envelope::new(&conv, GROK, HERMES, kind, &body);
            env.hop = hop;
            env.reply_to = reply_to.map(String::from);
            Ok(env)
        })();
        match r {
            Ok(env) => {
                Self::remember(&mut i, &env);
                i.in_flight += 1;
                self.changed(&mut i);
                drop(i);
                self.logw(json!({"event": "message", "dir": "grok->hermes", "env": env}));
                Ok(env)
            }
            Err(e) => {
                drop(i);
                Err(self.refuse("grok->hermes", &conv, &e))
            }
        }
    }

    /// Deliver to the Hermes bot and account the reply.
    async fn turn(
        self: &Rc<Self>,
        env: &Envelope,
        timeout: Duration,
    ) -> Result<(Envelope, Reply), Response> {
        self.turn_with(env, &frame(GROK, &env.body), None, timeout)
            .await
    }

    /// One Hermes turn with `prompt`. With `op_id`, the reply comes back as an
    /// `op_result` envelope for that request.
    async fn turn_with(
        self: &Rc<Self>,
        env: &Envelope,
        prompt: &str,
        op_id: Option<&str>,
        timeout: Duration,
    ) -> Result<(Envelope, Reply), Response> {
        let res = self.backend.ask(&env.conv_id, prompt, timeout).await;
        {
            let mut i = self.inner.borrow_mut();
            i.guard.end_turn(&format!("grok->hermes:{}", env.conv_id));
            i.in_flight -= 1;
        }
        let reply = match res {
            Ok(r) => r,
            Err(e) => {
                self.logw(json!({"event": "peer_error", "conv_id": env.conv_id, "id": env.id, "error": e.to_string()}));
                let code = if e == PeerError::Deadline {
                    "timeout"
                } else {
                    "peer"
                };
                return Err(err(code, e.to_string()));
            }
        };
        if reply.oob_stripped > 0 {
            self.logw(
                json!({"event": "oob_stripped", "conv_id": env.conv_id, "len": reply.oob_stripped}),
            );
        }
        if reply.rotated {
            self.logw(
                json!({"event": "peer_session_rotated", "conv_id": env.conv_id,
                "max_turns": self.opts.config.peer_session_max_turns,
                "max_tokens": self.opts.config.peer_session_max_tokens}),
            );
        }
        let text = self.scrub(&reply.text);
        let renv = match op_id {
            Some(id) => env.reply(Kind::OpResult, &op_result_body(id, OpStatus::Done, &text)),
            None if text.trim().is_empty() && reply.oob_stripped > 0 => {
                env.reply(Kind::Notice, OOB_ONLY_NOTICE)
            }
            None => env.reply(Kind::Msg, &text),
        };
        let tokens = reply.usage_tokens.unwrap_or_else(|| est_tokens(&renv.body));
        let mut i = self.inner.borrow_mut();
        if let Err(r) = i
            .guard
            .check_hop(renv.hop)
            .and_then(|_| i.guard.record_message(tokens))
        {
            drop(i);
            return Err(self.refuse("hermes-reply", &env.conv_id, &refusal(&r)));
        }
        Self::remember(&mut i, &renv);
        self.changed(&mut i);
        drop(i);
        self.logw(json!({"event": "message", "dir": "hermes->grok", "env": renv,
            "first_delta_ms": reply.first_delta_ms, "total_ms": reply.total_ms, "usage_tokens": reply.usage_tokens}));
        Ok((renv, reply))
    }

    /// Put a message in Grok's inbox and wake any `wait` (no webhook wake).
    fn enqueue(&self, env: Envelope, urgent: bool) {
        let mut i = self.inner.borrow_mut();
        i.state
            .push(INBOX, env, now_unix(), self.opts.config.queue_ttl_secs);
        i.urgent_pending |= urgent;
        self.changed(&mut i);
        drop(i);
        self.inbox.notify_waiters();
    }

    /// Put a message in Grok's inbox, wake any `wait`, maybe schedule a wake.
    fn deliver_to_grok(self: &Rc<Self>, env: Envelope, urgent: bool) {
        self.enqueue(env, urgent);
        self.maybe_schedule_wake();
    }

    /// Schedule a wake if the inbox is non-empty, no `wait` is open and none is
    /// pending: after the debounce, or when the Grok bot's active window lapses.
    fn maybe_schedule_wake(self: &Rc<Self>) {
        let mut i = self.inner.borrow_mut();
        // notices never wake on their own; they ride on the next real wake
        let wakeable = i
            .state
            .queues
            .get(INBOX)
            .is_some_and(|q| q.iter().any(|m| m.env.kind != Kind::Notice));
        if i.waiters > 0 || i.wake_pending || !wakeable {
            return;
        }
        let active_left = i.last_grok.map_or(Duration::ZERO, |t| {
            self.opts.active_window.saturating_sub(t.elapsed())
        });
        i.wake_pending = true;
        i.wake_gen += 1;
        let gen = i.wake_gen;
        drop(i);
        let me = self.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(active_left + me.opts.debounce).await;
            me.fire_wake(gen);
        });
    }

    /// Cancel any scheduled wake and schedule afresh (after `bye`).
    fn reschedule_wake(self: &Rc<Self>) {
        let mut i = self.inner.borrow_mut();
        i.wake_gen += 1;
        i.wake_pending = false;
        drop(i);
        self.maybe_schedule_wake();
    }

    fn fire_wake(self: &Rc<Self>, gen: u64) {
        let mut i = self.inner.borrow_mut();
        if i.wake_gen != gen {
            return; // superseded
        }
        i.wake_pending = false;
        if i.waiters == 0 && self.grok_active(&i) {
            // The Grok bot became active again since scheduling: re-arm for the new lapse
            drop(i);
            self.maybe_schedule_wake();
            return;
        }
        let now = now_unix();
        i.state.purge_expired(now);
        let count = i.state.len(INBOX);
        let skip = if i.waiters > 0 {
            Some("wait open".to_string())
        } else if count == 0 {
            Some("inbox empty".into())
        } else {
            i.state.alert.as_ref().map(|a| format!("stopped: {a}"))
        };
        if let Some(why) = skip {
            drop(i);
            self.logw(json!({"event": "wake_skipped", "reason": why}));
            return;
        }
        let urgent = std::mem::take(&mut i.urgent_pending);
        let cap = i.guard.limits.max_wakes;
        let wakes = i.guard.today().wakes;
        let urgent_only_from = (cap * 8).div_ceil(10);
        if wakes >= urgent_only_from && wakes < cap && !urgent {
            drop(i);
            self.logw(
                json!({"event": "refused", "what": "wake", "code": "urgent_only",
                "reason": format!("{wakes}/{cap} wakes used; past 80% only urgent messages wake")}),
            );
            return;
        }
        if let Err(r) = i.guard.record_wake() {
            drop(i);
            self.logw(json!({"event": "refused", "what": "wake", "code": "cap", "reason": format!("{r:?}")}));
            return;
        }
        let conv = i
            .state
            .queues
            .get(INBOX)
            .and_then(|q| q.back())
            .map(|m| m.env.conv_id.clone())
            .unwrap_or_default();
        self.changed(&mut i);
        drop(i);
        let body = wake::body(&conv, count);
        let creds_path = self.opts.paths.wake_credentials();
        if self.opts.config.dry_run {
            let host = match wake::load_creds(&creds_path) {
                Ok(Some(c)) => wake::host_only(&c.url),
                Ok(None) => "(credentials missing)".into(),
                Err(e) => format!("(credentials unusable: {e})"),
            };
            self.logw(json!({"event": "wake", "dry_run": true, "urgent": urgent,
                "would_post": {"host": host, "body": body, "auth": "header from key_mode (key not logged)"}}));
            return;
        }
        let creds = match wake::load_creds(&creds_path) {
            Ok(Some(c)) => c,
            Ok(None) => {
                self.logw(json!({"event": "wake_failed", "reason": "webhook credentials missing"}));
                return;
            }
            Err(e) => {
                self.logw(
                    json!({"event": "wake_failed", "reason": format!("credentials unusable: {e}")}),
                );
                return;
            }
        };
        let me = self.clone();
        tokio::task::spawn_local(async move {
            let host = wake::host_only(&creds.url);
            for attempt in 0..POST_ATTEMPTS {
                match wake::post(&creds, &body, me.opts.post_timeout).await {
                    PostOutcome::Ok(code) => {
                        me.logw(json!({"event": "wake", "dry_run": false, "host": host, "status": code, "attempt": attempt + 1, "body": body}));
                        return;
                    }
                    PostOutcome::Client(code) => {
                        let alert = format!("webhook returned HTTP {code}; waking stopped (fix credentials, then `g2h wake-reset` or restart g2hd)");
                        me.logw(json!({"event": "wake_failed", "host": host, "status": code, "alert": alert}));
                        let mut i = me.inner.borrow_mut();
                        i.state.alert = Some(alert);
                        me.changed(&mut i);
                        return;
                    }
                    PostOutcome::Retryable(why) => {
                        me.logw(json!({"event": "wake_retry", "host": host, "attempt": attempt + 1, "reason": why}));
                        if attempt + 1 < POST_ATTEMPTS {
                            tokio::time::sleep(me.opts.retry_base * 2u32.pow(attempt)).await;
                        }
                    }
                }
            }
            me.logw(
                json!({"event": "wake_failed", "host": host, "reason": "gave up after retries"}),
            );
        });
    }

    /// Put undelivered messages back at the front of Grok's inbox, in order.
    fn requeue(self: &Rc<Self>, envs: Vec<Envelope>) {
        let mut i = self.inner.borrow_mut();
        let now = now_unix();
        for e in envs.into_iter().rev() {
            i.state
                .push_front(INBOX, e, now, self.opts.config.queue_ttl_secs);
        }
        self.changed(&mut i);
        drop(i);
        self.logw(json!({"event": "requeued", "reason": "wait client gone"}));
        self.maybe_schedule_wake();
    }

    /// A `wait` whose client went away: it no longer counts as Grok-bot activity.
    fn wait_client_gone(self: &Rc<Self>) {
        self.inner.borrow_mut().last_grok = None;
        self.logw(json!({"event": "wait_abandoned"}));
        self.maybe_schedule_wake();
    }

    fn drain(&self) -> Vec<Envelope> {
        let mut i = self.inner.borrow_mut();
        let now = now_unix();
        let mut out = Vec::new();
        while let Some(e) = i.state.pop(INBOX, now) {
            out.push(e);
        }
        if !out.is_empty() {
            self.changed(&mut i);
            drop(i);
            let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
            self.logw(json!({"event": "delivered", "to": GROK, "ids": ids}));
        }
        out
    }

    fn timeout(secs: Option<u64>, default: u64) -> Result<Duration, Response> {
        let s = secs.unwrap_or(default);
        if s == 0 || s > MAX_TIMEOUT_SECS {
            return Err(err(
                "bad_request",
                format!("timeout must be 1..={MAX_TIMEOUT_SECS} s"),
            ));
        }
        Ok(Duration::from_secs(s))
    }

    /// Long-poll Grok's inbox. Returns `None` if `gone` resolves (client hung up)
    /// before anything was drained, so no message is ever taken for a dead client.
    async fn wait(
        self: &Rc<Self>,
        timeout_secs: Option<u64>,
        gone: impl Future<Output = ()>,
    ) -> Option<Response> {
        self.touch_grok();
        let t = match Self::timeout(timeout_secs, DEFAULT_WAIT_SECS) {
            Ok(t) => t,
            Err(e) => return Some(e),
        };
        let deadline = tokio::time::Instant::now() + t;
        self.inner.borrow_mut().waiters += 1;
        let _w = Waiting(self);
        tokio::pin!(gone);
        loop {
            let notified = self.inbox.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let envs = self.drain();
            if !envs.is_empty() {
                self.touch_grok();
                return Some(Response::Messages { envs });
            }
            tokio::select! {
                r = tokio::time::timeout_at(deadline, notified) => if r.is_err() { return Some(Response::Timeout) },
                () = &mut gone => return None,
            }
        }
    }

    fn log_op(&self, request_id: &str, op: &str, decision: &str, reason: &str) {
        self.logw(json!({"event": "op_decision", "request_id": request_id, "op": op, "decision": decision, "reason": reason}));
        let outcome = match decision {
            "run" => OpOutcome::Auto,
            "approved" => OpOutcome::Approved,
            "refused" | "denied" => OpOutcome::Refused,
            _ => return,
        };
        // never called with inner borrowed; skip the count rather than panic if it ever is
        if let Ok(mut i) = self.inner.try_borrow_mut() {
            i.guard.record_op(outcome);
            self.changed(&mut i);
        }
    }

    /// An `op_result` for Grok's inbox (refusals of queued ops, failures).
    fn op_result_to_grok(
        self: &Rc<Self>,
        conv: &str,
        request_id: &str,
        status: OpStatus,
        detail: &str,
    ) {
        let env = Envelope::new(
            conv,
            HERMES,
            GROK,
            Kind::OpResult,
            &op_result_body(request_id, status, detail),
        );
        self.logw(json!({"event": "message", "dir": "hermes->grok", "env": env}));
        self.deliver_to_grok(env, false);
    }

    /// Grok asks for an op: the allowlist decides; nothing unapproved reaches Hermes.
    fn send_op(self: &Rc<Self>, name: String, args: serde_json::Value) -> Response {
        let req = OpRequest {
            op: name,
            args,
            request_id: new_id(),
        };
        let conv = new_id();
        // shape and size are checked on the wire form
        let body = serde_json::to_string(&req).unwrap_or_default();
        let decision = match parse_op_body(&body) {
            Err(e) => Decision::Refuse(e),
            Ok(r) => {
                let mut i = self.inner.borrow_mut();
                let d =
                    self.ops
                        .check_in(&mut i.state.ops, &r, Origin { peer: GROK, hop: 0 }, &conv);
                self.changed(&mut i);
                d
            }
        };
        let (rid, op) = (req.request_id.clone(), req.op.clone());
        match decision {
            Decision::Run => {
                self.log_op(&rid, &op, "run", "tier auto");
                self.run_op(req, Tier::Auto, conv)
            }
            Decision::NeedsApproval(id) => {
                self.log_op(&id, &op, "pending_approval", "tier approve");
                self.request_approval(&id, &op, &req.args)
            }
            Decision::Refuse(why) => {
                self.log_op(&rid, &op, "refused", &why);
                op_response(&rid, OpStatus::Refused, why)
            }
        }
    }

    /// Owner -> Grok op: validated, logged and queued for Grok like a message.
    /// Not gated by our allowlist; Grok's owner decides on its side.
    fn op_to_grok(self: &Rc<Self>, name: String, args: serde_json::Value) -> Response {
        let req = OpRequest {
            op: name,
            args,
            request_id: new_id(),
        };
        let body = serde_json::to_string(&req).unwrap_or_default();
        if let Err(e) = parse_op_body(&body) {
            return err("bad_request", e);
        }
        let conv = new_id();
        let mut i = self.inner.borrow_mut();
        if let Err(e) = i
            .guard
            .check_hop(0)
            .and_then(|_| i.guard.record_message(est_tokens(&body)))
        {
            drop(i);
            return self.refuse("owner->grok op", &conv, &refusal(&e));
        }
        let book = &mut i.state.ops;
        book.to_grok.insert(req.request_id.clone(), req.op.clone());
        while book.to_grok.len() > TO_GROK_KEEP {
            book.to_grok.pop_first();
        }
        let env = Envelope::new(&conv, HERMES, GROK, Kind::Op, &body);
        Self::remember(&mut i, &env);
        drop(i);
        self.logw(json!({"event": "op_sent", "to": GROK, "request_id": req.request_id, "op": req.op, "env": env}));
        // owner ops are urgent: they may wake past 80% of the cap (not past it)
        self.deliver_to_grok(env, true);
        Response::Ok {
            id: Some(req.request_id),
        }
    }

    /// Grok's answer to an op the owner sent: single use, into the owner inbox.
    fn op_result_from_grok(
        self: &Rc<Self>,
        request_id: String,
        status: OpStatus,
        detail: String,
    ) -> Response {
        if status == OpStatus::PendingApproval {
            return err("bad_request", "status must be done, refused or failed");
        }
        let mut i = self.inner.borrow_mut();
        let Some(op) = i.state.ops.to_grok.remove(&request_id) else {
            return err(
                "unknown_request",
                format!("no open op sent to grok with request_id {request_id:?}"),
            );
        };
        let detail = self.scrub(&detail);
        let body = op_result_body(&request_id, status, &detail);
        let env = Envelope::new(&new_id(), GROK, HERMES, Kind::OpResult, &body);
        i.state.push(
            OWNER_INBOX,
            env.clone(),
            now_unix(),
            self.opts.config.queue_ttl_secs,
        );
        self.changed(&mut i);
        drop(i);
        self.logw(
            json!({"event": "op_result_from_grok", "request_id": request_id, "op": op, "env": env}),
        );
        Response::Ok { id: Some(env.id) }
    }

    fn drain_owner(&self) -> Vec<Envelope> {
        let mut i = self.inner.borrow_mut();
        let now = now_unix();
        let mut out = Vec::new();
        while let Some(e) = i.state.pop(OWNER_INBOX, now) {
            out.push(e);
        }
        if !out.is_empty() {
            self.changed(&mut i);
        }
        out
    }

    /// Hand an authorised op to the Hermes peer turn as an owner-authorised
    /// block (never inside peer framing); its reply lands in Grok's inbox.
    fn run_op(self: &Rc<Self>, req: OpRequest, tier: Tier, conv: String) -> Response {
        let rid = req.request_id.clone();
        let body = serde_json::to_string(&req).unwrap_or_default();
        let env = match self.admit(Kind::Op, &body, Some(conv), None) {
            Ok(e) => e,
            Err(Response::Error { message, .. }) => {
                self.log_op(&rid, &req.op, "failed", &message);
                return op_response(&rid, OpStatus::Failed, message);
            }
            Err(r) => return r,
        };
        let prompt = crate::framing::op_block(&req.op, &rid, tier, &req.args);
        let me = self.clone();
        let id = rid.clone();
        tokio::task::spawn_local(async move {
            let t = Duration::from_secs(MAX_TIMEOUT_SECS);
            match me.turn_with(&env, &prompt, Some(&id), t).await {
                Ok((renv, _)) => me.deliver_to_grok(renv, false),
                Err(Response::Error { message, .. }) => {
                    me.op_result_to_grok(&env.conv_id, &id, OpStatus::Failed, &message)
                }
                Err(_) => {}
            }
        });
        op_response(
            &rid,
            OpStatus::Done,
            format!("handed to the Hermes bot (tier {tier}); its reply arrives as an op_result"),
        )
    }

    /// Approve-tier op: with no `[ops_approver]` it is refused; otherwise a
    /// one-time code goes to the owner through the approver command.
    fn request_approval(self: &Rc<Self>, id: &str, op: &str, args: &serde_json::Value) -> Response {
        let refuse = |me: &Self, why: String| {
            let mut i = me.inner.borrow_mut();
            i.state.ops.pending.remove(id);
            me.changed(&mut i);
            drop(i);
            me.log_op(id, op, "refused", &why);
            op_response(id, OpStatus::Refused, why)
        };
        let Some(argv) = self
            .opts
            .config
            .ops_approver
            .as_ref()
            .map(|a| a.command.clone())
            .filter(|c| !c.is_empty())
        else {
            return refuse(
                self,
                "approve-each disabled: no out-of-band approver configured".into(),
            );
        };
        let code = match new_code() {
            Ok(c) => c,
            Err(e) => return refuse(self, format!("cannot make an approval code: {e}")),
        };
        let args = self.scrub(&args.to_string());
        let msg = format!(
            "g2h: the Grok bot asks to run op {op:?} with args {args}\nrequest {id}\nTo allow it once, run on your machine:\n  g2h approve {id} {code}\nThe code expires in {} min. Ignore this message to refuse.\n",
            self.opts.code_ttl.as_secs() / 60
        );
        self.logw(json!({"event": "op_code", "request_id": id, "code_sha256": code_hash(&code)}));
        self.codes.borrow_mut().insert(
            id.to_string(),
            ApprovalCode {
                code,
                expires: Instant::now() + self.opts.code_ttl,
                tries: 0,
            },
        );
        let me = self.clone();
        let (rid, rop) = (id.to_string(), op.to_string());
        tokio::task::spawn_local(async move {
            if let Err(e) = run_approver(&argv, &msg).await {
                me.codes.borrow_mut().remove(&rid);
                let _ = me.take_pending(&rid, false);
                me.log_op(
                    &rid,
                    &rop,
                    "refused",
                    &format!("approver command failed: {e}"),
                );
            }
        });
        op_response(id, OpStatus::PendingApproval, "pending owner approval")
    }

    /// The gate for Approve: the right one-time code, before it expires.
    /// Single use; `MAX_CODE_TRIES` wrong codes drop the request.
    fn check_code(self: &Rc<Self>, id: &str, code: &str) -> Result<(), Response> {
        let fail = |why: &str| {
            self.log_op(id, "", "approve_failed", why);
            Err(err("bad_code", why.to_string()))
        };
        let mut codes = self.codes.borrow_mut();
        let Some(c) = codes.get_mut(id) else {
            drop(codes);
            return fail(
                "no live approval code for this request (unknown, used, expired or g2hd restarted)",
            );
        };
        if c.expires <= Instant::now() {
            codes.remove(id);
            drop(codes);
            let _ = self.take_pending(id, false);
            return fail("approval code expired; the request was dropped");
        }
        if !code.is_empty() && ct_eq(code.trim(), &c.code) {
            codes.remove(id);
            return Ok(());
        }
        c.tries += 1;
        if c.tries >= MAX_CODE_TRIES {
            codes.remove(id);
            drop(codes);
            if let Ok(p) = self.take_pending(id, false) {
                let conv = if p.conv_id.is_empty() {
                    new_id()
                } else {
                    p.conv_id
                };
                self.op_result_to_grok(&conv, id, OpStatus::Refused, "approval failed");
            }
            return fail("wrong approval code; too many tries, the request was dropped");
        }
        drop(codes);
        fail("wrong approval code")
    }

    fn take_pending(&self, request_id: &str, approve: bool) -> Result<PendingOp, Response> {
        let mut i = self.inner.borrow_mut();
        let r = if approve {
            self.ops.approve(&mut i.state.ops, request_id)
        } else {
            self.ops.deny(&mut i.state.ops, request_id)
        };
        self.changed(&mut i);
        drop(i);
        r.map_err(|e| {
            self.log_op(
                request_id,
                "",
                if approve {
                    "approve_failed"
                } else {
                    "deny_failed"
                },
                &e,
            );
            err("op_unknown", e)
        })
    }

    async fn handle(self: &Rc<Self>, req: Request) -> Response {
        match req {
            Request::Ask {
                text,
                conv_id,
                reply_to,
                timeout_secs,
            } => {
                self.touch_grok();
                let t = match Self::timeout(timeout_secs, DEFAULT_ASK_SECS) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let env = match self.admit_outbound(&text, conv_id, reply_to.as_deref()) {
                    Ok(e) => e,
                    Err(r) => return r,
                };
                let r = self.turn(&env, t).await;
                // The Grok bot is still in the conversation; anything queued meanwhile
                // wakes once its active window lapses
                self.touch_grok();
                self.maybe_schedule_wake();
                match r {
                    Ok((renv, r)) => Response::Reply {
                        env: renv,
                        first_delta_ms: r.first_delta_ms,
                        total_ms: r.total_ms,
                    },
                    Err(r) => r,
                }
            }
            Request::Send {
                text,
                conv_id,
                reply_to,
                no_reply,
                notice,
            } => {
                self.touch_grok();
                let sent_at = Instant::now();
                let kind = if notice { Kind::Notice } else { Kind::Msg };
                let env = match self.admit(kind, &text, conv_id, reply_to.as_deref()) {
                    Ok(e) => e,
                    Err(r) => return r,
                };
                let id = env.id.clone();
                let me = self.clone();
                tokio::task::spawn_local(async move {
                    let t = Duration::from_secs(MAX_TIMEOUT_SECS);
                    if let Ok((renv, _)) = me.turn(&env, t).await {
                        me.send_reply(renv, no_reply || notice, sent_at);
                    }
                });
                Response::Ok { id: Some(id) }
            }
            Request::Wait { timeout_secs } => self
                .wait(timeout_secs, std::future::pending())
                .await
                .unwrap_or(Response::Timeout),
            Request::Recv => {
                self.touch_grok();
                Response::Messages { envs: self.drain() }
            }
            Request::Bye => {
                {
                    let mut i = self.inner.borrow_mut();
                    i.last_grok = None;
                    i.last_bye = Some(Instant::now());
                }
                self.logw(json!({"event": "bye"}));
                self.reschedule_wake();
                Response::Ok { id: None }
            }
            Request::HermesSend {
                text,
                conv_id,
                reply_to,
                urgent,
                end,
            } => {
                let conv = conv_id.unwrap_or_else(new_id);
                let mut i = self.inner.borrow_mut();
                let r = (|| {
                    let hop = Self::hop_for(&i, reply_to.as_deref())?;
                    i.guard.check_hop(hop).map_err(|e| refusal(&e))?;
                    let body = self.scrub(&text);
                    i.guard
                        .record_message(est_tokens(&body))
                        .map_err(|e| refusal(&e))?;
                    let mut env = Envelope::new(
                        &conv,
                        HERMES,
                        GROK,
                        if end { Kind::End } else { Kind::Msg },
                        &body,
                    );
                    env.hop = hop;
                    env.reply_to = reply_to.clone();
                    Ok(env)
                })();
                match r {
                    Ok(env) => {
                        Self::remember(&mut i, &env);
                        drop(i);
                        self.logw(json!({"event": "message", "dir": "hermes->grok", "urgent": urgent, "env": env}));
                        let id = env.id.clone();
                        self.deliver_to_grok(env, urgent);
                        Response::Ok { id: Some(id) }
                    }
                    Err(e) => {
                        drop(i);
                        self.refuse("hermes->grok", &conv, &e)
                    }
                }
            }
            Request::SendOp { name, args } => {
                self.touch_grok();
                self.send_op(name, args)
            }
            Request::OpToGrok { name, args } => self.op_to_grok(name, args),
            Request::OpResultFromGrok {
                request_id,
                status,
                detail,
            } => {
                self.touch_grok();
                self.op_result_from_grok(request_id, status, detail)
            }
            Request::RecvOwner => Response::Messages {
                envs: self.drain_owner(),
            },
            Request::Approve { request_id, code } => match self
                .check_code(&request_id, &code)
                .and_then(|()| self.take_pending(&request_id, true))
            {
                Ok(p) => {
                    self.log_op(&request_id, &p.req.op, "approved", "owner approved");
                    self.run_op(p.req, p.tier, p.conv_id)
                }
                Err(r) => r,
            },
            Request::Deny { request_id } => match self.take_pending(&request_id, false) {
                Ok(p) => {
                    self.codes.borrow_mut().remove(&request_id);
                    self.log_op(&request_id, &p.req.op, "denied", "owner denied");
                    let conv = if p.conv_id.is_empty() {
                        new_id()
                    } else {
                        p.conv_id
                    };
                    self.op_result_to_grok(
                        &conv,
                        &request_id,
                        OpStatus::Refused,
                        "denied by the owner",
                    );
                    op_response(&request_id, OpStatus::Refused, "denied by the owner")
                }
                Err(r) => r,
            },
            Request::Ops => {
                let mut i = self.inner.borrow_mut();
                let ops = self.ops.pending(&mut i.state.ops);
                self.changed(&mut i);
                Response::Pending { ops }
            }
            Request::Status => Response::Status(self.status().await),
            Request::Doctor => Response::Doctor {
                checks: self.doctor().await,
            },
            Request::WakeReset => {
                let mut i = self.inner.borrow_mut();
                let a = i.state.alert.take();
                self.changed(&mut i);
                drop(i);
                self.logw(json!({"event": "alert_cleared", "reason": "wake-reset", "alert": a}));
                Response::Ok { id: None }
            }
            Request::WakeTest {
                live,
                token,
                consent,
            } => self.wake_test(live, token, consent).await,
            Request::Report { token } => {
                self.expire_probes();
                let mut i = self.inner.borrow_mut();
                let Some((at, got)) = i.probes.get_mut(&token) else {
                    return err("unknown_token", "no selftest wake with this token");
                };
                let ms = at.elapsed().as_millis() as u64;
                *got = Some(ms);
                drop(i);
                self.logw(json!({"event": "selftest_report", "wake_to_report_ms": ms}));
                self.reports.notify_waiters();
                Response::Ok {
                    id: Some(ms.to_string()),
                }
            }
            Request::AwaitReport {
                token,
                timeout_secs,
            } => {
                let t = match Self::timeout(timeout_secs, DEFAULT_WAIT_SECS) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let deadline = tokio::time::Instant::now() + t;
                loop {
                    self.expire_probes();
                    let n = self.reports.notified();
                    tokio::pin!(n);
                    n.as_mut().enable();
                    match self.inner.borrow().probes.get(&token) {
                        None => return err("unknown_token", "no selftest wake with this token"),
                        Some((_, Some(ms))) => {
                            return Response::Ok {
                                id: Some(ms.to_string()),
                            }
                        }
                        Some((_, None)) => {}
                    }
                    if tokio::time::timeout_at(deadline, n).await.is_err() {
                        return Response::Timeout;
                    }
                }
            }
        }
    }

    fn expire_probes(&self) {
        let ttl = self.opts.probe_ttl;
        self.inner
            .borrow_mut()
            .probes
            .retain(|_, (at, _)| at.elapsed() < ttl);
    }

    /// Selftest wake: dry run logs the would-be POST; live fires exactly one POST
    /// (no retries) whose hint asks for `g2h report <token>`. Live is guarded:
    /// refused under `dry_run` without `consent`, while an alert is set, while
    /// another probe is in flight, and past `selftest_wakes_per_day`.
    async fn wake_test(self: &Rc<Self>, live: bool, token: String, consent: bool) -> Response {
        let creds_path = self.opts.paths.wake_credentials();
        let creds_file = creds_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let creds = match wake::load_creds(&creds_path) {
            Ok(Some(c)) => c,
            Ok(None) => return err("credentials", "webhook credentials missing (run g2h setup)"),
            Err(e) => return err("credentials", format!("credentials unusable: {e}")),
        };
        let host = wake::host_only(&creds.url);
        let body =
            json!({"conv_id": "selftest", "count": 0, "hint": format!("run g2h report {token}")})
                .to_string();
        if !live {
            self.logw(json!({"event": "wake", "dry_run": true, "selftest": true, "creds_file": creds_file,
                "would_post": {"host": host, "body": body, "auth": "header from key_mode (key not logged)"}}));
            return Response::Ok { id: Some(host) };
        }
        self.expire_probes();
        {
            let mut i = self.inner.borrow_mut();
            if self.opts.config.dry_run && !consent {
                return err(
                    "dry_run",
                    "dry_run is set: a live selftest wake needs explicit consent (g2h selftest --live)",
                );
            }
            if let Some(a) = &i.state.alert {
                return err("alert", format!("waking is stopped: {a}"));
            }
            if i.probe_in_flight {
                return err("busy", "a live selftest wake is already in flight");
            }
            let today = chrono::Local::now().date_naive().to_string();
            if i.state.selftest_wakes.0 != today {
                i.state.selftest_wakes = (today, 0);
            }
            let cap = self.opts.config.selftest_wakes_per_day;
            if i.state.selftest_wakes.1 >= cap {
                return err(
                    "cap",
                    format!("selftest_wakes_per_day ({cap}) reached; try again tomorrow"),
                );
            }
            i.state.selftest_wakes.1 += 1;
            i.probe_in_flight = true;
            i.probes.insert(token, (Instant::now(), None));
            self.changed(&mut i);
        }
        // clears the in-flight flag even if this request is dropped mid-POST
        struct InFlight<'a>(&'a RefCell<Inner>);
        impl Drop for InFlight<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().probe_in_flight = false;
            }
        }
        let _f = InFlight(&self.inner);
        let out = wake::post(&creds, &body, self.opts.post_timeout).await;
        let (ok, detail) = match &out {
            PostOutcome::Ok(c) => (true, format!("HTTP {c}")),
            PostOutcome::Client(c) => (false, format!("HTTP {c}")),
            PostOutcome::Retryable(w) => (false, w.clone()),
        };
        self.logw(json!({"event": "wake", "selftest": true, "live": true, "host": host, "result": detail}));
        if ok {
            Response::Ok { id: Some(host) }
        } else {
            err("wake_failed", format!("webhook POST to {host}: {detail}"))
        }
    }

    async fn status(self: &Rc<Self>) -> StatusInfo {
        let h = self.backend.health().await;
        let mut i = self.inner.borrow_mut();
        i.state.purge_expired(now_unix());
        let c = i.guard.today();
        let l = i.guard.limits;
        StatusInfo {
            dry_run: self.opts.config.dry_run,
            backend_ok: h.ok,
            backend_detail: self.scrub(&h.detail),
            messages_today: c.messages,
            messages_cap: l.max_messages,
            tokens_today: c.tokens,
            tokens_cap: l.max_tokens,
            wakes_today: c.wakes,
            wakes_cap: l.max_wakes,
            ops_auto: c.ops_auto,
            ops_approved: c.ops_approved,
            ops_refused: c.ops_refused,
            grok_inbox: i.state.len(INBOX) as u64,
            turns_in_flight: i.in_flight,
            waiters: i.waiters,
            grok_active: self.grok_active(&i),
            wake_pending: i.wake_pending,
            webhook: wake::creds_state(&self.opts.paths.wake_credentials()),
            alert: i.state.alert.clone(),
        }
    }

    async fn doctor(self: &Rc<Self>) -> Vec<Check> {
        let p = &self.opts.paths;
        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o777)
                .ok()
        };
        let mut checks = vec![Check {
            name: "daemon".into(),
            ok: true,
            detail: format!("running (pid {})", std::process::id()),
        }];
        let sep = self.opts.config.separate_user.as_ref();
        let (want_home, want_sock) = (Mode::of(sep).home_mode(), Mode::of(sep).socket_mode());
        checks.push(Check {
            name: "isolation".into(),
            ok: true,
            detail: isolation::describe(sep, self.uid),
        });
        let hm = mode(&p.home);
        checks.push(Check {
            name: "home perms".into(),
            ok: hm == Some(want_home),
            detail: format!(
                "{:o}{}",
                hm.unwrap_or(0),
                if hm == Some(want_home) {
                    String::new()
                } else {
                    format!(" — fix: chmod {want_home:o} <g2h home>")
                }
            ),
        });
        let sm = mode(&p.socket());
        checks.push(Check {
            name: "socket perms".into(),
            ok: sm == Some(want_sock),
            detail: format!(
                "{:o}{}",
                sm.unwrap_or(0),
                if sm == Some(want_sock) {
                    ""
                } else {
                    " — fix: restart g2hd"
                }
            ),
        });
        let cs = wake::creds_state(&p.wake_credentials());
        let creds_ok = cs == "set" || (cs == "missing" && self.opts.config.dry_run);
        let hint = if cs.starts_with("bad") {
            " — fix: chmod 600 <g2h home>/inbox.credentials (or credentials) and check its keys"
        } else if cs == "missing" {
            " — needed before dry_run = false: webhook_url, sender_key, key_mode (mode 0600)"
        } else {
            ""
        };
        checks.push(Check {
            name: "credentials".into(),
            ok: creds_ok,
            detail: format!("{cs}{hint}"),
        });
        let h = self.backend.health().await;
        checks.push(Check {
            name: "backend".into(),
            ok: h.ok,
            detail: format!(
                "{}{}",
                self.scrub(&h.detail),
                if h.ok { "" } else { " — fix: start the Hermes gateway for this profile, or check backend/hermes_home/profile/a2a_url in config.toml" }
            ),
        });
        let alert = self.inner.borrow().state.alert.clone();
        checks.push(Check {
            name: "wake".into(),
            ok: alert.is_none(),
            detail: alert.unwrap_or_else(|| {
                if self.opts.config.dry_run {
                    "dry_run (no webhook POSTs)".into()
                } else {
                    "live".into()
                }
            }),
        });
        checks
    }

    async fn serve_conn(self: Rc<Self>, stream: tokio::net::UnixStream) {
        let sep = self.opts.config.separate_user.clone();
        let peer = stream.peer_cred().ok().map(|c| c.uid());
        #[cfg(feature = "test-hooks")]
        let peer = self.opts.peer_uid_override.or(peer);
        if !isolation::peer_allowed(sep.as_ref(), self.uid, peer) {
            self.logw(json!({"event": "peer_refused", "uid": peer}));
            return;
        }
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        loop {
            let mut line = Vec::new();
            match (&mut r)
                .take(MAX_LINE + 1)
                .read_until(b'\n', &mut line)
                .await
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let resp = if line.len() as u64 > MAX_LINE {
                err("bad_request", "request line too long")
            } else {
                match std::str::from_utf8(&line)
                    .map_err(|e| e.to_string())
                    .and_then(|s| ipc::decode::<Request>(s).map_err(|e| e.to_string()))
                {
                    Ok(Request::Wait { timeout_secs }) => {
                        // EOF (or an error) on the read half while waiting means the client is gone.
                        let gone = async {
                            match r.fill_buf().await {
                                Ok([]) | Err(_) => {}
                                Ok(_) => std::future::pending().await,
                            }
                        };
                        match self.wait(timeout_secs, gone).await {
                            Some(resp) => resp,
                            None => {
                                self.wait_client_gone();
                                return;
                            }
                        }
                    }
                    Ok(Request::Approve { .. })
                        if !isolation::approve_allowed(sep.as_ref(), self.uid, peer) =>
                    {
                        self.logw(json!({"event": "approve_refused_uid", "uid": peer}));
                        err(
                            "not_owner",
                            "separate-user mode: approve runs as the g2h user or root (sudo -u <g2h user> g2h approve …)",
                        )
                    }
                    Ok(req) => self.handle(req).await,
                    Err(e) => err("bad_request", e),
                }
            };
            let Ok(out) = ipc::encode(&resp) else { return };
            if w.write_all(out.as_bytes()).await.is_err() {
                if let Response::Messages { envs } = resp {
                    self.requeue(envs);
                }
                return;
            }
            if line.len() as u64 > MAX_LINE {
                return;
            }
        }
    }
}

impl Bound {
    /// Serve until `shutdown` is notified; then flush state and remove the socket.
    pub async fn run<B: PeerBackend + 'static>(
        self,
        backend: B,
        shutdown: Rc<Notify>,
    ) -> Result<(), String> {
        let p = self.opts.paths.clone();
        let mut state = State::load(&p.state()).map_err(|e| {
            format!(
                "{}: {e} — if it is corrupt, move it aside (mv state.json state.json.corrupt) and restart",
                p.state().display()
            )
        })?;
        // approval codes live only in memory, so pending ops from before a restart cannot be approved
        let dropped = std::mem::take(&mut state.ops.pending).len();
        let mut guard = Guard::new(self.opts.config.limits(), LocalClock);
        guard.counters = state.counters.clone();
        // redact the values of both credentials files
        let fingerprints = match (
            wake::creds_fingerprints(&p.credentials()),
            wake::creds_fingerprints(&p.inbox_credentials()),
        ) {
            (Some(mut a), Some(b)) => {
                a.merge(b);
                Some(a)
            }
            (a, b) => a.or(b),
        };
        let mut log = Logger::new(p.log(), 16 << 20, 5);
        log.fingerprints = fingerprints.clone();
        let broker = Rc::new(Broker {
            uid: self.uid,
            opts: self.opts.clone(),
            backend,
            inner: RefCell::new(Inner {
                state,
                guard,
                meta: HashMap::new(),
                meta_order: VecDeque::new(),
                last_grok: None,
                waiters: 0,
                in_flight: 0,
                wake_pending: false,
                wake_gen: 0,
                last_bye: None,
                urgent_pending: false,
                dirty: dropped > 0,
                probes: HashMap::new(),
                probe_in_flight: false,
            }),
            inbox: Notify::new(),
            save: Notify::new(),
            reports: Notify::new(),
            log,
            fingerprints,
            codes: RefCell::new(HashMap::new()),
            ops: OpGuard::new(
                Allowlist::from(&self.opts.config),
                self.opts.config.peers.clone(),
                self.opts.config.hop_limit,
                self.opts.config.op_approval_ttl_secs,
                SystemClock,
            ),
        });
        {
            // re-learn hops of messages still queued, so reply_to keeps working across restarts
            let mut i = broker.inner.borrow_mut();
            let envs: Vec<Envelope> = i
                .state
                .queues
                .values()
                .flatten()
                .map(|q| q.env.clone())
                .collect();
            for e in &envs {
                Broker::<B>::remember(&mut i, e);
            }
        }
        broker.logw(json!({"event": "start", "pid": std::process::id(), "dry_run": self.opts.config.dry_run}));
        // the alert says "fix credentials, then restart g2hd": a restart clears it
        let alert = broker.inner.borrow_mut().state.alert.take();
        if let Some(a) = alert {
            broker.inner.borrow_mut().dirty = true;
            broker.logw(json!({"event": "alert_cleared", "reason": "restart", "alert": a}));
        }
        // a wake scheduled before a restart lived only in memory: re-arm it
        broker.maybe_schedule_wake();
        let saver = {
            let b = broker.clone();
            tokio::task::spawn_local(async move {
                loop {
                    b.save.notified().await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    b.save_now();
                }
            })
        };
        let listener =
            tokio::net::UnixListener::from_std(self.listener).map_err(|e| e.to_string())?;
        loop {
            tokio::select! {
                a = listener.accept() => match a {
                    Ok((s, _)) => { tokio::task::spawn_local(broker.clone().serve_conn(s)); }
                    Err(e) => eprintln!("g2hd: accept: {e}"),
                },
                _ = shutdown.notified() => break,
            }
        }
        drop(listener);
        saver.abort();
        broker.inner.borrow_mut().dirty = true;
        broker.save_now();
        let _ = std::fs::remove_file(p.socket());
        broker.logw(json!({"event": "shutdown"}));
        Ok(())
    }
}
