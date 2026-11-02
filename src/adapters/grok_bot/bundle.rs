//! The export bundle: what `export` writes inside the Grok box, what `verify` checks and what the Hermes side reads.
//!
//! Layout (every file listed with its SHA-256 in manifest.json; no timestamps in any hashed file):
//!   raw/...                  source files verbatim (secrets excluded)
//!   normalized/profile.json  settings.json  MEMORY.md  skills/<name>/...   (skill bodies with literal "\n" unescaped)
//!   memory/memory.jsonl      memory-status.json        (memory-drop contract, ported from grok-bot-backup)
//!   history/messages.jsonl   read-only snapshot of the history DB, or history/<file>.sqlite copied verbatim
//!   NOT_CAPTURED.md          what the export cannot carry, with the bot's own description of it
//!
//! A directory without manifest.json is read as a legacy hand export (profile.json, settings.json, MEMORY.md,
//! skills/<name>/SKILL.md at its top level, like `examples/acme/grok/`) and normalized in memory.
use crate::util::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const TOOL: &str = "botport";
/// The former tool name: export manifests it wrote are still accepted.
pub const LEGACY_TOOL: &str = "grok2hermes";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Bundle {
    pub root: PathBuf,
    pub agent_id: String,
    pub name: String,
    /// skill name -> (path inside the skill dir -> bytes), normalized
    pub skills: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    pub memory_md: Option<String>,
    /// every fact the Grok side holds: memory-drop rows' `fact`, and MEMORY.md lines (for echo prevention)
    pub facts: Vec<String>,
    /// Grok skills present in the export but excluded by the spec's allowlist (dropped by `retain_skills`)
    pub excluded: std::collections::BTreeSet<String>,
    /// relative path -> sha256 of every file the importer read (the sync digest covers exactly these)
    pub read: BTreeMap<String, String>,
    /// persona instructions (grok-bot-export only): written to SOUL.md's owned `persona` section
    pub instructions: String,
    /// user facts (grok-bot-export only): added to memories/USER.md as entries
    pub user_facts: Vec<String>,
}

impl Bundle {
    pub fn digest(&self) -> String {
        digest_pairs(&self.read)
    }

    /// Drop skills the spec does not import, from the skills and from the files read (so the sync digest, and
    /// everything compared against the export, never sees them). Returns the dropped names.
    pub fn retain_skills(&mut self, allowed: impl Fn(&str) -> bool) -> Vec<String> {
        let gone: Vec<String> = self
            .skills
            .keys()
            .filter(|n| !allowed(n))
            .cloned()
            .collect();
        for n in &gone {
            self.skills.remove(n);
        }
        let prefixes: Vec<String> = gone.iter().map(|n| format!("skills/{n}/")).collect();
        self.read
            .retain(|k, _| !prefixes.iter().any(|p| k.starts_with(p)));
        self.excluded.extend(gone.iter().cloned());
        gone
    }
}

/// Literal backslash-n sequences -> newlines (Grok's workflow store keeps skill bodies escaped).
pub fn unescape(s: &str) -> String {
    s.replace("\\n", "\n")
}

