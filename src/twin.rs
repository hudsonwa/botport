//! `botport grok list` and `botport twin ...`: the stable JSON interface a Hermes plugin calls (docs/TWIN.md).
//!
//! A twin is a mirror pair (src/mirror.rs) whose files all live in one state folder the caller names
//! (`--state-dir`): the pair configs and keys (`pairs/`), the Grok-side snapshots (`grok/<pair>/`), the bot's last
//! export if one was given (`sources/<pair>/`), and an index (`twin.json`). The Grok side of a twin is composed on this
//! machine from what the Grok desktop app's local cache holds (the bot's name and description; routine names) laid over
//! the bot's last export, if any, and written as a signed state file the mirror engine reads like any other. Nothing is
//! sent anywhere: botport makes no network calls for a twin.
//!
//! Output: with `--json`, exactly one JSON document on stdout (the envelope below), errors included; without it, a
//! few summary lines. Exit codes: 0 ok, 1 ok but the owner's attention is needed, 2 usage, 3 environment (no profile,
//! no pair, locked), 4 internal.
use crate::adapters::grok_bot::cache::{self, Cache, CacheBot};
use crate::adapters::grok_bot::mirror as gmirror;
use crate::core::item::Content;
use crate::core::mirror::{ItemId, Kind};
use crate::core::util::{utc_iso, write_private, Args};
use crate::mirror;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The JSON schema name of every twin document.
pub const SCHEMA: &str = "botport.twin.v1";
const INDEX: &str = "twin.json";
const INDEX_SCHEMA: &str = "botport-twin/1";
const CHECK: &str = "check.json";
/// The skill category a twin's skills live in on the Hermes side.
pub const CATEGORY: &str = "grok-twin";
/// The category the official importer puts Grok skills in: mirrored too when the profile has it.
const IMPORTER_CATEGORY: &str = "grok-bot-imports";
const MAX_RECENT: usize = 50;

const VALUED: &[&str] = &[
    "hermes-home",
    "state-dir",
    "grok-app-dir",
    "grok-id",
    "profile",
    "pair",
    "only",
    "from-export",
    "approve",
    "conflict",
    "keep",
    "point",
    "limit",
    "step",
    "pair-folder",
    "account",
    "item",
    "hermes-bin",
];
const FLAGS: &[&str] = &["json", "all", "dry-run", "help", "yes", "off", "rotate-key"];

const FEATURES: &[&str] = &[
    "grok-list",
    "twin-create",
    "twin-check",
    "twin-sync",
    "twin-status",
    "twin-conflicts",
    "twin-resolve",
    "twin-approve",
    "twin-restore",
    "twin-recent",
    "twin-remove",
    "twin-mirror",
    "twin-parity",
    "twin-doctor",
    "twin-demo",
];

/// A failure with its error code and exit code.
#[derive(Debug)]
pub struct Fail {
    code: &'static str,
    exit: u8,
    /// The message (paths are shown by label before it is printed).
    pub message: String,
    hint: Option<String>,
}

impl Fail {
    fn new(code: &'static str, exit: u8, message: impl Into<String>) -> Fail {
        Fail {
            code,
            exit,
            message: message.into(),
            hint: None,
        }
    }
    fn hint(mut self, h: impl Into<String>) -> Fail {
        self.hint = Some(h.into());
        self
    }
}

fn usage(m: impl Into<String>) -> Fail {
    Fail::new("usage", 2, m)
}

/// Map an engine error to a code: a held lock, a missing pair or profile, else internal.
fn engine(e: String) -> Fail {
    if e.starts_with("another botport run holds the lock") {
        Fail::new("locked", 3, e)
            .hint("another botport run is working on this twin; try again in a moment")
    } else if e.contains(crate::core::package::NEUTRAL_FORM_MISMATCH) {
        Fail::new("version_mismatch", 3, e).hint(format!(
            "the botport in the Grok box is another release: give the Mirror bot botport {} (the same release as here)",
            crate::adapters::grok_bot::bundle::VERSION
        ))
    } else if e.contains("no mirror config") || e.contains("no mirror state") {
        Fail::new("pair_not_found", 3, e)
    } else if e.contains("does not exist") {
        Fail::new("profile_not_found", 3, e)
    } else if e.contains("secret") {
        Fail::new("secret_refused", 3, e)
    } else {
        Fail::new("internal", 4, e)
    }
}

type R<T> = Result<T, Fail>;

/// A command's result: data, warnings, and whether the owner's attention is needed (exit 1).
struct Done {
    data: Value,
    warnings: Vec<Value>,
    attention: bool,
    lines: Vec<String>,
}

fn warn(code: &str, message: impl Into<String>) -> Value {
    json!({"code": code, "message": message.into()})
}

// ---------------------------------------------------------------- environment

