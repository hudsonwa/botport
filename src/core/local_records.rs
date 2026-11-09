//! The local reader interface's records (`botport-local/1`). Spec: docs/design/MIRROR.md section 6.2.
//!
//! Any program may write these normalised records into a `local-dir`, one folder per bot key; botport reads only them. The records carry names, times, counts and flags:
//! no message text, instructions, memories or user facts.
//!
//! ```text
//! <local-dir>/<bot-key>/activity.json    per-bot activity row
//! <local-dir>/<bot-key>/entries.jsonl    one entry record per line
//! <local-dir>/<bot-key>/routines.jsonl   one routine change per line
//! ```
#![warn(missing_docs)]

use super::util::{err, parse_utc_iso, read_confined_capped, Confined, Res};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Schema of an activity row.
pub const SCHEMA: &str = "botport-local/1";

/// A bot key as used in a path: `[a-z0-9][a-z0-9-]{0,63}`.
pub fn valid_bot_key(k: &str) -> Res<()> {
    let ok = !k.is_empty()
        && k.len() <= 64
        && k.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        err(format!(
            "invalid bot key {k:?}: 1-64 of a-z, 0-9 and '-', starting with a letter or digit"
        ))
    }
}

/// The per-bot activity row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// The reader's opaque id for the bot.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Newest activity, UTC `YYYY-MM-DDTHH:MM:SSZ`.
    pub last_activity: String,
    /// Unread entries.
    #[serde(default)]
    pub unread: u64,
    /// Whether the bot is waiting for the owner's reply.
    #[serde(default)]
    pub awaiting_reply: bool,
}

/// Who an entry is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// the owner
    User,
    /// this bot
    Bot,
    /// another bot, writing to this bot
    PeerToBot,
    /// this bot, writing to another bot
    BotToPeer,
    /// the platform
    System,
}

/// One entry (a message or event), flags only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// UTC time.
    pub time: String,
    /// Who it is from.
    pub speaker: Speaker,
    /// It asks something of the owner.
    #[serde(default)]
    pub open_ask: bool,
    /// The ask was answered.
    #[serde(default)]
    pub answered: bool,
}

/// What happened to a routine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutineAction {
    /// created
    Created,
    /// changed
    Updated,
    /// removed
    Deleted,
    /// paused
    Disabled,
}

/// One routine change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineEvent {
    /// The reader's opaque id for the routine.
    pub id: String,
    /// The routine's name.
    pub name: String,
    /// What happened.
    pub action: RoutineAction,
    /// UTC time.
    pub time: String,
}

/// Everything the reader wrote for one bot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BotRecords {
    /// The activity row, if present and valid.
    pub activity: Option<Activity>,
    /// Valid entries, oldest first.
    pub entries: Vec<Entry>,
    /// Valid routine events, oldest first.
    pub routines: Vec<RoutineEvent>,
    /// Lines skipped (unknown speaker or action, unreadable time, not JSON).
    pub skipped: usize,
}

impl BotRecords {
    /// Routines that exist now: the latest event per routine id is not `deleted`; (name, paused) by name.
    pub fn routine_names(&self) -> Vec<(String, bool)> {
        let mut last: std::collections::BTreeMap<&str, &RoutineEvent> = Default::default();
        for e in &self.routines {
            last.insert(&e.id, e);
        }
        let mut v: Vec<(String, bool)> = last
            .values()
            .filter(|e| e.action != RoutineAction::Deleted)
            .map(|e| (e.name.clone(), e.action == RoutineAction::Disabled))
            .collect();
        v.sort();
        v
    }

    /// Entries asking something of the owner and not answered.
    pub fn open_asks(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.open_ask && !e.answered)
    }
}

/// Whether records metadata is the running user's own and not writable by others (a reader creates its folder 0700
/// and files 0600); what someone else could have written is not read. Unix only: elsewhere nothing is checked.
fn private_md(m: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        super::util::my_uid() == Some(m.uid()) && m.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        true
    }
}

/// Test fixtures in folders owned by the running user, writable by nobody else: folders 0700 and files 0600
/// (whatever the umask), below `p`.
#[cfg(test)]
pub(crate) fn lock_down(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let md = std::fs::symlink_metadata(p).unwrap();
        if md.is_symlink() {
            return;
        }
        let mode = if md.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        if md.is_dir() {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                lock_down(&e.path());
            }
        }
    }
}

fn private_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && private_md(&m))
}

