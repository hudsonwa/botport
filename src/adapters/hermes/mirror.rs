//! Mirror mode's Hermes side (docs/design/MIRROR.md sections 3 and 4.3): a profile read into mirror items, and items
//! written back one at a time with a compare-and-swap on the live copy.
//!
//! Items and where they live:
//! - `persona.name`, `persona.instructions`: `SOUL.md` sections `persona-name` / `persona` between botport markers;
//!   text outside the markers is never touched.
//! - `skill:<name>`: `skills/<category>/<name>/` in the mirrored categories (the first one is where new skills go),
//!   carried byte for byte as packages.
//! - `routine:<name>`: the cron job of that name, written only through the `hermes` CLI.
//! - `connector:<id>`: `config.yaml` `mcp_servers.<id>` (url and `tools.include`; header and auth values are never
//!   read or written).
use super::sync::{self, live_jobs};
use crate::core::item::{canonical_text, Content};
use crate::core::mirror::{Coverage, ItemId, Kind, SideRead, View};
use crate::core::model::{Schedule, ScheduleKind};
use crate::core::package::{Limits, Package};
use crate::core::secrets;
use crate::core::util::{err, json_sorted, sha256_hex, utc_stamp, write_atomic, Res};
use serde_json::{json, Value};
use serde_yaml::{Mapping, Value as Y};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// SOUL.md section that holds `persona.name`.
pub const SOUL_NAME: &str = "persona-name";
/// SOUL.md section that holds `persona.instructions`.
pub const SOUL_INSTRUCTIONS: &str = "persona";
/// SOUL.md section that holds `persona.description` (the short text the Grok app shows for the bot).
pub const SOUL_DESCRIPTION: &str = "persona-description";

/// The SOUL.md section a persona item lives in.
pub fn soul_section(id: &ItemId) -> &'static str {
    match id.as_str() {
        "persona.name" => SOUL_NAME,
        "persona.description" => SOUL_DESCRIPTION,
        _ => SOUL_INSTRUCTIONS,
    }
}
/// The paused reason botport sets on a routine it paused until the owner approves enabling it.
pub const PAUSED_REASON: &str = "botport mirror: paused until the owner approves enabling it";

/// One Hermes profile as a mirror side.
#[derive(Debug, Clone)]
pub struct HermesSide {
    /// The profile directory.
    pub dest: PathBuf,
    /// The profile name (for the `hermes` CLI).
    pub profile: String,
    /// The `hermes` binary.
    pub hermes_bin: String,
    /// HERMES_HOME for the CLI, when it is not the default.
    pub hermes_home: Option<PathBuf>,
    /// Mirrored skill categories; the first is where new skills are written.
    pub categories: Vec<String>,
    /// Whether routines can be written (only through the `hermes` CLI, so only for a real profile).
    pub cron: bool,
    /// Routines botport holds paused until the owner approves enabling them (from the mirror state), each with the
    /// side digest of the copy botport wrote: a held job that is still that paused copy reads as enabled, pending
    /// approval; a job the owner has changed since (resumed, paused again, edited) is read as it is.
    pub held: BTreeMap<String, Option<String>>,
}

/// What a read of the profile found.
#[derive(Debug, Clone, Default)]
pub struct LocalItems {
    /// Items and coverage, for the merge engine.
    pub read: SideRead,
    /// Each item's content.
    pub content: BTreeMap<ItemId, Content>,
    /// Items found that cannot be mirrored, with the reason (an invalid name, a package over the limits, ...).
    pub unsupported: BTreeMap<String, String>,
}

/// The view of a content as this side stores it: the side digest is over the stored form (`side`), the neutral one
/// over the content.
fn view(id: &ItemId, c: &Content, side: &[u8]) -> View {
    View::new(sha256_hex(side), c.neutral_digest(id))
}

/// Whether Hermes would run the job: `enabled` and no pause marker (`state` "paused" or a `paused_at`), as Hermes'
/// own `is_job_runnable` decides (cron/jobs.py, https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/cron/jobs.py#L531-L539).
pub fn job_runnable(j: &Value) -> bool {
    // Python truthiness, as Hermes reads the record: a missing `enabled` is true; null, false, 0 or "" are false
    let truthy = |v: &Value| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    };
    let enabled = j.get("enabled").is_none_or(truthy);
    let marker =
        j["state"].as_str().is_some_and(|s| s.trim() == "paused") || truthy(&j["paused_at"]);
    enabled && !marker
}

/// The job fields that make up a routine on this side (Hermes' job record: https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/cron/jobs.py#L1896-L1940).
/// `paused_at` is included so a job paused again by hand is not the copy botport wrote.
fn job_side_form(j: &Value) -> Vec<u8> {
    let v = json!({
        "schedule": sync::cron_expr(j),
        "tz": job_tz(j),
        "prompt": j["prompt"],
        "enabled": job_runnable(j),
        "paused_reason": j["paused_reason"],
        "paused_at": j["paused_at"],
        "deliver": j["deliver"],
    });
    v.to_string().into_bytes()
}

/// The first line of a failed hermes call's stderr, for the error: the values botport passed (which can be another
/// side's text) are masked, a line that looks like it holds a secret is left out, and it is cut short.
fn stderr_hint(stderr: &[u8], args: &[&str]) -> String {
    let first = String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    let mut line = first;
    for a in args {
        // the whole argument and its value after `=`, each wherever it is repeated
        for v in [*a, a.split_once('=').map_or(*a, |(_, v)| v)] {
            if v.len() >= 4 {
                line = line.replace(v, "…");
            }
        }
    }
    // never a control or direction character on the owner's terminal
    let line: String = line
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                '?'
            } else {
                c
            }
        })
        .collect();
    if line.is_empty() || secrets::find_secret("hermes stderr", line.as_bytes()).is_some() {
        return String::new();
    }
    let cut: String = line.chars().take(160).collect();
    format!(": {cut}")
}

