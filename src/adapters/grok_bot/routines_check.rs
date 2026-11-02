//! EXPERIMENTAL `--check-routines <path|auto>`: warn about routines the Grok desktop app's local cache knows
//! (its automation-changed events) that the export left out. Opt-in, read-only, never fails the command.
//!
//! What is read (docs/REFERENCE.md "Routine check"): only regular files (never symlinks) named `<base32>.blob` whose
//! base32 key names a `transcript.replicas.*` slice: the user's local conversation transcripts, read in full. A file
//! PATH must pass the same name rule or nothing is opened; a directory (or `auto`, the app's
//! `sand-client-persistence` directory) is listed and only its matching files are read. Anything else (never
//! `sand-secrets.json`, cookies, webhook keys or any other file) is not opened. Only routine names are taken from the
//! events; nothing is copied, and file names (which carry the account key) are never printed.
//!
//! Assumed format (not confirmed against a real cache): each file is JSON or JSON lines; any object whose `type`,
//! `event` or `kind` is `automation-changed` (also `automation_changed`, `automationChanged`) names a routine at
//! `automation.name`, `payload.name` or `name`. A change with `deleted: true` or `action`/`change` of
//! `deleted`/`removed` forgets that routine.
use super::export_json::slug;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const APP_DIR: &str = "Grok Bot";
const CACHE_DIR: &str = "sand-client-persistence";
const SLICE: &str = ".transcript.replicas.";
const EVENT_TYPES: [&str; 3] = [
    "automation-changed",
    "automation_changed",
    "automationChanged",
];

/// The result: summary lines (WARN-prefixed when something needs a look) and a JSON block for the report.
pub struct Check {
    pub lines: Vec<String>,
    pub json: Value,
}

/// The per-OS default cache directory (`auto`), from the Electron userData convention.
pub fn default_dir() -> Option<PathBuf> {
    let env = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support"))
    } else if cfg!(windows) {
        env("APPDATA")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    }?;
    Some(base.join(APP_DIR).join(CACHE_DIR))
}

/// RFC 4648 base32 (any case, no padding) to text; None when it isn't base32.
fn base32(s: &str) -> Option<String> {
    let (mut bits, mut acc, mut out) = (0u32, 0u32, vec![]);
    for c in s.trim_end_matches('=').chars() {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u32 - 'A' as u32,
            c @ '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A regular file (not a symlink) named `<base32>.blob` whose key names a transcript-replica slice.
fn is_replica(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
        && p.extension().is_some_and(|x| x == "blob")
        && p.file_stem()
            .and_then(|s| base32(&s.to_string_lossy()))
            .is_some_and(|k| k.contains(SLICE))
}

/// The files the check may open: the file itself, or the transcript-replica blobs in a cache directory. Both go
/// through `is_replica`; Err (nothing opened) for a symlink or a file that fails it.
fn cache_files(target: &Path) -> Result<Vec<PathBuf>, &'static str> {
    let meta = std::fs::symlink_metadata(target).map_err(|_| "no Grok desktop cache found")?;
    if meta.file_type().is_symlink() {
        return Err("the path is a symlink; nothing was opened");
    }
    if meta.is_file() {
        return match is_replica(target) {
            true => Ok(vec![target.to_path_buf()]),
            false => Err("not a Grok transcript-replica file; nothing was opened"),
        };
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(target)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_replica(p))
        .collect();
    files.sort();
    Ok(files)
}

fn is_event(o: &serde_json::Map<String, Value>) -> bool {
    ["type", "event", "kind"].iter().any(|k| {
        o.get(*k)
            .and_then(Value::as_str)
            .is_some_and(|t| EVENT_TYPES.contains(&t))
    })
}

fn deleted(o: &serde_json::Map<String, Value>) -> bool {
    let removal = |v: &Value| matches!(v.as_str(), Some("deleted" | "removed"));
    o.get("deleted") == Some(&json!(true))
        || o.get("action").is_some_and(removal)
        || o.get("change").is_some_and(removal)
}

/// Walk one document; apply automation-changed events in order (slug -> display name).
fn collect(v: &Value, names: &mut BTreeMap<String, String>) {
    match v {
        Value::Object(o) => {
            if is_event(o) {
                let name = [&o.get("automation"), &o.get("payload")]
                    .iter()
                    .filter_map(|x| x.and_then(|x| x.get("name")))
                    .chain(o.get("name"))
                    .find_map(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                if let Some(n) = name {
                    let inner = o.get("automation").and_then(Value::as_object);
                    if deleted(o) || inner.is_some_and(deleted) {
                        names.remove(&slug(n));
                    } else {
                        names.insert(slug(n), n.to_string());
                    }
                }
                return;
            }
            o.values().for_each(|c| collect(c, names));
        }
        Value::Array(a) => a.iter().for_each(|c| collect(c, names)),
        _ => {}
    }
}

/// Parse one cache file as JSON, else as JSON lines; false when neither works.
fn read_file(p: &Path, names: &mut BTreeMap<String, String>) -> bool {
    let Ok(raw) = std::fs::read(p) else {
        return false;
    };
    let raw = String::from_utf8_lossy(&raw);
    if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
        collect(&v, names);
        return true;
    }
    let docs: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l.trim()))
        .collect::<Result<_, _>>()
        .unwrap_or_default();
    docs.iter().for_each(|d| collect(d, names));
    !docs.is_empty()
}

