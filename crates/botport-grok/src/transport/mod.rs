//! The transport seam: one trait, implemented by the debug-port transport ([`cdp`]) and the fixture-replaying
//! fake ([`fake`]). `ax`, `bot-shell` and `webhook` come in later phases and implement the same trait.
use crate::model::*;
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

pub mod cdp;
pub mod fake;
pub mod sidebar;
pub mod ws;

/// Right after a launch the page target is up and `document.readyState` is complete, but the app has not yet
/// rendered its sidebar (0 rows for about 0.5 s on 1.1.0), so the first read failed as a shape change. Poll `read`
/// every 250 ms until it stops failing with `Shape` or `timeout_ms` passes; other errors return at once.
pub fn wait_until_rendered<T>(
    clock: &crate::clock::SharedClock,
    timeout_ms: u64,
    mut read: impl FnMut() -> TResult<T>,
) -> TResult<T> {
    let t0 = clock.now_ms();
    loop {
        match read() {
            Err(TransportError::Shape { .. }) if clock.now_ms() - t0 < timeout_ms => {
                clock.sleep_ms(250)
            }
            r => return r,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cap {
    ListBots,
    Send,
    Attach,
    StateFlags,
    NeedsInput,
    ReadAny,
    ReadOpen,
    Stop,
    RoutinesList,
    RoutinesToggle,
    RoutinesAddViaBot,
    RoutinesRemove,
    Usage,
    Files,
    Lifecycle,
    Rows,
    /// P5: the transport's own route still opens and answers (the canary's first check).
    PortCheck,
    /// Read the app's "Execution on Local Computer" setting (`getLocalToolPermission`). Read only.
    LocalToolPermission,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Capabilities {
    pub caps: BTreeSet<Cap>,
    /// The app version the transport saw (`1.1.0`), if any.
    pub app_version: Option<String>,
    /// The version is in the tested-versions table ([`crate::version`]).
    pub tested: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransportError {
    /// exit 5: the app is not installed / not running / the port is closed.
    Unavailable(String),
    /// exit 5: the bot's cloud computer is asleep.
    BotComputerAsleep(String),
    /// exit 1: the app's shape is not what this version's parser expects; names the field.
    Shape { field: String, version: String },
    /// the engine tries the next transport; exit 5 if none is left.
    Unsupported(Cap),
    /// exit 1: an app/IPC error (scrubbed text).
    App(String),
    /// exit 4: a hard floor stopped the call at the transport edge (floors 1, 2, 4, 6).
    Floor(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Unavailable(s) => write!(f, "unavailable: {s}"),
            TransportError::BotComputerAsleep(s) => write!(f, "asleep: {s}"),
            TransportError::Shape { field, version } => {
                write!(
                    f,
                    "shape: Grok Bot {version}: field {field} missing or changed"
                )
            }
            TransportError::Unsupported(c) => write!(f, "unsupported: {c:?}"),
            TransportError::App(s) => write!(f, "app: {s}"),
            TransportError::Floor(s) => write!(f, "refused by hard floor: {s}"),
        }
    }
}

impl std::error::Error for TransportError {}

pub type TResult<T> = Result<T, TransportError>;

/// Every method has a default that returns `Unsupported`, so a transport implements only what it can do.
/// Routine add/run are engine-level (they compose send + wait + routines).
pub trait Transport: Send {
    /// `"debug-port"`, `"ax"`, `"bot-shell"`, `"webhook"` or `"fake"`.
    fn name(&self) -> &'static str;
    fn probe(&mut self) -> TResult<Capabilities>;
    /// P5 canary, first step: the route is still open and answering (debug port: `/json/version`, one page
    /// target, a trivial `Runtime.evaluate`), i.e. the vendor's DevTools gate has not closed it. Read-only.
    fn port_check(&mut self) -> TResult<String> {
        Err(TransportError::Unsupported(Cap::PortCheck))
    }
    /// The app's local-tool permission setting, normalised to `always` | `ask` | `never`. Read only: botport
    /// never sets it (it is the owner's setting).
    fn local_tool_permission(&mut self) -> TResult<String> {
        Err(TransportError::Unsupported(Cap::LocalToolPermission))
    }
    /// Open the route again after the app dropped it mid-run (seen live on 1.1.0: the debug-port websocket ends with
    /// EOF during a long `wait`). Read-only callers retry once after it; nothing that writes is ever retried.
    fn reconnect(&mut self) -> TResult<()> {
        Err(TransportError::Unsupported(Cap::StateFlags))
    }
    /// All bots, with state and needs_input (the props route).
    fn bots(&mut self) -> TResult<Vec<Bot>> {
        Err(TransportError::Unsupported(Cap::ListBots))
    }
    /// The sidebar rows read through a different route than [`Transport::bots`] (DOM attributes / AX).
    fn rows(&mut self) -> TResult<Vec<RowView>> {
        Err(TransportError::Unsupported(Cap::Rows))
    }
    /// Cheap state poll (about 30 ms on the debug port).
    fn bot_state(&mut self, _id: &str) -> TResult<BotSnapshot> {
        Err(TransportError::Unsupported(Cap::StateFlags))
    }
    /// Bring the bot's chat on screen (debug-port / ax need this before send and read).
    fn open(&mut self, _id: &str) -> TResult<()> {
        Ok(())
    }
    /// Send a user message; returns the new user message's key.
    fn send(&mut self, _id: &str, _text: &str, _files: &[PathBuf]) -> TResult<MessageKey> {
        Err(TransportError::Unsupported(Cap::Send))
    }
    /// Messages oldest first; with `since`, only those after that key. `limit` keeps the newest N (0 = all).
    fn read(&mut self, _id: &str, _since: Option<&str>, _limit: usize) -> TResult<Vec<Message>> {
        Err(TransportError::Unsupported(Cap::ReadOpen))
    }
    /// Stop the current turn. 1.1.0: a steer message (no native control exists).
    fn stop(&mut self, id: &str, steer: &str) -> TResult<StopMethod> {
        self.send(id, steer, &[]).map(|_| StopMethod::Steer)
    }
    fn routines(&mut self, _id: &str) -> TResult<Vec<Routine>> {
        Err(TransportError::Unsupported(Cap::RoutinesList))
    }
    fn routine_set_enabled(&mut self, _id: &str, _r: &str, _on: bool) -> TResult<Routine> {
        Err(TransportError::Unsupported(Cap::RoutinesToggle))
    }
    fn routine_remove(&mut self, _id: &str, _r: &str) -> TResult<()> {
        Err(TransportError::Unsupported(Cap::RoutinesRemove))
    }
    fn usage(&mut self) -> TResult<Usage> {
        Err(TransportError::Unsupported(Cap::Usage))
    }
    fn files(&mut self, _id: &str) -> TResult<Vec<FileEntry>> {
        Err(TransportError::Unsupported(Cap::Files))
    }
    fn file_attach(&mut self, _id: &str, _path: &Path) -> TResult<FileEntry> {
        Err(TransportError::Unsupported(Cap::Files))
    }
    fn file_remove(&mut self, _id: &str, _name: &str) -> TResult<()> {
        Err(TransportError::Unsupported(Cap::Files))
    }
    fn file_read(&mut self, _id: &str, _f: &FileEntry) -> TResult<Vec<u8>> {
        Err(TransportError::Unsupported(Cap::Files))
    }
    /// P3, additive: the bytes plus the app's own sha256 of them (lowercase hex) when it can compute one; the
    /// download PASS compares it with the file written to disk.
    fn file_read_checked(&mut self, id: &str, f: &FileEntry) -> TResult<(Vec<u8>, Option<String>)> {
        self.file_read(id, f).map(|b| (b, None))
    }
    /// P3, additive: a file the bot sent in chat (transcript message `key`, `Message.file`): its bytes and the
    /// app's sha256 of them. 1.1.0: the direct attachment reads fail over the port (`read-abandoned`), so the
    /// debug port opens the app's own preview and reads the bytes it loaded.
    fn attachment_read(
        &mut self,
        _id: &str,
        _key: &str,
        _f: &FileRef,
    ) -> TResult<(Vec<u8>, Option<String>)> {
        Err(TransportError::Unsupported(Cap::Files))
    }
    fn create(&mut self, _name: &str, _description: Option<&str>) -> TResult<Bot> {
        Err(TransportError::Unsupported(Cap::Lifecycle))
    }
    fn rename(&mut self, _id: &str, _new: &str) -> TResult<()> {
        Err(TransportError::Unsupported(Cap::Lifecycle))
    }
    /// Floor 4 is rechecked here by every implementation: `exact_name` must equal the bot's name or id.
    fn delete(&mut self, _id: &str, _exact_name: &str) -> TResult<()> {
        Err(TransportError::Unsupported(Cap::Lifecycle))
    }
}