pub fn load(dir: &Path) -> Res<Bundle> {
    if !dir.is_dir() {
        return err(format!("bundle not found: {}", dir.display()));
    }
    let exported = dir.join("manifest.json").is_file();
    // Exported: only the manifest-listed regular files verify read and hash-checked. Legacy: no symlink or
    // special entry anywhere, and every read is no-follow and confined to the bundle root.
    let bytes: BTreeMap<String, Vec<u8>> = if exported {
        let (v, good) = verify_read(dir)?;
        if !v.ok {
            return err(format!(
                "bundle fails verify ({}); re-export it",
                v.problems.join("; ")
            ));
        }
        good
    } else {
        let bad = special_entries(dir);
        if !bad.is_empty() {
            return err(format!(
                "bundle holds symlinks or special files ({}); refusing to read it",
                bad.join(", ")
            ));
        }
        let root = dir
            .canonicalize()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut out = BTreeMap::new();
        for rel in files_sorted(dir) {
            let r = rel_str(&rel);
            match read_confined(&root, dir, &r) {
                Ok(b) => {
                    out.insert(r, b);
                }
                Err(e) => return err(format!("{r}: refusing to read ({e:?})")),
            }
        }
        out
    };
    let base = if exported { "normalized/" } else { "" };
    let mut read = BTreeMap::new();
    let mut get = |rel: &str| -> Option<Vec<u8>> {
        let b = bytes.get(&format!("{base}{rel}"))?.clone();
        read.insert(rel.to_string(), sha256_hex(&b));
        Some(b)
    };
    let profile: Value = match get("profile.json") {
        Some(b) => serde_json::from_slice(&b).map_err(|e| format!("profile.json: {e}"))?,
        None => json!({}),
    };
    get("settings.json");
    let mut memory_md = match get("MEMORY.md") {
        Some(b) => Some(String::from_utf8(b).map_err(|_| "MEMORY.md is not UTF-8")?),
        None => None,
    };
    // skills/<n>/... for every <n> holding a SKILL.md, from the confined file set only
    let spre = format!("{base}skills/");
    let mut skill_files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for k in bytes.keys() {
        if let Some((n, r)) = k.strip_prefix(&spre).and_then(|t| t.split_once('/')) {
            skill_files
                .entry(n.to_string())
                .or_default()
                .push(r.to_string());
        }
    }
    let mut skills = BTreeMap::new();
    for (n, rels) in skill_files {
        if !rels.iter().any(|r| r == "SKILL.md") {
            continue;
        }
        let mut files = BTreeMap::new();
        for r in rels {
            let b = get(&format!("skills/{n}/{r}")).unwrap_or_default();
            let b = if !exported && r.ends_with(".md") {
                unescape(&String::from_utf8_lossy(&b)).into_bytes()
            } else {
                b
            };
            files.insert(r, b);
        }
        skills.insert(n, files);
    }
    let mut facts: Vec<String> = memory_md
        .iter()
        .flat_map(|m| m.lines().map(String::from))
        .collect();
    if let Some(b) = bytes.get("memory/memory.jsonl") {
        read.insert("memory/memory.jsonl".into(), sha256_hex(b));
        let rows = parse_jsonl(&String::from_utf8_lossy(b));
        facts.extend(
            rows.iter()
                .filter_map(|r| r["fact"].as_str().map(String::from)),
        );
        if memory_md.is_none() {
            memory_md = Some(render_memory(&rows));
        }
    }
    let name = profile["name"].as_str().unwrap_or("").to_string();
    let agent_id = if exported {
        let m: Value = serde_json::from_slice(bytes.get("manifest.json").map_or(&[][..], |b| b))
            .map_err(|e| format!("manifest.json: {e}"))?;
        m["agent_id"].as_str().unwrap_or("").to_string()
    } else {
        String::new()
    };
    Ok(Bundle {
        root: dir.to_path_buf(),
        agent_id,
        name,
        skills,
        memory_md,
        facts,
        excluded: Default::default(),
        read,
        instructions: String::new(),
        user_facts: vec![],
    })
}

// ---------------------------------------------------------------- memory-drop contract (from grok-bot-backup)
pub fn parse_jsonl(text: &str) -> Vec<Value> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str::<Value>(l)
                .ok()
                .filter(|v| v.is_object())
                .unwrap_or_else(|| {
                    json!({
            "date": "", "tier": "note", "scope": "", "source": "unparsed",
            "fact": format!("(line {} is not a JSON object; kept verbatim in raw/)", i + 1)})
                })
        })
        .collect()
}