struct Env {
    hermes_home: PathBuf,
    state: PathBuf,
    app_dir: Option<PathBuf>,
    hermes_bin: String,
    /// Paths given on the command line (and their resolved forms), with the label errors show instead.
    given: Vec<(PathBuf, &'static str)>,
}

impl Env {
    fn of(a: &Args) -> R<Env> {
        let hermes_home = a
            .path("hermes-home")
            .or_else(|| {
                std::env::var_os("HERMES_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| crate::core::util::home().join(".hermes"));
        let state = a
            .path("state-dir")
            .unwrap_or_else(|| hermes_home.join("botport-twin"));
        let mut given = vec![];
        for (k, label) in [
            ("from-export", "<export>"),
            ("pair-folder", "<pair folder>"),
            ("grok-app-dir", "<grok app folder>"),
            ("hermes-bin", "<hermes>"),
        ] {
            if let Some(p) = a.path(k) {
                if let Ok(c) = p.canonicalize() {
                    given.push((c, label));
                }
                given.push((p, label));
            }
        }
        // longest first, so a path is never half replaced by a shorter one inside it
        given.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));
        Ok(Env {
            hermes_home,
            state,
            app_dir: a.path("grok-app-dir"),
            hermes_bin: a.get("hermes-bin").unwrap_or("hermes").to_string(),
            given,
        })
    }

    /// The Grok app folder: `--grok-app-dir`, else the one the index remembers (demo mode), else the OS default.
    fn app_dir(&self, idx: &Index) -> Option<PathBuf> {
        self.app_dir
            .clone()
            .or_else(|| idx.grok_app_dir.as_ref().map(|d| self.state.join(d)))
            .or_else(cache::default_app_dir)
    }

    fn config(&self, pair: &str) -> PathBuf {
        self.state.join("pairs").join(format!("{pair}.toml"))
    }

    fn grok_dir(&self) -> PathBuf {
        self.state.join("grok")
    }

    fn source(&self, pair: &str) -> PathBuf {
        self.state.join("sources").join(pair)
    }

    fn profile_dir(&self, profile: &str) -> PathBuf {
        if profile == "default" {
            self.hermes_home.clone()
        } else {
            self.hermes_home.join("profiles").join(profile)
        }
    }

    fn mirror_args(&self, pair: &str, sub: &[&str], extra: &[&str]) -> R<Args> {
        let cfg = self.config(pair);
        let mut v: Vec<String> = vec!["mirror".into()];
        v.extend(sub.iter().map(|s| s.to_string()));
        v.extend(["--pair".into(), pair.into(), "--mirror-config".into()]);
        v.push(cfg.to_string_lossy().into_owned());
        v.extend([
            "--metrics".into(),
            self.state
                .join("metrics.csv")
                .to_string_lossy()
                .into_owned(),
        ]);
        v.extend(extra.iter().map(|s| s.to_string()));
        Args::parse(&v, MIRROR_VALUED, MIRROR_FLAGS).map_err(Fail::from_internal)
    }
}

impl Fail {
    fn from_internal(e: String) -> Fail {
        Fail::new("internal", 4, e)
    }
    /// A usage error (exit 2).
    pub fn usage(m: impl Into<String>) -> Fail {
        Fail::new("usage", 2, m)
    }
    /// An internal error (exit 4).
    pub fn internal(m: impl Into<String>) -> Fail {
        Fail::new("internal", 4, m)
    }
}

/// Whether `bin` is an executable on PATH (or an existing path).
fn which(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

const MIRROR_VALUED: &[&str] = &[
    "pair",
    "mirror-config",
    "metrics",
    "profile",
    "profile-dir",
    "hermes-home",
    "hermes-bin",
    "transport",
    "local-dir",
    "bot-key",
    "key-file",
    "category",
    "approve",
    "keep",
    "from",
];
const MIRROR_FLAGS: &[&str] = &[
    "json",
    "yes",
    "copy-one-sided",
    "force",
    "allow-retire",
    "no-hook-manual",
];

// ---------------------------------------------------------------- the index

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Index {
    schema: String,
    /// Demo mode: the fake Grok app folder, relative to the state folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grok_app_dir: Option<String>,
    /// The Grok account the twins were made from (its opaque label), when the cache holds more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(default)]
    pairs: BTreeMap<String, PairRec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PairRec {
    grok_id: String,
    grok_name: String,
    profile: String,
    created_at: String,
    #[serde(default)]
    adopted: bool,
    #[serde(default)]
    last_sync_at: Option<String>,
    #[serde(default)]
    last_change_at: Option<String>,
    #[serde(default)]
    last_state: String,
    /// Fingerprint of the last Grok-side snapshot written (to write a new one only on a change).
    #[serde(default)]
    grok_fp: String,
    #[serde(default)]
    secrets_withheld: Vec<Withheld>,
    /// Routine names the app's cache knows for the bot (owner's by-hand list).
    #[serde(default)]
    routines: Vec<String>,
    #[serde(default)]
    recent: Vec<Recent>,
    /// Connected to the owner's Mirror bot through this pair folder (a synced folder on this machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mirror_folder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Withheld {
    item: String,
    kind: String,
    location: String,
    direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recent {
    at: String,
    side: String,
    kind: String,
    summary: String,
}

fn load_index(e: &Env) -> R<Index> {
    match fs::read_to_string(e.state.join(INDEX)) {
        Ok(t) => {
            let i: Index = serde_json::from_str(&t)
                .map_err(|x| Fail::new("internal", 4, format!("{INDEX} is damaged: {x}")))?;
            if i.schema != INDEX_SCHEMA {
                return Err(Fail::new(
                    "internal",
                    4,
                    format!("{INDEX} has schema {:?}", i.schema),
                ));
            }
            // the index is a file on disk: every name that becomes part of a path is checked on every load
            let bad = i
                .pairs
                .iter()
                .any(|(p, r)| !safe_component(p) || !safe_component(&r.profile))
                || i.grok_app_dir
                    .as_deref()
                    .is_some_and(|d| d != crate::twin_demo::APP_DIR);
            if bad {
                return Err(Fail::new(
                    "internal",
                    4,
                    format!(
                        "{INDEX} holds a name that is not a plain folder name; nothing was done"
                    ),
                ));
            }
            Ok(i)
        }
        Err(_) => Ok(Index {
            schema: INDEX_SCHEMA.into(),
            ..Index::default()
        }),
    }
}

fn save_index(e: &Env, i: &Index) -> R<()> {
    let t = serde_json::to_string_pretty(i).map_err(|x| Fail::from_internal(x.to_string()))?;
    write_private(&e.state, INDEX, t.as_bytes()).map_err(engine)
}

/// The write lock for the state folder: one writing twin command at a time, failing fast.
fn lock(e: &Env) -> R<Option<crate::core::util::OpLock>> {
    crate::core::util::ensure_private_dir(&e.state).map_err(engine)?;
    crate::core::util::OpLock::try_acquire(&e.state).map_err(|x| {
        Fail::new("locked", 3, x)
            .hint("another botport twin command is running; try again in a moment")
    })
}

fn push_recent(r: &mut PairRec, side: &str, kind: &str, summary: String) {
    r.recent.push(Recent {
        at: utc_iso(),
        side: side.into(),
        kind: kind.into(),
        summary: summary.chars().take(120).collect(),
    });
    let n = r.recent.len();
    if n > MAX_RECENT {
        r.recent.drain(..n - MAX_RECENT);
    }
}

// ---------------------------------------------------------------- entry point

/// Run `botport grok ...`, `botport twin ...` or `botport version --json`; prints the result and returns the exit code.
pub fn main(raw: &[String]) -> u8 {
    let json_out = raw.iter().any(|s| s == "--json");
    let command = match (raw.first().map(String::as_str), raw.get(1)) {
        (Some("version"), _) => "version".to_string(),
        (Some(c), Some(s)) if !s.starts_with("--") => format!("{c} {s}"),
        (Some(c), _) => c.to_string(),
        _ => String::new(),
    };
    let parsed = Args::parse(raw, VALUED, FLAGS);
    let scrub_env = parsed.as_ref().ok().and_then(|a| Env::of(a).ok());
    let r = parsed.map_err(usage).and_then(|a| dispatch(&a, &command));
    // no absolute path leaves botport: the state folder, the Hermes home and the home folder are shown by label
    let r = r.map_err(|mut f| {
        f.message = scrub(&f.message, scrub_env.as_ref());
        f
    });
    match r {
        Ok(d) => {
            let exit = u8::from(d.attention);
            if json_out {
                let doc = json!({"schema": SCHEMA, "ok": true, "command": command, "data": d.data,
                                 "warnings": d.warnings, "error": Value::Null});
                // no absolute path in any field (engine text can name one): every string is scrubbed
                println!("{}", scrub_value(doc, scrub_env.as_ref()));
            } else {
                for l in d.lines.iter().take(10) {
                    println!("{}", crate::core::util::name_safe(l));
                }
                for w in &d.warnings {
                    eprintln!("botport: warning: {}", w["message"].as_str().unwrap_or(""));
                }
            }
            exit
        }
        Err(f) => {
            if json_out {
                let doc = json!({"schema": SCHEMA, "ok": false, "command": command, "data": Value::Null,
                                 "warnings": [], "error": {"code": f.code, "message": f.message, "hint": f.hint}});
                println!("{doc}");
            } else {
                eprintln!("botport: ERROR: {}", f.message);
                if let Some(h) = &f.hint {
                    eprintln!("botport: hint: {h}");
                }
            }
            f.exit
        }
    }
}

fn scrub_value(v: Value, e: Option<&Env>) -> Value {
    match v {
        Value::String(t) => Value::String(scrub(&t, e)),
        Value::Array(a) => Value::Array(a.into_iter().map(|x| scrub_value(x, e)).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, x)| (k, scrub_value(x, e))).collect())
        }
        x => x,
    }
}

fn scrub(m: &str, e: Option<&Env>) -> String {
    let mut out = m.to_string();
    if let Some(e) = e {
        // paths the caller gave are shown by what they are, never as given
        for (p, label) in &e.given {
            let s = p.to_string_lossy();
            if s.len() > 1 {
                out = out.replace(s.as_ref(), label);
            }
        }
        for (p, label) in [(&e.state, "state:"), (&e.hermes_home, "hermes:")] {
            let s = p.to_string_lossy();
            if !s.is_empty() {
                out = out
                    .replace(&format!("{s}/"), label)
                    .replace(s.as_ref(), label.trim_end_matches(':'));
            }
        }
    }
    let home = crate::core::util::home();
    let h = home.to_string_lossy();
    if h.len() > 1 {
        out = out.replace(h.as_ref(), "~");
    }
    out
}

fn dispatch(a: &Args, command: &str) -> R<Done> {
    if a.has("help") {
        return Ok(Done {
            data: json!({"usage": USAGE}),
            warnings: vec![],
            attention: false,
            lines: USAGE.lines().map(String::from).collect(),
        });
    }
    let e = Env::of(a)?;
    match command {
        "version" => Ok(Done {
            data: json!({"version": crate::adapters::grok_bot::bundle::VERSION, "schema": SCHEMA, "features": FEATURES}),
            warnings: vec![],
            attention: false,
            lines: vec![format!(
                "botport {}",
                crate::adapters::grok_bot::bundle::VERSION
            )],
        }),
        "grok list" => grok_list(a, &e),
        "twin create" => create(a, &e, false),
        "twin adopt" => create(a, &e, true),
        "twin check" => check(a, &e),
        "twin sync" | "twin pack" => sync(a, &e),
        "twin status" => status(a, &e),
        "twin conflicts" => conflicts(a, &e),
        "twin resolve" => resolve(a, &e),
        "twin approve" => approve(a, &e),
        "twin restore-points" => restore_points(a, &e),
        "twin restore" => restore(a, &e),
        "twin recent" => recent(a, &e),
        "twin remove" => remove(a, &e),
        "twin mirror" => mirror_bot(a, &e),
        "twin parity" => parity(a, &e),
        "twin doctor" => doctor(a, &e),
        "twin demo" => {
            crate::twin_demo::demo(a, &e.hermes_home, &e.state).and_then(|step| match step {
                Some(_) => sync(a, &Env { app_dir: None, ..e }),
                None => {
                    let mut b = vec!["twin".to_string(), "create".into(), "--all".into()];
                    b.extend([
                        "--hermes-home".into(),
                        e.hermes_home.to_string_lossy().into_owned(),
                    ]);
                    b.extend(["--state-dir".into(), e.state.to_string_lossy().into_owned()]);
                    b.extend([
                        "--from-export".into(),
                        crate::twin_demo::EXPORTS_MARK.into(),
                    ]);
                    let a2 = Args::parse(&b, VALUED, FLAGS).map_err(usage)?;
                    create(&a2, &e, false)
                }
            })
        }
        c => Err(usage(format!(
            "unknown command {c:?} (botport twin --help)"
        ))),
    }
}

/// `botport twin --help`.
pub const USAGE: &str = "botport grok list [--only GLOB]...           the bots in the Grok desktop app's local cache
botport twin create (--grok-id ID [--profile NAME] | --all [--only GLOB]...) [--from-export FILE] [--approve runnable|all] [--dry-run]
botport twin adopt --grok-id ID --profile NAME [--from-export FILE]   an existing profile (e.g. made by the official importer)
botport twin check                            stat-only: which twins have inputs that changed (no content read)
botport twin sync [--pair ID | --all] [--only GLOB]... [--dry-run]   (twin pack: the same)
botport twin status [--pair ID | --all] [--only GLOB]...
botport twin conflicts --pair ID              both copies and a diff of each open conflict
botport twin resolve --pair ID --conflict ITEM --keep hermes|grok
botport twin approve --pair ID --item ITEM    approve one change that waits (sync lists them under waiting)
botport twin restore-points --pair ID         kept copies per item
botport twin restore --pair ID --point ITEM@DIGEST [--dry-run]
botport twin recent --pair ID [--limit 10]    what changed lately (metadata only)
botport twin remove --pair ID [--dry-run]     stop twinning: removes the pair, its snapshots, kept copies and restore
                                              points; the Hermes profile stays as a normal profile
botport twin mirror --pair ID (--pair-folder DIR | --off | --rotate-key)   connect the twin to the owner's Mirror bot in Grok through a
                                              synced folder (e.g. Google Drive for desktop): skills travel both ways
botport twin parity --pair ID                 setup parity: each item in step, routines recreated, a model set (no model call)
botport twin doctor
botport twin demo [--step rename|conflict|secret]   an invented Acme pair; no Grok account needed
botport version --json
common: --json --hermes-home DIR --state-dir DIR [--grok-app-dir DIR] [--hermes-bin PATH]";

// ---------------------------------------------------------------- grok list

/// The random per-install secret account labels are keyed with (`state:account-salt`, 0600), made on first use.
fn install_salt(e: &Env) -> R<Vec<u8>> {
    let p = e.state.join("account-salt");
    if let Ok(md) = fs::symlink_metadata(&p) {
        if md.is_file() && md.len() == 64 {
            if let Ok(t) = fs::read_to_string(&p) {
                return Ok(t.into_bytes());
            }
        }
        return Err(Fail::from_internal(
            "state:account-salt is not a 64-character file".into(),
        ));
    }
    let k = crate::core::auth::new_key().map_err(engine)?;
    write_private(&e.state, "account-salt", k.as_bytes()).map_err(engine)?;
    Ok(k.into_bytes())
}

/// Read the cache for one command: the account the owner chose (`--account`, else the one the twins were made from),
/// and the transcripts only of the bots `routines_of` names.
fn read_cache(
    e: &Env,
    idx: &Index,
    account: Option<&str>,
    routines_of: &dyn Fn(&str, &str) -> bool,
) -> R<(Cache, Vec<Value>)> {
    let Some(dir) = e.app_dir(idx) else {
        return Ok((
            Cache::default(),
            vec![warn(
                "grok_app_not_found",
                "no Grok desktop app folder on this OS",
            )],
        ));
    };
    let account = account.or(idx.account.as_deref());
    let salt = install_salt(e)?;
    let c = cache::read_select(
        &dir,
        &cache::Select {
            account,
            salt: Some(&salt),
            routines_of,
        },
    )
    .map_err(|x| {
        if x.contains("--account") || x.contains("no account") {
            Fail::new("account_choice", 2, x).hint("botport grok list --account LABEL")
        } else {
            Fail::new("grok_app_not_found", 3, x)
        }
    })?;
    let mut w = vec![];
    if !c.found {
        w.push(warn(
            "grok_app_not_found",
            "the Grok desktop app's local cache was not found on this computer",
        ));
    }
    if c.unreadable > 0 {
        w.push(warn(
            "cache_unreadable",
            format!(
                "{} cache file(s) could not be read and were skipped",
                c.unreadable
            ),
        ));
    }
    Ok((c, w))
}

fn grok_list(a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let only = a.all("only");
    let (c, warnings) = read_cache(e, &idx, a.get("account"), &|_, n| cache::wanted(only, n))?;
    let twinned: BTreeMap<&str, &str> = idx
        .pairs
        .iter()
        .map(|(p, r)| (r.grok_id.as_str(), p.as_str()))
        .collect();
    let bots: Vec<Value> = c
        .bots
        .iter()
        .filter(|b| cache::wanted(only, &b.name))
        .map(|b| {
            json!({"grok_id": b.id, "name": b.name, "description": b.description, "routines": b.routines,
                   "twinned_pair_id": twinned.get(b.id.as_str()),
                   "fields": {"name": "live", "description": "live", "instructions": "mirror", "skills": "mirror", "routines": "names"}})
        })
        .collect();
    let lines = std::iter::once(format!("{} bot(s) in the Grok app's cache", bots.len()))
        .chain(bots.iter().map(|b| {
            format!(
                "  {}  {}",
                b["grok_id"].as_str().unwrap_or(""),
                b["name"].as_str().unwrap_or("")
            )
        }))
        .collect();
    Ok(Done {
        data: json!({"app_found": c.found, "bots": bots}),
        warnings,
        attention: false,
        lines,
    })
}

// ---------------------------------------------------------------- composing the Grok side

/// A pair id from a bot name: lowercase letters, digits and `-`, at most 40, unique among `taken`.
fn pair_id(name: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut s = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
        if s.len() >= 40 {
            break;
        }
    }
    let base = s.trim_end_matches('-').to_string();
    let base = if base.is_empty() {
        "twin".to_string()
    } else {
        base
    };
    let mut id = base.clone();
    let mut n = 2;
    while taken(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// Keep the screened items of the bot's export (never the export file itself): one guarded file per item under
/// `sources/<pair>/items/`, and the reasons for items not carried. An item the write guard refuses is not kept and is
/// reported like any other suspected secret.
fn save_source(e: &Env, pair: &str, items: &mut gmirror::GrokItems) -> R<()> {
    save_source_in(&e.state, pair, items)
}

/// [`save_source`] for a state folder (the demo's conflict step uses it).
pub(crate) fn save_source_in(state: &Path, pair: &str, items: &mut gmirror::GrokItems) -> R<()> {
    let dir = state.join("sources").join(pair);
    remove_path(&dir)?;
    let ids: Vec<ItemId> = items.content.keys().cloned().collect();
    for id in ids {
        let blob = items.content[&id].to_blob(&id);
        let rel = format!(
            "sources/{pair}/items/{}",
            crate::core::util::hex(id.as_str().as_bytes())
        );
        if let Err(why) = write_private(state, &rel, &blob) {
            items.content.remove(&id);
            let why = if why.contains("secret") {
                why.split_once(": ")
                    .map_or(why.clone(), |(_, w)| w.to_string())
            } else {
                why
            };
            items
                .unsupported
                .insert(id.to_string(), why.chars().take(300).collect());
        }
    }
    let u = serde_json::to_string(&items.unsupported)
        .map_err(|x| Fail::from_internal(x.to_string()))?;
    write_private(
        state,
        &format!("sources/{pair}/unsupported.json"),
        u.as_bytes(),
    )
    .map_err(engine)
}

/// The kept export items of a twin (empty when none were kept). Damaged or foreign files are skipped.
fn load_source(e: &Env, pair: &str) -> gmirror::GrokItems {
    let mut out = gmirror::GrokItems::default();
    let dir = e.source(pair).join("items");
    for f in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        let Some(id) = unhex(&name).and_then(|s| ItemId::parse(&s).ok()) else {
            continue;
        };
        let Ok(md) = fs::symlink_metadata(f.path()) else {
            continue;
        };
        if !md.is_file() || md.len() > 64 << 20 {
            continue;
        }
        if let Some(c) = fs::read(f.path())
            .ok()
            .and_then(|b| Content::from_blob(&id, &b).ok())
        {
            out.content.insert(id, c);
        }
    }
    if let Some(u) = fs::read_to_string(e.source(pair).join("unsupported.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<BTreeMap<String, String>>(&t).ok())
    {
        out.unsupported = u;
    }
    out
}

fn unhex(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let b: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    String::from_utf8(b?).ok()
}

/// Paths a failed create removes again (the profile folder only when this run made it).
struct Rollback {
    paths: Vec<PathBuf>,
    armed: bool,
}

impl Drop for Rollback {
    fn drop(&mut self) {
        if self.armed {
            for p in self.paths.iter().rev() {
                let _ = remove_path(p);
            }
        }
    }
}

/// The Grok side of a twin: the bot's last export (if any) with the cache's name and description laid over it.
fn compose(
    e: &Env,
    pair: &str,
    bot: Option<&CacheBot>,
    export: Option<&Path>,
    mirror_bot: bool,
) -> R<gmirror::GrokItems> {
    let mut items = match export {
        Some(x) => gmirror::read_items(x)
            .map_err(|x| Fail::new("usage", 2, format!("the export could not be read: {x}")))?,
        None => load_source(e, pair),
    };
    // an export and a cache read may both leave things out: nothing missing is ever taken as removed
    items.partial = true;
    // with the Mirror bot connected, skills come from the box through it: the export's copies would read as newer
    // than the box's whenever this snapshot is rewritten for a rename, and roll an edited skill back
    if mirror_bot {
        items.content.retain(|id, _| id.kind() != Kind::Skill);
        items.unsupported.retain(|k, _| !k.starts_with("skill:"));
    }
    if let Some(b) = bot {
        gmirror::put(&mut items, "persona.name", Content::Text(b.name.clone()));
        if !b.description.is_empty() {
            gmirror::put(
                &mut items,
                "persona.description",
                Content::Text(format!("{}\n", b.description)),
            );
        }
    }
    // defence in depth for the bot's own text (it lands in the twin's system prompt): a long random-looking string
    // is held back like a key, even when no known key format matches it
    for raw in [
        "persona.name",
        "persona.description",
        "persona.instructions",
    ] {
        let Ok(id) = ItemId::parse(raw) else { continue };
        if let Some(Content::Text(t)) = items.content.get(&id) {
            if let Some(line) = random_looking(t) {
                items.content.remove(&id);
                items.unsupported.insert(
                    raw.into(),
                    format!(
                        "line {line} looks like it holds a secret (a long random-looking string)"
                    ),
                );
            }
        }
    }
    // nothing on this machine can write the Grok side: every change bound for it goes on the owner's list
    // (with the owner's Mirror bot connected, skills travel through it instead)
    if !mirror_bot {
        items.by_hand.insert(
            Kind::Skill,
            "add it to the bot in the Grok app, or connect the Mirror bot (botport twin mirror)"
                .into(),
        );
    }
    for (k, why) in [
        (Kind::Persona, "set in the Grok app (the bot's settings)"),
        (Kind::Routine, "set up in the Grok app"),
        (Kind::Connector, "connect it in the Grok app"),
    ] {
        items.by_hand.insert(k, why.into());
    }
    Ok(items)
}

/// The first line holding a word of 32+ characters that looks random: long hex, or letters and digits mixed with
/// high entropy (a key in a format no rule knows). Words in links are checked too.
pub fn random_looking(t: &str) -> Option<usize> {
    for (n, line) in t.lines().enumerate() {
        for w in line.split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '/' | '?' | '&' | '=' | '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '<' | '>'
                )
        }) {
            let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            if w.len() < 32
                || !w
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'))
            {
                continue;
            }
            let hex = w.chars().all(|c| c.is_ascii_hexdigit());
            let digits = w.chars().filter(char::is_ascii_digit).count();
            let upper = w.chars().filter(char::is_ascii_uppercase).count();
            let lower = w.chars().filter(char::is_ascii_lowercase).count();
            let classes = [digits, upper, lower].iter().filter(|c| **c > 0).count();
            if hex || (classes >= 3 && entropy(w) >= 4.0) {
                return Some(n + 1);
            }
        }
    }
    None
}

fn entropy(w: &str) -> f64 {
    let mut f: BTreeMap<char, f64> = BTreeMap::new();
    for c in w.chars() {
        *f.entry(c).or_default() += 1.0;
    }
    let n = w.chars().count() as f64;
    -f.values().map(|c| (c / n) * (c / n).log2()).sum::<f64>()
}

/// Text for display with every line that looks like it holds a secret replaced by a fixed note (detection runs before
/// anything is shown; the value never leaves botport).
pub fn masked(t: &str) -> String {
    t.lines()
        .map(|l| {
            let hit = crate::core::secrets::find_secret("persona.instructions", l.as_bytes())
                .is_some()
                || crate::core::secrets::scan_value(l).is_some()
                || random_looking(l).is_some();
            if hit {
                "[line hidden by botport: suspected secret]".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if t.ends_with('\n') { "\n" } else { "" }
}

fn fingerprint(items: &gmirror::GrokItems) -> String {
    let d: BTreeMap<String, String> = items
        .digests()
        .into_iter()
        .map(|(k, (_, n))| (k.to_string(), n))
        .collect();
    let v = json!({"d": d, "u": items.unsupported});
    crate::core::util::sha256_hex(v.to_string().as_bytes())
}

/// "the description: a token-shaped value (sk-/xai-/...)" from the guard's reason (which names a position in the item's
/// stored form, never the value).
fn secret_location(kind: &str, why: &str) -> String {
    let what = why
        .split_once("looks like it holds a secret")
        .map(|(_, r)| {
            // one outer pair of brackets around the reason, never the reason's own
            let r = r.trim();
            r.strip_prefix('(')
                .and_then(|x| x.strip_suffix(')'))
                .unwrap_or(r)
                .trim()
                .to_string()
        })
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "a value that looks like a secret".into());
    let at = why
        .split_once(" looks like")
        .map(|(a, _)| a.trim())
        .unwrap_or("");
    let line = at
        .strip_prefix("line ")
        .and_then(|n| n.split_whitespace().next())
        .map(|n| format!(", line {n}"))
        .unwrap_or_default();
    format!("suspected secret in the {kind}{line}: {what} (a check, not a guarantee)")
        .chars()
        .take(200)
        .collect()
}

fn withheld_of(items: &gmirror::GrokItems) -> Vec<Withheld> {
    items
        .unsupported
        .iter()
        .filter(|(_, why)| why.contains("secret"))
        .map(|(id, why)| {
            let kind: String = ItemId::parse(id).map_or("item".into(), |i| kind_word(&i).into());
            Withheld {
                item: id.clone(),
                location: secret_location(&kind, why),
                kind,
                direction: "g2h".into(),
            }
        })
        .collect()
}

/// Write the composed Grok side as the next signed state file when it changed. Returns whether it did.
fn write_grok(e: &Env, pair: &str, rec: &mut PairRec, items: &gmirror::GrokItems) -> R<bool> {
    let fp = fingerprint(items);
    if fp == rec.grok_fp {
        return Ok(false);
    }
    let key = mirror::read_key(&e.config(pair).with_extension("key")).map_err(engine)?;
    let out = e.grok_dir().join(pair);
    crate::core::util::ensure_private_dir(&out).map_err(engine)?;
    gmirror::digest_items(items, &out, pair, &[], &utc_iso(), false, Some(&key)).map_err(engine)?;
    rec.grok_fp = fp;
    Ok(true)
}

fn kind_word(id: &ItemId) -> &'static str {
    match id.as_str() {
        "persona.name" => "name",
        "persona.description" => "description",
        "persona.instructions" => "instructions",
        _ => match id.kind() {
            Kind::Skill => "skill",
            Kind::Routine => "routine",
            Kind::Connector => "connector",
            Kind::Persona => "persona",
        },
    }
}

fn item_name(id: &ItemId) -> String {
    match id.kind() {
        Kind::Persona => kind_word(id).to_string(),
        _ => id.name().to_string(),
    }
}

// ---------------------------------------------------------------- create / adopt

fn create(a: &Args, e: &Env, adopt: bool) -> R<Done> {
    let dry = a.has("dry-run");
    let _lock = if dry { None } else { lock(e)? };
    let mut idx = load_index(e)?;
    let (want_id, want_all, only) = (a.get("grok-id"), a.has("all"), a.all("only"));
    let (c, mut warnings) = read_cache(e, &idx, a.get("account"), &|id, n| {
        want_id == Some(id) || (want_all && cache::wanted(only, n))
    })?;
    // the account the twins come from is remembered, so later commands read the same one
    if !dry && idx.account.is_none() && c.accounts.len() > 1 {
        idx.account = a.get("account").map(String::from);
    }
    let approve = a.get("approve").unwrap_or("runnable");
    if !matches!(approve, "runnable" | "all") {
        return Err(usage("--approve must be runnable or all"));
    }
    let only = a.all("only");
    let picked: Vec<CacheBot> = match (a.get("grok-id"), a.has("all")) {
        (Some(id), false) => {
            vec![c.bots.iter().find(|b| b.id == id).cloned().ok_or_else(|| {
                Fail::new(
                    "usage",
                    2,
                    format!("no bot with id {id:?} in the Grok app's cache"),
                )
                .hint("botport grok list")
            })?]
        }
        (None, true) => c
            .bots
            .iter()
            .filter(|b| cache::wanted(only, &b.name))
            .cloned()
            .collect(),
        _ => return Err(usage("give one of --grok-id ID or --all")),
    };
    if adopt && a.get("profile").is_none() {
        return Err(usage("twin adopt needs --profile NAME"));
    }
    if a.get("profile").is_some() && picked.len() != 1 {
        return Err(usage("--profile works with one bot (--grok-id)"));
    }
    let exports = crate::twin_demo::exports(a, &e.state)?;
    let mut pairs = vec![];
    let mut skipped = vec![];
    let mut attention = false;
    for b in &picked {
        if let Some((p, _)) = idx.pairs.iter().find(|(_, r)| r.grok_id == b.id) {
            skipped.push(json!({"grok_id": b.id, "reason": "already_twinned", "pair_id": p}));
            continue;
        }
        // an existing profile is never taken over by accident: without --profile (twin adopt names one), a bot whose
        // name matches an existing profile gets a new one ("acme-helper-2")
        let taken = |s: &str| {
            idx.pairs.contains_key(s)
                || (a.get("profile").is_none() && fs::symlink_metadata(e.profile_dir(s)).is_ok())
        };
        let pair = pair_id(&b.name, &taken);
        let profile = a
            .get("profile")
            .map(String::from)
            .unwrap_or_else(|| pair.clone());
        if !safe_component(&profile) {
            return Err(usage(
                "--profile must be 1-64 of letters, digits, '.', '_', '-', not starting with '.'",
            ));
        }
        let pdir = e.profile_dir(&profile);
        let exists = pdir.is_dir();
        if adopt && !exists {
            return Err(Fail::new(
                "profile_not_found",
                3,
                format!("no Hermes profile {profile:?}"),
            ));
        }
        if exists && pdir.join(mirror::STATE_FILE).exists() {
            skipped.push(
                json!({"grok_id": b.id, "reason": "profile_already_paired", "profile": profile}),
            );
            continue;
        }
        let export = exports
            .get(&b.name)
            .cloned()
            .or_else(|| a.path("from-export").filter(|_| picked.len() == 1));
        let items = compose(e, &pair, Some(b), export.as_deref(), false)?;
        let withheld = withheld_of(&items);
        let mut list: Vec<Value> = items
            .content
            .iter()
            .map(|(id, cn)| {
                // the owner previews what the twin will hold (suspected secrets masked first)
                let preview = capped(masked(&mirror::render_content(cn))).0;
                json!({"item": id.as_str(), "kind": kind_word(id), "name": item_name(id),
                       "action": if exists { "compare" } else { "create" }, "owner": "grok", "preview": preview})
            })
            .collect();
        for r in &b.routines {
            list.push(json!({"item": format!("routine:{r}"), "kind": "routine", "name": r, "action": "by_hand", "owner": "grok"}));
        }
        let schedules = items
            .content
            .keys()
            .filter(|i| i.kind() == Kind::Routine)
            .count();
        if dry {
            pairs.push(json!({"pair_id": pair, "grok": {"id": b.id, "name": b.name},
                "hermes": {"profile": profile, "created": !exists, "adopted": exists},
                "items": list, "secrets_withheld": withheld_json(&withheld), "schedules_paused": schedules, "restore_point": Value::Null}));
            continue;
        }
        // anything made from here on is removed again if the create fails half way
        let mut rb = Rollback {
            paths: vec![
                e.source(&pair),
                e.config(&pair),
                e.config(&pair).with_extension("key"),
                e.config(&pair).with_extension("brief"),
                e.grok_dir().join(&pair),
            ],
            armed: true,
        };
        // the export's screened items, kept so later syncs lay the cache over them again
        let mut withheld = withheld;
        if let Some(x) = &export {
            let mut base = gmirror::read_items(x)
                .map_err(|er| usage(format!("the export could not be read: {er}")))?;
            save_source(e, &pair, &mut base)?;
            withheld = withheld_of(&compose(e, &pair, Some(b), None, false)?);
        }
        if !exists {
            crate::core::util::ensure_private_dir(&pdir).map_err(engine)?;
            rb.paths.insert(0, pdir.clone());
        }

        // the pair key first: the Grok-side state file is signed with it before the pair is set up
        let kp = e.config(&pair).with_extension("key");
        if !kp.exists() {
            let k = crate::core::auth::new_key().map_err(engine)?;
            write_private(&e.state, &format!("pairs/{pair}.key"), k.as_bytes()).map_err(engine)?;
        }
        let mut rec = PairRec {
            grok_id: b.id.clone(),
            grok_name: b.name.clone(),
            profile: profile.clone(),
            created_at: utc_iso(),
            adopted: exists,
            routines: b.routines.clone(),
            ..PairRec::default()
        };
        write_grok(e, &pair, &mut rec, &items)?;
        let mut cats = vec![CATEGORY.to_string()];
        if pdir.join("skills").join(IMPORTER_CATEGORY).is_dir() {
            cats.push(IMPORTER_CATEGORY.into());
        }
        let home = e.hermes_home.to_string_lossy().into_owned();
        let grok = e.grok_dir().to_string_lossy().into_owned();
        let kps = kp.to_string_lossy().into_owned();
        let mut extra: Vec<&str> = vec![
            "--profile",
            &profile,
            "--hermes-home",
            &home,
            "--hermes-bin",
            &e.hermes_bin,
            "--transport",
            "local",
            "--local-dir",
            &grok,
            "--key-file",
            &kps,
            "--approve",
            approve,
            "--json",
            "--no-hook-manual",
        ];
        // a new profile takes the Grok side as it is; an existing one keeps its own copies and lists the differences
        if !exists {
            extra.push("--copy-one-sided");
        }
        let cat_args: Vec<String> = cats
            .iter()
            .flat_map(|c| ["--category".to_string(), c.clone()])
            .collect();
        extra.extend(cat_args.iter().map(String::as_str));
        let init = mirror::run(&e.mirror_args(&pair, &["init"], &extra)?).map_err(engine)?;
        let to_pick = init.json["to_pick"].as_u64().unwrap_or(0);
        rec.secrets_withheld = withheld.clone();
        push_recent(
            &mut rec,
            "both",
            "sync",
            if exists {
                "twin set up over an existing profile".into()
            } else {
                "twin created".into()
            },
        );
        idx.pairs.insert(pair.clone(), rec);
        save_index(e, &idx)?;
        // the first sync copies the Grok side into a new profile (under the approval rules)
        let mut first = match sync_one(e, &mut idx, &pair, &c, false) {
            Ok(v) => v,
            Err(f) => {
                // the rollback removes the files; the index forgets the pair
                idx.pairs.remove(&pair);
                let _ = save_index(e, &idx);
                return Err(f);
            }
        };
        // a new twin: creating it (after the preview) is the owner's approval of what the Grok side holds now
        if !exists {
            let waiting: Vec<String> = first.0["waiting"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|w| w["item"].as_str().map(String::from))
                // only what the preview showed as text the owner can read: the bot's name, description,
                // instructions and skills. Routines and connectors keep waiting for their own approval.
                .filter(|i| {
                    ItemId::parse(i)
                        .is_ok_and(|id| matches!(id.kind(), Kind::Persona | Kind::Skill))
                })
                .collect();
            for item in &waiting {
                mirror::run(&e.mirror_args(&pair, &["approve", item], &["--yes", "--json"])?)
                    .map_err(engine)?;
            }
            if !waiting.is_empty() {
                first = sync_one(e, &mut idx, &pair, &c, false)?;
                if let Some(v) = first.0["applied"].as_array_mut() {
                    for item in &waiting {
                        if v.iter().any(|x| x["item"] == item.as_str()) {
                            continue;
                        }
                        if let Ok(id) = ItemId::parse(item) {
                            v.push(json!({"item": item, "kind": kind_word(&id), "name": item_name(&id), "direction": "g2h"}));
                        }
                    }
                }
            }
        }
        save_index(e, &idx)?;
        attention |= to_pick > 0 || first.1;
        let mut entry = first.0;
        entry["grok"] = json!({"id": b.id, "name": b.name});
        entry["hermes"] = json!({"profile": profile, "created": !exists, "adopted": exists});
        entry["items"] = Value::Array(list);
        entry["schedules_paused"] = json!(schedules);
        rb.armed = false;
        pairs.push(entry);
    }
    if picked.is_empty() {
        warnings.push(warn("no_bots", "no bot matched"));
    }
    if !dry {
        let made: Vec<String> = pairs
            .iter()
            .filter_map(|p| p["pair_id"].as_str().map(String::from))
            .collect();
        settle(e, &idx, &made, &BTreeMap::new())?;
    }
    let lines = std::iter::once(format!(
        "{} twin(s) {}, {} skipped",
        pairs.len(),
        if dry { "previewed" } else { "created" },
        skipped.len()
    ))
    .chain(pairs.iter().map(|p| {
        format!(
            "  {} -> profile {}",
            p["pair_id"].as_str().unwrap_or(""),
            p["hermes"]["profile"].as_str().unwrap_or("")
        )
    }))
    .collect();
    Ok(Done {
        data: json!({"pairs": pairs, "skipped": skipped, "dry_run": dry}),
        warnings,
        attention,
        lines,
    })
}

fn withheld_json(w: &[Withheld]) -> Value {
    Value::Array(
        w.iter()
            .map(|x| json!({"item": x.item, "kind": x.kind, "location": x.location, "direction": x.direction}))
            .collect(),
    )
}

// ---------------------------------------------------------------- check

/// The stat signature of each twin's inputs: the cache slices, the profile's setup files, the export (times, sizes
/// and inodes only; nothing is opened).
fn signatures(e: &Env, idx: &Index) -> BTreeMap<String, String> {
    let cache_sig = e
        .app_dir(idx)
        .map(|d| {
            cache::files(&d)
                .iter()
                .map(|(p, _, _)| stamp(p))
                .collect::<Vec<_>>()
                .join(";")
        })
        .unwrap_or_default();
    idx.pairs
        .keys()
        .map(|p| {
            let s = e
                .mirror_args(p, &["status"], &[])
                .ok()
                .and_then(|a| mirror::signature(&a).ok())
                .unwrap_or_default();
            (
                p.clone(),
                format!(
                    "{cache_sig}|{s}|{}",
                    stamp(&e.source(p).join("unsupported.json"))
                ),
            )
        })
        .collect()
}

fn save_signatures(e: &Env, sig: &BTreeMap<String, String>) -> R<()> {
    let t = serde_json::to_string(sig).unwrap_or_default();
    write_private(&e.state, CHECK, t.as_bytes()).map_err(engine)
}

/// After a run that wrote: the twins it processed are recorded with their signatures after the run (so its own writes
/// cost no extra sync), or, when the cache changed during the run, with those taken before it read anything. Every
/// other twin keeps its last recorded signature.
fn settle(e: &Env, idx: &Index, done: &[String], before: &BTreeMap<String, String>) -> R<()> {
    let mut sig: BTreeMap<String, String> = fs::read_to_string(e.state.join(CHECK))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    sig.retain(|p, _| idx.pairs.contains_key(p));
    let now = signatures(e, idx);
    // a processed twin is recorded as it is now (its own writes are no news), unless the Grok app's cache changed while
    // the run went on: botport never writes the cache, so that change is the owner's and the next check reports it
    let cache_part = |s: &String| s.split('|').next().map(str::to_string);
    for p in done {
        let pick = match (before.get(p), now.get(p)) {
            (Some(b), Some(n)) if cache_part(b) != cache_part(n) => Some(b),
            (_, Some(n)) => Some(n),
            (b, None) => b,
        };
        if let Some(s) = pick {
            sig.insert(p.clone(), s.clone());
        }
    }
    save_signatures(e, &sig)
}

/// [`settle`] for a command on one twin (its own writes are not news).
fn settle_one(e: &Env, idx: &Index, pair: &str) -> R<()> {
    settle(e, idx, &[pair.to_string()], &BTreeMap::new())
}

fn check(_a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let sig = signatures(e, &idx);
    let old: BTreeMap<String, String> = fs::read_to_string(e.state.join(CHECK))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let changed: Vec<&String> = sig
        .iter()
        .filter(|(p, s)| old.get(*p) != Some(s))
        .map(|(p, _)| p)
        .collect();
    if (!changed.is_empty() || old.len() != sig.len()) && e.state.is_dir() {
        // written under the state folder's lock; when a writing command holds it, that command records them
        if let Ok(Some(_lock)) = crate::core::util::OpLock::try_acquire(&e.state) {
            save_signatures(e, &sig)?;
        }
    }
    Ok(Done {
        lines: vec![format!("{} twin(s) with changed inputs", changed.len())],
        data: json!({"changed": changed, "checked_at": utc_iso()}),
        warnings: vec![],
        attention: false,
    })
}

fn stamp(p: &Path) -> String {
    match fs::symlink_metadata(p) {
        Ok(m) => {
            #[cfg(unix)]
            let ino = std::os::unix::fs::MetadataExt::ino(&m);
            #[cfg(not(unix))]
            let ino = 0u64;
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            format!("{t}:{}:{ino}", m.len())
        }
        Err(_) => "-".into(),
    }
}

// ---------------------------------------------------------------- sync

fn selected(a: &Args, idx: &Index) -> R<Vec<String>> {
    let only = a.all("only");
    match a.get("pair") {
        Some(p) if idx.pairs.contains_key(p) => Ok(vec![p.to_string()]),
        Some(p) => Err(Fail::new("pair_not_found", 3, format!("no twin {p:?}"))
            .hint("botport twin status --all")),
        None => Ok(idx
            .pairs
            .iter()
            .filter(|(_, r)| cache::wanted(only, &r.grok_name))
            .map(|(p, _)| p.clone())
            .collect()),
    }
}

fn sync(a: &Args, e: &Env) -> R<Done> {
    let dry = a.has("dry-run");
    let _lock = if dry { None } else { lock(e)? };
    let mut idx = load_index(e)?;
    let todo = selected(a, &idx)?;
    let ids: Vec<String> = todo.iter().map(|p| idx.pairs[p].grok_id.clone()).collect();
    let (c, warnings) = read_cache(e, &idx, a.get("account"), &|id, _| {
        ids.iter().any(|x| x == id)
    })?;
    let mut pairs = vec![];
    let mut attention = false;
    // signatures from before anything was read: a change landing during the run is still seen by the next check
    let before = signatures(e, &idx);
    for p in &todo {
        // one twin that fails never stops the others: it is reported in its place
        match sync_one(e, &mut idx, p, &c, dry) {
            Ok((v, att)) => {
                attention |= att;
                pairs.push(v);
            }
            Err(f) if f.code == "locked" => return Err(f),
            Err(f) => {
                attention = true;
                pairs.push(json!({"pair_id": p, "state": "error", "applied": [], "waiting": [], "conflicts": [],
                    "secrets_withheld": [], "to_grok": 0, "restore_point": Value::Null, "dry_run": dry,
                    "error": {"code": f.code, "message": f.message.chars().take(300).collect::<String>()}}));
            }
        }
    }
    if !dry {
        save_index(e, &idx)?;
        settle(e, &idx, &todo, &before)?;
    }
    let lines = pairs
        .iter()
        .map(|p| {
            format!(
                "{}: {} ({} applied, {} conflicts)",
                p["pair_id"].as_str().unwrap_or(""),
                p["state"].as_str().unwrap_or(""),
                p["applied"].as_array().map_or(0, Vec::len),
                p["conflicts"].as_array().map_or(0, Vec::len)
            )
        })
        .collect();
    Ok(Done {
        data: json!({"pairs": pairs}),
        warnings,
        attention,
        lines,
    })
}

/// One twin: refresh its Grok side from the cache, run the mirror engine, record the outcome.
/// What waits on the Grok side, from the engine's own status: changes listed to apply in the Grok app by hand, and
/// changes sent to the Mirror bot not yet installed. `sync` and `status` both count it this one way.
fn grok_pending(e: &Env, pair: &str) -> (usize, usize) {
    match e
        .mirror_args(pair, &["status"], &["--json"])
        .ok()
        .and_then(|a| mirror::run(&a).ok())
    {
        Some(o) => (
            o.json["to_apply"].as_object().map_or(0, |m| m.len()),
            o.json["in_flight"].as_array().map_or(0, Vec::len),
        ),
        None => (0, 0),
    }
}

fn sync_one(e: &Env, idx: &mut Index, pair: &str, c: &Cache, dry: bool) -> R<(Value, bool)> {
    let mut rec = idx
        .pairs
        .get(pair)
        .cloned()
        .ok_or_else(|| Fail::new("pair_not_found", 3, format!("no twin {pair:?}")))?;
    let bot = c.bots.iter().find(|b| b.id == rec.grok_id);
    let grok_missing = c.found && bot.is_none();
    let items = compose(e, pair, bot, None, rec.mirror_folder.is_some())?;
    let mut withheld = withheld_of(&items);
    if !dry {
        if let Some(b) = bot {
            if b.name != rec.grok_name {
                push_recent(
                    &mut rec,
                    "grok",
                    "name_change",
                    "the bot was renamed in Grok".into(),
                );
                rec.grok_name = b.name.clone();
            }
            if b.routines != rec.routines {
                push_recent(
                    &mut rec,
                    "grok",
                    "routine_change",
                    "the bot's routines changed in Grok".into(),
                );
                rec.routines = b.routines.clone();
            }
        }
        write_grok(e, pair, &mut rec, &items)?;
    }
    let sub = if dry { "plan" } else { "sync" };
    let out = mirror::run(&e.mirror_args(pair, &[sub], &["--json"])?).map_err(engine)?;
    let mut applied = vec![];
    let mut waiting = vec![];
    let mut conflicts = vec![];
    for it in out.json["items"].as_array().into_iter().flatten() {
        let item = it["item"].as_str().unwrap_or("");
        let Ok(id) = ItemId::parse(item) else {
            continue;
        };
        let why = it["why"].as_str().unwrap_or("");
        match it["action"].as_str().unwrap_or("") {
            "to-local" | "retire-local" => applied.push(json!({"item": item, "kind": kind_word(&id), "name": item_name(&id), "direction": "g2h"})),
            "to-remote" | "retire-remote" => {
                applied.push(json!({"item": item, "kind": kind_word(&id), "name": item_name(&id), "direction": "h2g"}));
            }
            "needs-approval" => waiting.push(json!({"item": item, "reason": "needs_approval", "detail": why})),
            "conflict" => conflicts.push(json!(item)),
            "unsupported" if why.contains("secret") => {
                let w = Withheld { item: item.into(), kind: kind_word(&id).into(), location: secret_location(kind_word(&id), why), direction: "h2g".into() };
                if !withheld.iter().any(|x| x.item == w.item) {
                    withheld.push(w);
                }
            }
            _ => {}
        }
    }
    let (by_hand_g, in_flight) = if dry { (0, 0) } else { grok_pending(e, pair) };
    let to_grok = by_hand_g + in_flight;
    let state = if grok_missing {
        "grok_missing"
    } else if !conflicts.is_empty() || !waiting.is_empty() || !withheld.is_empty() {
        "needs_you"
    } else if to_grok > 0 {
        "changes_waiting"
    } else {
        "in_step"
    };
    if !dry {
        let now = utc_iso();
        rec.last_sync_at = Some(now.clone());
        for x in applied.iter().filter(|x| x["direction"] == "g2h") {
            let k = x["kind"].as_str().unwrap_or("item");
            push_recent(
                &mut rec,
                "grok",
                if k == "skill" { "skill_change" } else { "sync" },
                format!("{k} updated from Grok"),
            );
            rec.last_change_at = Some(now.clone());
        }
        if rec.secrets_withheld.len() < withheld.len() {
            push_recent(
                &mut rec,
                "both",
                "sync",
                format!("{} secret(s) held back", withheld.len()),
            );
        }
        rec.secrets_withheld = withheld.clone();
        rec.last_state = state.into();
        idx.pairs.insert(pair.to_string(), rec);
    }
    let attention = state == "needs_you" || state == "grok_missing";
    Ok((
        json!({"pair_id": pair, "state": state, "applied": applied, "waiting": waiting, "secrets_withheld": withheld_json(&withheld),
               "conflicts": conflicts, "to_grok": to_grok, "restore_point": Value::Null,
               "wall_ms": out.json["wall_ms"], "dry_run": dry}),
        attention,
    ))
}

// ---------------------------------------------------------------- status

fn coverage(mirror_bot: bool) -> Value {
    let skills_h2g = if mirror_bot {
        "syncs_via_mirror"
    } else {
        "by_hand"
    };
    json!([
        {"item": "name", "g2h": "syncs", "h2g": "by_hand"},
        {"item": "description", "g2h": "syncs", "h2g": "by_hand"},
        // honest: the Grok app's files hold no instructions, and the box reads back only skills this twin installed,
        // so a change made in Grok to either reaches the twin by hand (or a new one-time export)
        {"item": "instructions", "g2h": "by_hand", "h2g": "by_hand"},
        {"item": "skills", "g2h": "by_hand", "h2g": skills_h2g},
        {"item": "routines", "g2h": "by_hand", "h2g": "by_hand"},
        {"item": "connectors", "g2h": "by_hand", "h2g": "by_hand"}
    ])
}

fn status(a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let (c, warnings) = read_cache(e, &idx, a.get("account"), &|_, _| false)?;
    let mut pairs = vec![];
    let mut attention = false;
    for p in selected(a, &idx)? {
        let r = &idx.pairs[&p];
        let present = !c.found || c.bots.iter().any(|b| b.id == r.grok_id);
        let plan = mirror::run(&e.mirror_args(&p, &["plan"], &["--json"])?);
        let st = mirror::run(&e.mirror_args(&p, &["status"], &["--json"])?);
        let (mut to_h, mut to_g, mut wait) = (0, 0, 0);
        if let Ok(o) = &plan {
            for it in o.json["items"].as_array().into_iter().flatten() {
                match it["action"].as_str().unwrap_or("") {
                    "to-local" | "retire-local" => to_h += 1,
                    "to-remote" | "retire-remote" => to_g += 1,
                    "needs-approval" => wait += 1,
                    _ => {}
                }
            }
        }
        let conflicts = st
            .as_ref()
            .ok()
            .and_then(|o| o.json["conflicts"].as_array().map(Vec::len))
            .unwrap_or(0);
        // sent to the Mirror bot, not yet installed by it
        let in_flight = st
            .as_ref()
            .ok()
            .and_then(|o| o.json["in_flight"].as_array().map(Vec::len))
            .unwrap_or(0);
        let mut by_hand: Vec<Value> = r
            .routines
            .iter()
            .map(|n| json!({"item": format!("routine:{n}"), "side": "hermes",
                            "what": format!("Recreate routine \"{n}\" in Hermes (Grok keeps its schedule in the cloud)")}))
            .collect();
        if let Ok(o) = &st {
            if let Some(m) = o.json["to_apply"].as_object() {
                for item in m.keys() {
                    let what = match ItemId::parse(item) {
                        Ok(id) if id.kind() == Kind::Persona => {
                            format!("Update the bot's {} in the Grok app", kind_word(&id))
                        }
                        Ok(id) => format!(
                            "Apply the Hermes copy of {} \"{}\" in the Grok app",
                            kind_word(&id),
                            id.name()
                        ),
                        Err(_) => continue,
                    };
                    by_hand.push(json!({"item": item, "side": "grok", "what": what}));
                    to_g += 1;
                }
            }
        }
        let state = if st.is_err() {
            "error"
        } else if !present {
            "grok_missing"
        } else if conflicts > 0 || wait > 0 || !r.secrets_withheld.is_empty() {
            "needs_you"
        } else if to_h + to_g + in_flight > 0 {
            "changes_waiting"
        } else {
            "in_step"
        };
        attention |= matches!(state, "needs_you" | "grok_missing" | "error");
        let pdir = e.profile_dir(&r.profile);
        pairs.push(json!({"pair_id": p, "grok": {"id": r.grok_id, "name": r.grok_name, "present": present},
            "hermes": {"profile": r.profile}, "state": state, "last_sync_at": r.last_sync_at, "last_change_at": r.last_change_at,
            "pending": {"to_hermes": to_h, "to_grok": to_g, "in_flight": in_flight, "waiting_approval": wait}, "conflicts": conflicts,
            "coverage": coverage(r.mirror_folder.is_some()), "mirror_bot": {"connected": r.mirror_folder.is_some()}, "by_hand": by_hand, "restore_points": mirror::kept_copies(&pdir).len(),
            "secrets_withheld_total": r.secrets_withheld.len(), "secrets_withheld": withheld_json(&r.secrets_withheld),
            "error": st.err().map(|x| x.chars().take(300).collect::<String>())}));
    }
    let lines = pairs
        .iter()
        .map(|p| {
            format!(
                "{}: {}",
                p["pair_id"].as_str().unwrap_or(""),
                p["state"].as_str().unwrap_or("")
            )
        })
        .collect();
    Ok(Done {
        data: json!({"pairs": pairs}),
        warnings,
        attention,
        lines,
    })
}

// ---------------------------------------------------------------- conflicts, resolve

const MAX_TEXT: usize = 64 * 1024;

fn capped(t: String) -> (String, bool) {
    if t.len() <= MAX_TEXT {
        return (t, false);
    }
    let mut end = MAX_TEXT;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    (t[..end].to_string(), true)
}

fn one_pair<'a>(a: &'a Args, idx: &Index) -> R<&'a str> {
    let p = a
        .get("pair")
        .ok_or_else(|| usage("--pair ID is required"))?;
    if !idx.pairs.contains_key(p) {
        return Err(Fail::new("pair_not_found", 3, format!("no twin {p:?}")));
    }
    Ok(p)
}

