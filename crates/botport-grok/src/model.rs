//! The engine's JSON shapes (SPEC §3). Field names here are the public contract twin and MCP clients read.
use serde::Serialize;

pub type BotId = String;
pub type MessageKey = String;
pub type RoutineId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotState {
    Idle,
    Working,
    NeedsInput,
}

impl BotState {
    pub fn as_str(self) -> &'static str {
        match self {
            BotState::Idle => "idle",
            BotState::Working => "working",
            BotState::NeedsInput => "needs_input",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedsKind {
    Approval,
    Question,
}

/// Where a needs-input state was seen: the bot's `awaitingUserResponse` field, or its newest transcript entry
/// (a pending approval card or an unanswered question widget).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NeedsSource {
    #[default]
    Awaiting,
    Entry,
    /// A routine run's approval card (ROUTINE-TRUTH): `awaitingUserResponse` stays null; seen through the row /
    /// `lastEntry` text "Permission required: …", or a pending card while a routine run is running unfinished.
    /// A chat message does not cancel it, so it is never stale.
    Routine,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NeedsInput {
    pub kind: NeedsKind,
    pub reason: String,
    pub since: Option<String>,
    pub options: Vec<String>,
    #[serde(skip)]
    pub source: NeedsSource,
    /// `since` in Unix ms (an approval raised before our message is stale: a new message cancels it).
    #[serde(skip)]
    pub since_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Bot {
    pub id: BotId,
    pub name: String,
    pub main: bool,
    pub state: BotState,
    pub activity: Option<String>,
    pub needs_input: Option<NeedsInput>,
    pub unread: u64,
    pub last_key: MessageKey,
    pub last_at: Option<String>,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Bot,
    Event,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MsgKind {
    Text,
    Question,
    Approval,
    RoutineEvent,
    Feedback,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FileRef {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MsgOption {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Message {
    pub key: MessageKey,
    pub role: Role,
    pub kind: MsgKind,
    pub text: String,
    pub at: Option<String>,
    pub links: Vec<String>,
    pub file: Option<FileRef>,
    pub options: Option<Vec<MsgOption>>,
    /// For a question: the chosen value or `"skipped"`; for an approval: its status once decided
    /// (`allowed`, `denied`, ...). `None` while open.
    pub answered: Option<String>,
    #[serde(skip)]
    pub at_ms: Option<u64>,
}

impl Message {
    /// An approval card that is still pending, or a question nobody answered or skipped.
    pub fn is_open_prompt(&self) -> bool {
        matches!(self.kind, MsgKind::Approval | MsgKind::Question) && self.answered.is_none()
    }
}

/// The kind of the newest bot-authored transcript entry, as far as the state mapping needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Text,
    PendingApproval,
    OpenQuestion,
    Other,
}

/// Cheap state poll of one bot (the settle loop uses only this).
#[derive(Clone, Debug, PartialEq)]
pub struct BotSnapshot {
    pub state: BotState,
    pub activity: Option<String>,
    pub needs_input: Option<NeedsInput>,
    pub last_key: MessageKey,
    pub newest_bot_entry: Option<EntryKind>,
}

/// One sidebar row as the DOM shows it (data attributes and aria text): the second, independent read route
/// that PASS lines are checked against.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RowView {
    pub id: BotId,
    pub name: String,
    pub main: bool,
    pub awaiting: bool,
    pub composing: bool,
    pub desc: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AskResult {
    pub bot: Bot,
    pub messages: Vec<Message>,
    pub state: BotState,
    pub waited_s: f64,
    pub proof: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WatchEvent {
    #[serde(rename = "type")]
    pub kind: String,
    pub bot: String,
    pub bot_id: BotId,
    pub at: String,
    pub message: Option<Message>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WatchSummary {
    pub minutes: f64,
    pub events: u64,
}

/// SPEC §3 Routine (P3 fills the parsers; the shape is frozen here).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RoutineCan {
    pub toggle: bool,
    pub edit: bool,
    pub delete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Routine {
    pub id: RoutineId,
    pub slug: String,
    pub name: String,
    pub schedule: String,
    pub tz: Option<String>,
    pub enabled: bool,
    pub next_run: Option<String>,
    pub last_run: Option<String>,
    pub prompt: String,
    pub can: RoutineCan,
    /// P3, additive: a run is in progress (`status:"running"`, no finish time). Such a run's approval card is
    /// needs-input (signal 3 of §3a).
    pub running: bool,
    /// P3, additive: number of recorded runs (`record.runs`); `routines run` checks it grew.
    pub runs: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub content_type: String,
    pub updated_at: Option<String>,
}

/// P3: one file saved by `files download` (never over an existing file).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SavedFile {
    /// The file's name in the app.
    pub name: String,
    /// Where it was written (a new file; a name that was taken gets a ` (1)` suffix).
    pub saved_to: String,
    pub bytes: u64,
    pub sha256: String,
    /// The app's own sha256 of the bytes it handed over, when the app could compute it.
    pub app_sha256: Option<String>,
}

/// P3: the result of a routine change (add, enable, disable, remove, run).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RoutineChange {
    pub bot: Bot,
    pub action: String,
    /// The routine after the change (None after a remove).
    pub routine: Option<Routine>,
    /// How it was done: `ipc`, `ui` or `bot` (asked the bot in chat).
    pub via: String,
    /// add/run/remove via the bot: the bot's replies after our message.
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Usage {
    pub percent_used: f64,
    pub resets_at: Option<String>,
    pub plan: String,
    pub on_demand_cents: u64,
    pub on_demand_limit_cents: Option<u64>,
    pub available: bool,
}

/// How a stop was done. 1.1.0 has no native interrupt, so it is always `Steer` today.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopMethod {
    Native,
    Steer,
}
