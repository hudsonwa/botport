//! The Grok desktop app's local cache, read-only: the list of bots (id, name, description) and the routine names the
//! app saw being created. This is all the cache holds about a bot: its instructions, skills, routine schedules and
//! connectors live only in the bot's cloud computer.
//!
//! What is opened (and nothing else): regular files (never symlinks) in `<app dir>/sand-client-persistence/` named
//! `<base32 key>.blob` whose key is `sand.client.slice.account.<account>.roster.last-roster` or
//! `...transcript.replicas.<bot id>`, each at most [`MAX_BLOB`] bytes. Login material in the same folder
//! (`sand-secrets.json`, cookies) never matches and is never opened. Each blob is JSON `{"schemaVersion": n, "value": ..}`;
//! the roster's value is `{"rows": [{"id", "name", "description", "isGroup", "isRemovedFromSidebar", ..}]}`. The
//! same layout is read by the official agent-importers plugin (MIT, Nous Research), which this follows.
//!
//! Everything read here is untrusted text written by the app: names and descriptions are stripped of control
//! characters and capped, ids must be plain identifiers, and a file that does not parse is skipped (the cache is a hint).
use super::routines_check::{base32, collect};
use crate::core::util::{err, Res};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The cache folder inside the app's userData directory.
pub const CACHE_DIR: &str = "sand-client-persistence";
const PREFIX: &str = "sand.client.slice.account.";
const ROSTER: &str = "roster.last-roster";
const REPLICAS: &str = "transcript.replicas.";
/// The largest blob opened (bytes); a bigger one is skipped unread.
pub const MAX_BLOB: u64 = 16 << 20;
/// Caps on what is kept from a row.
pub const MAX_NAME: usize = 200;
/// Cap on a description (characters).
pub const MAX_DESCRIPTION: usize = 4000;
/// Cap on the number of bots listed.
pub const MAX_BOTS: usize = 1000;
const MAX_ROUTINES: usize = 200;

/// One bot as the cache knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheBot {
    /// The app's id for the bot (a plain identifier, checked).
    pub id: String,
    /// The bot's name (cleaned, capped).
    pub name: String,
    /// The text the app shows for the bot (cleaned, capped; empty when none).
    pub description: String,
    /// Names of routines the app saw created and not deleted (only for bots opened in the app; may be incomplete).
    pub routines: Vec<String>,
}

/// What the cache holds.
#[derive(Debug, Clone, Default)]
pub struct Cache {
    /// The cache folder exists.
    pub found: bool,
    /// The bots, in roster order (groups, hidden and removed rows left out).
    pub bots: Vec<CacheBot>,
    /// Files skipped because they could not be read or parsed, or past the read budget (count only; never named:
    /// names carry the account).
    pub unreadable: usize,
    /// The accounts the cache holds, by opaque label.
    pub accounts: Vec<String>,
}

/// The app's default userData directory for this OS (`Grok Bot` under Application Support / XDG config / APPDATA).
pub fn default_app_dir() -> Option<PathBuf> {
    super::routines_check::default_dir().and_then(|d| d.parent().map(Path::to_path_buf))
}

/// The slice a cache file holds, from its file name: `Some((account, slice))` for a `<base32>.blob` regular file.
fn slice_of(p: &Path) -> Option<(String, String)> {
    if p.extension().is_none_or(|x| x != "blob") {
        return None;
    }
    let key = base32(&p.file_stem()?.to_string_lossy())?;
    let rest = key.strip_prefix(PREFIX)?;
    let (account, slice) = rest.split_once('.')?;
    Some((account.to_string(), slice.to_string()))
}

/// The cache files botport may open, with their slice: regular files only, roster and transcript-replica slices only.
pub fn files(app_dir: &Path) -> Vec<(PathBuf, String, String)> {
    let dir = app_dir.join(CACHE_DIR);
    let mut out: Vec<(PathBuf, String, String)> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let p = e.path();
            let (account, slice) = slice_of(&p)?;
            (slice == ROSTER || slice.starts_with(REPLICAS)).then_some((p, account, slice))
        })
        .collect();
    out.sort();
    out
}