fn s(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::String(x) => x.trim().to_string(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

/// Stable order for memory rows: date, tier, scope, source, fact.
pub fn sort_rows(rows: &mut [Value]) {
    rows.sort_by_key(|r| {
        (
            s(r, "date"),
            s(r, "tier"),
            s(r, "scope"),
            s(r, "source"),
            s(r, "fact"),
        )
    });
}

/// Markdown view of memory rows grouped by tier (port of grok-bot-backup memory_drop.render_markdown).
pub fn render_memory(rows: &[Value]) -> String {
    let titles = [
        ("profile", "Persistent (profile)"),
        ("log", "Assistant-owned (log)"),
        ("note", "Assistant-owned (note)"),
        ("conversation", "Conversation scope"),
    ];
    let key = |r: &Value| {
        let (t, sc) = (s(r, "tier"), s(r, "scope"));
        if titles.iter().any(|(k, _)| *k == t) {
            t
        } else if sc == "conversation" {
            "conversation".into()
        } else if t.is_empty() {
            "unknown".into()
        } else {
            t
        }
    };
    let mut order: Vec<String> = titles.iter().map(|(k, _)| k.to_string()).collect();
    let mut buckets: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for r in rows {
        let k = key(r);
        if !order.contains(&k) {
            order.push(k.clone());
        }
        buckets.entry(k).or_default().push(r);
    }
    let mut out = String::from("# Memory (exported from the Grok Bot)\n\n");
    if rows.is_empty() {
        return out + "(no facts)\n";
    }
    for k in order {
        let Some(g) = buckets.get(&k) else { continue };
        let title = titles
            .iter()
            .find(|(t, _)| *t == k)
            .map(|(_, t)| t.to_string())
            .unwrap_or(k.clone());
        out += &format!("## {title}\n\n");
        for r in g {
            let or = |x: String, d: &str| if x.is_empty() { d.to_string() } else { x };
            out += &format!(
                "- {} · {} · {}: {}\n",
                or(s(r, "date"), "(no date)"),
                or(s(r, "scope"), "(no scope)"),
                or(s(r, "source"), "self"),
                or(s(r, "fact"), "(blank)").replace('\n', " ")
            );
        }
        out += "\n";
    }
    out
}

// ---------------------------------------------------------------- secrets
/// File names never exported (credentials stay in the Grok cloud / box).
pub fn secret_name(rel: &str) -> bool {
    let n = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
    n == ".env"
        || n.starts_with(".env.")
        || n == "auth.json"
        || n.contains("credential")
        || n.contains("token")
        || n.contains("secret")
        || n.ends_with(".pem")
        || n.ends_with(".key")
        || n.ends_with(".p12")
        || n == "id_rsa"
        || n == "id_ed25519"
        || n == ".netrc"
        || n == "cookies.sqlite"
}

/// Content that looks like a live credential (checked on text files before export): the bundle write guard,
/// value checks only (a skill file may mention "password" without holding one).
pub fn secret_content(b: &[u8]) -> bool {
    bundle_secret(b)
}

/// Content of an exported bundle that holds a live credential (verify): the write guard (rule B).
pub fn bundle_secret(b: &[u8]) -> bool {
    crate::core::secrets::guard("", String::from_utf8_lossy(b).as_bytes()).is_err()
}

// ---------------------------------------------------------------- export
pub struct ExportOpts {
    pub out: PathBuf,
    pub from_dir: Option<PathBuf>,
    pub agent_data: PathBuf,
    pub agent_id: Option<String>,
    pub memory_drop: PathBuf,
    pub history_db: Option<PathBuf>,
    pub history_sqlite: Option<PathBuf>,
    pub describe: Option<PathBuf>,
    pub max_memory_age_days: f64,
    pub strict: bool,
}

pub struct ExportResult {
    pub files: BTreeMap<String, String>,
    pub skills: usize,
    pub memory_rows: Option<usize>,
    pub history: String,
    pub excluded: Vec<String>,
    pub warnings: Vec<String>,
}

struct Out {
    root: PathBuf,
    files: BTreeMap<String, String>,
}

impl Out {
    fn put(&mut self, rel: &str, data: &[u8]) -> Res<()> {
        write_atomic(&self.root.join(rel), data, Some(0o644))?;
        self.files.insert(rel.to_string(), sha256_hex(data));
        Ok(())
    }
}

/// Remove a previous export at `out` (only the files its own manifest lists), or refuse a foreign non-empty dir.
pub fn clear_out(out: &Path) -> Res<()> {
    if !out.exists() {
        return Ok(());
    }
    let man = out.join("manifest.json");
    if man.is_file() {
        let m: Value =
            serde_json::from_slice(&read(&man)?).map_err(|e| format!("old manifest: {e}"))?;
        if m["tool"] != TOOL && m["tool"] != LEGACY_TOOL {
            return err(format!(
                "{} holds a manifest not written by {TOOL}; refusing to overwrite",
                out.display()
            ));
        }
        let listed: Vec<String> = m["files"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        for rel in listed
            .iter()
            .chain(std::iter::once(&"manifest.json".to_string()))
        {
            if rel.split('/').any(|c| c == ".." || c.is_empty()) {
                return err(format!("bad path in old manifest: {rel}"));
            }
            fs::remove_file(out.join(rel)).ok();
        }
        remove_empty_dirs(out);
    }
    if fs::read_dir(out)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
    {
        return err(format!(
            "{} is not empty and holds no {TOOL} export; choose an empty --out",
            out.display()
        ));
    }
    Ok(())
}

fn remove_empty_dirs(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                remove_empty_dirs(&e.path());
                fs::remove_dir(e.path()).ok();
            }
        }
    }
}

