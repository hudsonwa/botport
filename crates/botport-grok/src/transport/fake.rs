//! FakeTransport: replays the scrubbed fixtures of one app version (`tests/fixtures/app-1.1.0/`).
//!
//! - The roster and transcripts are kept as the app's raw JSON and parsed with [`crate::parse`] on every call,
//!   so the fake exercises the same parsers as the debug-port transport.
//! - `send()` starts the next queued turn script (`turns/<name>.json`): the recorded state, sampled every
//!   400 ms. The sample shown is picked by virtual time since the send, so the engine's settle loop runs in
//!   virtual time with a [`crate::clock::FakeClock`].
//! - When a sample's `lastMessageId` changes, a bot entry of the recorded kind is added to the transcript (its
//!   text is the row's last line, the only text a sample carries).
//! - Write calls change the in-memory state and are recorded, so PASS checks see the new state and tests can
//!   assert what the engine tried to change.
use super::{Cap, Capabilities, TResult, Transport, TransportError};
use crate::clock::SharedClock;
use crate::model::*;
use crate::parse;
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum RecordedWrite {
    Send {
        bot: BotId,
        text: String,
    },
    Stop {
        bot: BotId,
        steer: String,
    },
    /// P3
    RoutineEnabled {
        bot: BotId,
        routine: String,
        on: bool,
    },
    RoutineRemoved {
        bot: BotId,
        routine: String,
    },
    FileUploaded {
        bot: BotId,
        name: String,
        bytes: u64,
    },
    FileDeleted {
        bot: BotId,
        name: String,
    },
}

struct Active {
    bot: BotId,
    start_ms: u64,
    interval: u64,
    samples: Vec<Value>,
    applied: usize,
}

pub struct FakeTransport {
    dir: PathBuf,
    version: String,
    clock: SharedClock,
    agents: Vec<Value>,
    transcripts: HashMap<BotId, Vec<Value>>,
    chat_flags: HashMap<BotId, Value>,
    row_over: HashMap<BotId, Value>,
    queued: VecDeque<String>,
    active: Option<Active>,
    faults: Vec<(Cap, TransportError)>,
    /// How often [`Transport::reconnect`] was called.
    pub reconnects: u32,
    writes: Vec<RecordedWrite>,
    user_n: u64,
    seq: u64,
    no_entries: std::collections::HashSet<BotId>,
    /// Recorded routine runs per bot (`routineRuns` in a snapshot fixture).
    routine_runs: HashMap<BotId, Value>,
    /// The raw `getLocalToolPermission` answer (`localToolPermission` in roster.json); None: not readable.
    local_tool_permission: Option<Value>,
    /// Like the debug port in P1: no ReadAny, and only this bot's chat is readable (it is the one on screen).
    open_only: Option<BotId>,
    /// P3: `automations.json` byAgent (raw `listTeamBotAutomations` answers).
    automations: HashMap<BotId, Value>,
    /// P3: `files.json` byAgent (raw `listGrokBotAgentFiles` answers) and the bytes behind each path.
    files: HashMap<BotId, Value>,
    blobs: HashMap<String, Vec<u8>>,
    /// P3: `usage.json` (`weekly`, `summary`); None: no usage (the check is skipped).
    usage: Option<(Value, Value)>,
    /// P3: the app's routine menu → Delete is available (default); off makes the engine ask the bot.
    ui_remove: bool,
    /// P3: the bot does what a routine add / run / remove message asks (default on).
    bot_obeys: bool,
    /// P3: the app's sha256 is reported wrong (download check test).
    bad_app_digest: bool,
    /// Review L6: the app gives no sha256 for a file read.
    no_app_digest: bool,
    /// Review H2: what the app's own preview shows for a message key (the route taken when the attachment's
    /// path is not an app asset URL).
    previews: HashMap<String, Vec<u8>>,
    /// Large-account fix: a virtualized sidebar (the pinned main bot plus a window over the rest); None: every row
    /// is rendered. Shared counter of scroll steps taken by `open`.
    sidebar: Option<super::sidebar::VirtualSidebar>,
    sidebar_scrolls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Large-account fix (0.3.2): a chat is open, so the sidebar is "collapsed": no row is "pinned" and only the
    /// ", Main Bot" label suffix marks the main bot.
    collapsed: bool,
}

fn read_json(p: &Path) -> TResult<Value> {
    let s = std::fs::read_to_string(p)
        .map_err(|e| TransportError::Unavailable(format!("fixture {}: {e}", p.display())))?;
    serde_json::from_str(&s)
        .map_err(|e| TransportError::App(format!("fixture {}: {e}", p.display())))
}