fn conflicts(a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let p = one_pair(a, &idx)?;
    let list = mirror::conflict_copies(&e.mirror_args(p, &["status"], &[])?).map_err(engine)?;
    let mut out = vec![];
    for c in list {
        let h = c
            .hermes
            .as_ref()
            .map(|(_, cn)| masked(&mirror::render_content(cn)));
        let g = c
            .grok
            .as_ref()
            .and_then(|(_, cn)| cn.as_ref().map(|x| masked(&mirror::render_content(x))));
        let diff = crate::twin_demo::unified_diff(
            g.as_deref().unwrap_or(""),
            h.as_deref().unwrap_or(""),
            "grok",
            "hermes",
        );
        let side = |d: Option<&String>, t: Option<String>| match d {
            None => Value::Null,
            Some(d) => {
                let (text, cut) = t.map(capped).map_or((None, false), |(t, c)| (Some(t), c));
                json!({"digest": &d[..d.len().min(12)], "text": text, "text_truncated": cut})
            }
        };
        out.push(json!({"conflict_id": c.item.as_str(), "kind": kind_word(&c.item), "name": item_name(&c.item), "since": c.since,
            "hermes": side(c.hermes.as_ref().map(|x| &x.0), h), "grok": side(c.grok.as_ref().map(|x| &x.0), g),
            "diff": capped(diff).0}));
    }
    Ok(Done {
        lines: vec![format!("{} open conflict(s)", out.len())],
        attention: !out.is_empty(),
        data: json!({"conflicts": out}),
        warnings: vec![],
    })
}