fn read_opt(root: &Path, dir: &Path, name: &str) -> Res<Option<Vec<u8>>> {
    read_opt_capped(root, dir, name, u64::MAX)
}

fn read_opt_capped(root: &Path, dir: &Path, name: &str, cap: u64) -> Res<Option<Vec<u8>>> {
    match read_confined_capped(root, dir, name, private_md, cap) {
        Ok(b) => Ok(Some(b)),
        Err(Confined::Missing) => Ok(None),
        // never name the file: a reader's file names stay out of errors and logs
        Err(_) => err("local records file refused (not a regular file inside the local dir, the running user's own and not writable by others)"),
    }
}

fn lines<T: serde::de::DeserializeOwned>(
    b: &[u8],
    ok: impl Fn(&T) -> bool,
    skipped: &mut usize,
) -> Vec<T> {
    let mut out = vec![];
    for l in String::from_utf8_lossy(b)
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        match serde_json::from_str::<T>(l) {
            Ok(x) if ok(&x) => out.push(x),
            _ => *skipped += 1,
        }
    }
    out
}

/// The local dir and the bot's folder in it, checked: the running user's own and not writable by others.
fn bot_dir(local_dir: &Path, bot_key: &str) -> Res<(PathBuf, PathBuf)> {
    valid_bot_key(bot_key)?;
    let root = local_dir
        .canonicalize()
        .map_err(|_| "local dir not found".to_string())?;
    let dir = root.join(bot_key);
    for d in [&root, &dir] {
        if std::fs::symlink_metadata(d).is_ok() && !private_dir(d) {
            return err(
                "local records folder refused (not the running user's own, or writable by others)",
            );
        }
    }
    Ok((root, dir))
}

/// The other side's digest file `name` in the bot's folder, with the same checks as the records, or `None` when there
/// is none; a file over `max` bytes is refused (nothing past `max` + 1 bytes is read).
pub fn read_digest(local_dir: &Path, bot_key: &str, name: &str, max: u64) -> Res<Option<Vec<u8>>> {
    let (root, dir) = bot_dir(local_dir, bot_key)?;
    match read_opt_capped(&root, &dir, name, max.saturating_add(1))? {
        Some(b) if b.len() as u64 > max => {
            err("the local digest file is larger than a digest can be: refused")
        }
        b => Ok(b),
    }
}

