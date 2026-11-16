//! Command logic shared by the CLI and MCP: selector resolution, policy and version gates, the ask/wait settle
//! loop, stop, watch, and the PASS/FAIL line of every command. Each PASS is checked by a route other than the
//! one that did the work (the sidebar DOM rows, or the transcript), or says `self-check`.
use crate::clock::SharedClock;
use crate::log::ActionLog;
use crate::model::*;
use crate::parse;
use crate::policy::{Action, Caller, Decision, Policy, Target};
use crate::transport::{Cap, Capabilities, TResult, Transport, TransportError};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const POLL_MS: u64 = 400;
pub const DEFAULT_STEER: &str = "Please stop the current task now and reply STOPPED.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ExitCode {
    Done = 0,
    Other = 1,
    StillWorking = 2,
    NeedsUser = 3,
    Refused = 4,
    Unavailable = 5,
}

impl ExitCode {
    pub fn code(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Proof {
    pub pass: bool,
    pub what: String,
    pub check: String,
    /// The route the check used: `dom-rows`, `transcript`, `process-table`, `lsof`, or `self-check`.
    pub route: String,
}

impl Proof {
    pub fn pass(what: impl Into<String>, check: impl Into<String>, route: &str) -> Self {
        Proof {
            pass: true,
            what: what.into(),
            check: check.into(),
            route: route.into(),
        }
    }
    pub fn fail(what: impl Into<String>, why: impl Into<String>) -> Self {
        Proof {
            pass: false,
            what: what.into(),
            check: why.into(),
            route: "none".into(),
        }
    }
    /// `PASS: <what>; <check>` or `FAIL: <what>; <why>`.
    pub fn line(&self) -> String {
        let s = format!(
            "{}: {}; {}",
            if self.pass { "PASS" } else { "FAIL" },
            self.what,
            self.check
        );
        crate::policy::floors::scrub(&s)
    }
}

#[derive(Clone, Debug)]
pub struct Outcome<T> {
    pub value: Option<T>,
    pub exit: ExitCode,
    pub proof: Proof,
    pub hint: Option<String>,
    pub warnings: Vec<String>,
}

impl<T> Outcome<T> {
    pub fn ok(value: T, proof: Proof) -> Self {
        Outcome {
            value: Some(value),
            exit: ExitCode::Done,
            proof,
            hint: None,
            warnings: vec![],
        }
    }
    pub fn fail(exit: ExitCode, what: &str, why: impl Into<String>, hint: Option<String>) -> Self {
        Outcome {
            value: None,
            exit,
            proof: Proof::fail(what, why),
            hint,
            warnings: vec![],
        }
    }
    pub(crate) fn from_halt(h: Halt) -> Self {
        Outcome::fail(h.exit, &h.what, h.why, h.hint)
    }
}

/// How often one `wait` reconnects after the app dropped the debug-port connection.
const RECONNECTS: u32 = 3;

/// An early stop inside a command.
#[derive(Debug)]
pub struct Halt {
    pub exit: ExitCode,
    pub what: String,
    pub why: String,
    pub hint: Option<String>,
}

pub(crate) fn halt(exit: ExitCode, what: &str, why: impl Into<String>, hint: Option<&str>) -> Halt {
    Halt {
        exit,
        what: what.into(),
        why: why.into(),
        hint: hint.map(str::to_string),
    }
}

pub(crate) fn terr(what: &str, e: TransportError) -> Halt {
    match e {
        TransportError::Unavailable(s) | TransportError::BotComputerAsleep(s) => halt(
            ExitCode::Unavailable,
            what,
            s,
            Some("botport grok status; botport grok launch"),
        ),
        TransportError::Unsupported(c) => halt(
            ExitCode::Unavailable,
            what,
            format!("no enabled transport can do {c:?}"),
            Some("botport grok launch (opens the debug port)"),
        ),
        TransportError::Floor(s) => halt(ExitCode::Refused, what, s, None),
        e @ (TransportError::Shape { .. } | TransportError::App(_)) => {
            halt(ExitCode::Other, what, e.to_string(), None)
        }
    }
}

pub(crate) type R<T> = Result<T, Halt>;

#[derive(Clone, Debug)]
pub struct WaitOpts {
    pub timeout_s: f64,
    pub settle_s: f64,
}

impl Default for WaitOpts {
    fn default() -> Self {
        WaitOpts {
            timeout_s: 300.0,
            settle_s: 8.0,
        }
    }
}

/// Messages after `since` (all when `since` is None or not found), keeping the newest `limit` (0 = all).
pub fn slice_since(msgs: Vec<Message>, since: Option<&str>, limit: usize) -> Vec<Message> {
    let mut v = match since.and_then(|k| msgs.iter().position(|m| m.key == k)) {
        Some(i) => msgs[i + 1..].to_vec(),
        None => msgs,
    };
    if limit > 0 && v.len() > limit {
        v.drain(..v.len() - limit);
    }
    v
}

#[derive(Clone, Debug, Serialize)]
pub struct SendResult {
    pub bot: Bot,
    pub key: MessageKey,
}

#[derive(Clone, Debug, Serialize)]
pub struct StopResult {
    pub bot: Bot,
    pub method: StopMethod,
    pub state: BotState,
    pub idle_after_s: f64,
}

pub type Asker = Box<dyn FnMut(&str) -> bool + Send>;

pub struct Engine {
    transports: Vec<Box<dyn Transport>>,
    caps: Vec<Option<Capabilities>>,
    pub policy: Policy,
    pub log: ActionLog,
    pub clock: SharedClock,
    pub caller: Caller,
    pub trust_version: bool,
    /// Set by Ctrl-C; watch stops at the next poll.
    pub interrupted: Arc<AtomicBool>,
    /// watch stops here (the port TTL) unless `--extend`.
    pub deadline_ms: Option<u64>,
    /// Answers a policy `Ask` (a TTY prompt on the CLI; always "no" elsewhere).
    pub asker: Asker,
    /// Where passed canary reports live (`~/.botport/canary`); None: no canary can unlock an untested version.
    pub canary_dir: Option<std::path::PathBuf>,
    /// Review L7: when set (the CLI sets `~/.botport/grok.lock` for the debug port), every transport operation
    /// holds this advisory lock, so two botport processes (an MCP server and a CLI command) never interleave
    /// clicks on the same page. Additive; None in a library engine.
    pub page_lock: Option<std::path::PathBuf>,
    last_transport: &'static str,
    /// P3: the usage read once per engine for the usage check (None: not read yet).
    pub(crate) usage_seen: Option<Option<Usage>>,
}

impl Engine {
    pub fn new(
        transports: Vec<Box<dyn Transport>>,
        policy: Policy,
        clock: SharedClock,
        caller: Caller,
    ) -> Self {
        let n = transports.len();
        Engine {
            transports,
            caps: vec![None; n],
            policy,
            log: ActionLog::off(),
            clock,
            caller,
            trust_version: false,
            interrupted: Arc::new(AtomicBool::new(false)),
            deadline_ms: None,
            asker: Box::new(|_| false),
            canary_dir: None,
            page_lock: None,
            last_transport: "none",
            usage_seen: None,
        }
    }

    pub fn with_log(mut self, log: ActionLog) -> Self {
        self.log = log;
        self
    }

    /// The first transport whose probe succeeds (cached), e.g. for `status`.
    pub fn capabilities(&mut self) -> TResult<Capabilities> {
        let mut last = TransportError::Unavailable("no transport configured".into());
        for i in 0..self.transports.len() {
            match self.probe(i) {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn probe(&mut self, i: usize) -> TResult<Capabilities> {
        if let Some(c) = &self.caps[i] {
            return Ok(c.clone());
        }
        let c = self.transports[i].probe()?;
        self.caps[i] = Some(c.clone());
        Ok(c)
    }

    /// The name of the first transport whose probe succeeds (the one `capabilities` describes).
    pub fn transport_name(&mut self) -> &'static str {
        for i in 0..self.transports.len() {
            if self.probe(i).is_ok() {
                return self.transports[i].name();
            }
        }
        "none"
    }

    /// The app's local-tool permission (`always` | `ask` | `never`), read only; None if it cannot be read.
    pub fn local_tool_permission(&mut self) -> Option<String> {
        self.with_t(Cap::LocalToolPermission, |t| t.local_tool_permission())
            .ok()
    }

    /// P5 canary, first step: the transport's route still opens (see [`Transport::port_check`]).
    pub fn port_check(&mut self) -> TResult<String> {
        self.with_t(Cap::PortCheck, |t| t.port_check())
    }

    /// Run `f` on the first transport that has `cap`; `Unsupported` falls through to the next one.
    pub(crate) fn with_t<T>(
        &mut self,
        cap: Cap,
        mut f: impl FnMut(&mut dyn Transport) -> TResult<T>,
    ) -> TResult<T> {
        let mut last = TransportError::Unsupported(cap);
        for i in 0..self.transports.len() {
            match self.probe(i) {
                Ok(c) if c.caps.contains(&cap) => {}
                Ok(_) => continue,
                Err(e) => {
                    last = e;
                    continue;
                }
            }
            let lock = self
                .page_lock
                .as_deref()
                .and_then(|p| crate::util::lock_file(p).ok());
            let r = f(self.transports[i].as_mut());
            drop(lock);
            match r {
                Err(TransportError::Unsupported(c)) => last = TransportError::Unsupported(c),
                r => {
                    self.last_transport = self.transports[i].name();
                    return r;
                }
            }
        }
        Err(last)
    }

    pub(crate) fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    pub(crate) fn finish<T>(&mut self, cmd: &str, bot: Option<&str>, o: Outcome<T>) -> Outcome<T> {
        if self.policy.action_log {
            self.log.record(
                self.now(),
                cmd,
                bot,
                self.last_transport,
                o.exit.code(),
                &o.proof.line(),
            );
        }
        o
    }

    pub(crate) fn decide(&mut self, what: &str, action: Action, bot: Option<&Bot>) -> R<()> {
        let t = bot.map(|b| Target {
            id: &b.id,
            name: &b.name,
            main: b.main,
        });
        match self.policy.decide(&self.caller, &action, t.as_ref()) {
            Decision::Allow => Ok(()),
            Decision::Ask(q) => {
                if (self.asker)(&q) {
                    Ok(())
                } else {
                    Err(halt(
                        ExitCode::Refused,
                        what,
                        format!("not confirmed: {q}"),
                        None,
                    ))
                }
            }
            Decision::Refuse { setting, hint } => Err(halt(
                ExitCode::Refused,
                what,
                format!("refused by {setting}"),
                Some(&hint),
            )),
        }
    }

    /// Writes on a version outside the tested table: refused (exit 4, setting `unknown_version_writes`) until
    /// `botport grok canary` has passed on that version and transport, or `--trust-version` under fast.
    pub(crate) fn version_gate(&mut self, what: &str, warnings: &mut Vec<String>) -> R<()> {
        let caps = self.capabilities().map_err(|e| terr(what, e))?;
        if caps.tested {
            return Ok(());
        }
        let v = caps.app_version.unwrap_or_else(|| "unknown".into());
        let tname = self.transport_name();
        let canary = self
            .canary_dir
            .as_deref()
            .and_then(|d| crate::canary::passed(d, &v, tname));
        use crate::policy::VersionGate::*;
        match (self.policy.unknown_version_writes, canary) {
            (AfterCanary | AfterCanaryOrTrust, Some(at)) => {
                warnings.push(format!(
                    "Grok Bot {v} is not a tested version; writes allowed by the canary that passed at {at}"
                ));
                Ok(())
            }
            (AfterCanaryOrTrust, None) if self.trust_version => {
                warnings.push(format!(
                    "Grok Bot {v} is not a tested version; --trust-version given"
                ));
                Ok(())
            }
            (Blocked, _) => Err(halt(
                ExitCode::Refused,
                what,
                format!("Grok Bot {v} is not a tested version; writes are blocked by unknown_version_writes (careful)"),
                Some("botport grok policy preset balanced, then botport grok canary"),
            )),
            _ => Err(halt(
                ExitCode::Refused,
                what,
                format!("Grok Bot {v} is not a tested version; writes are blocked by unknown_version_writes until the canary passes"),
                Some("botport grok canary (or --trust-version under preset fast)"),
            )),
        }
    }

    pub(crate) fn read_warnings(&mut self) -> Vec<String> {
        match self.capabilities() {
            Ok(c) if !c.tested => vec![format!(
                "Grok Bot {} is not a tested version; reads may be incomplete",
                c.app_version.unwrap_or_else(|| "unknown".into())
            )],
            _ => vec![],
        }
    }

    // ---------------------------------------------------------------- bots + selectors

    pub(crate) fn list(&mut self, what: &str) -> R<(Vec<Bot>, Option<Vec<RowView>>)> {
        let mut bots = self
            .with_t(Cap::ListBots, |t| t.bots())
            .map_err(|e| terr(what, e))?;
        let rows = match self.with_t(Cap::Rows, |t| t.rows()) {
            Ok(r) => Some(r),
            Err(TransportError::Unsupported(_)) => None,
            Err(e) => return Err(terr(what, e)),
        };
        if let Some(rows) = &rows {
            for b in &mut bots {
                b.main = rows.iter().any(|r| r.id == b.id && r.main);
            }
        }
        Ok((bots, rows))
    }

    pub fn bots(&mut self) -> Outcome<Vec<Bot>> {
        let warnings = self.read_warnings();
        let r = (|| {
            let (bots, rows) = self.list("bots")?;
            let shown: Vec<&Bot> = bots.iter().filter(|b| !b.hidden).collect();
            let proof = match rows {
                None => Proof::pass(format!("{} bots", bots.len()), "self-check", "self-check"),
                Some(rows) => {
                    let mut a: Vec<(&str, &str)> = shown
                        .iter()
                        .map(|b| (b.id.as_str(), b.name.as_str()))
                        .collect();
                    let mut b: Vec<(&str, &str)> = rows
                        .iter()
                        .map(|r| (r.id.as_str(), r.name.as_str()))
                        .collect();
                    a.sort();
                    b.sort();
                    if a != b {
                        return Err(halt(
                            ExitCode::Other,
                            &format!("{} bots", bots.len()),
                            format!(
                                "the sidebar shows {} rows that differ from the app's list",
                                rows.len()
                            ),
                            None,
                        ));
                    }
                    Proof::pass(
                        format!("{} bots", bots.len()),
                        "matches sidebar",
                        "dom-rows",
                    )
                }
            };
            Ok(Outcome::ok(bots, proof))
        })();
        let mut o = r.unwrap_or_else(Outcome::from_halt);
        o.warnings.extend(warnings);
        self.finish("bots", None, o)
    }

    /// Exact id, exact name (case-insensitive), or a unique name prefix.
    pub fn resolve(bots: &[Bot], sel: &str) -> Result<Bot, Halt> {
        let what = format!("bot {sel:?}");
        if let Some(b) = bots.iter().find(|b| b.id == sel) {
            return Ok(b.clone());
        }
        let l = sel.to_lowercase();
        let exact: Vec<&Bot> = bots.iter().filter(|b| b.name.to_lowercase() == l).collect();
        let pick = if exact.is_empty() {
            bots.iter()
                .filter(|b| b.name.to_lowercase().starts_with(&l))
                .collect()
        } else {
            exact
        };
        match pick.len() {
            1 => Ok(pick[0].clone()),
            0 => Err(halt(
                ExitCode::Other,
                &what,
                "no bot matches",
                Some("botport grok bots"),
            )),
            _ => {
                let names: Vec<&str> = pick.iter().map(|b| b.name.as_str()).collect();
                Err(halt(
                    ExitCode::Other,
                    &what,
                    format!("ambiguous: {}", names.join(", ")),
                    Some("use the exact name or id"),
                ))
            }
        }
    }

    pub(crate) fn find(&mut self, what: &str, sel: &str) -> R<Bot> {
        if sel.trim().is_empty() {
            return Err(halt(
                ExitCode::Other,
                what,
                "no bot given",
                Some("botport grok bots"),
            ));
        }
        let (bots, _) = self.list(what)?;
        Self::resolve(&bots, sel)
    }

    pub(crate) fn snap(&mut self, what: &str, id: &str) -> R<BotSnapshot> {
        self.with_t(Cap::StateFlags, |t| t.bot_state(id))
            .map_err(|e| terr(what, e))
    }

    /// A read-only poll that survives the app dropping the debug-port connection once (live 1.1.0: `wait` ended
    /// with exit 5 "failed to fill whole buffer" mid-turn): reconnect and poll again, at most `RECONNECTS` times
    /// per wait. Only reads go through here, so nothing is sent twice.
    fn snap_or_reconnect(&mut self, what: &str, id: &str, left: &mut u32) -> R<BotSnapshot> {
        loop {
            match self.with_t(Cap::StateFlags, |t| t.bot_state(id)) {
                Err(TransportError::Unavailable(e)) if *left > 0 => {
                    *left -= 1;
                    if let Err(e2) = self.with_t(Cap::StateFlags, |t| t.reconnect()) {
                        return Err(terr(
                            what,
                            TransportError::Unavailable(format!("{e}; {e2}")),
                        ));
                    }
                }
                r => return r.map_err(|e| terr(what, e)),
            }
        }
    }

    pub(crate) fn row(&mut self, id: &str) -> Option<RowView> {
        self.with_t(Cap::Rows, |t| t.rows())
            .ok()?
            .into_iter()
            .find(|r| r.id == id)
    }

    pub(crate) fn read_raw(
        &mut self,
        what: &str,
        id: &str,
        since: Option<&str>,
        limit: usize,
    ) -> R<Vec<Message>> {
        let id2 = id.to_string();
        self.with_t(Cap::ReadOpen, |t| {
            t.open(&id2)?;
            t.read(&id2, since, limit)
        })
        .map_err(|e| terr(what, e))
    }

    pub(crate) fn refuse_if_approval(&mut self, what: &str, bot: &Bot) -> R<()> {
        let s = self.snap(what, &bot.id)?;
        if let Some(n) = s.needs_input.filter(|n| n.kind == NeedsKind::Approval) {
            return Err(halt(
                ExitCode::NeedsUser,
                what,
                format!("waiting for you: {}", n.reason),
                Some("answer the approval card in Grok Bot (botport never does: floor 2); a new message would cancel it"),
            ));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- send / ask / wait / read

    pub(crate) fn send_inner(
        &mut self,
        sel: &str,
        msg: &str,
        files: &[std::path::PathBuf],
        warnings: &mut Vec<String>,
    ) -> R<(Bot, MessageKey, Proof)> {
        let what = "send";
        if msg.trim().is_empty() {
            return Err(halt(ExitCode::Other, what, "empty message", None));
        }
        let bot = self.find(what, sel)?;
        self.decide(what, Action::Send, Some(&bot))?;
        self.version_gate(what, warnings)?;
        self.usage_gate(what, warnings)?;
        self.refuse_if_approval(what, &bot)?;
        let id = bot.id.clone();
        let key = self
            .with_t(Cap::Send, |t| {
                t.open(&id)?;
                t.send(&id, msg, files)
            })
            .map_err(|e| terr(what, e))?;
        let seen = self
            .read_raw(what, &id, None, 0)?
            .into_iter()
            .any(|m| m.key == key && m.role == Role::User && parse::same_sent_text(&m.text, msg));
        let proof = if seen {
            Proof::pass(
                format!("message {key} sent"),
                "seen in transcript",
                "transcript",
            )
        } else {
            return Err(halt(
                ExitCode::Other,
                &format!("message {key} sent"),
                "not seen in the transcript",
                None,
            ));
        };
        Ok((bot, key, proof))
    }

    pub fn send(
        &mut self,
        sel: &str,
        msg: &str,
        files: &[std::path::PathBuf],
    ) -> Outcome<SendResult> {
        let mut w = vec![];
        let o = match self.send_inner(sel, msg, files, &mut w) {
            Ok((bot, key, proof)) => Outcome::ok(SendResult { bot, key }, proof),
            Err(h) => Outcome::from_halt(h),
        };
        let id = o.value.as_ref().map(|v| v.bot.id.clone());
        let mut o = self.finish("send", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    pub fn ask(
        &mut self,
        sel: &str,
        msg: &str,
        files: &[std::path::PathBuf],
        opts: &WaitOpts,
    ) -> Outcome<AskResult> {
        let mut w = vec![];
        let o = match self.send_inner(sel, msg, files, &mut w) {
            Ok((bot, key, _)) => self.wait_loop(bot, Some(key), opts),
            Err(h) => Outcome::from_halt(h),
        };
        let id = o.value.as_ref().map(|v| v.bot.id.clone());
        let mut o = self.finish("ask", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    /// Wait for idle (with a reply after `since`, default: our last message) or needs-input.
    pub fn wait(&mut self, sel: &str, since: Option<&str>, opts: &WaitOpts) -> Outcome<AskResult> {
        let w = self.read_warnings();
        let r = (|| {
            let bot = self.find("wait", sel)?;
            self.decide("wait", Action::Read, Some(&bot))?;
            let base = match since {
                Some(k) => Some(k.to_string()),
                None => self
                    .read_raw("wait", &bot.id, None, 0)?
                    .into_iter()
                    .rev()
                    .find(|m| m.role == Role::User)
                    .map(|m| m.key),
            };
            Ok((bot, base))
        })();
        let o = match r {
            Ok((bot, base)) => self.wait_loop(bot, base, opts),
            Err(h) => Outcome::from_halt(h),
        };
        let id = o.value.as_ref().map(|v| v.bot.id.clone());
        let mut o = self.finish("wait", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    pub(crate) fn wait_loop(
        &mut self,
        mut bot: Bot,
        base: Option<String>,
        opts: &WaitOpts,
    ) -> Outcome<AskResult> {
        let what = "wait";
        let start = self.now();
        let settle_ms = (opts.settle_s * 1000.0) as u64;
        let asked_ms = (opts.timeout_s * 1000.0) as u64;
        // floor 5: a wait never runs past the port TTL (as `watch`); `--extend` clears the deadline
        let ttl_left = self.deadline_ms.map(|d| d.saturating_sub(start));
        let timeout_ms = ttl_left.map_or(asked_ms, |l| asked_ms.min(l));
        let ttl_capped = timeout_ms < asked_ms;
        let mut idle_since: Option<u64> = None;
        // the time of the message we wait after: an approval raised before it is stale (that message cancelled it)
        let base_ms = match &base {
            Some(k) => match self.read_raw(what, &bot.id, None, 0) {
                Ok(m) => m.iter().find(|m| &m.key == k).and_then(|m| m.at_ms),
                Err(h) => return Outcome::from_halt(h),
            },
            None => None,
        };
        let result = |bot: &Bot, msgs: Vec<Message>, state, now: u64, proof: &Proof| AskResult {
            bot: bot.clone(),
            messages: msgs,
            state,
            waited_s: (now - start) as f64 / 1000.0,
            proof: proof.line(),
        };
        let mut reconnects_left = RECONNECTS;
        loop {
            let s = match self.snap_or_reconnect(what, &bot.id, &mut reconnects_left) {
                Ok(s) => s,
                Err(h) => return Outcome::from_halt(h),
            };
            let now = self.now();
            bot.state = s.state;
            bot.activity = s.activity.clone();
            bot.needs_input = s.needs_input.clone();
            bot.last_key = s.last_key.clone();
            match s.state {
                BotState::NeedsInput => {
                    let n = s.needs_input.clone().expect("needs_input state carries it");
                    // an open prompt from before our message is not this turn's (e.g. a welcome widget)
                    let stale = n.source != NeedsSource::Routine
                        && matches!((n.since_ms, base_ms), (Some(a), Some(b)) if a < b);
                    let fresh = !stale
                        && (n.source != NeedsSource::Entry
                            || match self.read_raw(what, &bot.id, base.as_deref(), 0) {
                                Ok(m) => m.iter().any(Message::is_open_prompt),
                                Err(h) => return Outcome::from_halt(h),
                            });
                    if fresh {
                        let msgs = self
                            .read_raw(what, &bot.id, base.as_deref(), 0)
                            .unwrap_or_default();
                        let msgs: Vec<Message> =
                            msgs.into_iter().filter(|m| m.role != Role::User).collect();
                        let proof = Proof::fail("waiting for you", n.reason.clone());
                        let r = result(&bot, msgs, BotState::NeedsInput, now, &proof);
                        let hint = if n.kind == NeedsKind::Approval {
                            "answer the card in Grok Bot (botport never answers approval cards: floor 2), then botport grok wait"
                        } else {
                            "answer with botport grok send <bot> <answer>, then botport grok wait"
                        };
                        return Outcome {
                            value: Some(r),
                            exit: ExitCode::NeedsUser,
                            proof,
                            hint: Some(hint.into()),
                            warnings: vec![],
                        };
                    }
                    idle_since.get_or_insert(now);
                }
                BotState::Working => idle_since = None,
                BotState::Idle => {
                    idle_since.get_or_insert(now);
                }
            }
            if let Some(t0) = idle_since {
                if now - t0 >= settle_ms {
                    let msgs = match self.read_raw(what, &bot.id, base.as_deref(), 0) {
                        Ok(m) => m
                            .into_iter()
                            .filter(|m| m.role != Role::User)
                            .collect::<Vec<_>>(),
                        Err(h) => return Outcome::from_halt(h),
                    };
                    if !msgs.is_empty() {
                        let row = self.row(&bot.id);
                        let settled = format!("bot idle {} s", opts.settle_s);
                        let proof = match row {
                            None => Proof::pass(
                                "reply received",
                                format!("{settled}; self-check"),
                                "self-check",
                            ),
                            Some(r) if !r.awaiting && !r.composing => Proof::pass(
                                "reply received",
                                format!("{settled}; sidebar row idle"),
                                "dom-rows",
                            ),
                            Some(_) => {
                                let proof = Proof::fail(
                                    "reply received",
                                    "the sidebar row still shows the bot busy or waiting",
                                );
                                let r = result(&bot, msgs, s.state, now, &proof);
                                return Outcome {
                                    value: Some(r),
                                    exit: ExitCode::Other,
                                    proof,
                                    hint: None,
                                    warnings: vec![],
                                };
                            }
                        };
                        let r = result(&bot, msgs, BotState::Idle, now, &proof);
                        return Outcome::ok(r, proof);
                    }
                }
            }
            if now - start >= timeout_ms {
                let why = if ttl_capped {
                    "port TTL reached (--extend to go on)".to_string()
                } else if s.state == BotState::Working {
                    format!("timeout after {} s; bot still working", opts.timeout_s)
                } else {
                    format!("timeout after {} s; no reply yet", opts.timeout_s)
                };
                let msgs = self
                    .read_raw(what, &bot.id, base.as_deref(), 0)
                    .map(|m| m.into_iter().filter(|m| m.role != Role::User).collect())
                    .unwrap_or_default();
                let proof = Proof::fail("wait", why);
                let r = result(&bot, msgs, s.state, now, &proof);
                return Outcome {
                    value: Some(r),
                    exit: ExitCode::StillWorking,
                    proof,
                    hint: Some(format!("botport grok wait {:?}", bot.name)),
                    warnings: vec![],
                };
            }
            self.clock.sleep_ms(POLL_MS);
        }
    }

    pub fn read(&mut self, sel: &str, since: Option<&str>, limit: usize) -> Outcome<Vec<Message>> {
        let w = self.read_warnings();
        let r = (|| {
            let bot = self.find("read", sel)?;
            self.decide("read", Action::Read, Some(&bot))?;
            let msgs = self.read_raw("read", &bot.id, since, limit)?;
            let last = msgs
                .last()
                .map(|m| m.key.clone())
                .unwrap_or_else(|| "(none)".into());
            let p = Proof::pass(
                format!("{} messages up to {last}", msgs.len()),
                "self-check",
                "self-check",
            );
            Ok((bot.id, msgs, p))
        })();
        let (id, o) = match r {
            Ok((id, m, p)) => (Some(id), Outcome::ok(m, p)),
            Err(h) => (None, Outcome::from_halt(h)),
        };
        let mut o = self.finish("read", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    // ---------------------------------------------------------------- stop

    pub fn stop(&mut self, sel: &str, steer: Option<&str>) -> Outcome<StopResult> {
        let steer = steer.unwrap_or(DEFAULT_STEER).to_string();
        let mut w = vec![];
        let r = self.stop_inner(sel, &steer, &mut w);
        let o = r.unwrap_or_else(Outcome::from_halt);
        let id = o.value.as_ref().map(|v| v.bot.id.clone());
        let mut o = self.finish("stop", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    fn stop_inner(
        &mut self,
        sel: &str,
        steer: &str,
        w: &mut Vec<String>,
    ) -> R<Outcome<StopResult>> {
        let what = "stop";
        let mut bot = self.find(what, sel)?;
        self.decide(what, Action::Stop, Some(&bot))?;
        self.refuse_if_approval(what, &bot)?;
        let s = self.snap(what, &bot.id)?;
        if s.state != BotState::Working {
            bot.state = s.state;
            let p = Proof::pass(
                "nothing to stop",
                format!("bot {}", s.state.as_str()),
                "self-check",
            );
            return Ok(Outcome::ok(
                StopResult {
                    bot,
                    method: StopMethod::Steer,
                    state: s.state,
                    idle_after_s: 0.0,
                },
                p,
            ));
        }
        self.version_gate(what, w)?;
        let id = bot.id.clone();
        let method = self
            .with_t(Cap::Stop, |t| {
                t.open(&id)?;
                t.stop(&id, steer)
            })
            .map_err(|e| terr(what, e))?;
        let start = self.now();
        let mut idle_at: Option<u64> = None;
        loop {
            let s = self.snap(what, &id)?;
            let now = self.now();
            bot.state = s.state;
            match s.state {
                BotState::NeedsInput
                    if s.needs_input
                        .as_ref()
                        .is_some_and(|n| n.source != NeedsSource::Entry) =>
                {
                    return Err(halt(
                        ExitCode::NeedsUser,
                        what,
                        format!("waiting for you: {}", s.needs_input.unwrap().reason),
                        None,
                    ))
                }
                BotState::Working => idle_at = None,
                _ => {
                    let t0 = *idle_at.get_or_insert(now);
                    if now - t0 >= 1200 {
                        let after = (t0 - start) as f64 / 1000.0;
                        let p = match self.row(&id) {
                            Some(r) if r.composing || r.awaiting => {
                                return Err(halt(
                                    ExitCode::Other,
                                    what,
                                    "the sidebar row still shows the bot busy",
                                    None,
                                ))
                            }
                            Some(_) => Proof::pass(
                                format!("bot idle {after:.1} s after stop"),
                                "sidebar row idle",
                                "dom-rows",
                            ),
                            None => Proof::pass(
                                format!("bot idle {after:.1} s after stop"),
                                "self-check",
                                "self-check",
                            ),
                        };
                        return Ok(Outcome::ok(
                            StopResult {
                                bot,
                                method,
                                state: BotState::Idle,
                                idle_after_s: after,
                            },
                            p,
                        ));
                    }
                }
            }
            if now - start >= 30_000 {
                return Err(halt(
                    ExitCode::StillWorking,
                    what,
                    "still working 30 s after the steer message",
                    Some("botport grok wait <bot>, or stop it in the app"),
                ));
            }
            self.clock.sleep_ms(POLL_MS);
        }
    }

    // ---------------------------------------------------------------- watch

    /// One event per state change or new message until `for_s` (None = until Ctrl-C or the port TTL).
    pub fn watch(
        &mut self,
        sel: Option<&str>,
        for_s: Option<f64>,
        sink: &mut dyn FnMut(&WatchEvent),
    ) -> Outcome<WatchSummary> {
        let r = (|| -> R<Outcome<WatchSummary>> {
            let (bots, _) = self.list("watch")?;
            let only = match sel {
                Some(s) => Some(Self::resolve(&bots, s)?.id),
                None => None,
            };
            self.decide("watch", Action::Watch, None)?;
            // ReadAny reads any bot; with ReadOpen only (the debug port in P1) the bot open on screen is read
            // and the others' message events carry no text (watch never switches the screen)
            let read_cap = match self.capabilities() {
                Ok(c) if c.caps.contains(&Cap::ReadAny) => Some(Cap::ReadAny),
                Ok(c) if c.caps.contains(&Cap::ReadOpen) => Some(Cap::ReadOpen),
                _ => None,
            };
            let start = self.now();
            let end = for_s.map(|s| start + (s * 1000.0) as u64);
            let mut seen: std::collections::HashMap<String, (BotState, String)> = bots
                .iter()
                .map(|b| (b.id.clone(), (b.state, b.last_key.clone())))
                .collect();
            let mut events = 0u64;
            let why_end = loop {
                if self.interrupted.load(Ordering::SeqCst) {
                    break "interrupted";
                }
                let now = self.now();
                if end.is_some_and(|e| now >= e) {
                    break "time up";
                }
                if self.deadline_ms.is_some_and(|d| now >= d) {
                    break "port TTL reached (--extend to go on)";
                }
                self.clock.sleep_ms(POLL_MS);
                let (bots, _) = self.list("watch")?;
                for b in bots
                    .iter()
                    .filter(|b| only.as_ref().is_none_or(|o| *o == b.id))
                {
                    let prev = seen.get(&b.id).cloned();
                    let at = crate::util::rfc3339(self.now());
                    if prev.as_ref().map(|p| p.0) != Some(b.state) {
                        events += 1;
                        sink(&WatchEvent {
                            kind: b.state.as_str().into(),
                            bot: b.name.clone(),
                            bot_id: b.id.clone(),
                            at: at.clone(),
                            message: None,
                        });
                    }
                    if prev.as_ref().map(|p| p.1.as_str()) != Some(b.last_key.as_str()) {
                        let message = read_cap.and_then(|cap| {
                            let old = prev.as_ref().map(|p| p.1.clone());
                            self.with_t(cap, |t| t.read(&b.id, old.as_deref(), 0))
                                .ok()
                                .and_then(|m| m.into_iter().rev().find(|m| m.role != Role::User))
                        });
                        events += 1;
                        sink(&WatchEvent {
                            kind: "message".into(),
                            bot: b.name.clone(),
                            bot_id: b.id.clone(),
                            at,
                            message,
                        });
                    }
                    seen.insert(b.id.clone(), (b.state, b.last_key.clone()));
                }
            };
            let minutes = (self.now() - start) as f64 / 60_000.0;
            let p = Proof::pass(
                format!("watched {minutes:.1} min, {events} events"),
                format!("{why_end}; self-check"),
                "self-check",
            );
            Ok(Outcome::ok(WatchSummary { minutes, events }, p))
        })();
        let o = r.unwrap_or_else(Outcome::from_halt);
        self.finish("watch", None, o)
    }
}