fn pretty_json(b: &[u8]) -> Res<Vec<u8>> {
    let v: Value = serde_json::from_slice(b).map_err(|e| format!("json: {e}"))?;
    Ok(json_sorted(&v).into_bytes())
}

fn age_days(p: &Path) -> Option<f64> {
    let m = fs::metadata(p).ok()?.modified().ok()?;
    Some(
        std::time::SystemTime::now()
            .duration_since(m)
            .ok()?
            .as_secs_f64()
            / 86400.0,
    )
}

pub fn export(o: &ExportOpts) -> Res<ExportResult> {
    clear_out(&o.out)?;
    let mut out = Out {
        root: o.out.clone(),
        files: BTreeMap::new(),
    };
    let mut excluded = vec![];
    let mut warnings = vec![];
    // 1. Collect the source files: (raw rel path, absolute path)
    let mut src: Vec<(String, PathBuf)> = vec![];
    let agent_id;
    if let Some(d) = &o.from_dir {
        for rel in files_sorted(d) {
            src.push((rel_str(&rel), d.join(&rel)));
        }
        agent_id = o.agent_id.clone().unwrap_or_default();
    } else {
        let agents = o.agent_data.join("agents");
        agent_id = match &o.agent_id {
            Some(a) => a.clone(),
            None => {
                let mut ids: Vec<String> = fs::read_dir(&agents)
                    .map_err(|e| format!("{}: {e}", agents.display()))?
                    .flatten()
                    .filter(|e| e.path().join("profile.json").is_file())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.len() == 36)
                    .collect();
                ids.sort();
                if ids.len() != 1 {
                    return err(format!(
                        "{} agents under {}; pass --agent-id (one of: {})",
                        ids.len(),
                        agents.display(),
                        ids.join(", ")
                    ));
                }
                ids.remove(0)
            }
        };
        let ad = agents.join(&agent_id);
        for f in ["profile.json", "settings.json", "MEMORY.md"] {
            if ad.join(f).is_file() {
                src.push((f.to_string(), ad.join(f)));
            }
        }
        // Skills = user workflows; Cursor-managed/plugin skills are not the bot's own.
        let wf = o.agent_data.join("workflows");
        for rel in files_sorted(&wf) {
            let r = rel_str(&rel);
            if r.starts_with("managed-skills/") || r.starts_with("plugin-skills/") {
                continue;
            }
            src.push((format!("skills/{r}"), wf.join(&rel)));
        }
    }
    // Memory drop (bot-written JSONL + optional memory-status.json), per agent.
    let drop_dir = o.memory_drop.join("agents").join(&agent_id);
    let mut rows: Vec<Value> = vec![];
    let mut have_drop = false;
    let mut redacted = 0;
    for rel in files_sorted(&drop_dir) {
        let r = rel_str(&rel);
        if r.ends_with(".jsonl") || r == "memory-status.json" {
            let p = drop_dir.join(&rel);
            src.push((format!("memory-drop/{r}"), p.clone()));
            have_drop = true;
            if let Some(a) = age_days(&p) {
                if a > o.max_memory_age_days {
                    warnings.push(format!(
                        "memory drop {r} is {a:.1} days old (limit {})",
                        o.max_memory_age_days
                    ));
                }
            }
            if r.ends_with(".jsonl") {
                for mut row in parse_jsonl(&read_str(&p)?) {
                    if secret_content(row.to_string().as_bytes()) {
                        redacted += 1;
                        row = json!({"date": row["date"], "tier": row["tier"], "scope": row["scope"], "source": row["source"],
                                     "fact": "[redacted by botport: credential-like text]"});
                    }
                    rows.push(row);
                }
            }
        }
    }
    if redacted > 0 {
        warnings.push(format!(
            "memory: {redacted} row(s) redacted (credential-like text)"
        ));
    }
    if !have_drop && o.from_dir.is_none() {
        warnings.push(format!(
            "no memory drop at {} (ask the bot to write memory-agent.jsonl or memory-status.json)",
            drop_dir.display()
        ));
    }
    // 2. raw/ (secrets excluded by name and by content)
    let mut raw: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (rel, p) in &src {
        let b = read(p)?;
        if secret_name(rel) || secret_content(&b) {
            excluded.push(rel.clone());
            continue;
        }
        out.put(&format!("raw/{rel}"), &b)?;
        raw.insert(rel.clone(), b);
    }
    // 3. normalized/
    for f in ["profile.json", "settings.json"] {
        if let Some(b) = raw.get(f) {
            out.put(&format!("normalized/{f}"), &pretty_json(b)?)?;
        }
    }
    let mut skills = std::collections::BTreeSet::new();
    for (rel, b) in &raw {
        if let Some(r) = rel.strip_prefix("skills/") {
            let parts: Vec<&str> = r.splitn(2, '/').collect();
            if parts.len() == 2 && raw.contains_key(&format!("skills/{}/SKILL.md", parts[0])) {
                skills.insert(parts[0].to_string());
                let nb = if r.ends_with(".md") {
                    unescape(&String::from_utf8_lossy(b)).into_bytes()
                } else {
                    b.clone()
                };
                out.put(&format!("normalized/{rel}"), &nb)?;
            }
        }
    }
    sort_rows(&mut rows);
    let memory_rows = if have_drop { Some(rows.len()) } else { None };
    if have_drop {
        let jl: String = rows.iter().map(jsonl_line).collect();
        out.put("memory/memory.jsonl", jl.as_bytes())?;
        if let Some(b) = raw.get("memory-drop/memory-status.json") {
            out.put("memory/memory-status.json", &pretty_json(b)?)?;
        }
    }
    let memory_md = match raw.get("MEMORY.md") {
        Some(b) => Some(b.clone()),
        None if have_drop => Some(render_memory(&rows).into_bytes()),
        None => None,
    };
    if let Some(m) = memory_md {
        out.put("normalized/MEMORY.md", &m)?;
    }
    // 4. history
    let history = if let Some(p) = &o.history_sqlite {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or("history.sqlite".into());
        let b = read(p)?;
        if secret_content(&b) {
            // A credential pasted into a chat would ride along in the copy: refuse it (JSONL path redacts per row).
            excluded.push(format!("history/{name}"));
            warnings.push(format!("history {name} contains credential-like text: not copied (use the box export, which redacts per message)"));
            "excluded (credential-like text)".into()
        } else {
            out.put(&format!("history/{name}"), &b)?;
            format!("sqlite copied ({name})")
        }
    } else {
        let db = o
            .history_db
            .clone()
            .unwrap_or_else(|| o.agent_data.join("search-index.db"));
        if db.is_file() && !agent_id.is_empty() {
            match history_snapshot(&db, &agent_id) {
                Ok(lines) => {
                    let n = lines.len();
                    let mut red = 0;
                    let lines: Vec<String> = lines
                        .into_iter()
                        .map(|l| {
                            if secret_content(l.as_bytes()) {
                                red += 1;
                                let mut v: Value = serde_json::from_str(&l).unwrap_or(json!({}));
                                v["body"] = json!("[redacted by botport: credential-like text]");
                                jsonl_line(&v)
                            } else {
                                l
                            }
                        })
                        .collect();
                    if red > 0 {
                        warnings.push(format!(
                            "history: {red} message(s) redacted (credential-like text)"
                        ));
                    }
                    out.put("history/messages.jsonl", lines.concat().as_bytes())?;
                    format!("{n} messages")
                }
                Err(e) => {
                    warnings.push(format!("history not captured: {e}"));
                    "not captured".into()
                }
            }
        } else {
            "none".into()
        }
    };
    // 5. NOT_CAPTURED.md
    let describe = match &o.describe {
        Some(p) => read_str(p)?,
        None => {
            warnings.push("no --describe file: NOT_CAPTURED.md has the prompt to ask the bot, not its answers".into());
            String::new()
        }
    };
    out.put(
        "NOT_CAPTURED.md",
        not_captured(&describe, &excluded).as_bytes(),
    )?;
    if !describe.trim().is_empty() {
        let d = crate::describe::parse(&describe);
        out.put(
            "normalized/describe.json",
            crate::util::json_sorted(&d).as_bytes(),
        )?;
    }
    // 6. manifest (no timestamps: identical input -> identical bytes)
    let manifest = json!({"tool": TOOL, "version": VERSION, "agent_id": agent_id, "files": out.files,
                          "digest": digest_pairs(&out.files)});
    write_atomic(
        &o.out.join("manifest.json"),
        json_sorted(&manifest).as_bytes(),
        Some(0o644),
    )?;
    if o.strict && !warnings.is_empty() {
        return err(format!("--strict: {}", warnings.join("; ")));
    }
    Ok(ExportResult {
        files: out.files,
        skills: skills.len(),
        memory_rows,
        history,
        excluded,
        warnings,
    })
}