impl FakeTransport {
    /// The idle snapshot: `roster.json`, with `transcript-idle.json` as the main (first) bot's chat.
    pub fn load(dir: &Path, clock: SharedClock) -> TResult<Self> {
        let roster = read_json(&dir.join("roster.json"))?;
        let version = roster
            .get("app_version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let agents = roster
            .get("agents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let transcripts = HashMap::new();
        let ltp = roster.get("localToolPermission").cloned();
        if let (Some(main), Ok(t)) = (agents.first(), read_json(&dir.join("transcript-idle.json")))
        {
            let id = main
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut me = Self::build(dir, version, clock, agents, transcripts);
            me.take_chat(id, &t);
            me.local_tool_permission = ltp;
            return Ok(me);
        }
        let mut me = Self::build(dir, version, clock, agents, transcripts);
        me.local_tool_permission = ltp;
        Ok(me)
    }

    /// The awaiting-approval snapshot: `transcript-awaiting-approval.json` (its own agents list; the chat
    /// belongs to `currentAgentId`).
    pub fn load_awaiting(dir: &Path, clock: SharedClock) -> TResult<Self> {
        Self::load_snapshot(dir, "transcript-awaiting-approval.json", clock)
    }

    /// `transcript-routine-approval.json`: a routine run's approval card (awaitingUserResponse null, a later chat
    /// message and reply, the run still running with no finish time).
    pub fn load_routine_awaiting(dir: &Path, clock: SharedClock) -> TResult<Self> {
        Self::load_snapshot(dir, "transcript-routine-approval.json", clock)
    }

    /// Any single-chat snapshot fixture (`agents`, `currentAgentId`, `chat`, optional `routineRuns` by bot id).
    pub fn load_snapshot(dir: &Path, file: &str, clock: SharedClock) -> TResult<Self> {
        let t = read_json(&dir.join(file))?;
        let version = t
            .get("app_version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let agents = t
            .get("agents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let cur = t
            .get("currentAgentId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let transcripts = HashMap::new();
        let mut me = Self::build(dir, version, clock, agents, transcripts);
        me.take_chat(cur, &t);
        if let Some(Value::Object(m)) = t.get("routineRuns") {
            me.routine_runs = m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        Ok(me)
    }

    fn build(
        dir: &Path,
        version: String,
        clock: SharedClock,
        agents: Vec<Value>,
        transcripts: HashMap<BotId, Vec<Value>>,
    ) -> Self {
        FakeTransport {
            dir: dir.to_path_buf(),
            version,
            clock,
            agents,
            transcripts,
            chat_flags: HashMap::new(),
            row_over: HashMap::new(),
            queued: VecDeque::new(),
            active: None,
            faults: vec![],
            reconnects: 0,
            writes: vec![],
            user_n: 0,
            seq: 10_000,
            no_entries: Default::default(),
            routine_runs: HashMap::new(),
            local_tool_permission: None,
            open_only: None,
            automations: Self::by_agent(dir, "automations.json"),
            files: Self::by_agent(dir, "files.json"),
            blobs: HashMap::new(),
            usage: read_json(&dir.join("usage.json")).ok().map(|u| {
                (
                    u.get("weekly").cloned().unwrap_or(Value::Null),
                    u.get("summary").cloned().unwrap_or(Value::Null),
                )
            }),
            ui_remove: true,
            bot_obeys: true,
            bad_app_digest: false,
            no_app_digest: false,
            previews: HashMap::new(),
            sidebar: None,
            sidebar_scrolls: Default::default(),
            collapsed: false,
        }
    }

    fn by_agent(dir: &Path, file: &str) -> HashMap<BotId, Value> {
        read_json(&dir.join(file))
            .ok()
            .and_then(|v| v.get("byAgent").and_then(Value::as_object).cloned())
            .map(|m| m.into_iter().collect())
            .unwrap_or_default()
    }

    /// Large account: add synthetic bots (copies of the second fixture bot, ids `…-0000000100xx`, names
    /// `Bulk Bot NN`) until there are `total` bots.
    pub fn with_bots(mut self, total: usize) -> Self {
        let Some(tpl) = self.agents.get(1).cloned() else {
            return self;
        };
        let mut n = 0;
        while self.agents.len() < total {
            let mut a = tpl.clone();
            a["id"] = json!(format!("00000000-0000-4000-8000-0000001{n:05}"));
            a["name"] = json!(format!("Bulk Bot {n:02}"));
            a["hasUnread"] = json!(false);
            a["awaitingUserResponse"] = Value::Null;
            self.agents.push(a);
            n += 1;
        }
        self
    }

    /// Large account: the sidebar draws the pinned main bot plus `window` of the other rows, from row `top`.
    pub fn with_virtual_sidebar(mut self, window: usize, top: usize) -> Self {
        let ids = self
            .agents
            .iter()
            .skip(1)
            .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        self.sidebar = Some(super::sidebar::VirtualSidebar::new(ids, window, top));
        self
    }

    /// Large account with a chat open: every row's layout is "collapsed" (the main row is not "pinned").
    pub fn with_collapsed_sidebar(mut self) -> Self {
        self.collapsed = true;
        self
    }

    /// Scroll steps the virtualized sidebar has taken (live counter, readable after the engine owns the fake).
    pub fn sidebar_scrolls(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.sidebar_scrolls.clone()
    }

    fn row_rendered(&self, i: usize, id: &str) -> bool {
        match &self.sidebar {
            None => true,
            Some(sb) => i == 0 || sb.visible().iter().any(|v| v == id),
        }
    }

    /// P3: set the weekly usage percent and whether usage is available.
    pub fn with_usage(mut self, percent: f64, available: bool) -> Self {
        let (mut w, mut s) = self.usage.take().unwrap_or((json!({}), json!({})));
        w["percentUsed"] = json!(percent);
        s["hasAvailableUsage"] = json!(available);
        self.usage = Some((w, s));
        self
    }

    /// P3: no usage read at all (a transport without the usage call).
    pub fn without_usage(mut self) -> Self {
        self.usage = None;
        self
    }

    /// P3: the routine menu → Delete is not available; the engine must ask the bot.
    pub fn without_ui_remove(mut self) -> Self {
        self.ui_remove = false;
        self
    }

    /// P3: the bot replies but does not do what a routine message asks.
    pub fn bot_ignores_routine_requests(mut self) -> Self {
        self.bot_obeys = false;
        self
    }

    /// P3: put a file with these bytes in a bot's file list (synthetic; large-file tests).
    pub fn with_file(mut self, bot: &str, name: &str, bytes: Vec<u8>) -> Self {
        let path = format!("grok-bot/agents/{bot}/attachments/{name}");
        self.add_file(bot, name, &path, bytes, "application/octet-stream");
        self
    }

    /// Review M6: a file at an explicit path (two files may share a name in different folders).
    pub fn with_file_at(mut self, bot: &str, name: &str, path: &str, bytes: Vec<u8>) -> Self {
        let e = self
            .files
            .entry(bot.to_string())
            .or_insert_with(|| json!({"files": [], "totalBytes": 0, "canEdit": true}));
        let rec = json!({"name": name, "path": path, "sizeBytes": bytes.len(),
            "contentType": "application/octet-stream", "updatedAtMs": self.clock.now_ms(), "etag": "\"synthetic\""});
        if let Some(a) = e.get_mut("files").and_then(Value::as_array_mut) {
            a.retain(|f| f.get("path").and_then(Value::as_str) != Some(path));
            a.push(rec);
        }
        self.blobs.insert(path.to_string(), bytes);
        self
    }

    /// P3: the bot sent a file in chat (a synthetic `attachment` entry in its transcript).
    pub fn with_attachment(mut self, bot: &str, name: &str, bytes: Vec<u8>) -> Self {
        self.seq += 1;
        let key = format!("t{}s1", 800 + self.seq);
        let url = format!(
            "file:///home/box/agent-data/agents/{bot}/assets/synthetic-{}.bin",
            self.seq
        );
        let e = json!({"kind": "send-message", "id": key, "message": {"type": "attachment", "url": url,
            "file_name": name, "source_path": format!("/workspace/{name}"), "alt": name},
            "timestampMs": self.clock.now_ms(), "seq": self.seq});
        self.transcripts.entry(bot.to_string()).or_default().push(e);
        self.blobs.insert(url, bytes);
        self
    }

    /// Review H2: the bot sent an attachment entry with this `url` (any string, e.g. `file:///etc/hosts`). `direct`
    /// is what a raw read of that path would return; `preview` (if any) is what the app's own preview shows.
    pub fn with_attachment_url(
        mut self,
        bot: &str,
        name: &str,
        url: &str,
        direct: Vec<u8>,
        preview: Option<Vec<u8>>,
    ) -> Self {
        self.seq += 1;
        let key = format!("t{}s1", 800 + self.seq);
        let e = json!({"kind": "send-message", "id": key, "message": {"type": "attachment", "url": url,
            "file_name": name, "alt": name}, "timestampMs": self.clock.now_ms(), "seq": self.seq});
        self.transcripts.entry(bot.to_string()).or_default().push(e);
        self.blobs.insert(url.to_string(), direct);
        if let Some(p) = preview {
            self.previews.insert(key, p);
        }
        self
    }

    /// Review L6: the app gives no sha256 for file reads (the download can only self-check).
    pub fn without_app_digest(mut self) -> Self {
        self.no_app_digest = true;
        self
    }

    /// P3: the app's own sha256 comes back wrong (the download must fail and leave no file).
    pub fn with_bad_app_digest(mut self) -> Self {
        self.bad_app_digest = true;
        self
    }

    fn add_file(&mut self, bot: &str, name: &str, path: &str, bytes: Vec<u8>, ct: &str) {
        let e = self
            .files
            .entry(bot.to_string())
            .or_insert_with(|| json!({"files": [], "totalBytes": 0, "canEdit": true}));
        let rec = json!({"name": name, "path": path, "sizeBytes": bytes.len(), "contentType": ct,
            "updatedAtMs": self.clock.now_ms(), "etag": "\"synthetic\""});
        if let Some(a) = e.get_mut("files").and_then(Value::as_array_mut) {
            a.retain(|f| f.get("name").and_then(Value::as_str) != Some(name));
            a.push(rec);
        }
        self.blobs.insert(path.to_string(), bytes);
    }

    /// Synthetic bytes for a fixture file that has no recorded content: a repeated line, cut to its size.
    fn blob(&self, f: &FileEntry) -> Vec<u8> {
        if let Some(b) = self.blobs.get(&f.path) {
            return b.clone();
        }
        let line = format!("synthetic content of {}\n", f.name);
        line.bytes().cycle().take(f.bytes as usize).collect()
    }

    fn automations_mut(&mut self, bot: &str) -> &mut Vec<Value> {
        let e = self
            .automations
            .entry(bot.to_string())
            .or_insert_with(|| json!({"kind": "available", "automations": [], "policy": null}));
        if e.get("kind").and_then(Value::as_str) != Some("available") {
            *e = json!({"kind": "available", "automations": [], "policy": null});
        }
        e.get_mut("automations")
            .and_then(Value::as_array_mut)
            .expect("automations array")
    }

    /// The bot's side of a routine message (1.1.0 has no create/run call; the fake plays a bot that complies).
    fn play_bot(&mut self, bot: &str, text: &str) {
        if !self.bot_obeys {
            return;
        }
        let now = self.clock.now_ms();
        if let Some(spec) = crate::routines::parse_add_prompt(text) {
            self.seq += 1;
            let n = self.seq;
            let slug: String = spec
                .name
                .to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let sched = format!(
                "CRON_TZ={} {}",
                spec.tz.as_deref().unwrap_or("UTC"),
                spec.schedule
            );
            let a = json!({"automationId": format!("00000000-0000-4000-8000-{n:012}"), "id": slug, "name": spec.name,
                "schedule": sched, "isEnabled": true, "nextRunAt": now + 3_600_000,
                "viewerCapabilities": {"canOpen": true, "canToggle": true, "canEdit": true, "canDelete": true},
                "record": {"prompt": spec.prompt, "runs": []}});
            self.automations_mut(bot).push(a);
        } else if let Some(name) = crate::routines::parse_run_prompt(text) {
            for a in self.automations_mut(bot).iter_mut() {
                if a.get("name").and_then(Value::as_str) == Some(name.as_str()) {
                    a["lastRunAt"] = json!(now);
                    if let Some(r) = a.pointer_mut("/record/runs").and_then(Value::as_array_mut) {
                        r.push(json!({"id": "00000000-0000-4000-8000-000000000777", "trigger": "manual",
                            "startedAt": now, "finishedAt": null, "status": "running"}));
                    }
                }
            }
        } else if let Some(name) = crate::routines::parse_remove_prompt(text) {
            self.automations_mut(bot)
                .retain(|a| a.get("name").and_then(Value::as_str) != Some(name.as_str()));
        }
    }

    /// A recorded chat: its entries, or (schema drift) a note that it has none, so the parser names the field.
    fn take_chat(&mut self, id: String, t: &Value) {
        match t.pointer("/chat/entries").and_then(Value::as_array) {
            Some(e) => {
                self.transcripts.insert(id, e.clone());
            }
            None => {
                self.no_entries.insert(id);
            }
        }
    }

    /// Replace the recorded `getLocalToolPermission` answer (None: the read fails).
    pub fn with_local_tool_permission(mut self, raw: Option<Value>) -> Self {
        self.local_tool_permission = raw;
        self
    }

    /// Behave like the debug-port transport: drop `ReadAny`; only `bot` (the one open on screen) can be read.
    pub fn read_open_only(mut self, bot: &str) -> Self {
        self.open_only = Some(bot.to_string());
        self
    }

    /// Queue a turn script; the next `send()` starts it.
    pub fn with_turn(mut self, name: &str) -> Self {
        self.queued.push_back(name.to_string());
        self
    }

    /// Start a turn script now on `bot` (the bot is already working before the engine looks).
    /// Starts at the script's first working sample.
    pub fn with_running(mut self, bot: &str, turn: &str) -> TResult<Self> {
        self.start_turn(bot, turn, true)?;
        Ok(self)
    }

    /// Report a different app version (version-gate tests).
    pub fn with_version(mut self, v: &str) -> Self {
        self.version = v.to_string();
        self
    }

    /// Fault injection: the next call needing `cap` fails with `e`.
    pub fn fail_next(mut self, cap: Cap, e: TransportError) -> Self {
        self.faults.push((cap, e));
        self
    }

    pub fn writes(&self) -> &[RecordedWrite] {
        &self.writes
    }

    fn fault(&mut self, cap: Cap) -> TResult<()> {
        if let Some(i) = self.faults.iter().position(|(c, _)| *c == cap) {
            return Err(self.faults.remove(i).1);
        }
        Ok(())
    }

    fn idx(&self, id: &str) -> TResult<usize> {
        self.agents
            .iter()
            .position(|a| a.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| TransportError::App(format!("no bot with id {id}")))
    }

    fn start_turn(&mut self, bot: &str, name: &str, from_running: bool) -> TResult<()> {
        self.idx(bot)?;
        let t = read_json(&self.dir.join("turns").join(format!("{name}.json")))?;
        let samples = t
            .get("samples")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if samples.is_empty() {
            return Err(TransportError::App(format!("turn {name} has no samples")));
        }
        let interval = t
            .get("poll_interval_ms")
            .and_then(Value::as_u64)
            .unwrap_or(400)
            .max(1);
        let skip = if from_running {
            samples
                .iter()
                .position(|x| x.pointer("/agent/isRunningTurn") == Some(&Value::Bool(true)))
                .unwrap_or(0) as u64
        } else {
            0
        };
        self.active = Some(Active {
            bot: bot.to_string(),
            start_ms: self.clock.now_ms().saturating_sub(skip * interval),
            interval,
            samples,
            applied: 0,
        });
        self.apply(0);
        self.advance();
        Ok(())
    }

    /// Apply every sample up to the one virtual time has reached.
    fn advance(&mut self) {
        let Some(a) = &self.active else { return };
        let i = ((self.clock.now_ms().saturating_sub(a.start_ms)) / a.interval) as usize;
        let i = i.min(a.samples.len() - 1);
        let from = a.applied + 1;
        for k in from..=i {
            self.apply(k);
        }
    }

    fn apply(&mut self, k: usize) {
        let Some(a) = &mut self.active else { return };
        a.applied = k;
        let mut sample = a.samples[k].clone();
        let bot = a.bot.clone();
        // recorded times -> virtual time (the turn started at start_ms)
        let t0 = a.samples[0].get("t").and_then(Value::as_u64).unwrap_or(0);
        let start = a.start_ms;
        let rebase = |t: u64| (start + t).saturating_sub(t0);
        if let Some(t) = sample.get("t").and_then(Value::as_u64) {
            sample["t"] = json!(rebase(t));
        }
        if let Some(since) = sample
            .pointer("/agent/awaitingUserResponse/since")
            .and_then(Value::as_u64)
        {
            sample["agent"]["awaitingUserResponse"]["since"] = json!(rebase(since));
        }
        let Ok(i) = self.idx(&bot) else { return };
        let before = self.agents[i].get("lastMessageId").cloned();
        if let (Some(over), Some(obj)) = (
            sample.get("agent").and_then(Value::as_object),
            self.agents[i].as_object_mut(),
        ) {
            for (key, v) in over {
                obj.insert(key.clone(), v.clone());
            }
        }
        if let Some(c) = sample.get("chat") {
            self.chat_flags.insert(bot.clone(), c.clone());
        }
        if let Some(r) = sample.get("row") {
            self.row_over.insert(bot.clone(), r.clone());
        }
        let after = self.agents[i].get("lastMessageId").cloned();
        if k > 0 && after != before {
            if let Some(Value::String(mid)) = after {
                let e = self.synth_entry(&mid, &sample);
                if let Some(e) = e {
                    self.transcripts.entry(bot).or_default().push(e);
                }
            }
        }
    }

    fn synth_entry(&mut self, mid: &str, sample: &Value) -> Option<Value> {
        let last = sample
            .pointer("/chat/lastKinds")
            .and_then(Value::as_array)
            .and_then(|v| v.last())
            .and_then(Value::as_str)
            .unwrap_or("send-message:text:")
            .to_string();
        let desc = sample
            .pointer("/row/desc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let t = sample.get("t").and_then(Value::as_u64).unwrap_or(0);
        self.seq += 1;
        let mut parts = last.split(':');
        let (kind, ty) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let awaiting = sample
            .pointer("/agent/awaitingUserResponse")
            .filter(|v| !v.is_null());
        let e = match (kind, ty) {
            ("send-message", "local-tool-permission") => {
                let reason = awaiting
                    .and_then(|a| a.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let target = reason.split_once(": ").map(|x| x.1).unwrap_or(reason);
                let status = if awaiting.is_some() {
                    "pending"
                } else {
                    "allowed"
                };
                json!({"kind":"send-message","id":mid,"message":{"type":"local-tool-permission","ask":{
                    "requestId":"0000000000000099","action":"run-command","target":target,"status":status}},
                    "timestampMs":t,"seq":self.seq})
            }
            ("send-message", "widget") => {
                json!({"kind":"send-message","id":mid,"message":{"type":"widget",
                "widget":{"prompt":desc,"options":[]}},"timestampMs":t,"seq":self.seq})
            }
            ("event", "automation-changed") => {
                json!({"kind":"event","id":mid,"event":{"type":"automation-changed",
                "action":"created","automationId":"synthetic-routine","automationName":desc},"timestampMs":t,"seq":self.seq})
            }
            ("message", _) => return None,
            _ => {
                json!({"kind":"send-message","id":mid,"message":{"type":"text","content":desc},"timestampMs":t,"seq":self.seq})
            }
        };
        Some(e)
    }

    fn chat_value(&self, id: &str) -> Value {
        let mut c = Map::new();
        if !self.no_entries.contains(id) {
            c.insert(
                "entries".into(),
                Value::Array(self.transcripts.get(id).cloned().unwrap_or_default()),
            );
        }
        let f = self.chat_flags.get(id);
        for k in ["isAgentRunning", "isAgentComposing", "agentActivity"] {
            let v = f
                .and_then(|f| f.get(k))
                .cloned()
                .unwrap_or(if k == "agentActivity" {
                    Value::Null
                } else {
                    Value::Bool(false)
                });
            c.insert(k.into(), v);
        }
        c.insert("loadState".into(), json!("ready"));
        c.insert("hasOlder".into(), json!(false));
        Value::Object(c)
    }

    fn parsed_bots(&self) -> TResult<Vec<Bot>> {
        let mut out = parse::roster(&json!({ "agents": self.agents }), &self.version)?;
        if let Some(m) = out.first_mut() {
            m.main = true;
        }
        Ok(out)
    }
}

impl Transport for FakeTransport {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn probe(&mut self) -> TResult<Capabilities> {
        let mut caps: BTreeSet<Cap> = [
            Cap::ListBots,
            Cap::Rows,
            Cap::Send,
            Cap::StateFlags,
            Cap::NeedsInput,
            Cap::ReadAny,
            Cap::ReadOpen,
            Cap::Stop,
            Cap::PortCheck,
            Cap::LocalToolPermission,
            Cap::RoutinesList,
            Cap::RoutinesToggle,
            Cap::RoutinesAddViaBot,
            Cap::RoutinesRemove,
            Cap::Usage,
            Cap::Files,
        ]
        .into_iter()
        .collect();
        if self.open_only.is_some() {
            caps.remove(&Cap::ReadAny);
        }
        if !self.ui_remove {
            caps.remove(&Cap::RoutinesRemove);
        }
        if self.usage.is_none() {
            caps.remove(&Cap::Usage);
        }
        Ok(Capabilities {
            caps,
            app_version: Some(self.version.clone()),
            tested: crate::version::is_tested(&self.version),
        })
    }

    fn local_tool_permission(&mut self) -> TResult<String> {
        self.fault(Cap::LocalToolPermission)?;
        self.local_tool_permission
            .as_ref()
            .and_then(parse::local_tool_permission)
            .ok_or_else(|| TransportError::App("getLocalToolPermission: no readable answer".into()))
    }

    fn port_check(&mut self) -> TResult<String> {
        self.fault(Cap::PortCheck)?;
        Ok(format!(
            "fixtures for Grok Bot {} loaded (offline; no debug port)",
            self.version
        ))
    }

    fn bots(&mut self) -> TResult<Vec<Bot>> {
        self.fault(Cap::ListBots)?;
        self.advance();
        let mut bots = self.parsed_bots()?;
        for b in &mut bots {
            if let Ok(snap) = self.bot_state_inner(&b.id) {
                b.state = snap.state;
                b.activity = snap.activity;
                b.needs_input = snap.needs_input;
            }
        }
        Ok(bots)
    }

    /// The DOM route: built from the raw agents and the recorded row attributes, without the props parser.
    fn rows(&mut self) -> TResult<Vec<RowView>> {
        self.fault(Cap::Rows)?;
        self.advance();
        let mut out = vec![];
        for (i, a) in self.agents.iter().enumerate() {
            let id = a.get("id").and_then(Value::as_str).unwrap_or_default();
            if !self.row_rendered(i, id) {
                continue;
            }
            let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
            let unread = a.get("hasUnread").and_then(Value::as_bool).unwrap_or(false);
            let over = self.row_over.get(id);
            let aw = a.get("awaitingUserResponse").filter(|v| !v.is_null());
            let desc = over
                .and_then(|o| o.get("desc"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    aw.and_then(|w| w.get("reason"))
                        .and_then(Value::as_str)
                        .map(|r| format!("Waiting for you: {r}"))
                })
                .or_else(|| {
                    a.pointer("/lastEntry/text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let flag =
                |k: &str| over.and_then(|o| o.get(k)).and_then(Value::as_str) == Some("true");
            let v = json!({
                "id": id,
                // the app's label shape: the pinned main bot's row says ", Main Bot" (live P1 acceptance)
                // and a chat-turn approval row says ", Needs attention"
                "label": format!("{name}{}{}{}", if i == 0 { ", Main Bot" } else { "" },
                    if aw.is_some() { ", Needs attention" } else { "" },
                    if unread { ", Unread activity" } else { "" }),
                "desc": desc,
                "awaiting": if flag("awaiting") || (over.is_none() && aw.is_some()) { "true" } else { "false" },
                "composing": if flag("composing") { "true" } else { "false" },
                "layout": if self.collapsed { "collapsed" } else if i == 0 { "pinned" } else { "expanded" },
            });
            if let Some(r) = parse::row(&v) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// With a virtualized sidebar, select the bot as the debug port does: scroll until its row renders.
    fn open(&mut self, id: &str) -> TResult<()> {
        if let Some(sb) = &mut self.sidebar {
            let first = self
                .agents
                .first()
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str);
            if first != Some(id) {
                let before = sb.scrolls;
                let r = super::sidebar::scroll_to_row(sb, id);
                self.sidebar_scrolls
                    .fetch_add(sb.scrolls - before, std::sync::atomic::Ordering::SeqCst);
                r?;
            }
        }
        Ok(())
    }

    fn reconnect(&mut self) -> TResult<()> {
        self.reconnects += 1;
        Ok(())
    }

    fn bot_state(&mut self, id: &str) -> TResult<BotSnapshot> {
        self.fault(Cap::StateFlags)?;
        self.advance();
        self.bot_state_inner(id)
    }

    fn send(&mut self, id: &str, text: &str, files: &[PathBuf]) -> TResult<MessageKey> {
        self.fault(Cap::Send)?;
        if !files.is_empty() {
            return Err(TransportError::Unsupported(Cap::Attach));
        }
        self.advance();
        self.idx(id)?;
        self.user_n += 1;
        self.seq += 1;
        let key = format!("t{}u", 900 + self.user_n);
        // As the 1.1.0 composer does: each line break of a multi-line message is stored as a paragraph break.
        let stored = text.replace("\n\n", "\n").replace('\n', "\n\n");
        let e = json!({"kind":"message","id":key,"role":"user","content":stored,"isStreaming":false,
            "timestampMs":self.clock.now_ms(),"clientNonce":format!("00000000-0000-4000-8000-{:012}", 900 + self.user_n),
            "seq":self.seq,"locallyAuthored":true});
        self.transcripts.entry(id.to_string()).or_default().push(e);
        self.writes.push(RecordedWrite::Send {
            bot: id.to_string(),
            text: text.to_string(),
        });
        self.play_bot(id, text);
        if let Some(t) = self.queued.pop_front() {
            self.start_turn(id, &t, false)?;
        }
        Ok(key)
    }

    fn read(&mut self, id: &str, since: Option<&str>, limit: usize) -> TResult<Vec<Message>> {
        self.fault(Cap::ReadOpen)?;
        self.advance();
        self.idx(id)?;
        if self.open_only.as_deref().is_some_and(|o| o != id) {
            return Err(TransportError::App("the bot's chat is not open".into()));
        }
        let chat = parse::chat(&self.chat_value(id), &self.version)?;
        Ok(crate::engine::slice_since(chat.messages, since, limit))
    }

    fn routines(&mut self, id: &str) -> TResult<Vec<Routine>> {
        self.fault(Cap::RoutinesList)?;
        self.idx(id)?;
        let v = self
            .automations
            .get(id)
            .cloned()
            .unwrap_or_else(|| json!({"kind": "off"}));
        parse::routines(&v, &self.version)
    }

    fn routine_set_enabled(&mut self, id: &str, r: &str, on: bool) -> TResult<Routine> {
        self.fault(Cap::RoutinesToggle)?;
        self.idx(id)?;
        let a = self
            .automations_mut(id)
            .iter_mut()
            .find(|a| a.get("automationId").and_then(Value::as_str) == Some(r))
            .ok_or_else(|| TransportError::App("setTeamBotAutomationEnabled: not found".into()))?;
        a["isEnabled"] = json!(on);
        let v = json!({"kind": "updated", "automation": a.clone()});
        self.writes.push(RecordedWrite::RoutineEnabled {
            bot: id.to_string(),
            routine: r.to_string(),
            on,
        });
        parse::routine_updated(&v, &self.version)
    }

    fn routine_remove(&mut self, id: &str, r: &str) -> TResult<()> {
        if !self.ui_remove {
            return Err(TransportError::Unsupported(Cap::RoutinesRemove));
        }
        self.fault(Cap::RoutinesRemove)?;
        self.idx(id)?;
        self.automations_mut(id)
            .retain(|a| a.get("automationId").and_then(Value::as_str) != Some(r));
        self.writes.push(RecordedWrite::RoutineRemoved {
            bot: id.to_string(),
            routine: r.to_string(),
        });
        Ok(())
    }

    fn usage(&mut self) -> TResult<Usage> {
        self.fault(Cap::Usage)?;
        let (w, s) = self
            .usage
            .clone()
            .ok_or(TransportError::Unsupported(Cap::Usage))?;
        parse::usage(&w, &s, &self.version)
    }

    fn files(&mut self, id: &str) -> TResult<Vec<FileEntry>> {
        self.fault(Cap::Files)?;
        self.idx(id)?;
        let v = self
            .files
            .get(id)
            .cloned()
            .unwrap_or_else(|| json!({"files": [], "totalBytes": 0, "canEdit": true}));
        parse::files(&v, &self.version)
    }

    fn file_attach(&mut self, id: &str, path: &Path) -> TResult<FileEntry> {
        self.fault(Cap::Files)?;
        self.idx(id)?;
        let bytes = std::fs::read(path)
            .map_err(|e| TransportError::App(format!("{}: {e}", path.display())))?;
        let name = crate::files::safe_name(
            &path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
        self.seq += 1;
        let p = format!(
            "grok-bot/agents/{id}/attachments/01SYNTHETIC{:015}.bin",
            self.seq
        );
        let n = bytes.len() as u64;
        self.add_file(
            id,
            &name,
            &p,
            bytes,
            &crate::files::guess_content_type(path),
        );
        self.writes.push(RecordedWrite::FileUploaded {
            bot: id.to_string(),
            name: name.clone(),
            bytes: n,
        });
        self.files(id)?
            .into_iter()
            .find(|f| f.path == p)
            .ok_or_else(|| TransportError::App("upload not listed".into()))
    }

    fn file_remove(&mut self, id: &str, name: &str) -> TResult<()> {
        self.fault(Cap::Files)?;
        self.idx(id)?;
        if let Some(a) = self
            .files
            .get_mut(id)
            .and_then(|v| v.get_mut("files"))
            .and_then(Value::as_array_mut)
        {
            a.retain(|f| f.get("name").and_then(Value::as_str) != Some(name));
        }
        self.writes.push(RecordedWrite::FileDeleted {
            bot: id.to_string(),
            name: name.to_string(),
        });
        Ok(())
    }

    fn file_read(&mut self, id: &str, f: &FileEntry) -> TResult<Vec<u8>> {
        self.fault(Cap::Files)?;
        self.idx(id)?;
        Ok(self.blob(f))
    }

    fn file_read_checked(&mut self, id: &str, f: &FileEntry) -> TResult<(Vec<u8>, Option<String>)> {
        let b = self.file_read(id, f)?;
        // the same chunked path the debug port uses: base64 chunks, reassembled
        let b = crate::files::assemble(b.len(), |o, n| Ok(crate::files::b64_encode(&b[o..o + n])))
            .map_err(TransportError::App)?;
        let sha = if self.bad_app_digest {
            "0".repeat(64)
        } else {
            crate::files::sha256_hex(&b)
        };
        Ok((b, (!self.no_app_digest).then_some(sha)))
    }

    fn attachment_read(
        &mut self,
        id: &str,
        key: &str,
        f: &FileRef,
    ) -> TResult<(Vec<u8>, Option<String>)> {
        self.fault(Cap::Files)?;
        self.idx(id)?;
        // as the debug port: a path that is not the app's own asset URL is never read directly (review H2)
        let direct = crate::policy::floors::attachment_url_ok(&f.path, id)
            .then(|| self.blobs.get(&f.path).cloned())
            .flatten();
        let b = direct
            .or_else(|| self.previews.get(key).cloned())
            .ok_or_else(|| TransportError::App("the preview could not load the file".into()))?;
        let sha = if self.bad_app_digest {
            "0".repeat(64)
        } else {
            crate::files::sha256_hex(&b)
        };
        Ok((b, Some(sha)))
    }

    fn stop(&mut self, id: &str, steer: &str) -> TResult<StopMethod> {
        self.writes.push(RecordedWrite::Stop {
            bot: id.to_string(),
            steer: steer.to_string(),
        });
        self.send(id, steer, &[]).map(|_| StopMethod::Steer)
    }
}

impl FakeTransport {
    fn bot_state_inner(&self, id: &str) -> TResult<BotSnapshot> {
        let i = self.idx(id)?;
        let bot = parse::agent(&self.agents[i], &self.version)?;
        let chat = parse::chat(&self.chat_value(id), &self.version)?;
        let runs = self
            .routine_runs
            .get(id)
            .is_some_and(parse::routine_run_open);
        Ok(parse::snapshot_with(&bot, Some(&chat), runs))
    }
}
