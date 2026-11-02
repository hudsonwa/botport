//! Persistent broker state: per-direction inbound queues with TTL, and guard
//! counters. Stored as one JSON file replaced by atomic rename (write temp,
//! fsync, rename) — small state, no native deps, trivially musl-friendly.

use crate::envelope::Envelope;
use crate::guard::Counters;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    pub env: Envelope,
    /// Unix seconds after which the message is dropped.
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub queues: BTreeMap<String, VecDeque<Queued>>,
    pub counters: Counters,
    /// Set when waking was stopped (e.g. the webhook answered 4xx).
    #[serde(default)]
    pub alert: Option<String>,
    /// Live selftest wakes: (local day, count) for `selftest_wakes_per_day`.
    #[serde(default)]
    pub selftest_wakes: (String, u64),
    /// Pending op approvals and seen op request ids.
    #[serde(default)]
    pub ops: crate::ops::OpBook,
}

/// Write `bytes` to `path` atomically: a unique 0600 temp file in the same
/// dir (pid + counter + nanos), fsync, rename.
pub fn save_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp.{}.{}.{nanos}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = std::path::PathBuf::from(tmp);
    let res = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

impl State {
    pub fn push(&mut self, direction: &str, env: Envelope, now: i64, ttl_secs: i64) {
        self.queues
            .entry(direction.to_string())
            .or_default()
            .push_back(Queued {
                env,
                expires_at: now + ttl_secs,
            });
    }

    /// Re-insert at the head of the queue (an undelivered message goes first again).
    pub fn push_front(&mut self, direction: &str, env: Envelope, now: i64, ttl_secs: i64) {
        self.queues
            .entry(direction.to_string())
            .or_default()
            .push_front(Queued {
                env,
                expires_at: now + ttl_secs,
            });
    }

    /// Drop expired entries everywhere; returns how many were dropped.
    pub fn purge_expired(&mut self, now: i64) -> usize {
        let mut n = 0;
        for q in self.queues.values_mut() {
            let before = q.len();
            q.retain(|m| m.expires_at > now);
            n += before - q.len();
        }
        n
    }

    /// Oldest unexpired message for `direction`.
    pub fn pop(&mut self, direction: &str, now: i64) -> Option<Envelope> {
        let q = self.queues.get_mut(direction)?;
        while let Some(m) = q.pop_front() {
            if m.expires_at > now {
                return Some(m.env);
            }
        }
        None
    }

    pub fn len(&self, direction: &str) -> usize {
        self.queues.get(direction).map_or(0, |q| q.len())
    }

    pub fn load(path: &Path) -> io::Result<State> {
        match fs::read(path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        save_atomic(path, &serde_json::to_vec(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Kind;

    #[test]
    fn ttl_and_order() {
        let mut s = State::default();
        s.push(
            "a->b",
            Envelope::new("c", "a", "b", Kind::Msg, "1"),
            100,
            10,
        );
        s.push(
            "a->b",
            Envelope::new("c", "a", "b", Kind::Msg, "2"),
            105,
            10,
        );
        assert_eq!(s.pop("a->b", 112).unwrap().body, "2");
        assert!(s.pop("a->b", 112).is_none());
        s.push("x", Envelope::new("c", "a", "b", Kind::Msg, "3"), 0, 5);
        assert_eq!(s.purge_expired(5), 1);
        assert_eq!(s.len("x"), 0);
    }

    #[test]
    fn push_front_goes_first() {
        let mut s = State::default();
        s.push("in", Envelope::new("c", "a", "b", Kind::Msg, "2"), 0, 10);
        s.push_front("in", Envelope::new("c", "a", "b", Kind::Msg, "1"), 0, 10);
        assert_eq!(s.pop("in", 1).unwrap().body, "1");
        assert_eq!(s.pop("in", 1).unwrap().body, "2");
    }

    #[test]
    fn save_load_roundtrip() {
        let d = std::env::temp_dir().join(format!("g2h-store-{}", std::process::id()));
        let p = d.join("state.json");
        assert_eq!(State::load(&p).unwrap(), State::default());
        let mut s = State::default();
        s.push("a->b", Envelope::new("c", "a", "b", Kind::Msg, "hi"), 0, 60);
        s.counters.messages = 7;
        s.save(&p).unwrap();
        assert_eq!(State::load(&p).unwrap(), s);
        fs::remove_file(&p).unwrap();
        fs::remove_dir(&d).unwrap();
    }

    #[test]
    fn save_is_0600_with_unique_temp() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("g2h-store-perm-{}", std::process::id()));
        let p = d.join("state.json");
        // a stale fixed-name temp must not be reused or block the save
        fs::create_dir_all(&d).unwrap();
        let stale = d.join("state.json.tmp");
        fs::create_dir_all(&stale).unwrap();
        State::default().save(&p).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir(&stale).unwrap();
        fs::remove_file(&p).unwrap();
        fs::remove_dir(&d).unwrap();
    }
}