/// Read one blob's `value` (None: missing, too big, a symlink, not a regular file, or not JSON). The file is opened
/// once without following a link and checked through that handle, so it cannot be swapped between check and read.
fn value(p: &Path) -> Option<Value> {
    use std::io::Read;
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut o,
        rustix::fs::OFlags::NOFOLLOW.bits() as i32,
    );
    let f = o.open(p).ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() || md.len() > MAX_BLOB {
        return None;
    }
    let mut raw = Vec::new();
    f.take(MAX_BLOB + 1).read_to_end(&mut raw).ok()?;
    if raw.len() as u64 > MAX_BLOB {
        return None;
    }
    let v: Value = serde_json::from_slice(&raw).ok()?;
    v.get("value").cloned()
}

/// Text from the app, made safe to keep: control characters dropped (newlines and tabs kept when `multiline`),
/// trimmed, capped at `cap` characters.
pub fn clean(t: &str, cap: usize, multiline: bool) -> String {
    let t: String = t
        .chars()
        .filter(|c| !c.is_control() || (multiline && (*c == '\n' || *c == '\t')))
        .filter(|c| !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}'))
        .take(cap)
        .collect();
    t.trim().to_string()
}

/// A bot id: 1-128 of `A-Z a-z 0-9 . _ : -`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

fn flag(row: &Value, k: &str) -> bool {
    row.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Which bots to read: the account (by its opaque label, see [`account_label`]; required when the cache holds more than
/// one) and which bots' routine names to look up (by id and name). Only those bots' transcript files are opened.
pub struct Select<'a> {
    /// The account's label, when the owner chose one.
    pub account: Option<&'a str>,
    /// The per-install secret the labels are keyed with (see [`account_label`]); without one the accounts are
    /// labelled by position ("account-1", "account-2", in the order of their ids).
    pub salt: Option<&'a [u8]>,
    /// Whether a bot's transcript is read for its routine names.
    pub routines_of: &'a dyn Fn(&str, &str) -> bool,
}

/// Every bot, every routine name (tests and the demo).
pub fn read(app_dir: &Path) -> Res<Cache> {
    read_select(
        app_dir,
        &Select {
            account: None,
            salt: None,
            routines_of: &|_, _| true,
        },
    )
}

/// An account's label: the first 8 hex characters of an HMAC of its id under a random per-install secret, so the label
/// can be neither looked up from a guessed id nor matched across installs. The id itself (it can name the login
/// provider and user) is never shown.
pub fn account_label(salt: &[u8], account: &str) -> String {
    crate::core::auth::hmac_sha256(salt, account.as_bytes())[..8].to_string()
}

/// The most transcript files and bytes one read opens.
const MAX_REPLICA_FILES: usize = 200;
const MAX_REPLICA_BYTES: u64 = 64 << 20;

/// Read the cache under `app_dir` (the app's userData directory). Never fails for a missing or odd cache: it reports
/// `found: false` or skips what it cannot read. Err for a cache path that is a symlink, and for a cache holding more
/// than one account when none was chosen (botport never guesses which account is the owner's).
pub fn read_select(app_dir: &Path, sel: &Select) -> Res<Cache> {
    let dir = app_dir.join(CACHE_DIR);
    match fs::symlink_metadata(&dir) {
        Err(_) => return Ok(Cache::default()),
        Ok(m) if m.file_type().is_symlink() => {
            return err("the Grok app's cache folder is a symlink; nothing was opened")
        }
        Ok(m) if !m.is_dir() => return Ok(Cache::default()),
        Ok(_) => {}
    }
    let mut cache = Cache {
        found: true,
        ..Cache::default()
    };
    let files = files(app_dir);
    let rosters: Vec<&(PathBuf, String, String)> =
        files.iter().filter(|(_, _, s)| s == ROSTER).collect();
    // without a per-install secret, the labels follow the account ids' order
    let mut ids: Vec<&String> = rosters.iter().map(|(_, a, _)| a).collect();
    ids.sort();
    let label = |a: &str| match sel.salt {
        Some(k) => account_label(k, a),
        None => format!(
            "account-{}",
            ids.iter()
                .position(|x| x.as_str() == a)
                .map_or(0, |i| i + 1)
        ),
    };
    cache.accounts = rosters.iter().map(|(_, a, _)| label(a)).collect();
    let roster = match (sel.account, rosters.len()) {
        (_, 0) => return Ok(cache),
        (None, 1) => rosters[0],
        (None, _) => {
            return err(format!(
                "the Grok app's cache holds {} accounts ({}): choose one with --account LABEL",
                rosters.len(),
                cache.accounts.join(", ")
            ))
        }
        (Some(l), _) => *rosters
            .iter()
            .find(|(_, a, _)| label(a) == l)
            .ok_or_else(|| {
                format!(
                    "no account {l:?} in the Grok app's cache (accounts: {})",
                    cache.accounts.join(", ")
                )
            })?,
    };
    let account = roster.1.clone();
    let Some(v) = value(&roster.0) else {
        cache.unreadable += 1;
        return Ok(cache);
    };
    let rows = v
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for row in rows.iter().filter(|r| r.is_object()) {
        if flag(row, "isGroup")
            || flag(row, "isRemovedFromSidebar")
            || flag(row, "isHiddenFromSidebar")
        {
            continue;
        }
        let id = match row.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => continue,
        };
        let name = clean(
            row.get("name").and_then(Value::as_str).unwrap_or(""),
            MAX_NAME,
            false,
        );
        if !valid_id(&id) || name.is_empty() || cache.bots.iter().any(|b| b.id == id) {
            continue;
        }
        let description = clean(
            row.get("description").and_then(Value::as_str).unwrap_or(""),
            MAX_DESCRIPTION,
            true,
        );
        cache.bots.push(CacheBot {
            id,
            name,
            description,
            routines: vec![],
        });
        if cache.bots.len() >= MAX_BOTS {
            break;
        }
    }
    // the bots are chosen first; then only their transcripts (of this account) are opened, within a budget
    let wanted: BTreeMap<String, usize> = cache
        .bots
        .iter()
        .enumerate()
        .filter(|(_, b)| (sel.routines_of)(&b.id, &b.name))
        .map(|(i, b)| (b.id.clone(), i))
        .collect();
    let (mut n, mut bytes) = (0usize, 0u64);
    for (p, _, slice) in files
        .iter()
        .filter(|(_, a, s)| s.starts_with(REPLICAS) && *a == account)
    {
        let id = percent_decode(&slice[REPLICAS.len()..]);
        let Some(&i) = wanted.get(&id) else { continue };
        let size = fs::symlink_metadata(p).map_or(0, |m| m.len());
        if n >= MAX_REPLICA_FILES || bytes.saturating_add(size) > MAX_REPLICA_BYTES {
            cache.unreadable += 1;
            continue;
        }
        n += 1;
        bytes = bytes.saturating_add(size);
        let mut names = BTreeMap::new();
        match value(p) {
            Some(v) => collect(&v, &mut names),
            None => {
                cache.unreadable += 1;
                continue;
            }
        }
        cache.bots[i].routines = names
            .values()
            .map(|n| clean(n, MAX_NAME, false))
            .filter(|n| !n.is_empty())
            .take(MAX_ROUTINES)
            .collect();
    }
    Ok(cache)
}