/// The error for a failed hermes call. A command that carried a routine prompt (`cron create`, or `--prompt=`) gets
/// only its exit code and a fixed hint: its stderr can repeat the prompt, so none of it is shown. Any other command
/// gets the first stderr line ([`stderr_hint`]), shown escaped.
fn failure_message(args: &[&str], code: i32, stderr: &[u8]) -> String {
    let (verb, sub) = (
        args.first().copied().unwrap_or(""),
        args.get(1).copied().unwrap_or(""),
    );
    let carried_prompt = sub == "create" || args.iter().any(|a| a.starts_with("--prompt="));
    if carried_prompt {
        return format!(
            "hermes {verb} {sub} failed (exit {code}); its output is not shown because the command carried a routine \
             prompt: run `hermes cron list` to see the job, or rerun with the Hermes logs open"
        );
    }
    format!(
        "hermes {verb} {sub} failed (exit {code}){}",
        crate::core::util::name_safe(&stderr_hint(stderr, args))
    )
}

/// A job's own zone: Hermes keeps it in the top-level `timezone` (empty: the profile's default).
fn job_tz(j: &Value) -> String {
    j["timezone"].as_str().unwrap_or("").to_string()
}

fn job_schedule(j: &Value) -> Schedule {
    let expr = sync::cron_expr(j);
    Schedule {
        name: j["name"].as_str().unwrap_or("").to_string(),
        kind: if expr.is_empty() {
            ScheduleKind::Event
        } else {
            ScheduleKind::Scheduled
        },
        cron: (!expr.is_empty()).then_some(expr),
        tz: job_tz(j),
        action: j["prompt"].as_str().unwrap_or("").to_string(),
        enabled: job_runnable(j),
    }
}

fn load_config(dest: &Path) -> Res<Option<Y>> {
    let p = dest.join("config.yaml");
    let Some(t) = crate::core::yaml11::read_config_yaml(&p)? else {
        return Ok(None);
    };
    let y: Y = crate::core::yaml11::parse(&crate::core::yaml11::read(&t))
        .map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(Some(y))
}

/// The connector fields botport reads from an `mcp_servers` entry: url and `tools.include`.
fn connector_fields(id: &str, v: &Y) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), json!(id));
    if let Some(u) = v.get("url").and_then(Y::as_str) {
        m.insert("url".to_string(), json!(u));
        let http = u.starts_with("http://") || u.starts_with("https://");
        m.insert(
            "kind".to_string(),
            json!(if http { "mcp-http" } else { "mcp-stdio" }),
        );
    } else {
        m.insert("kind".to_string(), json!("mcp-stdio"));
    }
    let tools: Vec<String> = v
        .get("tools")
        .and_then(|t| t.get("include"))
        .and_then(Y::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    if !tools.is_empty() {
        m.insert("tools".to_string(), json!(tools));
    }
    m
}

/// SOUL.md sections by name (only text between botport markers).
fn soul_marked(dest: &Path) -> BTreeMap<String, String> {
    let Some(t) = sync::read_soul(&dest.join("SOUL.md")) else {
        return BTreeMap::new();
    };
    if !t.contains("<!-- botport:begin ") {
        return BTreeMap::new();
    }
    sync::soul_sections(&t).into_iter().collect()
}

fn marker(kind: &str, n: &str) -> String {
    format!("<!-- botport:{kind} {n} -->")
}

/// `text` with section `n` set to `c` (replaced in place, or appended) or removed (`None`); everything else kept.
pub fn soul_with(text: &str, n: &str, c: Option<&str>) -> String {
    let (b, e) = (marker("begin", n), marker("end", n));
    let block = c.map(|c| {
        let nl = if c.ends_with('\n') || c.is_empty() {
            ""
        } else {
            "\n"
        };
        format!("{b}\n{c}{nl}{e}\n")
    });
    if let Some(s) = text.find(&b) {
        if let Some(rel) = text[s..].find(&e) {
            let end = s + rel + e.len();
            let end = if text[end..].starts_with('\n') {
                end + 1
            } else {
                end
            };
            return format!(
                "{}{}{}",
                &text[..s],
                block.unwrap_or_default(),
                &text[end..]
            );
        }
    }
    match block {
        None => text.to_string(),
        Some(bl) if text.is_empty() => bl,
        Some(bl) => format!("{text}{}{bl}", if text.ends_with('\n') { "" } else { "\n" }),
    }
}

