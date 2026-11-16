//! Parsers from the app's raw renderer props (the sidebar's `agents[]`, the chat's `entries[]`) to the model.
//! Every transport that reads props, and the fake that replays recorded props, goes through these functions,
//! so the offline suite exercises the same parsing code the live transport uses.
use crate::model::*;
use crate::transport::{TResult, TransportError};
use crate::util::rfc3339;
use crate::version::fields_for;
use serde_json::Value;

fn shape(field: &str, version: &str) -> TransportError {
    TransportError::Shape {
        field: field.to_string(),
        version: version.to_string(),
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn ms(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_u64).map(rfc3339)
}

/// `awaitingUserResponse: {tabId, reason, since}` -> NeedsInput.
pub fn awaiting(v: &Value) -> Option<NeedsInput> {
    let a = v.get("awaitingUserResponse")?;
    if a.is_null() {
        return None;
    }
    let reason = s(a, "reason");
    let approval = s(a, "tabId") == "auto-review" || reason.starts_with("Permission");
    Some(NeedsInput {
        kind: if approval {
            NeedsKind::Approval
        } else {
            NeedsKind::Question
        },
        reason,
        since: ms(a, "since"),
        options: if approval {
            vec!["Allow once".into(), "Deny once".into()]
        } else {
            vec![]
        },
        source: NeedsSource::Awaiting,
        since_ms: a.get("since").and_then(Value::as_u64),
    })
}

/// The row / `lastEntry` text of a routine approval card (1.1.0).
pub const ROUTINE_PERMISSION_PREFIX: &str = "Permission required:";

/// A routine approval card seen through `lastEntry.text` (the same text is the row's `aria-description`).
pub fn routine_permission(v: &Value) -> Option<NeedsInput> {
    let t = v.pointer("/lastEntry/text").and_then(Value::as_str)?;
    t.starts_with(ROUTINE_PERMISSION_PREFIX)
        .then(|| NeedsInput {
            kind: NeedsKind::Approval,
            reason: t.to_string(),
            since: None,
            options: vec!["Allow once".into(), "Deny once".into()],
            source: NeedsSource::Routine,
            since_ms: None,
        })
}

/// A routine run that is still running with no finish time (`listTeamBotAutomations` runs, or a plain list of
/// runs). Accepts `{automations:[{runs:[..]}]}`, `[{runs:[..]}]` or `[run, ..]`.
pub fn routine_run_open(v: &Value) -> bool {
    let open = |r: &Value| {
        r.get("status").and_then(Value::as_str) == Some("running")
            && r.get("finishedAt").is_none_or(Value::is_null)
    };
    let list = v
        .get("automations")
        .and_then(Value::as_array)
        .or_else(|| v.as_array());
    list.into_iter().flatten().any(|x| match x.get("runs") {
        Some(Value::Array(runs)) => runs.iter().any(open),
        _ => open(x),
    })
}

/// `getLocalToolPermission()`'s answer (a string, or an object with `value`/`permission`/`mode`) normalised to
/// `always` | `ask` | `never`; anything else is None (the field is then left out).
pub fn local_tool_permission(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.as_str(),
        Value::Object(o) => ["value", "permission", "mode"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str))?,
        _ => return None,
    };
    let s = s.trim().to_ascii_lowercase();
    matches!(s.as_str(), "always" | "ask" | "never").then_some(s)
}

/// One sidebar agent object. `main` is set by the caller (the DOM's pinned row; the props do not say).
pub fn agent(v: &Value, version: &str) -> TResult<Bot> {
    for f in fields_for(version).agent_required {
        if v.get(*f).is_none() {
            return Err(shape(&format!("agents[].{f}"), version));
        }
    }
    let working = b(v, "isRunningTurn") || b(v, "isRunning") || b(v, "isComposingMessage");
    let needs = awaiting(v).or_else(|| routine_permission(v));
    let state = if needs.is_some() {
        BotState::NeedsInput
    } else if working {
        BotState::Working
    } else {
        BotState::Idle
    };
    Ok(Bot {
        id: s(v, "id"),
        name: s(v, "name"),
        main: false,
        state,
        activity: None,
        needs_input: needs,
        unread: v.get("unreadCount").and_then(Value::as_u64).unwrap_or(0),
        last_key: s(v, "lastMessageId"),
        last_at: ms(v, "lastActivityAt"),
        hidden: b(v, "isHiddenFromSidebar"),
    })
}

/// The sidebar props `{agents: [...]}`.
pub fn roster(v: &Value, version: &str) -> TResult<Vec<Bot>> {
    let a = v
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| shape("agents", version))?;
    a.iter().map(|x| agent(x, version)).collect()
}