/// `%XX` decoding of a slice key part (the app URL-encodes ids); invalid escapes are kept as they are.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Shell-style glob on a name, case-insensitive: `*` any run, `?` one character. Linear-time matching (no
/// backtracking blow-up); a pattern over 200 characters matches nothing.
pub fn glob(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    if p.len() > 200 {
        return false;
    }
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || (p[i] != '*' && p[i] == n[j])) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

/// Whether a name passes the `--only` globs (no globs: every name).
pub fn wanted(only: &[String], name: &str) -> bool {
    only.is_empty() || only.iter().any(|g| glob(g, name))
}

/// Write a fixture file as it is (0600): a new temporary file with a random name (never an existing file or link),
/// then renamed over the target; removed again on failure. The demo plants an obviously fake key on purpose, so this
/// skips the write guard that would refuse it.
fn write_fixture_file(p: &Path, data: &[u8]) -> Res<()> {
    let dir = p.parent().ok_or("fixture path has no folder")?;
    let tmp = dir.join(format!(
        ".fixture-{}.tmp",
        crate::adapters::mirror::pack::try_random_id()?
    ));
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        std::os::unix::fs::OpenOptionsExt::custom_flags(
            &mut o,
            rustix::fs::OFlags::NOFOLLOW.bits() as i32,
        );
    }
    let r = o
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, data))
        .and_then(|_| fs::rename(&tmp, p));
    if let Err(e) = r {
        let _ = fs::remove_file(&tmp);
        return err(format!("fixture not written: {e}"));
    }
    Ok(())
}