impl HermesSide {
    /// Read every mirrored item. A local read sees everything, so coverage is complete for every kind.
    pub fn read(&self) -> Res<LocalItems> {
        let mut out = LocalItems {
            read: SideRead::complete(),
            ..LocalItems::default()
        };
        let add = |out: &mut LocalItems, raw: String, c: Content, side: Vec<u8>| match ItemId::parse(
            &raw,
        ) {
            Ok(id) => {
                out.read.items.insert(id.clone(), view(&id, &c, &side));
                out.content.insert(id, c);
            }
            Err(e) => {
                out.unsupported.insert(raw, e);
            }
        };
        let soul = soul_marked(&self.dest);
        for (sec, id) in [
            (SOUL_NAME, "persona.name"),
            (SOUL_DESCRIPTION, "persona.description"),
            (SOUL_INSTRUCTIONS, "persona.instructions"),
        ] {
            if let Some(t) = soul.get(sec) {
                add(
                    &mut out,
                    id.into(),
                    Content::Text(t.clone()),
                    t.clone().into_bytes(),
                );
            }
        }
        for cat in &self.categories {
            let root = self.dest.join("skills").join(cat);
            // a skill a crash left aside in the middle of a swap is put back before anything reads the folder
            crate::core::package::recover_into(&self.stage_dir(&root), &root)?;
            crate::core::package::recover(&root)?;
            let Ok(rd) = fs::read_dir(&root) else {
                continue;
            };
            let mut names: Vec<String> = rd
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .filter(|n| !n.starts_with('.'))
                .collect();
            names.sort();
            for n in names {
                let raw = format!("skill:{n}");
                if out.content.keys().any(|k| k.as_str() == raw) {
                    out.unsupported.insert(
                        raw,
                        "the same skill name is in two mirrored categories".into(),
                    );
                    continue;
                }
                match Package::read(&root.join(&n), &n, &Limits::default()) {
                    Ok(p) => match p.secret_at() {
                        Some(at) => {
                            out.unsupported
                                .insert(raw, format!("looks like it holds a secret ({at})"));
                        }
                        None => {
                            let d = p.digest();
                            add(&mut out, raw, Content::Package(p), d.into_bytes());
                        }
                    },
                    Err(e) => {
                        out.unsupported.insert(raw, e);
                    }
                }
            }
        }
        let mut job_names = BTreeSet::new();
        for j in live_jobs(&self.dest)? {
            let mut s = job_schedule(&j);
            // two jobs with one name: which one is the routine is the owner's call; neither is carried
            if !job_names.insert(s.name.clone()) {
                let raw = format!("routine:{}", s.name);
                if let Ok(i) = ItemId::parse(&raw) {
                    out.content.remove(&i);
                }
                out.unsupported
                    .insert(raw, "two jobs have this name".into());
                continue;
            }
            // a job botport holds paused until the owner approves enabling it is that routine, enabled pending
            // approval: in step with an enabled copy on the other side (its side digest still says paused)
            let side = job_side_form(&j);
            if !s.enabled && self.holds(&s.name, &side) {
                s.enabled = true;
            }
            add(
                &mut out,
                format!("routine:{}", s.name),
                Content::Routine(s),
                side,
            );
        }
        if let Some(y) = load_config(&self.dest)? {
            if let Some(m) = y.get("mcp_servers").and_then(Y::as_mapping) {
                let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for (k, v) in m {
                    let Some(key) = k.as_str() else { continue };
                    // a server Hermes does not load (`enabled: false`, https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/website/docs/user-guide/features/mcp.md#L506)
                    // is not offered: it is held on both sides, never read as removed
                    if v.get("enabled").and_then(Y::as_bool) == Some(false) {
                        out.unsupported.insert(
                            format!("connector:{key}"),
                            "disabled on the Hermes side (enabled: false)".into(),
                        );
                        continue;
                    }
                    let has_url = v.get("url").and_then(Y::as_str).is_some();
                    let jv = serde_json::to_value(v).unwrap_or_default();
                    let row = crate::adapters::connectors::row_for_hermes(key, &jv);
                    // a local-command server under a public connector's name is not that connector: never mirrored
                    if let Some(r) = crate::adapters::connectors::find(key).filter(|_| !has_url) {
                        // held under the owner's key and under the connector's own id (however the key is spelled)
                        for raw in [format!("connector:{key}"), format!("connector:{}", r.id)] {
                            out.unsupported.insert(
                                raw,
                                "a local-command server under a public connector's name".into(),
                            );
                        }
                        continue;
                    }
                    // an entry of a public connector (its official url) is that connector, whatever the owner's key
                    let id = row
                        .filter(|_| has_url)
                        .map_or(key.to_string(), |r| r.id.clone());
                    by_id.entry(id.clone()).or_default().push(key.to_string());
                    let f = connector_fields(&id, v);
                    let side = json_sorted(&f).into_bytes();
                    add(
                        &mut out,
                        format!("connector:{id}"),
                        Content::Connector(f),
                        side,
                    );
                }
                // two entries for one public connector: which one to carry is the owner's call
                for (id, keys) in by_id.into_iter().filter(|(_, k)| k.len() > 1) {
                    if let Ok(i) = ItemId::parse(&format!("connector:{id}")) {
                        out.read.items.remove(&i);
                        out.content.remove(&i);
                    }
                    out.unsupported.insert(
                        format!("connector:{id}"),
                        format!(
                            "{} entries in config.yaml are the same connector",
                            keys.len()
                        ),
                    );
                }
            }
        }
        // the per-side digest of a skill is its package digest, not a hash of it
        for (id, c) in &out.content {
            if let Content::Package(p) = c {
                out.read
                    .items
                    .insert(id.clone(), View::new(p.digest(), p.neutral_digest()));
            }
        }
        for k in [Kind::Persona, Kind::Skill, Kind::Routine, Kind::Connector] {
            out.read.coverage.insert(k, Coverage::Complete);
        }
        // an item that is there but cannot be offered must not read as deleted
        for (raw, why) in &out.unsupported {
            if let Ok(id) = ItemId::parse(raw) {
                out.read.items.remove(&id);
                out.read.unsupported.insert(id, why.clone());
            }
        }
        Ok(out)
    }

    /// The current view of one item (None: absent).
    pub fn current(&self, id: &ItemId) -> Res<Option<View>> {
        Ok(self.read()?.read.items.get(id).cloned())
    }