fn resolve(a: &Args, e: &Env) -> R<Done> {
    let _lock = lock(e)?;
    let mut idx = load_index(e)?;
    let p = one_pair(a, &idx)?.to_string();
    let item = a
        .get("conflict")
        .ok_or_else(|| usage("--conflict ITEM is required"))?;
    ItemId::parse(item).map_err(usage)?;
    let keep = a
        .get("keep")
        .ok_or_else(|| usage("--keep hermes|grok is required"))?;
    if !matches!(keep, "hermes" | "grok") {
        return Err(usage("--keep must be hermes or grok"));
    }
    let o =
        mirror::run(&e.mirror_args(&p, &["pick", item], &["--keep", keep, "--yes", "--json"])?)
            .map_err(engine)?;
    if let Some(r) = idx.pairs.get_mut(&p) {
        push_recent(
            r,
            "both",
            "sync",
            format!("conflict on {item} resolved: kept {keep}"),
        );
    }
    save_index(e, &idx)?;
    settle_one(e, &idx, &p)?;
    Ok(Done {
        lines: o.lines,
        data: json!({"applied": [{"item": item, "keep": keep}], "restore_point": Value::Null}),
        warnings: vec![],
        attention: false,
    })
}

fn approve(a: &Args, e: &Env) -> R<Done> {
    let _lock = lock(e)?;
    let mut idx = load_index(e)?;
    let p = one_pair(a, &idx)?.to_string();
    let item = a
        .get("item")
        .ok_or_else(|| usage("--item ITEM is required (from sync's waiting list)"))?;
    ItemId::parse(item).map_err(usage)?;
    let o = mirror::run(&e.mirror_args(&p, &["approve", item], &["--yes", "--json"])?)
        .map_err(engine)?;
    if let Some(r) = idx.pairs.get_mut(&p) {
        push_recent(r, "both", "sync", format!("{item} approved"));
    }
    save_index(e, &idx)?;
    let gid = idx.pairs[&p].grok_id.clone();
    let (c, _) = read_cache(e, &idx, a.get("account"), &|id, _| id == gid)?;
    let (v, att) = sync_one(e, &mut idx, &p, &c, false)?;
    save_index(e, &idx)?;
    settle_one(e, &idx, &p)?;
    Ok(Done {
        lines: o.lines,
        data: json!({"approved": [{"item": item, "to": o.json["to"]}], "sync": v}),
        warnings: vec![],
        attention: att,
    })
}