pub const DESCRIBE_PROMPT: &str = "Describe, for a re-import into another agent framework, without any secret values: \
(1) your live instructions / system prompt in full; (2) every schedule or automation (name, cron or time, timezone, \
what it does, where it delivers); (3) every connector / MCP server / integration (name, URL or host, which tools you \
use from it); (4) which credentials exist (names only). Use headings ## Instructions, ## Schedules, ## Connectors, \
## Credentials.";

fn not_captured(describe: &str, excluded: &[String]) -> String {
    let mut s = String::from("# Not captured by the export\n\nGrok Bot keeps these in its cloud, not in files, so the \
export cannot copy them. The importer rebuilds them from the bot's own description below.\n\n\
- **Instructions** (live system prompt)\n- **Schedules** (automations / watchers)\n\
- **Connectors** (MCP servers, integrations)\n- **Credentials** — never exported; re-create them on the Hermes side.\n\n");
    if !excluded.is_empty() {
        s += "## Files left out as secrets\n\n";
        for e in excluded {
            s += &format!("- `{e}`\n");
        }
        s += "\n";
    }
    s += "## The bot's description\n\n";
    if describe.trim().is_empty() {
        s += &format!("(not supplied) Ask the bot:\n\n> {DESCRIBE_PROMPT}\n\nSave its answer and re-run export with \
`--describe FILE`.\n");
    } else {
        // A line that looks like it holds a value is withheld (its list marker kept, so it stays a checklist item).
        for l in describe.trim_end().lines() {
            match crate::core::secrets::guard_str(l) {
                Some(r) => {
                    let lead = &l[..l.len() - l.trim_start().len()];
                    let marker = if l.trim_start().starts_with("- ") {
                        "- "
                    } else {
                        ""
                    };
                    s += &format!(
                        "{lead}{marker}(line withheld: looks like it holds a secret ({r}))\n"
                    );
                }
                None => {
                    s += l;
                    s += "\n";
                }
            }
        }
    }
    s
}

