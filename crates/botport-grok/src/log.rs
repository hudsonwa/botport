//! The action log: one JSON line per command in `~/.botport/grok-actions.jsonl` (0600). It records what was done,
//! never message text, port details or secret values (floors 3 and 5).
use crate::policy::floors::scrub;
use serde_json::json;
use std::path::PathBuf;

pub struct ActionLog {
    path: Option<PathBuf>,
}

impl ActionLog {
    pub fn new(path: Option<PathBuf>) -> Self {
        ActionLog { path }
    }
    pub fn off() -> Self {
        ActionLog { path: None }
    }
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    pub fn record(
        &self,
        at_ms: u64,
        cmd: &str,
        bot_id: Option<&str>,
        transport: &str,
        exit: u8,
        proof: &str,
    ) {
        let Some(p) = &self.path else { return };
        let line = json!({
            "at": crate::util::rfc3339(at_ms),
            "cmd": cmd,
            "bot_id": bot_id,
            "transport": transport,
            "exit": exit,
            "proof": scrub(proof),
        });
        // a failed log write never fails the command
        let _ = crate::util::append_private(p, &line.to_string());
    }
}

/// The turn ledger: one JSON line per paid call botport made (`ask`, `send`, and the asks inside `routines add|run`)
/// in `~/.botport/grok-turns.jsonl` (0600): when, which command, which caller tag and bot id. Never message text.
/// Reads (`read`, `routines list`, `usage`, ...) are never recorded. The weekly budget counts these lines.
#[derive(Default)]
pub struct TurnLedger {
    path: Option<PathBuf>,
}

/// One week in milliseconds: the budget counts the turns of the last 7 days.
pub const WEEK_MS: u64 = 7 * 86_400_000;

impl TurnLedger {
    pub fn new(path: Option<PathBuf>) -> Self {
        TurnLedger { path }
    }
    pub fn off() -> Self {
        TurnLedger { path: None }
    }
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Append one paid turn. An error means the budget cannot see this turn; callers warn.
    pub fn record(&self, at_ms: u64, cmd: &str, caller: &str, bot_id: &str) -> std::io::Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        let line = json!({"at_ms": at_ms, "cmd": cmd, "caller": scrub(caller), "bot_id": bot_id});
        crate::util::append_private(p, &line.to_string())
    }

    /// Paid turns recorded in the 7 days up to `now_ms` (0 when off or unreadable).
    pub fn this_week(&self, now_ms: u64) -> u64 {
        let Some(p) = &self.path else { return 0 };
        let Ok(s) = std::fs::read_to_string(p) else {
            return 0;
        };
        let from = now_ms.saturating_sub(WEEK_MS);
        s.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["at_ms"].as_u64())
            .filter(|t| *t > from && *t <= now_ms)
            .count() as u64
    }
}