// ---------------------------------------------------------------- restore points

fn restore_points(a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let p = one_pair(a, &idx)?;
    let pdir = e.profile_dir(&idx.pairs[p].profile);
    let points: Vec<Value> = mirror::kept_copies(&pdir)
        .into_iter()
        .map(|(id, d, t)| json!({"id": format!("{}@{}", id.as_str(), &d[..12]), "at": crate::core::util::utc_iso_at(t),
                                 "reason": "kept copy", "scope": "item", "item": id.as_str(), "kind": kind_word(&id), "name": item_name(&id)}))
        .collect();
    Ok(Done {
        lines: vec![format!("{} restore point(s)", points.len())],
        data: json!({"points": points}),
        warnings: vec![],
        attention: false,
    })
}

fn restore(a: &Args, e: &Env) -> R<Done> {
    let dry = a.has("dry-run");
    let _lock = if dry { None } else { lock(e)? };
    let mut idx = load_index(e)?;
    let p = one_pair(a, &idx)?.to_string();
    let point = a
        .get("point")
        .ok_or_else(|| usage("--point ITEM@DIGEST is required (twin restore-points)"))?;
    let (item, short) = point
        .rsplit_once('@')
        .ok_or_else(|| usage("--point must be ITEM@DIGEST"))?;
    ItemId::parse(item).map_err(usage)?;
    if short.len() < 12 || !short.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(usage(
            "--point: the digest part needs at least 12 hex characters",
        ));
    }
    if dry {
        return Ok(Done {
            lines: vec![format!("would restore {item} from {short}")],
            data: json!({"restored": [item], "dry_run": true, "restore_point": Value::Null}),
            warnings: vec![],
            attention: false,
        });
    }
    let o = mirror::run(&e.mirror_args(
        &p,
        &["restore", item],
        &["--from", short, "--yes", "--json"],
    )?)
    .map_err(engine)?;
    if let Some(r) = idx.pairs.get_mut(&p) {
        push_recent(
            r,
            "hermes",
            "sync",
            format!("{item} restored from a kept copy"),
        );
    }
    save_index(e, &idx)?;
    settle_one(e, &idx, &p)?;
    Ok(Done {
        lines: o.lines,
        data: json!({"restored": [item], "dry_run": false, "restore_point": Value::Null}),
        warnings: vec![],
        attention: false,
    })
}

