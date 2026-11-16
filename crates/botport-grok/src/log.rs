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