/// The chat props of the bot that is open on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Chat {
    pub messages: Vec<Message>,
    pub running: bool,
    pub composing: bool,
    pub activity: Option<String>,
}

impl Chat {
    /// The newest bot-authored message (not user, not event).
    pub fn newest_bot(&self) -> Option<&Message> {
        self.messages.iter().rev().find(|m| m.role == Role::Bot)
    }
}

pub fn activity(v: &Value) -> Option<String> {
    let a = v.get("agentActivity")?;
    match a.get("kind").and_then(Value::as_str)? {
        "thinking" => Some("is thinking".into()),
        "tool" => Some(
            a.get("detail")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "is using {}",
                        a.get("tool").and_then(Value::as_str).unwrap_or("a tool")
                    )
                }),
        ),
        k => Some(format!("is {k}")),
    }
}

pub fn chat(v: &Value, version: &str) -> TResult<Chat> {
    let f = fields_for(version);
    for k in f.chat_required {
        if v.get(*k).is_none() {
            return Err(shape(&format!("chat.{k}"), version));
        }
    }
    let entries = v
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| shape("chat.entries", version))?;
    let mut messages = Vec::with_capacity(entries.len());
    for e in entries {
        for k in f.entry_required {
            if e.get(*k).is_none() {
                return Err(shape(&format!("chat.entries[].{k}"), version));
            }
        }
        if let Some(m) = entry(e) {
            messages.push(m);
        }
    }
    Ok(Chat {
        messages,
        running: b(v, "isAgentRunning"),
        composing: b(v, "isAgentComposing"),
        activity: activity(v),
    })
}

fn links(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|w| w.starts_with("https://") || w.starts_with("http://"))
        .map(|w| {
            w.trim_end_matches(|c: char| ",.;)]>\"'".contains(c))
                .to_string()
        })
        .collect()
}

fn msg(key: String, role: Role, kind: MsgKind, text: String, at: Option<String>) -> Message {
    Message {
        key,
        role,
        kind,
        links: links(&text),
        text,
        at,
        file: None,
        options: None,
        answered: None,
        at_ms: None,
    }
}

/// One transcript entry. Kinds the app does not show (and unknown kinds) return None.
/// The sidebar read's result (`{agents, hiddenAgents, unavailableReason}` or `{error}` when the props were not
/// found): a missing sidebar is a shape error naming the field.
pub fn sidebar_props(v: Value, version: &str) -> TResult<Value> {
    if v.get("error").is_some() || v.is_null() {
        return Err(shape("sidebar props (agents, listRows)", version));
    }
    Ok(v)
}

/// The id prefix of a user message the app shows before its server has accepted it (1.1.0).
pub const OPTIMISTIC_PREFIX: &str = "optimistic:";

/// True when a transcript message is the text we sent. The composer stores each line break of a multi-line message
/// as a paragraph break (seen live on 1.1.0: "a\nb" comes back as "a\n\nb"), so whitespace runs are compared as one
/// space; every other character must match.
pub fn same_sent_text(stored: &str, sent: &str) -> bool {
    let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    norm(stored) == norm(sent)
}

/// The server key of a sent message: in 1.1.0 our message first appears as `optimistic:<clientNonce>` and about
/// 2 s later is replaced by `t<turn>u` with the same `clientNonce`. A key of the first kind never appears again, so
/// `wait --since` and ask's "reply after our message" need the second. None while it is still optimistic.
pub fn confirmed_key(entries: &Value, key: &str) -> Option<String> {
    let Some(nonce) = key.strip_prefix(OPTIMISTIC_PREFIX) else {
        return Some(key.to_string());
    };
    entries.as_array()?.iter().find_map(|e| {
        let id = s(e, "id");
        (s(e, "clientNonce") == nonce && !id.is_empty() && !id.starts_with(OPTIMISTIC_PREFIX))
            .then_some(id)
    })
}

pub fn entry(e: &Value) -> Option<Message> {
    let mut m = entry_inner(e)?;
    m.at_ms = e.get("timestampMs").and_then(Value::as_u64);
    Some(m)
}