/// Consistent read-only snapshot of the box's history index -> JSONL (sorted keys, ordered by time, seq).
/// Uses python3's sqlite3 backup API (includes WAL content), as grok-bot-backup does; the box always has python3.
pub fn history_snapshot(db: &Path, agent_id: &str) -> Res<Vec<String>> {
    const PY: &str = "import json,sqlite3,sys\n\
src=sqlite3.connect('file:'+sys.argv[1]+'?mode=ro',uri=True)\nmem=sqlite3.connect(':memory:')\nsrc.backup(mem)\n\
src.close()\nfor r in mem.execute('select seq,entry_id,role,timestamp_ms,body from messages where agent_id=? \
order by timestamp_ms,seq,id',(sys.argv[2],)):\n  print(json.dumps(dict(zip(('seq','entry_id','role','timestamp_ms',\
'body'),r)),sort_keys=True,ensure_ascii=False))\n";
    let o = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(PY)
        .arg(db)
        .arg(agent_id)
        .output()
        .map_err(|e| format!("python3: {e}"))?;
    if !o.status.success() {
        return err(String::from_utf8_lossy(&o.stderr)
            .lines()
            .last()
            .unwrap_or("python3 failed")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l)
                .map(|v| jsonl_line(&v))
                .unwrap_or_else(|_| format!("{l}\n"))
        })
        .collect())
}