    /// Where a skill of category folder `cat_dir` is assembled before it is moved in: inside the profile, outside its
    /// skills folder (one staging folder per category).
    fn stage_dir(&self, cat_dir: &Path) -> PathBuf {
        let cat = cat_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.dest.join("botport/stage").join(cat)
    }

    fn skill_dir(&self, name: &str) -> PathBuf {
        for cat in &self.categories {
            let d = self.dest.join("skills").join(cat);
            if d.join(name).is_dir() {
                return d;
            }
        }
        self.dest.join("skills").join(&self.categories[0])
    }

    /// Write `c` as item `id`, but only if the live copy still has digest `expect` (None: must be absent); returns
    /// the digest read back after the write, or `None` when the live copy changed meanwhile (skipped, nothing
    /// written). `paused` writes a routine paused by policy (until the owner approves enabling it).
    pub fn write(
        &self,
        id: &ItemId,
        c: &Content,
        expect: Option<&str>,
        paused: bool,
    ) -> Res<Option<String>> {
        if self.current(id)?.map(|v| v.digest).as_deref() != expect {
            return Ok(None);
        }
        match (id.kind(), c) {
            (Kind::Persona, Content::Text(t)) if t.contains("<!-- botport:") => {
                return err(format!(
                    "{id}: the text holds a botport marker; not written"
                ));
            }
            (Kind::Persona, Content::Text(t)) => {
                let sec = soul_section(id);
                let p = self.dest.join("SOUL.md");
                let live = sync::read_soul(&p).unwrap_or_default();
                let text = if id.as_str() == "persona.name" {
                    canonical_text(id, t)
                } else {
                    t.clone()
                };
                write_atomic(
                    &p,
                    soul_with(&live, sec, Some(&text)).as_bytes(),
                    Some(0o600),
                )?;
            }
            (Kind::Skill, Content::Package(p)) => {
                if p.name != id.name() {
                    return err(format!("{id}: package has another name"));
                }
                let dir = self.skill_dir(id.name());
                p.write_via(&dir, &self.stage_dir(&dir), false)?;
            }
            (Kind::Routine, Content::Routine(s)) => self.write_routine(id, s, paused)?,
            (Kind::Connector, Content::Connector(m)) => self.write_connector(id, Some(m))?,
            _ => return err(format!("{id}: content of the wrong kind")),
        }
        Ok(self.current(id)?.map(|v| v.digest))
    }

    /// Retire item `id` if its live copy still has digest `expect`: a skill moves to `backups/botport/retired-<stamp>/`,
    /// a SOUL section is removed, a routine is removed through the `hermes` CLI, a connector entry is removed.
    /// Returns false when the live copy changed meanwhile (nothing done).
    pub fn retire(&self, id: &ItemId, expect: &str) -> Res<bool> {
        if self.current(id)?.map(|v| v.digest).as_deref() != Some(expect) {
            return Ok(false);
        }
        match id.kind() {
            Kind::Persona => {
                let sec = soul_section(id);
                let p = self.dest.join("SOUL.md");
                let live = sync::read_soul(&p).unwrap_or_default();
                write_atomic(&p, soul_with(&live, sec, None).as_bytes(), Some(0o600))?;
            }
            Kind::Skill => {
                let from = self.skill_dir(id.name()).join(id.name());
                let to = self
                    .dest
                    .join(sync::BACKUPS)
                    .join(format!("retired-{}", utc_stamp()));
                fs::create_dir_all(&to).map_err(|e| format!("mkdir {}: {e}", to.display()))?;
                fs::rename(&from, to.join(id.name())).map_err(|e| format!("retire {id}: {e}"))?;
            }
            Kind::Routine => {
                let jid = self
                    .job_id(id.name())?
                    .ok_or_else(|| format!("{id}: job not found"))?;
                self.hermes(&["cron", "remove", "--", &jid])?;
            }
            Kind::Connector => self.write_connector(id, None)?,
        }
        Ok(true)
    }

    /// Whether the routine `id` is a job botport paused until the owner approves enabling it.
    pub fn paused_by_policy(&self, id: &ItemId) -> Res<bool> {
        Ok(live_jobs(&self.dest)?.iter().any(|j| {
            j["name"].as_str() == Some(id.name())
                && !job_runnable(j)
                && self.holds(id.name(), &job_side_form(j))
        }))
    }

    /// Whether a paused job (in its side form) is one botport holds: the hold names it, and the job is still exactly
    /// the copy botport wrote (a hold without a digest is one being written now).
    fn holds(&self, name: &str, side: &[u8]) -> bool {
        match self.held.get(name) {
            Some(Some(d)) => *d == sha256_hex(side),
            Some(None) => true,
            None => false,
        }
    }

    /// Whether the routine `id` exists and is actually running (not paused) on this side.
    pub fn running(&self, id: &ItemId) -> Res<bool> {
        Ok(live_jobs(&self.dest)?
            .iter()
            .any(|j| j["name"].as_str() == Some(id.name()) && job_runnable(j)))
    }

    /// Switch on a routine botport paused awaiting approval; returns its side digest read back.
    pub fn resume(&self, id: &ItemId) -> Res<String> {
        if !self.cron {
            return err(format!(
                "{id}: routines are switched on only through the hermes CLI"
            ));
        }
        let jid = self
            .job_id(id.name())?
            .ok_or_else(|| format!("{id}: job not found"))?;
        self.hermes(&["cron", "resume", "--", &jid])?;
        self.current(id)?
            .map(|v| v.digest)
            .ok_or_else(|| format!("{id}: gone after resume"))
    }