// ---------------------------------------------------------------- the Mirror bot

/// A new pair key replaces the old one (which then verifies nothing). The next sync signs a new Grok-side snapshot
/// with it; the Mirror bot needs the new key file before it can read packs again.
fn rotate_key(e: &Env, pair: &str, rec: &mut PairRec) -> R<()> {
    let k = crate::core::auth::new_key().map_err(engine)?;
    write_private(&e.state, &format!("pairs/{pair}.key"), k.as_bytes()).map_err(engine)?;
    rec.grok_fp.clear();
    Ok(())
}

/// `twin mirror --pair ID --pair-folder DIR`: skills (and the change log) travel through the owner's Mirror bot, which
/// reads and writes the pair folder from inside Grok on its routine. botport only reads and writes files in that folder
/// on this machine (a folder the owner's sync app keeps in step); it makes no network calls and holds no Drive token.
fn mirror_bot(a: &Args, e: &Env) -> R<Done> {
    let _lock = lock(e)?;
    let mut idx = load_index(e)?;
    let p = one_pair(a, &idx)?.to_string();
    let cfg_path = e.config(&p);
    let mut cfg = mirror::read_config(&cfg_path).map_err(engine)?;
    if a.has("rotate-key") {
        let rec = idx
            .pairs
            .get_mut(&p)
            .ok_or_else(|| Fail::new("pair_not_found", 3, "no such twin"))?;
        rotate_key(e, &p, rec)?;
        push_recent(rec, "both", "sync", "pair key replaced".into());
        save_index(e, &idx)?;
        settle_one(e, &idx, &p)?;
        return Ok(Done {
            lines: vec![format!("twin {p}: a new pair key; give the Mirror bot the new key file (attach it, never paste it)")],
            data: json!({"pair_id": p, "key_rotated": true, "pair_key": format!("state:pairs/{p}.key")}),
            warnings: vec![],
            attention: false,
        });
    }
    let off = a.has("off");
    let folder = match (a.path("pair-folder"), off) {
        (Some(_), true) | (None, false) => {
            return Err(usage("give one of --pair-folder DIR or --off"))
        }
        (Some(d), false) => Some(d),
        (None, true) => None,
    };
    // the pair folder, resolved: no link anywhere on its path, and outside the state folder and the Hermes home
    let folder = match folder {
        None => None,
        Some(d) => {
            if !d.is_absolute() {
                return Err(usage("--pair-folder must be an absolute path"));
            }
            let mut p = PathBuf::new();
            for c in d.components() {
                p.push(c);
                // a link is allowed only when root owns it (the system's own, like macOS's /var): a link someone
                // else made could move the folder elsewhere later
                if let Ok(m) = fs::symlink_metadata(&p) {
                    #[cfg(unix)]
                    let system = std::os::unix::fs::MetadataExt::uid(&m) == 0;
                    #[cfg(not(unix))]
                    let system = false;
                    if m.file_type().is_symlink() && !system {
                        return Err(usage(
                            "the pair folder's path holds a link: give the real folder",
                        ));
                    }
                }
            }
            let real = d.canonicalize().map_err(|_| {
                usage("the pair folder does not exist: create it in your synced folder first")
            })?;
            if !real.is_dir() {
                return Err(usage("the pair folder must be a folder"));
            }
            let inside = |root: &Path| {
                root.canonicalize()
                    .is_ok_and(|r| real.starts_with(&r) || r.starts_with(&real))
            };
            if inside(&e.state) || inside(&e.hermes_home) {
                return Err(usage(
                    "the pair folder must be outside the Hermes home and botport's state folder",
                ));
            }
            Some(real)
        }
    };
    cfg.transport = if folder.is_some() {
        "hybrid".into()
    } else {
        "local".into()
    };
    cfg.drive_dir = folder.clone();
    mirror::write_config(&cfg_path, &cfg).map_err(engine)?;
    let rec = idx
        .pairs
        .get_mut(&p)
        .ok_or_else(|| Fail::new("pair_not_found", 3, "no such twin"))?;
    rec.mirror_folder = folder.as_ref().map(|d| {
        d.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().to_string())
    });
    rec.grok_fp.clear();
    // disconnecting stops trusting the old key: the box's copy verifies nothing from now on
    if off {
        rotate_key(e, &p, rec)?;
    }
    push_recent(
        rec,
        "both",
        "sync",
        if off {
            "Mirror bot disconnected".into()
        } else {
            "Mirror bot connected".into()
        },
    );
    let grok_id = rec.grok_id.clone();
    save_index(e, &idx)?;
    settle_one(e, &idx, &p)?;
    if off {
        return Ok(Done {
            lines: vec![format!(
                "twin {p}: Mirror bot disconnected; skills are listed by hand again"
            )],
            data: json!({"pair_id": p, "connected": false}),
            warnings: vec![],
            attention: false,
        });
    }
    let briefs = mirror::write_briefs(&e.mirror_args(&p, &["status"], &[])?, &p).map_err(engine)?;
    let label = |x: &Path| {
        x.strip_prefix(&e.state)
            .map(|r| format!("state:{}", r.display()))
            .unwrap_or_default()
    };
    let steps = vec![
        json!({"id": "mirror_bot", "what": "If your Grok account has no Mirror bot yet, create one in the Grok app and paste its charter (below) as its instructions."}),
        json!({"id": "share_folder", "what": "Share the pair folder with the Mirror bot's Google Drive connection (the folder your sync app keeps in step)."}),
        json!({"id": "add_pair", "what": format!("In the Mirror bot's chat, attach the brief AGENT-MIRROR.md and the pair key FILE (attach the file; never paste its text), and ask: Please add the pair {p} for my bot (agent id {grok_id}); it is a twin, so use pair-skills. This request is mine.")}),
        json!({"id": "test", "what": "Press Test on the Mirror bot's routine, then Sync now here."}),
    ];
    Ok(Done {
        lines: std::iter::once(format!(
            "twin {p}: connected to the Mirror bot; finish these steps in Grok:"
        ))
        .chain(
            steps
                .iter()
                .map(|s| format!("  - {}", s["what"].as_str().unwrap_or(""))),
        )
        .collect(),
        data: json!({"pair_id": p, "connected": true, "briefs": label(&briefs), "pair_key": label(&cfg_path.with_extension("key")),
                     "steps": steps, "charter": mirror::charter()}),
        warnings: vec![],
        attention: false,
    })
}