// ---------------------------------------------------------------- verify
pub struct VerifyResult {
    pub ok: bool,
    pub problems: Vec<String>,
    pub files: usize,
    pub digest: String,
}

pub fn verify(dir: &Path) -> Res<VerifyResult> {
    verify_read(dir).map(|(v, _)| v)
}

/// [`verify`], plus the bytes of every listed file that passed (read once, no-follow, hash checked), keyed by
/// manifest path, with `manifest.json` itself. [`load`] uses exactly these bytes.
fn verify_read(dir: &Path) -> Res<(VerifyResult, BTreeMap<String, Vec<u8>>)> {
    if !dir.join("manifest.json").is_file() && dir.join("profile.json").is_file() {
        return Err(
            "no manifest.json: this is a legacy hand export; verify needs an `export` bundle"
                .into(),
        );
    }
    let root = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let mb = match read_confined(&root, dir, "manifest.json") {
        Ok(b) => b,
        Err(Confined::NotRegular) | Err(Confined::Outside) => {
            return err("manifest.json is not a regular file in the bundle (symlink or special)")
        }
        Err(e) => return err(format!("manifest.json: {e:?}")),
    };
    let m: Value = serde_json::from_slice(&mb).map_err(|e| format!("manifest.json: {e}"))?;
    let files = m["files"].as_object().ok_or("manifest has no files")?;
    let mut problems = vec![];
    let mut seen = BTreeMap::new();
    let mut good = BTreeMap::new();
    // Every symlink or special entry anywhere in the bundle, listed or not: nothing is ever read through one.
    for r in special_entries(dir) {
        problems.push(format!("not a regular file (symlink or special): {r}"));
    }
    for (rel, sha) in files {
        seen.insert(rel.clone(), sha.as_str().unwrap_or("").to_string());
        if crate::core::util::safe_rel("manifest", rel).is_err() {
            problems.push(format!("unsafe path in manifest: {rel}"));
            continue;
        }
        // Only regular files read through no symlink, confined to the bundle root (not a link to /etc/hosts).
        match read_confined(&root, dir, rel) {
            Ok(b) if bundle_secret(&b) => {
                let why = crate::core::secrets::guard(rel, &b)
                    .err()
                    .unwrap_or_default();
                problems.push(format!("credential-like content: {why}"))
            }
            Ok(b) if Some(sha256_hex(&b).as_str()) == sha.as_str() => {
                good.insert(rel.clone(), b);
            }
            Ok(_) => problems.push(format!("hash mismatch: {rel}")),
            Err(Confined::Missing) => problems.push(format!("missing: {rel}")),
            Err(Confined::NotRegular) => {
                problems.push(format!("not a regular file (symlink or special): {rel}"))
            }
            Err(Confined::Outside) => problems.push(format!("outside the bundle: {rel}")),
            Err(Confined::Io(e)) => problems.push(format!("unreadable: {rel}: {e}")),
        }
    }
    for rel in files_sorted(dir) {
        let r = rel_str(&rel);
        if r != "manifest.json" && !files.contains_key(&r) {
            problems.push(format!("not in manifest: {r}"));
        }
        if secret_name(&r) {
            problems.push(format!("secret-like file in bundle: {r}"));
        }
    }
    let digest = digest_pairs(&seen);
    if m["digest"].as_str() != Some(digest.as_str()) {
        problems.push("manifest digest does not match its file list".into());
    }
    good.insert("manifest.json".into(), mb);
    Ok((
        VerifyResult {
            ok: problems.is_empty(),
            problems,
            files: files.len(),
            digest,
        },
        good,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn verify_rejects_symlinks_out_of_the_bundle() {
        let d = std::env::temp_dir().join(format!("botport-verify-link-{}", std::process::id()));
        let b = d.join("bundle");
        fs::create_dir_all(b.join("sub")).unwrap();
        let outside = d.join("outside.txt");
        fs::write(&outside, "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, b.join("a.txt")).unwrap();
        // a link through a symlinked directory, too
        std::os::unix::fs::symlink(&d, b.join("sub/up")).unwrap();
        let sha = sha256_hex(b"outside\n");
        let files: BTreeMap<String, String> = [
            ("a.txt".to_string(), sha.clone()),
            ("sub/up/outside.txt".to_string(), sha),
        ]
        .into();
        let man = serde_json::json!({"files": files, "digest": digest_pairs(&files)});
        fs::write(b.join("manifest.json"), serde_json::to_vec(&man).unwrap()).unwrap();
        let r = verify(&b).unwrap();
        assert!(!r.ok, "{:?}", r.problems);
        assert!(
            r.problems
                .iter()
                .any(|p| p.contains("a.txt") && p.contains("not a regular file")),
            "{:?}",
            r.problems
        );
        assert!(
            r.problems
                .iter()
                .any(|p| p.contains("sub/up/outside.txt") && p.contains("outside the bundle")),
            "{:?}",
            r.problems
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn secrets_by_name_and_content() {
        assert!(secret_name("a/.env"));
        assert!(secret_name("creds/git_token.txt"));
        assert!(!secret_name("skills/x/SKILL.md"));
        assert!(secret_content(b"key = xai-ABCDEFGHIJKLMNOPQRSTUV"));
        assert!(secret_content(b"-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(!secret_content(b"the xai- prefix is documented"));
        assert!(secret_content(b"PROVIDER=sk-0123456789abcdef0123456789"));
        assert!(secret_content(
            b"Authorization: Bearer abcdefghijklmnopqrstuvwxyz012"
        ));
        assert!(secret_content(b"password: hunter2hunter2"));
        assert!(!secret_content(b"the CRM Postgres password lives in .env"));
        // A pure template is not a value (rule B).
        assert!(!secret_content(b"PGPASSWORD=${PGPASSWORD_FROM_ENV}"));
        // verify: prose that only mentions a secret is not one (memory fact, chat message, NOT_CAPTURED line)
        for b in [
            &br#"{"fact":"the user keeps the router password and the api secret in the vault app"}"#[..],
            br#"{"body":"remind me to rotate the secret and the password"}"#,
            br#"{"body":"suite: 12 pass / 0 soft-pass; Pass: run_the_suite"}"#,
            b"- a skill mentions a password and a secret key; review it by hand",
        ] {
            assert!(!bundle_secret(b));
        }
        assert!(bundle_secret(b"password: hunter2hunter2"));
        assert!(bundle_secret(b"PROVIDER=sk-0123456789abcdef0123456789"));
        assert!(bundle_secret(b"see https://bob:hunter2@example.com/x")); // trufflehog:ignore (fake)
        assert!(!bundle_secret(b"PGPASSWORD=${PGPASSWORD_FROM_ENV}"));
    }

    #[test]
    fn retain_skills_drops_files_from_the_digest() {
        let d = std::env::temp_dir().join(format!("g2h-retain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for (n, body) in [("keep", "k"), ("drop", "d1")] {
            fs::create_dir_all(d.join("skills").join(n)).unwrap();
            fs::write(d.join("skills").join(n).join("SKILL.md"), body).unwrap();
        }
        fs::write(d.join("profile.json"), "{}").unwrap();
        let mut a = load(&d).unwrap();
        assert_eq!(a.retain_skills(|n| n == "keep"), vec!["drop".to_string()]);
        fs::write(d.join("skills/drop/SKILL.md"), "d2 changed").unwrap();
        let mut b = load(&d).unwrap();
        b.retain_skills(|n| n == "keep");
        assert_eq!(
            a.digest(),
            b.digest(),
            "an excluded skill never changes the sync digest"
        );
        assert!(
            a.skills.contains_key("keep")
                && !a.skills.contains_key("drop")
                && a.excluded.contains("drop")
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn memory_render_groups_and_handles_bad_lines() {
        let mut rows = parse_jsonl(
            "{\"tier\":\"note\",\"fact\":\"b\",\"date\":\"2026-10-02\"}\nnot json\n\
{\"tier\":\"profile\",\"fact\":\"a\",\"date\":\"2026-10-01\"}\n",
        );
        sort_rows(&mut rows);
        let md = render_memory(&rows);
        assert!(md.find("Persistent").unwrap() < md.find("Assistant-owned (note)").unwrap());
        assert!(md.contains("not a JSON object"));
        assert_eq!(
            render_memory(&[]),
            "# Memory (exported from the Grok Bot)\n\n(no facts)\n"
        );
    }
}