    fn job_id(&self, name: &str) -> Res<Option<String>> {
        let jobs: Vec<Value> = live_jobs(&self.dest)?
            .into_iter()
            .filter(|j| j["name"].as_str() == Some(name))
            .collect();
        if jobs.len() > 1 {
            return err(format!("two cron jobs are named {name:?}"));
        }
        Ok(jobs
            .first()
            .and_then(|j| j["id"].as_str().map(String::from)))
    }

    fn hermes(&self, args: &[&str]) -> Res<()> {
        let pairs: Vec<(&str, &str)> = args.iter().map(|a| ("arg", *a)).collect();
        secrets::guard_args("hermes cron", &pairs)?;
        let mut cmd = Command::new(&self.hermes_bin);
        if let Some(h) = &self.hermes_home {
            cmd.env("HERMES_HOME", h);
        }
        cmd.args(["-p", &self.profile]).args(args);
        let o = cmd
            .output()
            .map_err(|e| format!("{}: {e}", self.hermes_bin))?;
        if !o.status.success() {
            return err(failure_message(
                args,
                o.status.code().unwrap_or(-1),
                &o.stderr,
            ));
        }
        Ok(())
    }

    /// Whether this Hermes's `cron create` takes `--timezone` (read once per Hermes binary, keyed by its path, size,
    /// mtime and inode so an upgraded Hermes is asked again; a help that cannot be read counts as yes, and the create
    /// itself then says what went wrong).
    fn cron_has_timezone(&self) -> bool {
        type Key = (String, Option<(u64, Option<std::time::SystemTime>, u64)>);
        static SEEN: std::sync::Mutex<Vec<(Key, bool)>> = std::sync::Mutex::new(Vec::new());
        let id = fs::metadata(&self.hermes_bin).ok().map(|m| {
            #[cfg(unix)]
            let ino = std::os::unix::fs::MetadataExt::ino(&m);
            #[cfg(not(unix))]
            let ino = 0;
            (m.len(), m.modified().ok(), ino)
        });
        let bin = (self.hermes_bin.clone(), id);
        if let Some(v) = SEEN
            .lock()
            .ok()
            .and_then(|s| s.iter().find(|(b, _)| *b == bin).map(|(_, v)| *v))
        {
            return v;
        }
        let mut cmd = Command::new(&self.hermes_bin);
        if let Some(h) = &self.hermes_home {
            cmd.env("HERMES_HOME", h);
        }
        let has = match cmd
            .args(["-p", &self.profile, "cron", "create", "--help"])
            .output()
        {
            Ok(o) if o.status.success() => {
                let text = [o.stdout, o.stderr].concat();
                String::from_utf8_lossy(&text).contains("--timezone")
            }
            _ => true,
        };
        if let Ok(mut s) = SEEN.lock() {
            s.push((bin, has));
        }
        has
    }

    fn write_routine(&self, id: &ItemId, s: &Schedule, paused: bool) -> Res<()> {
        if !self.cron {
            return err(format!(
                "{id}: routines are written only through the hermes CLI (a named profile, or --hermes-bin)"
            ));
        }
        if s.kind == ScheduleKind::Event {
            return err(format!(
                "{id}: event-triggered routines have no Hermes cron equivalent"
            ));
        }
        let sched = super::schedule::normalize(s.cron.as_deref().unwrap_or(""))?;
        let mut tz = (!s.tz.is_empty() && s.tz != "local").then_some(s.tz.as_str());
        // a Hermes whose cron cannot store a zone: UTC (also what an export without a zone reads as) is left out, any
        // other zone is refused with the reason rather than written at the wrong time
        if tz.is_some() && !self.cron_has_timezone() {
            match tz {
                Some(z) if crate::core::routines::UTC.contains(&z.to_ascii_lowercase().as_str()) => {
                    tz = None
                }
                Some(z) => {
                    return err(format!(
                        "{id}: this Hermes cannot store a routine timezone ({}): update Hermes, or set the routine up there by hand",
                        crate::core::routines::shown_tz(z)
                    ))
                }
                None => {}
            }
        }
        let enable = s.enabled && !paused;
        // every value goes as --opt=value and every positional after `--`: no text from the other side can be read
        // as an option by the hermes CLI
        let opt = |k: &str, v: &str| format!("--{k}={v}");
        match self.job_id(id.name())? {
            None => {
                let mut a = vec![
                    "cron".to_string(),
                    "create".into(),
                    opt("name", id.name()),
                    opt("deliver", "local"),
                ];
                if let Some(tz) = tz {
                    a.push(opt("timezone", tz));
                }
                if !enable {
                    a.push("--paused".into());
                    if s.enabled {
                        a.push(opt("paused-reason", PAUSED_REASON));
                    }
                }
                a.extend(["--".into(), sched.clone(), s.action.clone()]);
                self.hermes(&a.iter().map(String::as_str).collect::<Vec<_>>())
            }
            Some(jid) => {
                let mut a = vec![
                    "cron".to_string(),
                    "edit".into(),
                    opt("schedule", &sched),
                    opt("prompt", &s.action),
                ];
                if let Some(tz) = tz {
                    a.push(opt("timezone", tz));
                }
                a.extend(["--".into(), jid.clone()]);
                self.hermes(&a.iter().map(String::as_str).collect::<Vec<_>>())?;
                self.hermes(&["cron", if enable { "resume" } else { "pause" }, "--", &jid])
            }
        }
    }