/// Read the records for `bot_key` under `local_dir` (missing files are empty).
pub fn read(local_dir: &Path, bot_key: &str) -> Res<BotRecords> {
    let (root, dir) = bot_dir(local_dir, bot_key)?;
    let mut r = BotRecords::default();
    if let Some(b) = read_opt(&root, &dir, "activity.json")? {
        match serde_json::from_slice::<Activity>(&b) {
            Ok(a) if a.schema == SCHEMA && parse_utc_iso(&a.last_activity).is_some() => {
                r.activity = Some(a)
            }
            _ => r.skipped += 1,
        }
    }
    if let Some(b) = read_opt(&root, &dir, "entries.jsonl")? {
        r.entries = lines(
            &b,
            |e: &Entry| parse_utc_iso(&e.time).is_some(),
            &mut r.skipped,
        );
        r.entries.sort_by(|a, b| a.time.cmp(&b.time));
    }
    if let Some(b) = read_opt(&root, &dir, "routines.jsonl")? {
        r.routines = lines(
            &b,
            |e: &RoutineEvent| parse_utc_iso(&e.time).is_some() && !e.name.trim().is_empty(),
            &mut r.skipped,
        );
        r.routines.sort_by(|a, b| a.time.cmp(&b.time));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        crate::core::util::test_dir(&format!("local-{tag}"))
    }

    #[test]
    fn bot_keys_are_checked() {
        for ok in ["demo-bot", "a", "0x", &"a".repeat(64)] {
            assert!(valid_bot_key(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "Demo", "a/b", "..", "a.b", "a b", &"a".repeat(65)] {
            assert!(valid_bot_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reads_invented_records_and_skips_bad_lines() {
        let d = tmp("read");
        let b = d.join("demo-bot");
        fs::create_dir_all(&b).unwrap();
        fs::write(
            b.join("activity.json"),
            r#"{"schema": "botport-local/1", "id": "x1", "name": "Demo Bot", "last_activity": "2000-01-04T00:00:00Z", "unread": 2, "awaiting_reply": true, "extra": "ignored"}"#,
        )
        .unwrap();
        fs::write(
            b.join("entries.jsonl"),
            [
                r#"{"time": "2000-01-04T00:00:00Z", "speaker": "bot", "open_ask": true}"#,
                r#"{"time": "2000-01-03T00:00:00Z", "speaker": "user"}"#,
                r#"{"time": "2000-01-03T12:00:00Z", "speaker": "peer_to_bot", "open_ask": true, "answered": true}"#,
                r#"{"time": "2000-01-03T12:00:00Z", "speaker": "someone"}"#,
                r#"{"time": "yesterday", "speaker": "bot"}"#,
                "not json",
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            b.join("routines.jsonl"),
            [
                r#"{"id": "r1", "name": "demo-routine", "action": "created", "time": "2000-01-01T00:00:00Z"}"#,
                r#"{"id": "r2", "name": "old-routine", "action": "created", "time": "2000-01-01T00:00:00Z"}"#,
                r#"{"id": "r2", "name": "old-routine", "action": "deleted", "time": "2000-01-02T00:00:00Z"}"#,
                r#"{"id": "r3", "name": "paused-routine", "action": "disabled", "time": "2000-01-02T00:00:00Z"}"#,
                r#"{"id": "r4", "name": "x", "action": "renamed", "time": "2000-01-02T00:00:00Z"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        lock_down(&d);
        let r = read(&d, "demo-bot").unwrap();
        assert_eq!(r.activity.as_ref().unwrap().unread, 2);
        assert_eq!(r.entries.len(), 3);
        assert_eq!(r.entries[0].speaker, Speaker::User, "sorted by time");
        assert_eq!(r.open_asks().count(), 1);
        assert_eq!(r.skipped, 4);
        assert_eq!(
            r.routine_names(),
            vec![
                ("demo-routine".to_string(), false),
                ("paused-routine".to_string(), true)
            ]
        );
        // a missing bot folder is empty, an invalid key is refused before any path is built
        assert_eq!(read(&d, "other-bot").unwrap(), BotRecords::default());
        assert!(read(&d, "../demo-bot").is_err());
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn records_others_could_write_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("modes");
        fs::create_dir_all(d.join("demo-bot")).unwrap();
        let f = d.join("demo-bot/entries.jsonl");
        fs::write(&f, "").unwrap();
        lock_down(&d);
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read(&d, "demo-bot").is_ok());
        fs::set_permissions(&f, fs::Permissions::from_mode(0o660)).unwrap();
        let e = read(&d, "demo-bot").unwrap_err();
        assert!(
            e.contains("writable by others") && !e.contains("entries"),
            "{e}"
        );
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(d.join("demo-bot"), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(read(&d, "demo-bot").is_err());
        fs::set_permissions(d.join("demo-bot"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_digest_is_read_with_the_records_checks_and_a_cap() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("digest");
        fs::create_dir_all(d.join("demo-bot")).unwrap();
        assert_eq!(
            read_digest(&d, "demo-bot", "digest-peer.md", 16).unwrap(),
            None
        );
        let f = d.join("demo-bot/digest-peer.md");
        fs::write(&f, "# Demo digest\n").unwrap();
        lock_down(&d);
        assert_eq!(
            read_digest(&d, "demo-bot", "digest-peer.md", 16)
                .unwrap()
                .unwrap(),
            b"# Demo digest\n"
        );
        assert!(
            read_digest(&d, "demo-bot", "digest-peer.md", 8).is_err(),
            "over the cap"
        );
        fs::set_permissions(&f, fs::Permissions::from_mode(0o620)).unwrap();
        assert!(
            read_digest(&d, "demo-bot", "digest-peer.md", 16).is_err(),
            "writable by others"
        );
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(d.join("demo-bot"), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            read_digest(&d, "demo-bot", "digest-peer.md", 16).is_err(),
            "folder writable by others"
        );
        fs::set_permissions(d.join("demo-bot"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn links_are_refused_without_naming_the_file() {
        let d = tmp("links");
        let outside = tmp("links-outside");
        fs::write(outside.join("entries.jsonl"), "{}").unwrap();
        fs::create_dir_all(d.join("demo-bot")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("entries.jsonl"),
            d.join("demo-bot/entries.jsonl"),
        )
        .unwrap();
        let e = read(&d, "demo-bot").unwrap_err();
        assert!(!e.contains("entries"), "{e}");
        fs::remove_dir_all(&d).ok();
        fs::remove_dir_all(&outside).ok();
    }
}