/// The one-line note for a check that did not run.
pub fn skip_line(why: &str) -> String {
    format!("routine check (experimental): skipped ({why})")
}

/// Compare the cache's routine names with the export's (matched by slug, so case and punctuation don't matter).
pub fn check(spec: &str, export_names: &[String]) -> Check {
    let target = if spec == "auto" {
        default_dir()
    } else {
        Some(PathBuf::from(spec))
    };
    let skipped = |why: String| Check {
        lines: vec![skip_line(&why)],
        json: json!({"experimental": true, "skipped": why}),
    };
    let Some(target) = target else {
        return skipped("no Grok desktop cache found".into());
    };
    let files = match cache_files(&target) {
        Ok(f) => f,
        Err(why) => return skipped(why.into()),
    };
    if files.is_empty() {
        return skipped("the cache holds no automation event files".into());
    }
    let mut names = BTreeMap::new();
    let mut bad = 0;
    for f in &files {
        if !read_file(f, &mut names) {
            bad += 1;
        }
    }
    let have: Vec<String> = export_names.iter().map(|n| slug(n)).collect();
    let missing: Vec<&String> = names
        .iter()
        .filter(|(k, _)| !have.contains(k))
        .map(|(_, v)| v)
        .collect();
    let mut lines = vec![];
    if bad > 0 {
        lines.push(format!(
            "WARN routine check: {bad} of {} cache file(s) could not be read as JSON (skipped)",
            files.len()
        ));
    }
    if missing.is_empty() {
        lines.push(format!(
            "routine check (experimental): all {} routine(s) the desktop cache knows are in the export",
            names.len()
        ));
    }
    for m in &missing {
        lines.push(format!(
            "WARN routine {m:?} is in the Grok desktop cache but not in the export (re-run the export prompt?)"
        ));
    }
    Check {
        lines,
        json: json!({"experimental": true, "files": files.len(), "unreadable": bad,
                     "cache_routines": names.len(), "missing": missing}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // synthetic fixtures, named like replica slices of a made-up account
    const REPLICA: &str = "tests/fixtures/routines-cache/onqw4zbomnwgszlooqxhg3djmnss4yldmnxxk3tufztgs6duovzgkltuojqw443dojuxa5boojsxa3djmnqxgltsge.blob";
    const MALFORMED: &str = "tests/fixtures/routines-cache/onqw4zbomnwgszlooqxhg3djmnss4yldmnxxk3tufztgs6duovzgkltuojqw443dojuxa5boojsxa3djmnqxgltsgi.blob";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("botport-rc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn b32(s: &str) -> String {
        const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
        let (mut out, mut acc, mut bits) = (String::new(), 0u32, 0);
        for b in s.bytes() {
            acc = (acc << 8) | b as u32;
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(A[((acc >> bits) & 31) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(A[((acc << (5 - bits)) & 31) as usize] as char);
        }
        out
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn base32_round_trip() {
        let k = "sand.client.slice.account.x.transcript.replicas.abc";
        assert_eq!(base32(&b32(k)).as_deref(), Some(k));
    }

    #[test]
    fn all_present() {
        let c = check(REPLICA, &names(&["Sample report", "weekly-review"]));
        assert_eq!(c.lines.len(), 1, "{:?}", c.lines);
        assert!(c.lines[0].contains("all 2 routine(s)"));
    }

    #[test]
    fn one_missing_is_named() {
        let c = check(REPLICA, &names(&["Sample Report"]));
        assert_eq!(c.json["missing"], json!(["Weekly Review"]));
        assert!(c
            .lines
            .iter()
            .any(|l| l.starts_with("WARN") && l.contains("Weekly Review")));
        // the deleted routine is not reported
        assert!(!c.lines.iter().any(|l| l.contains("Old Digest")));
    }

    #[test]
    fn absent_cache_is_a_note() {
        let c = check("tests/fixtures/routines-cache/nope", &names(&[]));
        assert!(c.lines[0].contains("skipped (no Grok desktop cache found)"));
        assert!(!c.lines[0].starts_with("WARN"));
    }

    #[test]
    fn malformed_cache_warns() {
        let c = check(MALFORMED, &names(&[]));
        assert!(c.lines[0].starts_with("WARN") && c.lines[0].contains("1 of 1"));
    }

    #[test]
    fn directory_reads_only_replica_blobs() {
        let d = tmp("dir");
        let ev = r#"{"type":"automation-changed","automation":{"name":"Inbox Sweep"}}"#;
        let replica = format!(
            "{}.blob",
            b32("sand.client.slice.account.x.transcript.replicas.r1")
        );
        std::fs::write(d.join(replica), format!("{ev}\n{ev}\n")).unwrap();
        // same event in a file that is not a replica slice (and a secrets-like name): never read
        let other = format!(
            "{}.blob",
            b32("sand.client.slice.account.x.roster.last-roster")
        );
        let ev2 = r#"{"type":"automation-changed","name":"Should Not Appear"}"#;
        std::fs::write(d.join(other), ev2).unwrap();
        std::fs::write(d.join("sand-secrets.json"), ev2).unwrap();
        let c = check(d.to_str().unwrap(), &names(&[]));
        assert_eq!(c.json["missing"], json!(["Inbox Sweep"]));
        assert_eq!(c.json["files"], json!(1));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn ev(name: &str) -> String {
        format!(r#"{{"type":"automation-changed","name":"{name}"}}"#)
    }

    #[test]
    fn file_mode_refuses_non_replica_files() {
        let d = tmp("filemode");
        let webhook = format!(
            "{}.blob",
            b32("sand.client.slice.account.x.automation-webhook-keys")
        );
        for f in [
            "sand-secrets.json",
            "Cookies",
            "Cookies-journal",
            &webhook,
            "notes.blob",
        ] {
            std::fs::write(d.join(f), ev("FROM-SECRET")).unwrap();
            let c = check(d.join(f).to_str().unwrap(), &names(&[]));
            assert_eq!(c.lines.len(), 1, "{f}: {:?}", c.lines);
            assert!(
                c.lines[0].contains("skipped (not a Grok transcript-replica file"),
                "{f}: {:?}",
                c.lines
            );
            assert!(!c.lines[0].contains("FROM-SECRET") && c.json["missing"].is_null());
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        let d = tmp("symlink");
        let app = d.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(d.join("sand-secrets.json"), ev("FROM-SECRETS-FILE")).unwrap();
        let replica = app.join(format!(
            "{}.blob",
            b32("sand.client.slice.account.x.transcript.replicas.r9")
        ));
        std::os::unix::fs::symlink(d.join("sand-secrets.json"), &replica).unwrap();
        // file mode: a replica-named symlink is refused
        let c = check(replica.to_str().unwrap(), &names(&[]));
        assert!(
            c.lines[0].contains("skipped (the path is a symlink"),
            "{:?}",
            c.lines
        );
        // dir mode: the symlink is skipped, so no file is left
        let c = check(app.to_str().unwrap(), &names(&[]));
        assert!(
            c.lines[0].contains("skipped (the cache holds no"),
            "{:?}",
            c.lines
        );
        // a symlinked cache directory is refused too
        let link = d.join("linked");
        std::os::unix::fs::symlink(&app, &link).unwrap();
        let c = check(link.to_str().unwrap(), &names(&[]));
        assert!(
            c.lines[0].contains("skipped (the path is a symlink"),
            "{:?}",
            c.lines
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