    fn write_connector(&self, id: &ItemId, m: Option<&BTreeMap<String, Value>>) -> Res<()> {
        let p = self.dest.join("config.yaml");
        let mut y = load_config(&self.dest)?.unwrap_or(Y::Mapping(Mapping::new()));
        let root = y.as_mapping_mut().ok_or("config.yaml is not a mapping")?;
        let key = Y::String("mcp_servers".into());
        if !root.contains_key(&key) {
            root.insert(key.clone(), Y::Mapping(Mapping::new()));
        }
        let servers = root
            .get_mut(&key)
            .and_then(Y::as_mapping_mut)
            .ok_or("config.yaml mcp_servers is not a mapping")?;
        // an existing entry of the same public connector keeps the owner's key; otherwise the item's id is the key
        let name = servers
            .iter()
            .find_map(|(k, v)| {
                let key = k.as_str()?;
                let row = crate::adapters::connectors::row_for_hermes(
                    key,
                    &serde_json::to_value(v).ok()?,
                )?;
                (row.id == id.name() && key != id.name()).then(|| k.clone())
            })
            .unwrap_or_else(|| Y::String(id.name().to_string()));
        match m {
            None => {
                servers.remove(&name);
            }
            Some(m) => {
                let url = m.get("url").and_then(Value::as_str).ok_or_else(|| {
                    format!("{id}: no url to write (carried as is, set it up by hand)")
                })?;
                if let Some(r) = secrets::connector_url(url) {
                    return err(format!("{id}: url refused ({r})"));
                }
                // the public url policy: https only, a private host only if that url is already configured here
                let configured: std::collections::BTreeSet<String> = servers
                    .values()
                    .filter_map(|v| v.get("url").and_then(Y::as_str).map(String::from))
                    .collect();
                crate::adapters::connectors::check_url(url, &configured)
                    .map_err(|r| format!("{id}: url refused ({r})"))?;
                if servers
                    .get(&name)
                    .is_some_and(|e| e.get("command").is_some())
                {
                    return err(format!(
                        "{id}: the entry runs a local command; a url is not added to it"
                    ));
                }
                let tools: Vec<String> = m
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect();
                // keep fields botport does not own (headers, auth) on an existing entry, but only for the same origin:
                // credentials kept for one host never go to another
                let mut entry = servers
                    .get(&name)
                    .cloned()
                    .unwrap_or(Y::Mapping(Mapping::new()));
                // an entry that had no url (a local command) and gains one counts as a move too
                let old = entry.get("url").and_then(Y::as_str).map(String::from);
                if old.is_none_or(|o| !crate::core::util::same_origin(&o, url)) {
                    if let Some(e) = entry.as_mapping_mut() {
                        e.remove(Y::String("headers".into()));
                        e.remove(Y::String("auth".into()));
                    }
                }
                let new = sync::mcp_entry(url, &tools);
                if let (Some(e), Some(n)) = (entry.as_mapping_mut(), new.as_mapping()) {
                    e.remove(Y::String("tools".into()));
                    for (k, v) in n {
                        e.insert(k.clone(), v.clone());
                    }
                }
                servers.insert(name, entry);
            }
        }
        // the same writer as v0.1's MCP entries: YAML 1.1-safe scalars, the file's leading comment lines kept
        let head: String = crate::core::yaml11::read_config_yaml(&p)?
            .unwrap_or_default()
            .lines()
            .take_while(|l| l.starts_with('#'))
            .map(|l| format!("{l}\n"))
            .collect();
        let body = crate::core::yaml11::emit_checked(&y)?;
        let text = if head.is_empty() {
            body
        } else {
            format!("{head}\n{body}")
        };
        write_atomic(&p, text.as_bytes(), Some(0o600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_call_that_carried_a_prompt_shows_no_stderr() {
        let stderr = b"error: bad prompt: Demo secret plan for Tuesday\n";
        for args in [
            &[
                "cron",
                "create",
                "--name=demo",
                "--",
                "0 9 * * 1",
                "Demo secret plan for Tuesday",
            ][..],
            &[
                "cron",
                "edit",
                "--schedule=0 9 * * 1",
                "--prompt=Demo secret plan for Tuesday",
                "--",
                "j1",
            ][..],
        ] {
            let m = failure_message(args, 2, stderr);
            assert!(m.contains("(exit 2)") && m.contains("not shown"), "{m}");
            assert!(
                !m.contains("Demo") && !m.contains("Tuesday") && !m.contains("bad prompt"),
                "{m}"
            );
        }
        // a call without a prompt shows its first stderr line, escaped
        let m = failure_message(
            &["cron", "remove", "--", "j1"],
            1,
            b"no job \xe2\x80\xae1j here\n",
        );
        assert!(m.starts_with("hermes cron remove failed (exit 1): "), "{m}");
        assert!(!m.contains('\u{202e}'), "{m}");
    }

    #[test]
    fn a_job_runs_only_when_hermes_would_run_it() {
        for (j, runs) in [
            (json!({}), true),
            (json!({"enabled": true}), true),
            (json!({"enabled": false}), false),
            (json!({"enabled": null}), false),
            (json!({"enabled": 0}), false),
            (json!({"enabled": ""}), false),
            (json!({"enabled": true, "state": "paused"}), false),
            (
                json!({"enabled": true, "paused_at": "2000-01-01T00:00:00Z"}),
                false,
            ),
            (json!({"enabled": true, "paused_at": null}), true),
            (json!({"enabled": true, "paused_at": ""}), true),
        ] {
            assert_eq!(job_runnable(&j), runs, "{j}");
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = crate::core::util::test_dir(&format!("hmirror-{tag}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn side(d: &Path) -> HermesSide {
        HermesSide {
            dest: d.to_path_buf(),
            profile: "demo-bot".into(),
            hermes_bin: "hermes-not-used".into(),
            hermes_home: None,
            categories: vec!["mirror".into(), "extra".into()],
            cron: false,
            held: BTreeMap::new(),
        }
    }

    fn id(s: &str) -> ItemId {
        ItemId::parse(s).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn an_upgraded_hermes_is_asked_again_about_timezone() {
        use std::os::unix::fs::PermissionsExt;
        let d = crate::core::util::test_dir("hermes-tz");
        let bin = d.join("hermes");
        let put_bin = |help: &str| {
            let t = d.join("hermes.new");
            fs::write(&t, format!("#!/bin/sh\necho '{help}'\n")).unwrap();
            fs::set_permissions(&t, fs::Permissions::from_mode(0o700)).unwrap();
            fs::rename(&t, &bin).unwrap();
        };
        put_bin("usage: cron create --name");
        let mut s = side(&d);
        s.hermes_bin = bin.to_string_lossy().into_owned();
        assert!(!s.cron_has_timezone());
        assert!(!s.cron_has_timezone());
        put_bin("usage: cron create --name --timezone ZONE");
        assert!(s.cron_has_timezone());
        fs::remove_dir_all(&d).ok();
    }

    /// An invented profile: SOUL with owner text and a marked persona, two skills, a job, a connector.
    fn profile(d: &Path) {
        put(
            &d.join("SOUL.md"),
            "Owner text stays.\n<!-- botport:begin persona -->\nExample instructions.\n<!-- botport:end persona -->\n",
        );
        put(
            &d.join("skills/mirror/demo-skill/SKILL.md"),
            "# Demo skill\n",
        );
        put(&d.join("skills/extra/other-skill/SKILL.md"), "# Other\n");
        put(
            &d.join("skills/not-mirrored/hidden/SKILL.md"),
            "# Not mirrored\n",
        );
        put(
            &d.join("cron/jobs.json"),
            r#"{"jobs": [{"id": "j1", "name": "demo-routine", "prompt": "Example prompt.", "schedule": {"expr": "0 9 * * 1"}, "enabled": true}]}"#,
        );
        put(
            &d.join("config.yaml"),
            "model: example-model\nmcp_servers:\n  example-connector:\n    url: https://mcp.example.test/mcp\n    headers:\n      X-Demo: kept\n",
        );
    }

    #[test]
    fn reads_every_kind_and_only_mirrored_categories() {
        let d = tmp("read");
        profile(&d);
        let r = side(&d).read().unwrap();
        let ids: Vec<&str> = r.read.items.keys().map(ItemId::as_str).collect();
        assert_eq!(
            ids,
            [
                "connector:example-connector",
                "persona.instructions",
                "routine:demo-routine",
                "skill:demo-skill",
                "skill:other-skill"
            ]
        );
        assert!(r.unsupported.is_empty(), "{:?}", r.unsupported);
        let Content::Connector(c) = &r.content[&id("connector:example-connector")] else {
            panic!()
        };
        assert!(!c.contains_key("headers"), "header values are never read");
        assert_eq!(r.read.coverage.len(), 4);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn soul_sections_are_replaced_appended_and_removed_in_place() {
        let t = "Owner text.\n<!-- botport:begin persona -->\nold\n<!-- botport:end persona -->\nMore owner text.\n";
        let r = soul_with(t, "persona", Some("new\n"));
        assert_eq!(r, "Owner text.\n<!-- botport:begin persona -->\nnew\n<!-- botport:end persona -->\nMore owner text.\n");
        let a = soul_with(t, "persona-name", Some("Demo"));
        assert!(a.starts_with(t) && a.ends_with("<!-- botport:begin persona-name -->\nDemo\n<!-- botport:end persona-name -->\n"));
        assert_eq!(
            soul_with(t, "persona", None),
            "Owner text.\nMore owner text.\n"
        );
        assert_eq!(
            soul_with("No markers", "persona", Some("x")),
            "No markers\n<!-- botport:begin persona -->\nx\n<!-- botport:end persona -->\n"
        );
        assert_eq!(soul_with("", "persona", None), "");
    }

    #[test]
    fn writes_are_compare_and_swap() {
        let d = tmp("cas");
        profile(&d);
        let s = side(&d);
        let ins = id("persona.instructions");
        let before = s.current(&ins).unwrap().unwrap();
        // a stale expectation writes nothing
        assert_eq!(
            s.write(
                &ins,
                &Content::Text("Changed.\n".into()),
                Some("stale"),
                false
            )
            .unwrap(),
            None
        );
        let after = s
            .write(
                &ins,
                &Content::Text("Changed.\n".into()),
                Some(&before.digest),
                false,
            )
            .unwrap()
            .unwrap();
        assert_ne!(after, before.digest);
        let soul = fs::read_to_string(d.join("SOUL.md")).unwrap();
        assert!(soul.starts_with("Owner text stays.\n") && soul.contains("Changed.\n"));
        // a new skill goes to the first category; an existing one stays where it is
        let mut files = BTreeMap::new();
        files.insert("SKILL.md".to_string(), (b"# New\n".to_vec(), false));
        let p = Package::from_files("new-skill", files.clone()).unwrap();
        s.write(&id("skill:new-skill"), &Content::Package(p), None, false)
            .unwrap()
            .unwrap();
        assert!(d.join("skills/mirror/new-skill/SKILL.md").is_file());
        let other = id("skill:other-skill");
        let cur = s.current(&other).unwrap().unwrap();
        let p2 = Package::from_files("other-skill", files).unwrap();
        s.write(&other, &Content::Package(p2), Some(&cur.digest), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read_to_string(d.join("skills/extra/other-skill/SKILL.md")).unwrap(),
            "# New\n"
        );
        // retire: moved to a backup, and only from the expected digest
        let cur = s.current(&other).unwrap().unwrap();
        assert!(!s.retire(&other, "stale").unwrap());
        assert!(s.retire(&other, &cur.digest).unwrap());
        assert!(!d.join("skills/extra/other-skill").exists());
        assert!(fs::read_dir(d.join(sync::BACKUPS)).unwrap().count() == 1);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn connectors_keep_fields_botport_does_not_own() {
        let d = tmp("conn");
        profile(&d);
        let s = side(&d);
        let c = id("connector:example-connector");
        let cur = s.current(&c).unwrap().unwrap();
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), json!("example-connector"));
        m.insert("url".to_string(), json!("https://mcp.example.test/v2"));
        m.insert("tools".to_string(), json!(["search"]));
        s.write(&c, &Content::Connector(m.clone()), Some(&cur.digest), false)
            .unwrap()
            .unwrap();
        let cfg = fs::read_to_string(d.join("config.yaml")).unwrap();
        assert!(
            cfg.contains("https://mcp.example.test/v2")
                && cfg.contains("X-Demo")
                && cfg.contains("kept")
                && cfg.contains("search"),
            "{cfg}"
        );
        assert!(cfg.contains("model: \"example-model\""), "{cfg}");
        // a url on another host: the headers kept for the old one are dropped, never sent to the new one
        let cur = s.current(&c).unwrap().unwrap();
        m.insert("url".to_string(), json!("https://other.example.test/mcp"));
        s.write(&c, &Content::Connector(m.clone()), Some(&cur.digest), false)
            .unwrap()
            .unwrap();
        let cfg = fs::read_to_string(d.join("config.yaml")).unwrap();
        assert!(
            cfg.contains("https://other.example.test/mcp") && !cfg.contains("X-Demo"),
            "{cfg}"
        );
        // a url with a credential in it is refused
        let cur = s.current(&c).unwrap().unwrap();
        m.insert(
            "url".to_string(),
            json!("https://user:pass@mcp.example.test/mcp"),
        );
        assert!(s
            .write(&c, &Content::Connector(m), Some(&cur.digest), false)
            .is_err());
        let cur = s.current(&c).unwrap().unwrap();
        assert!(s.retire(&c, &cur.digest).unwrap());
        assert!(!fs::read_to_string(d.join("config.yaml"))
            .unwrap()
            .contains("example-connector"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_public_connector_under_the_owners_key_is_that_connector() {
        let d = tmp("conn-row");
        put(
            &d.join("config.yaml"),
            "mcp_servers:\n  my-code-host:\n    url: https://api.githubcopilot.com/mcp/\n",
        );
        let s = side(&d);
        let r = s.read().unwrap();
        let gh = id("connector:github");
        let v = r.read.items[&gh].clone();
        let Content::Connector(mut m) = r.content[&gh].clone() else {
            panic!()
        };
        m.insert("tools".to_string(), json!(["search"]));
        s.write(&gh, &Content::Connector(m), Some(&v.digest), false)
            .unwrap()
            .unwrap();
        let cfg = fs::read_to_string(d.join("config.yaml")).unwrap();
        assert!(
            cfg.contains("my-code-host") && !cfg.contains("github:"),
            "the owner's key is kept: {cfg}"
        );
        // a config.yaml that is a symlink is neither read nor rewritten
        #[cfg(unix)]
        {
            let real = d.join("elsewhere.yaml");
            fs::rename(d.join("config.yaml"), &real).unwrap();
            std::os::unix::fs::symlink(&real, d.join("config.yaml")).unwrap();
            assert!(s.read().unwrap_err().contains("symlink"));
            let e = s
                .write(
                    &gh,
                    &Content::Connector(BTreeMap::new()),
                    Some(&v.digest),
                    false,
                )
                .unwrap_err();
            assert!(e.contains("symlink"), "{e}");
            assert_eq!(fs::read_to_string(&real).unwrap(), cfg);
            assert!(fs::symlink_metadata(d.join("config.yaml"))
                .unwrap()
                .is_symlink());
        }
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn routines_need_the_cli_and_problem_items_are_listed() {
        let d = tmp("cron");
        profile(&d);
        put(&d.join("skills/mirror/.hidden/SKILL.md"), "x");
        put(
            &d.join("skills/extra/demo-skill/SKILL.md"),
            "# Same name, other category\n",
        );
        put(
            &d.join("skills/mirror/leaky/SKILL.md"),
            "api_key=sk-ABCDEFGHIJKLMNOPQRSTUVWX123\n",
        );
        let s = side(&d);
        let r = s.read().unwrap();
        assert!(r.unsupported["skill:demo-skill"].contains("two mirrored categories"));
        assert!(r.unsupported["skill:leaky"].contains("secret"));
        assert!(!r.unsupported.contains_key("skill:.hidden"));
        let rt = id("routine:demo-routine");
        let cur = s.current(&rt).unwrap().unwrap();
        let Content::Routine(sched) = &r.content[&rt] else {
            panic!()
        };
        assert!(s
            .write(
                &rt,
                &Content::Routine(sched.clone()),
                Some(&cur.digest),
                false
            )
            .unwrap_err()
            .contains("hermes CLI"));
        fs::remove_dir_all(&d).ok();
    }
}