/// Test and demo helper: write a cache folder holding `bots` (and their routine names) under `app_dir`, in the layout
/// [`read`] expects. The account id is invented.
pub fn write_fixture(app_dir: &Path, bots: &[CacheBot]) -> Res<()> {
    let dir = app_dir.join(CACHE_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let key = |slice: &str| format!("{PREFIX}demo-account.{slice}");
    let rows: Vec<Value> = bots
        .iter()
        .map(|b| serde_json::json!({"id": b.id, "name": b.name, "description": b.description, "isGroup": false}))
        .collect();
    let roster = serde_json::json!({"schemaVersion": 4, "value": {"rows": rows}});
    write_fixture_file(
        &dir.join(format!("{}.blob", base32_encode(&key(ROSTER)))),
        roster.to_string().as_bytes(),
    )?;
    for b in bots.iter().filter(|b| !b.routines.is_empty()) {
        let entries: Vec<Value> = b
            .routines
            .iter()
            .map(|r| serde_json::json!({"kind": "event", "event": {"type": "automation-changed", "action": "created", "automationId": format!("auto-{}", r.len()), "automationName": r}}))
            .collect();
        let doc = serde_json::json!({"schemaVersion": 1, "value": {"entries": entries}});
        write_fixture_file(
            &dir.join(format!(
                "{}.blob",
                base32_encode(&key(&format!("{REPLICAS}{}", b.id)))
            )),
            doc.to_string().as_bytes(),
        )?;
    }
    Ok(())
}

/// RFC 4648 base32, lowercase, no padding (the app's file-name form).
pub fn base32_encode(s: &str) -> String {
    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let (mut acc, mut bits, mut out) = (0u32, 0u32, String::new());
    for &b in s.as_bytes() {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(A[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(A[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        crate::core::util::test_dir("cache")
    }

    fn bot(id: &str, name: &str, desc: &str, routines: &[&str]) -> CacheBot {
        CacheBot {
            id: id.into(),
            name: name.into(),
            description: desc.into(),
            routines: routines.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn base32_round_trip() {
        let k = "sand.client.slice.account.demo-account.roster.last-roster";
        assert_eq!(base32(&base32_encode(k)).as_deref(), Some(k));
    }

    #[test]
    fn reads_the_roster_and_routine_names() {
        let d = tmp();
        let bots = [
            bot(
                "b-acme-0001",
                "Acme Helper",
                "Answers questions about Acme widgets.",
                &["Morning summary"],
            ),
            bot("b-recipe-0002", "Recipe Planner", "", &[]),
        ];
        write_fixture(&d, &bots).unwrap();
        // login material next to it is never opened, even when it is not JSON
        fs::write(d.join(CACHE_DIR).join("sand-secrets.json"), b"\x00not json").unwrap();
        let c = read(&d).unwrap();
        assert!(c.found);
        assert_eq!(c.bots, bots);
        assert_eq!(c.unreadable, 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn skips_groups_removed_rows_bad_ids_and_cleans_text() {
        let d = tmp();
        write_fixture(&d, &[]).unwrap();
        let rows = serde_json::json!({"schemaVersion": 4, "value": {"rows": [
            {"id": "g1", "name": "Team", "isGroup": true},
            {"id": "r1", "name": "Gone", "isRemovedFromSidebar": true},
            {"id": "bad id/..", "name": "Bad"},
            {"id": "b1", "name": "Acme\u{1b}[31m Helper\u{202e}", "description": "line one\nline two\u{7}"},
            {"id": "b2", "name": "   "}
        ]}});
        let f = d.join(CACHE_DIR).join(format!(
            "{}.blob",
            base32_encode(&format!("{PREFIX}demo-account.{ROSTER}"))
        ));
        fs::write(&f, rows.to_string()).unwrap();
        let c = read(&d).unwrap();
        assert_eq!(c.bots.len(), 1);
        assert_eq!(c.bots[0].name, "Acme[31m Helper");
        assert_eq!(c.bots[0].description, "line one\nline two");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_missing_cache_is_not_an_error() {
        let d = tmp();
        let c = read(&d.join("nope")).unwrap();
        assert!(!c.found && c.bots.is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deleted_routines_are_forgotten() {
        let d = tmp();
        write_fixture(&d, &[bot("b1", "Acme Helper", "", &[])]).unwrap();
        let doc = serde_json::json!({"schemaVersion": 1, "value": {"entries": [
            {"kind": "event", "event": {"type": "automation-changed", "action": "created", "automationId": "a1", "automationName": "Tidy the inbox"}},
            {"kind": "event", "event": {"type": "automation-changed", "action": "created", "automationId": "a2", "automationName": "Old one"}},
            {"kind": "event", "event": {"type": "automation-changed", "action": "deleted", "automationId": "a2", "automationName": "Old one"}}
        ]}});
        let f = d.join(CACHE_DIR).join(format!(
            "{}.blob",
            base32_encode(&format!("{PREFIX}demo-account.{REPLICAS}b1"))
        ));
        fs::write(&f, doc.to_string()).unwrap();
        assert_eq!(read(&d).unwrap().bots[0].routines, ["Tidy the inbox"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn two_accounts_need_a_choice_and_never_show_the_account_id() {
        let d = tmp();
        write_fixture(&d, &[bot("b1", "Acme Helper", "", &[])]).unwrap();
        let rows = serde_json::json!({"schemaVersion": 4, "value": {"rows": [{"id": "b2", "name": "Recipe Planner"}]}});
        let f = d.join(CACHE_DIR).join(format!(
            "{}.blob",
            base32_encode(&format!("{PREFIX}second-login.{ROSTER}"))
        ));
        fs::write(&f, rows.to_string()).unwrap();
        let e = read(&d).unwrap_err();
        assert!(
            e.contains("--account") && !e.contains("second-login") && !e.contains("demo-account"),
            "{e}"
        );
        // a random per-install secret: labels differ across installs and cannot be looked up from a guessed id
        let k1 = crate::core::auth::new_key().unwrap();
        let k2 = crate::core::auth::new_key().unwrap();
        let l = account_label(k1.as_bytes(), "second-login");
        assert_ne!(l, account_label(k2.as_bytes(), "second-login"));
        // without a secret, accounts are labelled by position only
        assert!(e.contains("account-1") && e.contains("account-2"), "{e}");
        let c = read_select(
            &d,
            &Select {
                account: Some(&l),
                salt: Some(k1.as_bytes()),
                routines_of: &|_, _| true,
            },
        )
        .unwrap();
        assert_eq!(c.bots[0].name, "Recipe Planner");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn transcripts_of_another_account_are_ignored() {
        let d = tmp();
        write_fixture(&d, &[bot("b1", "Acme Helper", "", &[])]).unwrap();
        let doc = serde_json::json!({"schemaVersion": 1, "value": {"entries": [
            {"kind": "event", "event": {"type": "automation-changed", "action": "created", "automationName": "Elsewhere"}}]}});
        let f = d.join(CACHE_DIR).join(format!(
            "{}.blob",
            base32_encode(&format!("{PREFIX}other-account.{REPLICAS}b1"))
        ));
        fs::write(&f, doc.to_string()).unwrap();
        assert!(read(&d).unwrap().bots[0].routines.is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_blob_is_not_followed() {
        let d = tmp();
        write_fixture(&d, &[]).unwrap();
        let f = d.join(CACHE_DIR).join(format!(
            "{}.blob",
            base32_encode(&format!("{PREFIX}demo-account.{ROSTER}"))
        ));
        let target = d.join("elsewhere.json");
        fs::write(
            &target,
            r#"{"schemaVersion":4,"value":{"rows":[{"id":"x1","name":"Linked"}]}}"#,
        )
        .unwrap();
        fs::remove_file(&f).unwrap();
        std::os::unix::fs::symlink(&target, &f).unwrap();
        assert!(read(&d).unwrap().bots.is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn glob_is_linear_on_hostile_patterns() {
        let p = "*a".repeat(60) + "b";
        let n = "a".repeat(5000);
        let t = std::time::Instant::now();
        assert!(!glob(&p, &n));
        assert!(t.elapsed().as_millis() < 500);
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob(&"x".repeat(201), "x"));
    }

    #[test]
    fn globs() {
        assert!(glob("acme*", "Acme Helper"));
        assert!(glob("*Planner", "Recipe Planner"));
        assert!(!glob("Acme", "Acme Helper"));
        assert!(wanted(&[], "x"));
        assert!(!wanted(&["Recipe*".into()], "Acme Helper"));
    }
}