// ---------------------------------------------------------------- parity

/// Names of the profile's Hermes cron jobs (cron/jobs.json, read leniently; empty when none).
fn cron_names(pdir: &Path) -> Vec<String> {
    use std::io::Read;
    // opened once without following a link, checked and read through that handle, with a size cap
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut o,
        rustix::fs::OFlags::NOFOLLOW.bits() as i32,
    );
    let Ok(f) = o.open(pdir.join("cron/jobs.json")) else {
        return vec![];
    };
    const CAP: u64 = 4 << 20;
    if !f.metadata().is_ok_and(|m| m.is_file() && m.len() <= CAP) {
        return vec![];
    }
    let mut b = Vec::new();
    if f.take(CAP + 1).read_to_end(&mut b).is_err() || b.len() as u64 > CAP {
        return vec![];
    }
    let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
    let jobs = v.get("jobs").cloned().unwrap_or(v);
    let list: Vec<Value> = match jobs {
        Value::Array(a) => a,
        Value::Object(o) => o.into_values().collect(),
        _ => vec![],
    };
    list.iter()
        .filter_map(|j| j.get("name").and_then(Value::as_str).map(str::to_lowercase))
        .collect()
}

/// `twin parity`: does the twin hold what the Grok bot holds, as far as botport can see? One check per item, one per
/// routine the Grok app's cache names, and one for a model. No model is called; a behaviour test is `botport test`.
fn parity(a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let p = one_pair(a, &idx)?.to_string();
    let r = &idx.pairs[&p];
    let pdir = e.profile_dir(&r.profile);
    let plan = mirror::run(&e.mirror_args(&p, &["plan"], &["--json"])?).map_err(engine)?;
    let mut checks = vec![];
    for it in plan.json["items"].as_array().into_iter().flatten() {
        let item = it["item"].as_str().unwrap_or("");
        let Ok(id) = ItemId::parse(item) else {
            continue;
        };
        let action = it["action"].as_str().unwrap_or("");
        let pass = action == "in-sync";
        let detail = match action {
            "in-sync" => "the same on both sides".to_string(),
            "unsupported" => format!("not carried: {}", masked(it["why"].as_str().unwrap_or(""))),
            "conflict" => "changed on both sides: pick one".into(),
            "to-local" | "needs-approval" => "a Grok change waits to reach the twin".into(),
            "to-remote" => "a Hermes change waits to reach Grok".into(),
            "in-flight" => "on its way to Grok (Mirror bot)".into(),
            a => a.replace('-', " "),
        };
        checks.push(json!({"id": item, "name": if id.kind() == Kind::Persona { kind_word(&id).to_string() } else { format!("{} {}", kind_word(&id), item_name(&id)) }, "pass": pass, "detail": detail}));
    }
    let jobs = cron_names(&pdir);
    for n in &r.routines {
        let have = jobs.iter().any(|j| j.contains(&n.to_lowercase()));
        checks.push(json!({"id": format!("routine:{n}"), "name": format!("routine {n}"), "pass": have,
            "detail": if have { "a Hermes cron job with this name exists".to_string() } else { "not recreated in Hermes yet (Grok keeps routine schedules in the cloud)".into() }}));
    }
    let model = !crate::adapters::hermes::gateway::model(&pdir)
        .model
        .is_empty();
    checks.push(json!({"id": "model", "name": "model", "pass": model,
        "detail": if model { "the twin has a model set" } else { "the twin has no model set: hermes -p <profile> model" }}));
    let passed = checks.iter().filter(|c| c["pass"] == true).count();
    Ok(Done {
        lines: std::iter::once(format!(
            "twin {p}: {passed}/{} setup checks pass",
            checks.len()
        ))
        .chain(checks.iter().filter(|c| c["pass"] == false).map(|c| {
            format!(
                "  FAIL {}: {}",
                c["name"].as_str().unwrap_or(""),
                c["detail"].as_str().unwrap_or("")
            )
        }))
        .collect(),
        attention: passed < checks.len(),
        data: json!({"passed": passed, "total": checks.len(), "kind": "setup", "checks": checks}),
        warnings: vec![],
    })
}