fn entry_inner(e: &Value) -> Option<Message> {
    let key = s(e, "id");
    let at = ms(e, "timestampMs");
    match e.get("kind").and_then(Value::as_str)? {
        "message" => {
            let role = if s(e, "role") == "user" {
                Role::User
            } else {
                Role::Bot
            };
            Some(msg(key, role, MsgKind::Text, s(e, "content"), at))
        }
        "send-message" => {
            let m = e.get("message")?;
            match m.get("type").and_then(Value::as_str)? {
                "text" => Some(msg(key, Role::Bot, MsgKind::Text, s(m, "content"), at)),
                "widget" => {
                    let w = m.get("widget")?;
                    let mut x = msg(key, Role::Bot, MsgKind::Question, s(w, "prompt"), at);
                    x.options = Some(
                        w.get("options")
                            .and_then(Value::as_array)
                            .map(|o| {
                                o.iter()
                                    .map(|o| MsgOption {
                                        label: s(o, "label"),
                                        value: s(o, "value"),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    );
                    x.answered = match e.get("respondedValue") {
                        Some(Value::String(v)) => Some(v.clone()),
                        Some(v) if !v.is_null() => Some(v.to_string()),
                        _ if b(e, "widgetSkipped") => Some("skipped".into()),
                        _ => None,
                    };
                    Some(x)
                }
                "local-tool-permission" => {
                    let a = m.get("ask")?;
                    let text = format!("Permission needed: {} {}", s(a, "action"), s(a, "target"));
                    let mut x = msg(key, Role::Bot, MsgKind::Approval, text, at);
                    x.options = Some(vec![
                        MsgOption {
                            label: "Allow once".into(),
                            value: "allow".into(),
                        },
                        MsgOption {
                            label: "Deny once".into(),
                            value: "deny".into(),
                        },
                    ]);
                    let st = s(a, "status");
                    x.answered = (st != "pending").then_some(st);
                    Some(x)
                }
                // a file the bot sent (1.1.0: `file_name`, `source_path` in its workspace, and `url`, the
                // `file:///home/box/…/assets/<hash>.<ext>` path the app's attachment reads take: kept as `path`)
                "attachment" => {
                    let name = ["file_name", "fileName", "name"]
                        .iter()
                        .find_map(|k| m.get(*k).and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_string();
                    let path = ["url", "source_path", "sourcePath", "path"]
                        .iter()
                        .find_map(|k| m.get(*k).and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_string();
                    let mut x = msg(key, Role::Bot, MsgKind::Text, s(m, "content"), at);
                    if x.text.is_empty() {
                        x.text = format!("[file {name}]");
                    }
                    x.file = Some(FileRef { name, path });
                    Some(x)
                }
                other => Some(msg(key, Role::Bot, MsgKind::Text, format!("[{other}]"), at)),
            }
        }
        "event" => {
            let ev = e.get("event")?;
            if ev.get("type").and_then(Value::as_str) == Some("automation-changed") {
                let t = format!("routine {}: {}", s(ev, "action"), s(ev, "automationName"));
                Some(msg(key, Role::Event, MsgKind::RoutineEvent, t, at))
            } else {
                None
            }
        }
        "feedback" => Some(msg(key, Role::Event, MsgKind::Feedback, s(e, "state"), at)),
        _ => None,
    }
}

/// The state mapping (SPEC §3): awaitingUserResponse, then the working flags, then an open prompt as the
/// newest bot entry.
pub fn snapshot(bot: &Bot, chat: Option<&Chat>) -> BotSnapshot {
    snapshot_with(bot, chat, false)
}

/// [`snapshot`] plus the routine signals (ROUTINE-TRUTH): with a routine run open (`routine_running`), any
/// pending approval card in the transcript is that run's card and needs the user, even under a newer chat
/// message and even while the working flags are set. Without an open run, a pending card that is the newest bot
/// entry still needs the user when the bot is working (routine cards can come with `isRunning` true).
pub fn snapshot_with(bot: &Bot, chat: Option<&Chat>, routine_running: bool) -> BotSnapshot {
    let newest = chat.and_then(Chat::newest_bot);
    let newest_kind = newest.map(|m| match m.kind {
        MsgKind::Approval if m.answered.is_none() => EntryKind::PendingApproval,
        MsgKind::Question if m.answered.is_none() => EntryKind::OpenQuestion,
        MsgKind::Text => EntryKind::Text,
        _ => EntryKind::Other,
    });
    let mut state = bot.state;
    let mut needs = bot.needs_input.clone();
    if let Some(c) = chat {
        if state == BotState::Idle && (c.running || c.composing) {
            state = BotState::Working;
        }
    }
    if state != BotState::NeedsInput {
        let pending = |m: &&Message| m.kind == MsgKind::Approval && m.answered.is_none();
        let card = if routine_running {
            chat.and_then(|c| c.messages.iter().rev().find(pending))
        } else if state == BotState::Working {
            newest.filter(pending)
        } else {
            None
        };
        if let Some(m) = card {
            state = BotState::NeedsInput;
            needs = Some(NeedsInput {
                kind: NeedsKind::Approval,
                reason: m.text.clone(),
                since: m.at.clone(),
                options: vec!["Allow once".into(), "Deny once".into()],
                source: NeedsSource::Routine,
                since_ms: m.at_ms,
            });
        }
    }
    if state == BotState::Idle {
        if let Some(m) = newest.filter(|m| m.is_open_prompt()) {
            state = BotState::NeedsInput;
            needs = Some(NeedsInput {
                kind: if m.kind == MsgKind::Approval {
                    NeedsKind::Approval
                } else {
                    NeedsKind::Question
                },
                reason: m.text.clone(),
                since: m.at.clone(),
                options: m
                    .options
                    .iter()
                    .flatten()
                    .map(|o| o.label.clone())
                    .collect(),
                source: NeedsSource::Entry,
                since_ms: m.at_ms,
            });
        }
    }
    BotSnapshot {
        state,
        activity: chat
            .and_then(|c| c.activity.clone())
            .or_else(|| bot.activity.clone()),
        needs_input: needs,
        last_key: bot.last_key.clone(),
        newest_bot_entry: newest_kind,
    }
}

/// A DOM row from its data attributes: `aria-label` (name + ", Main Bot" on the pinned row + ", Unread activity"), `aria-description` (the last
/// line or "Waiting for you: ..."), `data-awaiting`, `data-composing`, `data-layout`.
/// Status suffixes the app appends to a sidebar row's `aria-label` (1.1.0, seen live).
pub const ROW_LABEL_SUFFIXES: &[&str] = &[", Unread activity", ", Needs attention"];

pub fn row(v: &Value) -> Option<RowView> {
    let id = s(v, "id");
    if id.is_empty() {
        return None;
    }
    let label = s(v, "label");
    let main = s(v, "layout") == "pinned";
    // 1.1.0 labels: "<name>" plus status suffixes (", Unread activity", ", Needs attention"); the pinned main
    // bot's row also says ", Main Bot". Strip them in any order.
    let mut name = label.as_str();
    loop {
        let before = name;
        for sfx in ROW_LABEL_SUFFIXES {
            name = name.strip_suffix(sfx).unwrap_or(name);
        }
        if main {
            name = name.strip_suffix(", Main Bot").unwrap_or(name);
        }
        if name == before {
            break;
        }
    }
    let name = name.to_string();
    let desc = s(v, "desc");
    let truthy = |k: &str| matches!(v.get(k), Some(Value::String(x)) if x == "true") || b(v, k);
    Some(RowView {
        id,
        name,
        main,
        awaiting: truthy("awaiting")
            || desc.starts_with("Waiting for you")
            || desc.starts_with(ROUTINE_PERMISSION_PREFIX),
        composing: truthy("composing"),
        desc,
    })
}

// ---------------------------------------------------------------- P3: routines, usage, files

/// `CRON_TZ=<tz> m h dom mon dow` -> (`m h dom mon dow`, Some(tz)); a plain cron string has no tz.
pub fn split_cron_tz(s: &str) -> (String, Option<String>) {
    let s = s.trim();
    match s.strip_prefix("CRON_TZ=") {
        Some(rest) => match rest.split_once(char::is_whitespace) {
            Some((tz, cron)) => (cron.trim().to_string(), Some(tz.to_string())),
            None => (String::new(), Some(rest.to_string())),
        },
        None => (s.to_string(), None),
    }
}

/// One automation from `listTeamBotAutomations` / `setTeamBotAutomationEnabled` (1.1.0).
pub fn routine(v: &Value, version: &str) -> TResult<Routine> {
    for f in ["automationId", "name", "isEnabled"] {
        if v.get(f).is_none() {
            return Err(shape(&format!("automations[].{f}"), version));
        }
    }
    let rec = v.get("record").unwrap_or(&Value::Null);
    let sched = v
        .get("schedule")
        .and_then(Value::as_str)
        .or_else(|| rec.get("schedule").and_then(Value::as_str))
        .unwrap_or_default();
    let (schedule, tz) = split_cron_tz(sched);
    let caps = v
        .get("viewerCapabilities")
        .or_else(|| rec.get("viewerCapabilities"))
        .unwrap_or(&Value::Null);
    let runs = rec
        .get("runs")
        .or_else(|| v.get("runs"))
        .and_then(Value::as_array);
    let running = runs.is_some_and(|r| routine_run_open(&Value::Array(r.clone())));
    Ok(Routine {
        id: s(v, "automationId"),
        slug: v
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| rec.get("id").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
        name: s(v, "name"),
        schedule,
        tz,
        enabled: b(v, "isEnabled"),
        next_run: ms(v, "nextRunAt").or_else(|| ms(rec, "nextRunAt")),
        last_run: ms(v, "lastRunAt").or_else(|| ms(rec, "lastRunAt")),
        prompt: s(rec, "prompt"),
        can: RoutineCan {
            toggle: b(caps, "canToggle"),
            edit: b(caps, "canEdit"),
            delete: b(caps, "canDelete"),
        },
        running,
        runs: runs.map_or(0, |r| r.len() as u64),
    })
}

/// `listTeamBotAutomations({agentId})`: `{kind:"available", automations:[..]}`; `{kind:"off"}` (no routines
/// for this bot, e.g. after its bot was deleted) is an empty list. Any other kind is an app answer we do not know.
pub fn routines(v: &Value, version: &str) -> TResult<Vec<Routine>> {
    match v.get("kind").and_then(Value::as_str) {
        Some("available") => v
            .get("automations")
            .and_then(Value::as_array)
            .ok_or_else(|| shape("listTeamBotAutomations().automations", version))?
            .iter()
            .map(|a| routine(a, version))
            .collect(),
        Some("off") => Ok(vec![]),
        Some(k) => Err(TransportError::App(format!(
            "listTeamBotAutomations: routines are {k} for this bot"
        ))),
        None => Err(shape("listTeamBotAutomations().kind", version)),
    }
}

/// `setTeamBotAutomationEnabled(..)` -> `{kind:"updated", automation}`.
pub fn routine_updated(v: &Value, version: &str) -> TResult<Routine> {
    match v.get("kind").and_then(Value::as_str) {
        Some("updated") => routine(
            v.get("automation")
                .ok_or_else(|| shape("setTeamBotAutomationEnabled().automation", version))?,
            version,
        ),
        Some(k) => Err(TransportError::App(format!(
            "setTeamBotAutomationEnabled: {k}"
        ))),
        None => Err(shape("setTeamBotAutomationEnabled().kind", version)),
    }
}

/// `getCursorWeeklyUsage({})` and `getCursorUsageSummary({})` -> Usage. The summary's `dashboardUrl` (it holds
/// the account id) is never read.
pub fn usage(weekly: &Value, summary: &Value, version: &str) -> TResult<Usage> {
    let pct = weekly
        .get("percentUsed")
        .and_then(Value::as_f64)
        .or_else(|| summary.get("sandUsagePercent").and_then(Value::as_f64))
        .ok_or_else(|| shape("getCursorWeeklyUsage().percentUsed", version))?;
    let reset = weekly
        .get("nextResetMs")
        .and_then(Value::as_u64)
        .or_else(|| {
            summary
                .get("sandUsageResetTimestampMs")
                .and_then(Value::as_u64)
        });
    let plan = ["unifiedPlanName", "grokPlanLabel"]
        .iter()
        .find_map(|k| {
            weekly
                .get(*k)
                .and_then(Value::as_str)
                .or_else(|| summary.get(*k).and_then(Value::as_str))
        })
        .unwrap_or_default()
        .to_string();
    let od = summary.get("onDemand").unwrap_or(&Value::Null);
    Ok(Usage {
        percent_used: (pct * 10.0).round() / 10.0,
        resets_at: reset.map(rfc3339),
        plan,
        on_demand_cents: od.get("usedCents").and_then(Value::as_u64).unwrap_or(0),
        on_demand_limit_cents: od.get("limitCents").and_then(Value::as_u64),
        available: summary
            .get("hasAvailableUsage")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

/// One file record (`listGrokBotAgentFiles().files[]`, or `uploadGrokBotAgentFile`'s answer).
pub fn file_entry(v: &Value, version: &str) -> TResult<FileEntry> {
    for f in ["name", "path"] {
        if v.get(f).and_then(Value::as_str).is_none() {
            return Err(shape(&format!("files[].{f}"), version));
        }
    }
    Ok(FileEntry {
        name: s(v, "name"),
        path: s(v, "path"),
        bytes: v
            .get("sizeBytes")
            .or_else(|| v.get("size"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        content_type: s(v, "contentType"),
        updated_at: ms(v, "updatedAtMs"),
    })
}

/// `listGrokBotAgentFiles({agentId})` -> `{files:[..], totalBytes, canEdit}`.
pub fn files(v: &Value, version: &str) -> TResult<Vec<FileEntry>> {
    v.get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| shape("listGrokBotAgentFiles().files", version))?
        .iter()
        .map(|f| file_entry(f, version))
        .collect()
}

/// The upload answer: the file record itself, or `{file: record}`.
pub fn uploaded(v: &Value, version: &str) -> TResult<FileEntry> {
    file_entry(v.get("file").unwrap_or(v), version)
}