// ---------------------------------------------------------------- remove

/// What botport keeps in a twinned profile (removed with the twin; the profile's own files stay).
const PROFILE_FILES: &[&str] = &[mirror::STATE_FILE, mirror::REPORT_FILE, "botport-manual.md"];
const PROFILE_DIRS: &[&str] = &["botport", "backups/botport"];

/// A pair id or profile name safe as one path component: 1-64 of `a-z A-Z 0-9 . _ -`, not starting with `.`.
pub(crate) fn safe_component(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with('.')
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn remove_path(p: &Path) -> R<bool> {
    match fs::symlink_metadata(p) {
        Err(_) => Ok(false),
        Ok(m) if m.is_dir() => fs::remove_dir_all(p)
            .map(|_| true)
            .map_err(|x| Fail::from_internal(format!("remove: {x}"))),
        Ok(_) => fs::remove_file(p)
            .map(|_| true)
            .map_err(|x| Fail::from_internal(format!("remove: {x}"))),
    }
}

fn remove(a: &Args, e: &Env) -> R<Done> {
    let dry = a.has("dry-run");
    let _lock = if dry { None } else { lock(e)? };
    let mut idx = load_index(e)?;
    let p = one_pair(a, &idx)?.to_string();
    // the index is a file on disk: its names are checked again before anything is deleted
    let profile = idx.pairs[&p].profile.clone();
    if !safe_component(&p) || !safe_component(&profile) {
        return Err(Fail::new(
            "internal",
            4,
            "the twin index holds an invalid pair or profile name; nothing was removed",
        ));
    }
    let pdir = e.profile_dir(&profile);
    // a profile folder that is a link is not removed from: its files may be anyone's
    if fs::symlink_metadata(&pdir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Fail::new(
            "internal",
            4,
            "the twin's profile folder is a link; nothing was removed",
        ));
    }
    // one botport run at a time on the profile
    // a profile already deleted by hand leaves nothing to lock there (and nothing of it to remove)
    let _plock = if dry || !pdir.is_dir() {
        None
    } else {
        crate::core::util::OpLock::try_acquire(&pdir).map_err(engine)?
    };
    let mut targets: Vec<PathBuf> = vec![
        e.config(&p),
        e.config(&p).with_extension("key"),
        e.grok_dir().join(&p),
        e.state.join("sources").join(&p),
    ];
    targets.extend(PROFILE_FILES.iter().map(|f| pdir.join(f)));
    targets.extend(PROFILE_DIRS.iter().map(|d| pdir.join(d)));
    let mut removed = vec![];
    // every target must resolve inside the state folder or the profile (a link on the way is never followed out)
    let roots: Vec<PathBuf> = [e.state.canonicalize().ok(), pdir.canonicalize().ok()]
        .into_iter()
        .flatten()
        .collect();
    for t in &targets {
        let Some(parent) = t.parent().and_then(|x| x.canonicalize().ok()) else {
            continue;
        };
        // no link anywhere between its folder (state or profile) and the target itself
        let root = if t.starts_with(&e.state) {
            e.state.clone()
        } else {
            pdir.clone()
        };
        if let Ok(rel) = t.strip_prefix(&root) {
            let mut q = root.clone();
            for c in rel.components() {
                q.push(c);
                if fs::symlink_metadata(&q).is_ok_and(|m| m.file_type().is_symlink()) && &q != t {
                    return Err(Fail::new(
                        "internal",
                        4,
                        "a folder on the way to a twin file is a link; nothing was removed",
                    ));
                }
            }
        }
        if !roots.iter().any(|r| parent.starts_with(r)) {
            return Err(Fail::new(
                "internal",
                4,
                "a twin file is outside its folders; nothing was removed",
            ));
        }
    }
    for t in &targets {
        let label = if let Ok(r) = t.strip_prefix(&e.state) {
            format!("state:{}", r.display())
        } else if let Ok(r) = t.strip_prefix(&e.hermes_home) {
            format!("hermes:{}", r.display())
        } else {
            continue;
        };
        if dry {
            if fs::symlink_metadata(t).is_ok() {
                removed.push(label);
            }
        } else if remove_path(t)? {
            removed.push(label);
        }
    }
    if !dry {
        idx.pairs.remove(&p);
        save_index(e, &idx)?;
        settle(e, &idx, &[], &BTreeMap::new())?;
    }
    Ok(Done {
        lines: vec![format!(
            "twin {p} {}: {} path(s)",
            if dry { "would be removed" } else { "removed" },
            removed.len()
        )],
        data: json!({"pair_id": p, "removed": removed, "dry_run": dry, "profile_kept": true}),
        warnings: vec![],
        attention: false,
    })
}

// ---------------------------------------------------------------- recent, doctor

fn recent(a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let p = one_pair(a, &idx)?;
    let limit: usize = a
        .get("limit")
        .map_or(Ok(10), str::parse)
        .map_err(|_| usage("--limit must be a number"))?;
    let entries: Vec<Value> = idx.pairs[p]
        .recent
        .iter()
        .rev()
        .take(limit.min(MAX_RECENT))
        .map(|r| json!({"at": r.at, "side": r.side, "kind": r.kind, "summary": r.summary}))
        .collect();
    Ok(Done {
        lines: entries
            .iter()
            .map(|x| {
                format!(
                    "{} {}",
                    x["at"].as_str().unwrap_or(""),
                    x["summary"].as_str().unwrap_or("")
                )
            })
            .collect(),
        data: json!({"entries": entries}),
        warnings: vec![],
        attention: false,
    })
}

fn doctor(_a: &Args, e: &Env) -> R<Done> {
    let idx = load_index(e)?;
    let mut checks = vec![];
    let mut add = |id: String, ok: bool, message: String, fix: &str| {
        checks.push(json!({"id": id, "ok": ok, "message": message, "fix": fix}));
    };
    add(
        "botport_version".into(),
        true,
        format!("botport {}", crate::adapters::grok_bot::bundle::VERSION),
        "",
    );
    let app = e.app_dir(&idx);
    let found = app
        .as_ref()
        .is_some_and(|d| d.join(cache::CACHE_DIR).is_dir());
    add(
        "grok_app".into(),
        found,
        if found {
            "the Grok desktop app's cache is readable".into()
        } else {
            "the Grok desktop app's cache was not found".into()
        },
        if found {
            ""
        } else {
            "install and open the Grok desktop app, or try botport twin demo"
        },
    );
    let hermes = which(&e.hermes_bin);
    add(
        "hermes_cli".into(),
        hermes,
        if hermes {
            "hermes is on PATH".into()
        } else {
            "hermes is not on PATH".into()
        },
        if hermes {
            ""
        } else {
            "install Hermes Agent; routines need the hermes CLI"
        },
    );
    let writable = crate::core::util::ensure_private_dir(&e.state).is_ok();
    add(
        "state_dir".into(),
        writable,
        if writable {
            "the state folder is writable".into()
        } else {
            "the state folder is not writable".into()
        },
        "",
    );
    // metadata only, safe to attach to a bug report: twins are numbered, never named
    for (n, (p, r)) in idx.pairs.iter().enumerate() {
        let files =
            e.config(p).is_file() && e.profile_dir(&r.profile).join(mirror::STATE_FILE).is_file();
        // the Grok side's files must come from the same botport release (the box's copy is the owner's to update)
        let same_release = !files
            || e.mirror_args(p, &["plan"], &["--json"])
                .ok()
                .and_then(|a| mirror::run(&a).err())
                .is_none_or(|x| !x.contains(crate::core::package::NEUTRAL_FORM_MISMATCH));
        let ok = files && same_release;
        add(
            format!("twin-{}", n + 1),
            ok,
            if ok {
                "pair set up".into()
            } else if files {
                "the Grok box runs another botport release: update it to this one".into()
            } else {
                "pair files missing".into()
            },
            if ok {
                ""
            } else if files {
                "give the Mirror bot the botport release that runs here (see twin mirror's steps)"
            } else {
                "set the twin up again: botport twin create"
            },
        );
    }
    let bad = checks.iter().any(|c| c["ok"] == false);
    Ok(Done {
        lines: checks
            .iter()
            .map(|c| {
                format!(
                    "{} {}",
                    if c["ok"] == true { "ok  " } else { "FAIL" },
                    c["message"].as_str().unwrap_or("")
                )
            })
            .collect(),
        data: json!({"checks": checks}),
        warnings: vec![],
        attention: bad,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_looking_strings_are_caught_and_prose_is_not() {
        assert_eq!(
            random_looking(concat!(
                "Answer briefly.\nvalue ",
                "Zx81Kq0LmP2vT7rB",
                "9nW4cY6hJ3sD5fG1aQ8eR"
            )),
            Some(2)
        );
        assert_eq!(
            random_looking("see https://example.invalid/a?t=0123456789abcdef0123456789abcdef01"),
            Some(1)
        );
        assert_eq!(
            random_looking("You are Acme Helper, the help desk for Acme widgets."),
            None
        );
        assert_eq!(
            random_looking("internationalization-and-localization-guidelines-v2"),
            None
        );
    }

    #[test]
    fn secret_location_keeps_the_reason_whole() {
        let l = secret_location("description", "$[0]{entry 2} looks like it holds a secret (token-shaped value (sk-/xai-/gh*_/xox*-/AKIA...))");
        assert_eq!(l, "suspected secret in the description: token-shaped value (sk-/xai-/gh*_/xox*-/AKIA...) (a check, not a guarantee)");
        assert!(
            secret_location("instructions", "line 3 looks like it holds a secret (JWT)")
                .contains("instructions, line 3: JWT")
        );
    }

    #[test]
    fn masked_hides_secret_lines_only() {
        let t = format!(
            "Be kind.\nkey {}{}\nBye.\n",
            "sk-", "demo-0000NOTAREALKEY0000"
        );
        let m = masked(&t);
        assert_eq!(
            m,
            "Be kind.\n[line hidden by botport: suspected secret]\nBye.\n"
        );
    }

    #[test]
    fn more_key_formats_are_caught() {
        for k in [
            concat!("AIza", "SyA", "00000000000000000000000000000000"),
            concat!("sk_live_", "00000000NOTAREALKEY00"),
            concat!("hf_", "0000000000NOTAREALKEY0000000000"),
            concat!(
                "-----BEGIN OPENSSH ",
                "PRIVATE KEY-----\nAAAA\n-----END OPENSSH ",
                "PRIVATE KEY-----"
            ),
        ] {
            assert!(
                crate::core::secrets::find_secret("persona.instructions", k.as_bytes()).is_some(),
                "{k}"
            );
        }
    }
}
