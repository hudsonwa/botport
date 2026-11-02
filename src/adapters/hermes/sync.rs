//! Hermes side: render the wanted profile state from a bundle + spec, diff it against a live profile (plan / drift),
//! and apply it (apply / sync) behind a restore point.
//!
//! Ownership rules (ported from agent-importers sync.py and an earlier hand-made renderer):
//! - skills: one unit per skill dir under `<profile>/skills/<category>/`; a unit's tree digest is recorded in
//!   `<profile>/botport.lock.json` when written (grok2hermes.lock.json still read). A live unit that differs from both the wanted and the recorded
//!   digest was edited on the Hermes side: CONFLICT, never overwritten.
//! - SOUL.md: owned sections between `<!-- botport:begin NAME -->` / `end` markers (old `grok2hermes:` markers still read); text outside them is the
//!   Hermes side's. A legacy SOUL.md without markers that equals the template exactly counts as in sync.
//! - config.yaml: only the template's leaf paths (minus `preserve`) are owned; lists keep Hermes-side extra items.
//! - USER.md seed entries are added if missing, never removed. Cron jobs are matched by name.
use super::spec::Spec;
use crate::core::adapter::Source;
pub use crate::core::adapter::{item, Applied, Item, St};
use crate::core::model::BotModel;
use crate::core::secrets;
use crate::core::util::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_yaml::{Mapping, Value as Y};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LOCK: &str = "botport.lock.json";
/// Names written before the rename to botport: read as a fallback, migrated to the new names by the next apply/sync.
pub const LEGACY_LOCK: &str = "grok2hermes.lock.json";
pub const LEGACY_MANUAL: &str = "grok2hermes-manual.md";
const LEGACY_MARK: &str = "<!-- grok2hermes:";
const MARK: &str = "<!-- botport:";
/// Restore points and retired skills (relative to the profile dir).
pub const BACKUPS: &str = "backups/botport";
/// The checklist of things botport cannot set up (names only), in the profile dir.
pub const MANUAL: &str = "botport-manual.md";
const ENTRY_DELIM: &str = "\n§\n";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Lock {
    pub version: u32,
    pub tool_version: String,
    pub bundle_digest: String,
    pub files_read: BTreeMap<String, String>,
    pub skills: BTreeMap<String, String>,
    pub soul: BTreeMap<String, String>,
    pub config: BTreeMap<String, String>,
    /// Hermes -> Grok direction: skills exported from this profile (name -> tree digest at export)
    #[serde(default)]
    pub h2g_skills: BTreeMap<String, String>,
    /// cron jobs already sent to the Grok side as requested actions (name -> digest of the request)
    #[serde(default)]
    pub h2g_cron: BTreeMap<String, String>,
    /// sent skills a later Grok export was seen to contain (only then can their absence mean "deleted on Grok")
    #[serde(default)]
    pub h2g_seen: BTreeSet<String>,
    /// Grok skills ever excluded by the allowlist (h2g never treats a skill of that name as Hermes-made)
    #[serde(default)]
    pub grok_excluded: BTreeSet<String>,
    /// every version of each skill ever sent to (or agreed with) the Grok side: a Grok copy equal to any of them
    /// is one the Grok side holds unchanged
    #[serde(default)]
    pub h2g_history: BTreeMap<String, BTreeSet<String>>,
    /// cron jobs made from the bot's schedules (name -> digest of prompt + schedule as last written or adopted)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub g2h_cron: BTreeMap<String, String>,
}

/// One plan/apply run: the model (the only bot content read), its source bookkeeping, and the target profile.
pub struct Ctx<'a> {
    pub model: &'a BotModel,
    pub source: &'a Source,
    pub spec: &'a Spec,
    pub profile: String,
    pub dest: PathBuf,
    pub real_home: bool,
    pub hermes_bin: String,
    /// HERMES_HOME for the hermes CLI when it is not ~/.hermes (None = the CLI's default)
    pub hermes_home: Option<PathBuf>,
    /// the model's skills as files (text and binary), by skill name
    pub skills: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// the memory document the memory skill holds: the model's `memory_text`, else its facts as `- fact` lines
    pub memory_md: Option<String>,
    /// facts about the user (`user` scope or `profile` tier): USER.md entries
    pub user_facts: Vec<String>,
}

/// A fact that belongs in USER.md rather than MEMORY.md.
pub fn is_user_fact(f: &crate::core::model::Fact) -> bool {
    f.scope.as_deref() == Some("user") || f.tier.as_deref() == Some("profile")
}

impl<'a> Ctx<'a> {
    pub fn new(
        model: &'a BotModel,
        source: &'a Source,
        dest: &Path,
        o: &'a super::Opts,
    ) -> Ctx<'a> {
        let notes: String = model
            .memory
            .iter()
            .filter(|f| !is_user_fact(f))
            .map(|f| format!("- {}\n", f.text))
            .collect();
        Ctx {
            model,
            source,
            spec: &o.spec,
            profile: o.profile.clone(),
            dest: dest.to_path_buf(),
            real_home: o.real_home,
            hermes_bin: o.hermes_bin.clone(),
            hermes_home: o.hermes_home.clone(),
            skills: model
                .skills
                .iter()
                .map(|s| (s.name.clone(), s.bytes()))
                .collect(),
            memory_md: model
                .memory_text
                .clone()
                .or_else(|| (!notes.is_empty()).then_some(notes)),
            user_facts: model
                .memory
                .iter()
                .filter(|f| is_user_fact(f))
                .map(|f| f.text.clone())
                .collect(),
        }
    }
}

/// The bot's display name ("Bot" when it has none).
pub(crate) fn bot_name(m: &BotModel) -> &str {
    if m.persona.name.is_empty() {
        "Bot"
    } else {
        &m.persona.name
    }
}

// ---------------------------------------------------------------- wanted state
/// SOUL.md sections as (name, content)
pub type Sections = Vec<(String, String)>;
/// The config template: (head comment lines, values, preserved dotted paths split into segments)
pub type ConfigWant = (String, Y, Vec<Vec<String>>);

pub struct Want {
    pub cat: String,
    /// bundle skills that originated on the Hermes side (exported by export-hermes): never installed back
    pub hermes_origin: Vec<Item>,
    /// unit (path under skills/, e.g. "sample-skills/lookup-notes" or "sample-skills/DESCRIPTION.md") -> files
    pub units: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    pub soul: Option<Sections>,
    /// SOUL.md sections come only from the export's persona: a live SOUL.md without markers is Hermes-side text
    /// that is kept, with the sections appended after it
    pub soul_append: bool,
    pub config: Option<ConfigWant>,
    pub user_seed: Option<String>,
    pub cron: Vec<CronJob>,
    /// unit holding the bundle's MEMORY.md, if any
    pub memory_unit: Option<String>,
    /// cron jobs made from the bot's own schedules (`"<Bot>: <routine>"`, created paused)
    pub model_cron: Vec<CronJob>,
    /// MCP connectors with an http(s) url: (server id, owned `mcp_servers.<id>` entry)
    pub mcp: Vec<(String, Y)>,
    /// (item name, what to do by hand): written to botport-manual.md, names only
    pub manual: Vec<(String, String)>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CronJob {
    pub name: String,
    pub schedule: String,
    pub deliver: String,
    pub prompt: String,
    /// created paused with this reason (jobs made from the bot's schedules)
    #[serde(default)]
    pub paused_reason: Option<String>,
    /// spec cron files: only `enabled: true` creates a live job; missing or `false` creates it paused
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl CronJob {
    fn paused(&self) -> bool {
        self.paused_reason.is_some() || self.enabled != Some(true)
    }
}

fn job_digest(prompt: &str, schedule: &str) -> String {
    sha256_hex(format!("{prompt}\0{schedule}").as_bytes())
}

/// (paused cron jobs, owned MCP entries, manual items)
type Described = (Vec<CronJob>, Vec<(String, Y)>, Vec<(String, String)>);

/// The first of `fields` that looks like it holds a secret, as the reason.
fn suspicious(fields: &[&str]) -> Option<secrets::Reason> {
    fields.iter().find_map(|f| secrets::scan(f))
}

/// The model's schedules, connectors, credentials and manual items -> paused cron jobs, owned MCP entries and
/// manual items (names only).
pub fn from_described(
    m: &BotModel,
    spec_cron: &[CronJob],
    template_owned: &[String],
    template_urls: &[String],
) -> Described {
    let mut manual: Vec<(String, String)> = m
        .manual
        .iter()
        .map(|x| (x.item.clone(), x.todo.clone()))
        .collect();
    let jobs = schedules_to_cron(m, spec_cron, &mut manual);
    let mcp = connectors_to_mcp(m, template_owned, template_urls, &mut manual);
    credentials_to_manual(m, &mut manual);
    manual.sort();
    manual.dedup_by(|a, b| a.0 == b.0);
    (jobs, mcp, manual)
}

/// Scheduled schedules -> cron jobs `<Bot>: <name>`, created paused; the rest -> manual items.
fn schedules_to_cron(
    m: &BotModel,
    spec_cron: &[CronJob],
    manual: &mut Vec<(String, String)>,
) -> Vec<CronJob> {
    let mut jobs = vec![];
    for x in &m.schedules {
        let key = format!("schedule {}", x.name);
        // A schedule that looks like it holds a secret is never written: the checklist gets the reason, not the text.
        if let Some(r) = suspicious(&[&x.name, &x.action, x.cron.as_deref().unwrap_or(""), &x.tz]) {
            manual.push((
                format!("schedule {}", secrets::display(&x.name)),
                format!("not imported: looks like it holds a secret ({r}); its text was not copied, set it up by hand"),
            ));
            continue;
        }
        if x.kind == crate::core::model::ScheduleKind::Event {
            manual.push((key, "event-triggered on the Grok side: Hermes cron cannot subscribe to it; set up a trigger by hand".into()));
            continue;
        }
        // Only a string Hermes' own parse_schedule accepts becomes a job; anything else is a manual item, never a guess.
        let schedule = match x.cron.as_deref().map(super::schedule::normalize) {
            None => {
                manual.push((
                    key,
                    "no schedule in the description: create the Hermes cron job by hand".into(),
                ));
                continue;
            }
            Some(Err(why)) => {
                manual.push((key, why));
                continue;
            }
            Some(Ok(s)) => s,
        };
        if x.action.trim().is_empty() {
            manual.push((
                key,
                "no prompt in the description: create the Hermes cron job by hand".into(),
            ));
            continue;
        }
        let name = format!("{}: {}", bot_name(m), x.name);
        if spec_cron.iter().any(|j| j.name == name) {
            continue;
        }
        // Hermes cron has no per-job timezone: a zone the routine carries is named, otherwise the profile's zone applies.
        let tz = if x.tz.is_empty() {
            NO_ZONE.to_string()
        } else {
            format!("{} (check the host timezone)", x.tz)
        };
        jobs.push(CronJob {
            name,
            schedule,
            deliver: "local".into(),
            // a clean prompt is copied byte-identical
            prompt: x.action.clone(),
            paused_reason: Some(format!(
                "Imported by botport from the Grok bot; schedule is in {tz}, resume once the Grok routine is off"
            )),
            enabled: None,
        });
    }
    jobs
}

/// The paused-reason wording for a routine that named no timezone.
const NO_ZONE: &str = "the Hermes profile's timezone (the routine named none)";
/// The plan/--json/--details note on such a job.
pub const PROFILE_TZ: &str = "runs in the Hermes profile's timezone";

/// MCP connectors with an http(s) url -> owned `mcp_servers.<id>` entries (unless the config template owns
/// them); other connectors -> manual items.
fn connectors_to_mcp(
    m: &BotModel,
    template_owned: &[String],
    template_urls: &[String],
    manual: &mut Vec<(String, String)>,
) -> Vec<(String, Y)> {
    let mut mcp = vec![];
    for x in &m.connectors {
        let id = crate::adapters::grok_bot::export_json::slug(&x.name);
        let url = x.url.clone().unwrap_or_default();
        let key = format!("connector {}", x.name);
        let mut fields = vec![x.name.as_str(), url.as_str()];
        fields.extend(x.tools.iter().map(String::as_str));
        // A connector whose name, url or tools look like they hold a secret is never written to config.yaml.
        if let Some(r) = suspicious(&fields).or_else(|| secrets::connector_url(&url)) {
            manual.push((
                format!("connector {}", secrets::display(&x.name)),
                format!("not imported: looks like it holds a secret ({r}); add it by hand"),
            ));
            continue;
        }
        if x.kind == "mcp-http" && !url.is_empty() {
            let path = format!("mcp_servers.{id}");
            if template_urls.contains(&url)
                || template_owned
                    .iter()
                    .any(|p| *p == path || p.starts_with(&format!("{path}.")))
            {
                continue; // the spec's config template owns it
            }
            mcp.push((id, mcp_entry(&url, &x.tools)));
        } else {
            manual.push((
                key,
                format!(
                    "{} connector: enable the matching Hermes tool or MCP server by hand",
                    x.kind
                ),
            ));
        }
    }
    mcp
}

/// An owned `mcp_servers.<id>` entry: the url and, when the connector names tools, `tools.include`.
pub fn mcp_entry(url: &str, tools: &[String]) -> Y {
    let mut m = Mapping::new();
    m.insert(key_y("url"), Y::String(url.to_string()));
    if !tools.is_empty() {
        let mut t = Mapping::new();
        t.insert(
            key_y("include"),
            Y::Sequence(tools.iter().cloned().map(Y::String).collect()),
        );
        m.insert(key_y("tools"), Y::Mapping(t));
    }
    Y::Mapping(m)
}

/// Credential names -> manual items (a value-like name is not shown).
fn credentials_to_manual(m: &BotModel, manual: &mut Vec<(String, String)>) {
    for x in &m.credentials {
        if let Some(r) = secrets::credential_name(&x.name) {
            manual.push((
                "credential (name not shown)".into(),
                format!("not imported: {r}; set it up by hand"),
            ));
            continue;
        }
        manual.push((
            format!("credential {}", x.name),
            "set it on the Hermes side by hand (e.g. the profile's .env); no value was copied"
                .into(),
        ));
    }
}

fn key_y(s: &str) -> Y {
    Y::String(s.into())
}

/// The manual checklist as written; ticks (`- [x]`) are the user's and do not count as a change.
pub fn manual_text(items: &[(String, String)]) -> String {
    let mut t = String::from("# Set up by hand (botport)\n\nWhat botport could not set up from the Grok bot's description. Names only: no secret value is copied.\n\n");
    for (n, what) in items {
        t += &format!("- [ ] {n}: {what}\n");
    }
    t
}

fn untick(t: &str) -> String {
    t.lines()
        .map(
            |l| match l.strip_prefix("- [x] ").or(l.strip_prefix("- [X] ")) {
                Some(r) => format!("- [ ] {r}\n"),
                None => format!("{l}\n"),
            },
        )
        .collect()
}

fn memory_header(c: &Ctx, name: &str) -> Res<String> {
    match c.spec.memory_skill.as_ref().and_then(|m| m.header.as_ref()) {
        Some(h) => Ok(c.spec.subst(&read_str(h)?)),
        None => Ok(format!("---\nname: {name}\ndescription: >-\n  Durable memory exported from the Grok Bot \"{}\". \
Load before answering questions about its own facts, setup or history.\n---\n# Memory (synced from the Grok Bot)\n",
                           if c.model.persona.name.is_empty() { "bot" } else { &c.model.persona.name })),
    }
}

pub fn want(c: &Ctx) -> Res<Want> {
    let cat = c.spec.category(&c.model.persona.name);
    plain_name("skill_category", &cat)?;
    if let Some(m) = &c.spec.memory_skill {
        plain_name("memory_skill.name", &m.name)?;
    }
    let (hermes_origin, mut units, mut mem_manual) = want_skills(c, &cat);
    let memory_unit = want_memory_unit(c, &cat, &mut units, &mut mem_manual)?;
    if let Some(d) = &c.spec.category_description {
        let names: Vec<String> = units
            .keys()
            .map(|u| u.split('/').nth(1).unwrap_or("").to_string())
            .collect();
        let text = format!(
            "---\ndescription: \"{}\"\n---\n",
            d.replace("{names}", &names.join(", ")).replace('"', "\\\"")
        );
        units.insert(
            format!("{cat}/DESCRIPTION.md"),
            [(String::new(), text.into_bytes())].into(),
        );
    }
    let (soul, soul_append) = want_soul(c)?;
    let config = want_config(c)?;
    let user_seed = want_user_seed(c)?;
    let cron = want_spec_cron(c)?;
    let mut owned = vec![];
    if let Some((_, tpl, _)) = &config {
        leaves(tpl, &mut vec![], &mut owned);
    }
    let owned: Vec<String> = owned.into_iter().map(|(p, _)| p.join(".")).collect();
    let urls: Vec<String> = config
        .as_ref()
        .and_then(|(_, tpl, _)| tpl.get("mcp_servers"))
        .and_then(|m| m.as_mapping())
        .into_iter()
        .flatten()
        .filter_map(|(_, v)| v.get("url").and_then(|u| u.as_str()).map(String::from))
        .collect();
    let (model_cron, mcp, mut manual) = from_described(c.model, &cron, &owned, &urls);
    manual.extend(mem_manual);
    manual.sort();
    manual.dedup_by(|a, b| a.0 == b.0);
    Ok(Want {
        cat,
        hermes_origin,
        units,
        soul,
        soul_append,
        config,
        user_seed,
        cron,
        memory_unit,
        model_cron,
        mcp,
        manual,
    })
}

type Units = BTreeMap<String, BTreeMap<String, Vec<u8>>>;

/// The model's skills as owned units `<cat>/<name>`; skills that came from this profile (h2g) are reported, not
/// installed back; a skill with a secret-looking file goes to the checklist.
fn want_skills(c: &Ctx, cat: &str) -> (Vec<Item>, Units, Vec<(String, String)>) {
    let lk = read_lock(&c.dest).unwrap_or_default();
    let (mut origin, mut units, mut manual) = (vec![], BTreeMap::new(), vec![]);
    for (n, files) in &c.skills {
        // Loop prevention: a skill this profile exported comes back in the Grok export; it already lives here.
        if let Some(sent) = lk.h2g_skills.get(n) {
            origin.push(hermes_origin_item(c, cat, n, files, sent, &lk.h2g_history));
            continue;
        }
        // a skill name or file path that would leave skills/<cat>/<name>/ goes to the checklist, unread and unwritten
        if let Err(e) = plain_name("skill name", n).and_then(|_| {
            files
                .keys()
                .filter(|f| !(f.is_empty() && files.len() == 1)) // a single-file skill
                .try_for_each(|f| safe_rel("skill file", f))
        }) {
            manual.push((format!("skill {n}"), format!("not imported: {e}")));
            continue;
        }
        // a skill file that would fail the write guard goes to the checklist (name and reason), not into the profile
        if let Some(f) = files.iter().find_map(|(f, b)| {
            secrets::guard(&format!("skills/{cat}/{n}/{f}"), b)
                .err()
                .map(|_| f.clone())
        }) {
            manual.push((
                format!("skill {n}"),
                format!("not imported: {f} looks like it holds a secret; its text was not copied, add it by hand"),
            ));
            continue;
        }
        units.insert(format!("{cat}/{n}"), files.clone());
    }
    (origin, units, manual)
}

/// A source skill that this profile sent (h2g): in sync when it is a version we sent, or equal to the live
/// Hermes copy again; otherwise a conflict to merge by hand.
fn hermes_origin_item(
    c: &Ctx,
    cat: &str,
    n: &str,
    files: &BTreeMap<String, Vec<u8>>,
    sent: &str,
    history: &BTreeMap<String, BTreeSet<String>>,
) -> Item {
    let grok_d = tree_digest_files(files);
    if sent == grok_d || history.get(n).is_some_and(|h| h.contains(&grok_d)) {
        return item(
            "skills",
            n,
            St::InSync,
            "originated on the Hermes side (not installed back)",
        );
    }
    // equal to the Hermes original again (merged by hand): in sync; the next h2g sync re-baselines
    let live_equal = files_sorted(&c.dest.join("skills"))
        .iter()
        .map(|r| rel_str(r))
        .filter_map(|r| r.strip_suffix("/SKILL.md").map(String::from))
        .filter(|d| d.rsplit('/').next() == Some(n) && d.split('/').next() != Some(cat))
        .any(|d| {
            read_tree(&c.dest.join("skills").join(&d))
                .map(|t| tree_digest_files(&t) == grok_d)
                .unwrap_or(false)
        });
    if live_equal {
        item(
            "skills",
            n,
            St::InSync,
            "originated on the Hermes side; both copies equal",
        )
    } else {
        item("skills", n, St::Conflict,
            "originated on the Hermes side and changed on the Grok side since; merge by hand (make both copies equal)")
    }
}

/// The memory skill `<cat>/<name>`: header + the memory document, minus lines that look like secrets.
fn want_memory_unit(
    c: &Ctx,
    cat: &str,
    units: &mut Units,
    manual: &mut Vec<(String, String)>,
) -> Res<Option<String>> {
    let Some(mem) = &c.memory_md else {
        return Ok(None);
    };
    let name = c
        .spec
        .memory_skill
        .as_ref()
        .map(|m| m.name.clone())
        .unwrap_or(format!("{cat}-memory"));
    // a memory line that would fail the write guard (rule B) goes to the checklist, not into the skill
    let mut kept = String::new();
    for l in mem.split_inclusive('\n') {
        match secrets::guard_str(l) {
            Some(r) => manual.push((
                format!("memory {}", secrets::display(l.trim().trim_start_matches("- "))),
                format!("not imported: looks like it holds a secret ({r}); its text was not copied, add it by hand"),
            )),
            None => kept.push_str(l),
        }
    }
    let text = memory_header(c, &name)? + "\n" + &kept;
    units.insert(
        format!("{cat}/{name}"),
        [("SKILL.md".to_string(), text.into_bytes())].into(),
    );
    Ok(Some(format!("{cat}/{name}")))
}

/// SOUL.md sections: the spec's template, plus a `persona` section from the model's instructions. The bool is
/// true when the sections come only from the persona (a live SOUL.md without markers is then kept, and the
/// sections appended).
fn want_soul(c: &Ctx) -> Res<(Option<Sections>, bool)> {
    let mut soul = match &c.spec.soul {
        Some(t) => Some(soul_sections(&c.spec.subst(&read_str(&t.template)?))),
        None => None,
    };
    let instructions = &c.model.persona.instructions;
    let soul_append = soul.is_none() && !instructions.trim().is_empty();
    if !instructions.trim().is_empty() {
        let s = soul.get_or_insert_with(Vec::new);
        if !s.iter().any(|(n, _)| n == "persona") {
            // as `wrap` writes it back, so a written section reads back equal
            let t = instructions.trim_end();
            s.push(("persona".into(), format!("{t}\n")));
        }
    }
    Ok((soul, soul_append))
}

/// The spec's config template (head comment, values with `inherit` applied, preserved paths).
fn want_config(c: &Ctx) -> Res<Option<ConfigWant>> {
    let Some(cfg) = &c.spec.config else {
        return Ok(None);
    };
    let text = c.spec.subst(&read_str(&cfg.template)?);
    let head: String = text
        .lines()
        .take_while(|l| l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut v: Y = serde_yaml::from_str(&crate::core::yaml11::read(&text))
        .map_err(|e| format!("config template: {e}"))?;
    for (path, file) in &cfg.inherit {
        let segs: Vec<String> = path.split('.').map(String::from).collect();
        if let Some(src) = load_yaml(Path::new(file))? {
            if let Some(x) = get_path(&src, &segs) {
                set_path(&mut v, &segs, x.clone());
            }
        }
    }
    let preserve: Vec<Vec<String>> = cfg
        .preserve
        .iter()
        .map(|p| p.split('.').map(String::from).collect())
        .collect();
    Ok(Some((head, v, preserve)))
}

/// USER.md seed entries: the spec's seed file plus the model's user facts.
fn want_user_seed(c: &Ctx) -> Res<Option<String>> {
    let mut user_seed = match &c.spec.user {
        Some(u) => Some(read_str(&u.seed)?),
        None => None,
    };
    if !c.user_facts.is_empty() {
        let mut e = entries(user_seed.as_deref().unwrap_or(""));
        for f in &c.user_facts {
            if !e.contains(f) {
                e.push(f.clone());
            }
        }
        user_seed = Some(e.join(ENTRY_DELIM));
    }
    Ok(user_seed)
}

/// The spec's cron file jobs.
fn want_spec_cron(c: &Ctx) -> Res<Vec<CronJob>> {
    let Some(cr) = &c.spec.cron else {
        return Ok(vec![]);
    };
    #[derive(Deserialize)]
    struct F {
        #[serde(default)]
        jobs: Vec<CronJob>,
    }
    let f: F = serde_yaml::from_str(&read_str(&cr.file)?).map_err(|e| format!("cron file: {e}"))?;
    let mut jobs = f.jobs;
    for j in &mut jobs {
        // the spec is the owner's own file: a schedule Hermes would reject stops the plan instead of failing at apply.
        // Kept in Hermes' stored form (`every 2h` -> `every 120m`, a phrase -> its cron) so it compares with `cron_expr`.
        match super::schedule::normalize(&j.schedule) {
            Ok(s) => j.schedule = s,
            Err(why) => return err(format!("cron file: job {}: {why}", j.name)),
        }
    }
    Ok(jobs)
}

/// Upstream's skill category for imported skills.
const IMPORTS: &str = crate::adapters::grok_bot::export_json::IMPORTS;

/// Every `- fact` line of a MEMORY.md is already an entry of memories/MEMORY.md or USER.md (upstream merged them).
fn memory_in_store(dest: &Path, memory_md: &str) -> bool {
    let mut have = BTreeSet::new();
    for f in ["memories/MEMORY.md", "memories/USER.md"] {
        have.extend(entries(
            &fs::read_to_string(dest.join(f)).unwrap_or_default(),
        ));
    }
    let facts: Vec<&str> = memory_md
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.strip_prefix("- ").unwrap_or(l))
        .collect();
    !have.is_empty() && !facts.is_empty() && facts.iter().all(|f| have.contains(*f))
}

// ---------------------------------------------------------------- SOUL sections
fn begin(n: &str) -> String {
    format!("<!-- botport:begin {n} -->")
}
fn end(n: &str) -> String {
    format!("<!-- botport:end {n} -->")
}

/// (name, content) sections of a text with markers; the whole text as section "soul" when it has none.
pub fn soul_sections(t: &str) -> Vec<(String, String)> {
    match find_sections(t) {
        Some(v) if !v.is_empty() => v.into_iter().map(|(n, c, _, _)| (n, c)).collect(),
        _ => vec![("soul".into(), t.to_string())],
    }
}

/// (name, content, start byte of begin marker, end byte after end marker line)
fn find_sections(t: &str) -> Option<Vec<(String, String, usize, usize)>> {
    let mut out = vec![];
    let mut pos = 0;
    while let Some(i) = t[pos..].find("<!-- botport:begin ") {
        let s = pos + i;
        let close = t[s..].find("-->")? + s;
        let name = t[s + "<!-- botport:begin ".len()..close].trim().to_string();
        let body_start = t[close..]
            .find('\n')
            .map(|x| close + x + 1)
            .unwrap_or(t.len());
        let e = t[body_start..].find(&end(&name))? + body_start;
        let after = t[e..].find('\n').map(|x| e + x + 1).unwrap_or(t.len());
        out.push((name, t[body_start..e].to_string(), s, after));
        pos = after;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Begin/end markers that do not pair up one-to-one (unterminated, stray end, or a duplicated section name).
fn markers_malformed(t: &str) -> bool {
    let names = |pre: &str| -> Vec<String> {
        t.match_indices(pre)
            .filter_map(|(i, _)| {
                t[i + pre.len()..]
                    .find("-->")
                    .map(|e| t[i + pre.len()..i + pre.len() + e].trim().to_string())
            })
            .collect()
    };
    let (b, e) = (names("<!-- botport:begin "), names("<!-- botport:end "));
    let uniq: BTreeSet<&String> = b.iter().collect();
    b.len() != e.len()
        || uniq.len() != b.len()
        || find_sections(t).map(|v| v.len()).unwrap_or(0) != b.len()
}

fn wrap(n: &str, c: &str) -> String {
    let nl = if c.ends_with('\n') || c.is_empty() {
        ""
    } else {
        "\n"
    };
    format!("{}\n{c}{nl}{}\n", begin(n), end(n))
}

/// New SOUL.md text: owned sections replaced in place, missing ones appended, everything else kept.
pub fn render_soul(live: Option<&str>, want: &[(String, String)]) -> String {
    let Some(live) = live else {
        return want.iter().map(|(n, c)| wrap(n, c)).collect();
    };
    let Some(secs) = find_sections(live) else {
        return want.iter().map(|(n, c)| wrap(n, c)).collect();
    };
    let mut out = String::new();
    let mut last = 0;
    let mut done = BTreeSet::new();
    for (n, c, s, e) in secs {
        out += &live[last..s];
        match want.iter().find(|(wn, _)| *wn == n) {
            Some((_, wc)) => out += &wrap(&n, wc),
            None => out += &wrap(&n, &c),
        }
        done.insert(n);
        last = e;
    }
    out += &live[last..];
    for (n, c) in want {
        if !done.contains(n) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out += &wrap(n, c);
        }
    }
    out
}

// ---------------------------------------------------------------- YAML paths
fn key(s: &str) -> Y {
    Y::String(s.to_string())
}

fn key_str(k: &Y) -> String {
    match k {
        Y::String(s) => s.clone(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

pub fn leaves(v: &Y, pre: &mut Vec<String>, out: &mut Vec<(Vec<String>, Y)>) {
    match v {
        Y::Mapping(m) if !m.is_empty() => {
            for (k, x) in m {
                pre.push(key_str(k));
                leaves(x, pre, out);
                pre.pop();
            }
        }
        _ => out.push((pre.clone(), v.clone())),
    }
}

fn get_path<'a>(v: &'a Y, p: &[String]) -> Option<&'a Y> {
    let mut cur = v;
    for seg in p {
        cur = cur.as_mapping()?.get(key(seg))?;
    }
    Some(cur)
}

pub fn set_path(v: &mut Y, p: &[String], val: Y) {
    if !v.is_mapping() {
        *v = Y::Mapping(Mapping::new());
    }
    let m = v.as_mapping_mut().unwrap();
    if p.len() == 1 {
        m.insert(key(&p[0]), val);
        return;
    }
    let child = m.entry(key(&p[0])).or_insert(Y::Mapping(Mapping::new()));
    set_path(child, &p[1..], val);
}

fn under(p: &[String], roots: &[Vec<String>]) -> bool {
    roots
        .iter()
        .any(|r| p.len() >= r.len() && p[..r.len()] == r[..])
}

/// The template's list with the live list's extra items kept after the item that preceded them in live.
pub fn merge_list(want: &[Y], live: &[Y]) -> Vec<Y> {
    let mut out: Vec<Y> = want.to_vec();
    let mut prev: Option<Y> = None;
    for x in live {
        if !out.contains(x) {
            let at = prev
                .as_ref()
                .and_then(|p| out.iter().position(|y| y == p))
                .map(|i| i + 1)
                .unwrap_or(0);
            out.insert(at, x.clone());
        }
        prev = Some(x.clone());
    }
    out
}

fn yhash(v: &Y) -> String {
    sha256_hex(
        serde_json::to_string(&serde_json::to_value(v).unwrap_or_default())
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// A config list that `hermes config set` may have stored as a JSON-ish string.
fn parse_list(v: Option<&Y>) -> Vec<String> {
    match v {
        Some(Y::String(s)) => s
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .map(|x| {
                x.trim_matches(|c: char| c == ' ' || c == '\'' || c == '"')
                    .to_string()
            })
            .filter(|x| !x.is_empty())
            .collect(),
        Some(Y::Sequence(q)) => q.iter().map(key_str).collect(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------- live profile reads
pub fn skill_name(md: &Path) -> String {
    let text = fs::read_to_string(md).unwrap_or_default();
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(fm) = rest.split("---").next() {
            if let Ok(Y::Mapping(m)) = serde_yaml::from_str::<Y>(fm) {
                if let Some(n) = m.get(key("name")) {
                    let n = key_str(n).trim().to_string();
                    if !n.is_empty() && n != "~" {
                        return n;
                    }
                }
            }
        }
    }
    md.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Catalog names of installed skills outside the owned category (what `skills.disabled` matches).
pub fn other_skills(skills_root: &Path, cat: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for rel in files_sorted(skills_root) {
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if parts.last().map(|s| s.as_str()) != Some("SKILL.md") || parts[0] == cat {
            continue;
        }
        if parts[..parts.len() - 1].iter().any(|p| {
            p.starts_with('.')
                || ["references", "templates", "assets", "scripts"].contains(&p.as_str())
        }) {
            continue;
        }
        out.insert(skill_name(&skills_root.join(&rel)));
    }
    out.into_iter().collect()
}

pub fn live_unit(dest: &Path, unit: &str) -> Option<BTreeMap<String, Vec<u8>>> {
    safe_rel("skill", unit).ok()?;
    let p = dest.join("skills").join(unit);
    if p.is_file() {
        return Some([(String::new(), fs::read(&p).ok()?)].into());
    }
    if p.is_dir() {
        return read_tree(&p).ok();
    }
    None
}

pub fn read_lock(dest: &Path) -> Option<Lock> {
    let p = [LOCK, LEGACY_LOCK]
        .into_iter()
        .map(|f| dest.join(f))
        .find(|p| p.is_file())?;
    serde_json::from_slice(&fs::read(p).ok()?).ok()
}

/// SOUL.md with any pre-rename section markers read as the current ones.
pub fn read_soul(p: &Path) -> Option<String> {
    fs::read_to_string(p)
        .ok()
        .map(|t| t.replace(LEGACY_MARK, MARK))
}

/// The manual checklist, at the new name or else the old one.
fn read_manual(dest: &Path) -> Option<String> {
    fs::read_to_string(dest.join(MANUAL))
        .or_else(|_| fs::read_to_string(dest.join(LEGACY_MANUAL)))
        .ok()
}

/// True when the profile still holds a file or SOUL marker under the pre-rename name (the next apply migrates it).
pub fn has_legacy_names(dest: &Path) -> bool {
    dest.join(LEGACY_LOCK).is_file()
        || dest.join(LEGACY_MANUAL).is_file()
        || fs::read_to_string(dest.join("SOUL.md")).is_ok_and(|t| t.contains(LEGACY_MARK))
}

/// Move pre-rename names to the current ones: SOUL markers rewritten, the checklist renamed (ticks kept), the
/// old lock removed once the new one is written (by apply). Idempotent.
fn migrate_legacy(dest: &Path) -> Res<()> {
    let sp = dest.join("SOUL.md");
    if let Ok(t) = fs::read_to_string(&sp) {
        if t.contains(LEGACY_MARK) {
            write_atomic(&sp, t.replace(LEGACY_MARK, MARK).as_bytes(), Some(0o600))?;
        }
    }
    let (om, nm) = (dest.join(LEGACY_MANUAL), dest.join(MANUAL));
    if om.is_file() {
        if nm.is_file() {
            fs::remove_file(&om).map_err(|e| e.to_string())?;
        } else {
            fs::rename(&om, &nm).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn load_yaml(p: &Path) -> Res<Option<Y>> {
    if !p.is_file() {
        return Ok(None);
    }
    serde_yaml::from_str(&crate::core::yaml11::read(&read_str(p)?))
        .map(Some)
        .map_err(|e| format!("{}: {e}", p.display()))
}

pub fn live_jobs(dest: &Path) -> Vec<serde_json::Value> {
    let Ok(b) = fs::read(dest.join("cron/jobs.json")) else {
        return vec![];
    };
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
    let arr = if v.is_object() { v["jobs"].clone() } else { v };
    arr.as_array().cloned().unwrap_or_default()
}

pub fn cron_expr(j: &serde_json::Value) -> String {
    let s = &j["schedule"];
    s["expr"]
        .as_str()
        .or(j["schedule_display"].as_str())
        .or(s["display"].as_str())
        .unwrap_or("")
        .to_string()
}

pub fn entries(t: &str) -> Vec<String> {
    if t.trim().is_empty() {
        return vec![];
    }
    t.split('§')
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect()
}

// ---------------------------------------------------------------- plan
pub struct Plan {
    pub items: Vec<Item>,
    pub want: Want,
    pub lock: Option<Lock>,
    /// config.yaml as apply would write it (None = no config change)
    pub config_text: Option<String>,
    pub config_owned: BTreeMap<String, String>,
}

pub fn plan(c: &Ctx) -> Res<Plan> {
    let w = want(c)?;
    let lock = read_lock(&c.dest);
    let rec = lock.clone().unwrap_or_default();
    let mut items = vec![];
    if let Some(l) = &lock {
        items.extend(plan_source(c, l));
    }
    items.extend(w.hermes_origin.iter().cloned());
    items.extend(plan_skills(c, &w, &rec));
    if let Some(ws) = &w.soul {
        items.extend(plan_soul(c, ws, w.soul_append, &rec));
    }
    let (mut config_text, mut config_owned) = (None, BTreeMap::new());
    if let Some(cfg) = &w.config {
        let (cfg_items, text, owned) = plan_config(c, cfg, &w.cat, &rec)?;
        items.extend(cfg_items);
        (config_text, config_owned) = (text, owned);
    }
    if let Some(seed) = &w.user_seed {
        items.push(plan_user(c, seed));
    }
    let jobs = live_jobs(&c.dest);
    items.extend(plan_spec_cron(c, &w, &jobs));
    items.extend(plan_model_cron(c, &w.model_cron, &jobs, &rec));
    if !w.mcp.is_empty() {
        let live = load_yaml(&c.dest.join("config.yaml"))?;
        for (id, want) in &w.mcp {
            items.push(plan_mcp(live.as_ref(), id, want, &rec));
        }
    }
    items.extend(plan_manual(c, &w.manual));
    Ok(Plan {
        items,
        want: w,
        lock,
        config_text,
        config_owned,
    })
}

/// The source vs the last sync: which files changed.
fn plan_source(c: &Ctx, l: &Lock) -> Option<Item> {
    if l.bundle_digest == c.source.digest() {
        return None;
    }
    let read = &c.source.files_read;
    let changed: Vec<String> = read
        .keys()
        .chain(l.files_read.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|f| read.get(*f) != l.files_read.get(*f))
        .cloned()
        .collect();
    Some(item(
        "export",
        "bundle",
        St::Update,
        format!(
            "{} file(s) changed since the last sync: {}",
            changed.len(),
            changed.join(", ")
        ),
    ))
}

/// Skill units: wanted ones (create / update / conflict / adopted) and live ones the source no longer has.
fn plan_skills(c: &Ctx, w: &Want, rec: &Lock) -> Vec<Item> {
    let mut items = vec![];
    let mut live_units: BTreeSet<String> = BTreeSet::new();
    if let Ok(rd) = fs::read_dir(c.dest.join("skills").join(&w.cat)) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with('.') {
                live_units.insert(format!("{}/{n}", w.cat));
            }
        }
    }
    for (u, files) in &w.units {
        items.extend(plan_unit(c, w, rec, u, files));
    }
    for u in live_units.iter().filter(|u| !w.units.contains_key(*u)) {
        let ld = live_unit(&c.dest, u)
            .map(|l| tree_digest_files(&l))
            .unwrap_or_default();
        items.push(if rec.skills.get(u) == Some(&ld) {
            item(
                "skills",
                u,
                St::Retire,
                "no longer in the export; moves to backups/",
            )
        } else {
            item(
                "skills",
                u,
                St::Conflict,
                "in the owned category but not in the export (not written by botport, or edited)",
            )
        });
    }
    items
}

/// One wanted skill unit (plus a `duplicate` item when `hermes migrate grok-bot` also installed it).
fn plan_unit(
    c: &Ctx,
    w: &Want,
    rec: &Lock,
    u: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Vec<Item> {
    let mut items = vec![];
    let wd = tree_digest_files(files);
    let leaf = u.split_once('/').map(|x| x.1).unwrap_or(u);
    // `hermes migrate grok-bot` output: the same skill under skills/grok-bot-imports/<slug>
    let imported = [
        leaf.to_string(),
        crate::adapters::grok_bot::export_json::slug(leaf),
    ]
    .into_iter()
    .map(|n| format!("{IMPORTS}/{n}"))
    .find(|n| c.dest.join("skills").join(n).join("SKILL.md").is_file());
    let live = live_unit(&c.dest, u);
    if let (Some(imp), Some(_)) = (&imported, &live) {
        items.push(item(
            "skills",
            u,
            St::Duplicate,
            format!("in both skills/{u} and skills/{imp} (from hermes migrate grok-bot); keep one by hand, neither is deleted"),
        ));
    }
    let it = match live {
        None if imported.is_some() => item(
            "skills",
            u,
            St::InSync,
            format!(
                "adopted: present as skills/{} (hermes migrate grok-bot)",
                imported.unwrap_or_default()
            ),
        ),
        None if w.memory_unit.as_deref() == Some(u)
            && memory_in_store(&c.dest, c.memory_md.as_deref().unwrap_or("")) =>
        {
            item(
                "skills",
                u,
                St::InSync,
                "adopted: every fact already in memories/MEMORY.md or USER.md (hermes migrate grok-bot)",
            )
        }
        None => item("skills", u, St::Create, "missing in live"),
        Some(l) => {
            let ld = tree_digest_files(&l);
            if ld == wd {
                item("skills", u, St::InSync, "")
            } else if rec.skills.get(u) == Some(&ld) {
                item(
                    "skills",
                    u,
                    St::Update,
                    "export changed; live copy untouched since last sync",
                )
            } else if rec.skills.contains_key(u) {
                item(
                    "skills",
                    u,
                    St::Conflict,
                    "edited on the Hermes side since the last sync; not overwritten",
                )
            } else {
                Item { unrecorded: true, ..item("skills", u, St::Conflict, "differs from the export and no sync record (apply --adopt to take it over)") }
            }
        }
    };
    items.push(it);
    items
}

/// SOUL.md: per owned section when the live file has markers; the whole file when it has none.
fn plan_soul(c: &Ctx, ws: &[(String, String)], soul_append: bool, rec: &Lock) -> Vec<Item> {
    let mut items = vec![];
    let live = read_soul(&c.dest.join("SOUL.md"));
    match (&live, live.as_deref().and_then(find_sections)) {
        (None, _) => items.push(item("soul", "SOUL.md", St::Create, "missing in live")),
        (Some(l), _) if markers_malformed(l) => items.push(item(
            "soul",
            "SOUL.md",
            St::Conflict,
            "botport markers unbalanced or duplicated; fix by hand",
        )),
        (Some(l), None) if soul_append => {
            // Hermes-side text (or upstream's import of the same persona): kept; sections appended
            for (n, wc) in ws {
                if l.contains(wc.trim()) {
                    items.push(item(
                        "soul",
                        n,
                        St::InSync,
                        "already in SOUL.md (no markers)",
                    ));
                } else {
                    items.push(item("soul", n, St::Create, "section missing"));
                }
            }
        }
        (Some(l), None) => {
            if ws.len() == 1 && *l == ws[0].1 {
                items.push(item(
                    "soul",
                    "SOUL.md",
                    St::InSync,
                    "whole file equals the template (no markers)",
                ));
            } else if rec.soul.get("__file__") == Some(&sha256_hex(l.as_bytes())) {
                items.push(item("soul", "SOUL.md", St::Update, "template changed"));
            } else {
                items.push(Item {
                    unrecorded: !rec.soul.contains_key("__file__"),
                    ..item(
                        "soul",
                        "SOUL.md",
                        St::Conflict,
                        "differs from the template and has no botport markers",
                    )
                });
            }
        }
        (Some(_), Some(secs)) => {
            for (n, wc) in ws {
                match secs.iter().find(|s| s.0 == *n) {
                    None => items.push(item("soul", n, St::Create, "section missing")),
                    Some(s) if s.1 == *wc => items.push(item("soul", n, St::InSync, "")),
                    Some(s) if rec.soul.get(n) == Some(&sha256_hex(s.1.as_bytes())) => {
                        items.push(item("soul", n, St::Update, "template changed"))
                    }
                    Some(_) => items.push(Item {
                        unrecorded: !rec.soul.contains_key(n),
                        ..item("soul", n, St::Conflict, "section edited on the Hermes side")
                    }),
                }
            }
        }
    }
    items
}

/// (items, config.yaml as apply would write it, owned path -> value digest)
type ConfigPlan = (Vec<Item>, Option<String>, BTreeMap<String, String>);

/// config.yaml: the template's owned leaf paths (minus `preserve`), compared one by one.
fn plan_config(
    c: &Ctx,
    (head, tpl, preserve): &ConfigWant,
    cat: &str,
    rec: &Lock,
) -> Res<ConfigPlan> {
    let mut items = vec![];
    let mut config_owned = BTreeMap::new();
    let live = load_yaml(&c.dest.join("config.yaml"))?;
    let mut new = live.clone().unwrap_or_else(|| tpl.clone());
    let mut owned = vec![];
    leaves(tpl, &mut vec![], &mut owned);
    if c.spec.disable_other_skills {
        let dis: Vec<Y> = other_skills(&c.dest.join("skills"), cat)
            .into_iter()
            .map(Y::String)
            .collect();
        owned.retain(|(p, _)| p.join(".") != "skills.disabled");
        owned.push((vec!["skills".into(), "disabled".into()], Y::Sequence(dis)));
    }
    let mut changed = false;
    for (path, wv) in owned {
        if under(&path, preserve) {
            continue;
        }
        let dotted = path.join(".");
        let lv = live.as_ref().and_then(|l| get_path(l, &path)).cloned();
        let target = config_target(&dotted, &wv, lv.as_ref());
        config_owned.insert(dotted.clone(), yhash(&target));
        if config_same(&dotted, &wv, lv.as_ref(), &target) {
            continue;
        }
        changed = true;
        set_path(&mut new, &path, target.clone());
        items.push(config_item(&dotted, lv.as_ref(), &target, rec));
    }
    let mut config_text = None;
    if changed || live.is_none() {
        config_text = Some(format!(
            "{head}\n{}",
            crate::core::yaml11::emit_checked(&new)?
        ));
    }
    if items.is_empty() {
        items.push(item(
            "config",
            "config.yaml",
            St::InSync,
            format!("{} owned path(s)", config_owned.len()),
        ));
    }
    Ok((items, config_text, config_owned))
}

/// The value an owned path should hold: the template's; a list keeps the live list's extra items
/// (except `skills.disabled`, which is owned whole).
fn config_target(dotted: &str, wv: &Y, lv: Option<&Y>) -> Y {
    match (wv, lv) {
        (Y::Sequence(ws), Some(Y::Sequence(ls))) if dotted != "skills.disabled" => {
            Y::Sequence(merge_list(ws, ls))
        }
        _ => wv.clone(),
    }
}

/// The live value already says what the template wants (an empty template mapping matches any mapping;
/// `skills.disabled` compares as a set).
fn config_same(dotted: &str, wv: &Y, lv: Option<&Y>, target: &Y) -> bool {
    match lv {
        Some(Y::Mapping(_)) if matches!(wv, Y::Mapping(m) if m.is_empty()) => true,
        Some(l) if dotted == "skills.disabled" => {
            parse_list(Some(l)).into_iter().collect::<BTreeSet<_>>()
                == parse_list(Some(target)).into_iter().collect()
        }
        Some(l) => l == target,
        None => false,
    }
}

/// A differing owned config path: create / update / conflict (values redacted).
fn config_item(dotted: &str, lv: Option<&Y>, target: &Y, rec: &Lock) -> Item {
    let detail_extra = if dotted == "skills.disabled" {
        let l: BTreeSet<String> = parse_list(lv).into_iter().collect();
        let t: BTreeSet<String> = parse_list(Some(target)).into_iter().collect();
        format!(
            " [to disable: {}; to re-enable: {}]",
            t.difference(&l).cloned().collect::<Vec<_>>().join(", "),
            l.difference(&t).cloned().collect::<Vec<_>>().join(", ")
        )
    } else {
        String::new()
    };
    let st = match lv {
        None => item("config", dotted, St::Create, "missing in live"),
        Some(l) if rec.config.get(dotted) == Some(&yhash(l)) => {
            item("config", dotted, St::Update, "template changed")
        }
        Some(_) if rec.config.contains_key(dotted) => item(
            "config",
            dotted,
            St::Conflict,
            "edited on the Hermes side (values redacted)",
        ),
        Some(_) => Item {
            unrecorded: true,
            ..item(
                "config",
                dotted,
                St::Conflict,
                "differs from the template (values redacted)",
            )
        },
    };
    Item {
        detail: st.detail.clone() + &detail_extra,
        ..st
    }
}

/// USER.md: every seed entry present (entries are added, never removed).
fn plan_user(c: &Ctx, seed: &str) -> Item {
    let live = fs::read_to_string(c.dest.join("memories/USER.md")).unwrap_or_default();
    let have = entries(&live);
    let missing: Vec<String> = entries(seed)
        .into_iter()
        .filter(|e| !have.contains(e))
        .collect();
    if missing.is_empty() {
        item("user", "USER.md", St::InSync, "seed entries present")
    } else {
        item(
            "user",
            "USER.md",
            St::Create,
            format!("{} seed entr(y/ies) missing", missing.len()),
        )
    }
}

/// The spec's cron jobs (matched by name), and paused jobs `hermes migrate grok-bot` already made.
fn plan_spec_cron(c: &Ctx, w: &Want, jobs: &[serde_json::Value]) -> Vec<Item> {
    let mut items = vec![];
    // paused jobs `hermes migrate grok-bot` created ("<Bot>: <routine>"): already present, never re-created
    if !c.model.persona.name.is_empty() {
        let pre = format!("{}: ", c.model.persona.name);
        for j in jobs {
            let n = j["name"].as_str().unwrap_or("");
            if n.starts_with(&pre)
                && j["enabled"] == json!(false)
                && !w.cron.iter().chain(&w.model_cron).any(|x| x.name == n)
            {
                items.push(item(
                    "cron",
                    n,
                    St::InSync,
                    "adopted: paused job from hermes migrate grok-bot",
                ));
            }
        }
    }
    for j in &w.cron {
        if !c.real_home {
            // cron is managed through the hermes CLI, which only serves ~/.hermes (or --no-cron): not compared here
            items.push(item(
                "cron",
                &j.name,
                St::Skip,
                "not managed here (--no-cron or not ~/.hermes)",
            ));
            continue;
        }
        let same: Vec<&serde_json::Value> = jobs
            .iter()
            .filter(|x| x["name"].as_str() == Some(&j.name))
            .collect();
        items.push(match same.first() {
            None => item("cron", &j.name, St::Create, "missing"),
            Some(_) if same.len() > 1 => item(
                "cron",
                &j.name,
                St::Conflict,
                format!("{} live jobs with this name", same.len()),
            ),
            Some(cur) => spec_job_item(j, cur),
        });
    }
    items
}

/// A spec job that exists live: in sync, update (prompt/deliver/schedule differ) or paused by the owner.
fn spec_job_item(j: &CronJob, cur: &serde_json::Value) -> Item {
    let diff: Vec<&str> = [
        ("prompt", cur["prompt"].as_str() == Some(&j.prompt)),
        ("deliver", cur["deliver"].as_str() == Some(&j.deliver)),
        ("schedule", cron_expr(cur) == j.schedule),
    ]
    .iter()
    .filter(|(_, ok)| !ok)
    .map(|(k, _)| *k)
    .collect();
    // a job the spec creates paused is expected to be paused; resuming it on the Hermes side is the owner's call
    if cur["enabled"] == json!(false) && !j.paused() {
        item(
            "cron",
            &j.name,
            St::Conflict,
            "paused/disabled on the Hermes side",
        )
    } else if diff.is_empty() {
        item("cron", &j.name, St::InSync, "")
    } else {
        item(
            "cron",
            &j.name,
            St::Update,
            format!("{} differ(s)", diff.join(", ")),
        )
    }
}

/// The bot's own schedules: paused cron jobs, adopted when a job of that name exists.
fn plan_model_cron(
    c: &Ctx,
    model_cron: &[CronJob],
    jobs: &[serde_json::Value],
    rec: &Lock,
) -> Vec<Item> {
    let mut items = vec![];
    for j in model_cron {
        let same: Vec<&serde_json::Value> = jobs
            .iter()
            .filter(|x| x["name"].as_str() == Some(&j.name))
            .collect();
        if !c.real_home {
            items.push(if same.is_empty() {
                item(
                    "cron",
                    &j.name,
                    St::Skip,
                    "not managed here (--no-cron or not ~/.hermes)",
                )
            } else {
                item(
                    "cron",
                    &j.name,
                    St::InSync,
                    "adopted: a job with this name exists",
                )
            });
            continue;
        }
        let want_d = job_digest(&j.prompt, &j.schedule);
        items.push(match same.as_slice() {
            [] => item("cron", &j.name, St::Create, "missing (created paused)"),
            [cur] => {
                let live_d = job_digest(cur["prompt"].as_str().unwrap_or(""), &cron_expr(cur));
                match rec.g2h_cron.get(&j.name) {
                    _ if live_d == want_d => item("cron", &j.name, St::InSync, ""),
                    None => item(
                        "cron",
                        &j.name,
                        St::InSync,
                        "adopted: a job with this name exists",
                    ),
                    Some(r) if *r == live_d => item(
                        "cron",
                        &j.name,
                        St::Update,
                        "description changed; live job untouched since last sync",
                    ),
                    Some(_) => item(
                        "cron",
                        &j.name,
                        St::Conflict,
                        "edited on the Hermes side; not overwritten",
                    ),
                }
            }
            more => item(
                "cron",
                &j.name,
                St::Conflict,
                format!("{} live jobs with this name", more.len()),
            ),
        });
    }
    // one item per job: a job whose routine named no zone says so in the plan, --json and --details
    for (it, j) in items.iter_mut().zip(model_cron) {
        if j.paused_reason
            .as_deref()
            .is_some_and(|r| r.contains(NO_ZONE))
        {
            it.detail = if it.detail.is_empty() {
                PROFILE_TZ.into()
            } else {
                format!("{}; {PROFILE_TZ}", it.detail)
            };
        }
    }
    items
}

/// One MCP connector: owned `mcp_servers.<id>` leaves, adopted when the live entry already says the same.
fn plan_mcp(live: Option<&Y>, id: &str, want: &Y, rec: &Lock) -> Item {
    let name = format!("mcp_servers.{id}");
    let pre = vec!["mcp_servers".to_string(), id.to_string()];
    let mut ls = vec![];
    leaves(want, &mut pre.clone(), &mut ls);
    let live_vals: Vec<Y> = ls
        .iter()
        .map(|(p, _)| {
            live.and_then(|l| get_path(l, p))
                .cloned()
                .unwrap_or(Y::Null)
        })
        .collect();
    let want_vals: Vec<Y> = ls.iter().map(|(_, v)| v.clone()).collect();
    let lh = yhash(&Y::Sequence(live_vals.clone()));
    let exists = live.and_then(|l| get_path(l, &pre)).is_some();
    // `hermes migrate grok-bot` names the server after the whole app name: the same url is the same server
    let same_url = live
        .and_then(|l| get_path(l, &["mcp_servers".to_string()]))
        .and_then(|m| m.as_mapping())
        .and_then(|m| {
            m.iter()
                .find(|(_, v)| v.get("url").is_some() && v.get("url") == want.get("url"))
        })
        .map(|(k, _)| key_str(k));
    if live_vals == want_vals {
        item("mcp", &name, St::InSync, "")
    } else if let Some(other) = same_url.filter(|o| !exists && o != id) {
        item(
            "mcp",
            &name,
            St::InSync,
            format!("adopted: mcp_servers.{other} has this url"),
        )
    } else if !exists {
        item("mcp", &name, St::Create, "missing in live")
    } else if rec.config.get(&name) == Some(&lh) {
        item("mcp", &name, St::Update, "description changed")
    } else if rec.config.contains_key(&name) {
        item(
            "mcp",
            &name,
            St::Conflict,
            "edited on the Hermes side (values redacted)",
        )
    } else {
        Item {
            unrecorded: true,
            ..item(
                "mcp",
                &name,
                St::Conflict,
                "differs from the description and no sync record (apply --adopt to take it over)",
            )
        }
    }
}

/// What needs a human: one item each, and the checklist file.
fn plan_manual(c: &Ctx, manual: &[(String, String)]) -> Vec<Item> {
    if manual.is_empty() {
        return vec![];
    }
    let mut items: Vec<Item> = manual
        .iter()
        .map(|(n, what)| item("manual", n, St::Manual, what.clone()))
        .collect();
    let want_t = manual_text(manual);
    items.push(match read_manual(&c.dest) {
        None => item("manual", MANUAL, St::Create, "missing"),
        Some(l) if untick(&l) == want_t => item("manual", MANUAL, St::InSync, ""),
        Some(_) => item("manual", MANUAL, St::Update, "list changed (ticks kept)"),
    });
    items
}

// ---------------------------------------------------------------- apply
fn should_write(i: &Item, adopt: bool) -> bool {
    i.status.is_change() || (adopt && i.status == St::Conflict && i.unrecorded)
}

pub fn restore_point(dest: &Path, cat: &str, keep: usize) -> Res<PathBuf> {
    use std::io::Write;
    let dir = dest.join(BACKUPS);
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut add = |rel: String| -> Res<()> {
        let b = read(&dest.join(&rel))?;
        // The archive is compressed, so the write guard cannot see into it: guard each entry's plaintext here. A hit
        // refuses the restore point (and so the apply, before any change): a secret never reaches disk, not even zipped.
        if let Some(at) = secrets::find_secret(&rel, &b) {
            return Err(format!(
                "live file {rel}: {at}. botport will not copy it into a restore point, so no restore point was \
                 written and nothing was applied. Move the value to the profile's .env, then retry"
            ));
        }
        z.start_file(rel, opts).map_err(|e| e.to_string())?;
        z.write_all(&b).map_err(|e| e.to_string())
    };
    for f in [
        "config.yaml",
        "SOUL.md",
        "memories/USER.md",
        "cron/jobs.json",
        MANUAL,
        LOCK,
        LEGACY_MANUAL,
        LEGACY_LOCK,
    ] {
        if dest.join(f).is_file() {
            add(f.to_string())?;
        }
    }
    let sk = dest.join("skills").join(cat);
    for rel in files_sorted(&sk) {
        add(format!("skills/{cat}/{}", rel_str(&rel)))?;
    }
    let zip = z.finish().map_err(|e| e.to_string())?.into_inner();
    // directories only once every entry passed the guard: a refused restore point leaves nothing behind
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = utc_stamp();
    let mut n = 0;
    let mut path = dir.join(format!("restore-{stamp}-{n:02}.zip"));
    while path.exists() {
        n += 1;
        path = dir.join(format!("restore-{stamp}-{n:02}.zip"));
    }
    write_atomic(&path, &zip, Some(0o600))?;
    // keep the newest `keep`
    let mut all: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("restore-") && n.ends_with(".zip")
                })
                .unwrap_or(false)
        })
        .collect();
    all.sort();
    while all.len() > keep {
        fs::remove_file(all.remove(0)).ok();
    }
    Ok(path)
}

pub fn apply(c: &Ctx, adopt: bool, dry: bool) -> Res<Applied> {
    let p = plan(c)?;
    let todo: Vec<&Item> = p
        .items
        .iter()
        .filter(|i| should_write(i, adopt) && i.area != "export")
        .collect();
    let mut out: Vec<Item> = p.items.clone();
    if dry {
        return Ok(Applied {
            items: out,
            restore_point: None,
            restart_needed: false,
        });
    }
    let mut rp = None;
    let legacy = has_legacy_names(&c.dest);
    if !todo.is_empty() || legacy {
        fs::create_dir_all(&c.dest).map_err(|e| e.to_string())?;
        rp = Some(restore_point(&c.dest, &p.want.cat, 5)?);
    }
    if legacy {
        migrate_legacy(&c.dest)?;
    }
    let mut lock = p.lock.clone().unwrap_or_default();
    apply_skills(c, &p, &todo, &mut out, &mut lock, adopt)?;
    if let Some(ws) = &p.want.soul {
        apply_soul(c, &p, ws, &todo, &mut out, &mut lock, adopt)?;
    }
    let mut restart = apply_config(c, &p, &todo, &mut out, &mut lock, adopt)?;
    apply_user(c, &p, &todo, &mut out)?;
    restart |= apply_mcp(c, &p, &todo, &mut out, &mut lock, adopt)?;
    apply_manual(c, &p, &todo, &mut out)?;
    record_model_cron(c, &p.want.model_cron, &out, &mut lock);
    apply_cron(c, &p, &todo, &mut out)?;
    write_lock(c, &mut lock)?;
    Ok(Applied {
        items: out,
        restore_point: rp,
        restart_needed: restart,
    })
}

/// Mark the items of one unit written: a conflict taken over (`--adopt`) becomes an update.
fn mark_done(out: &mut [Item], area: &str, name: &str, detail: &str) {
    for i in out.iter_mut().filter(|i| i.area == area && i.name == name) {
        if i.status == St::Conflict {
            i.status = St::Update;
            i.unrecorded = false;
            i.detail = format!("adopted -> {detail}");
        } else {
            i.detail = format!("{} -> {detail}", i.status.word());
        }
    }
}

/// A conflict that stays one in this run (not taken over by `--adopt`).
fn kept_conflict(i: &Item, adopt: bool) -> bool {
    i.status == St::Conflict && !(adopt && i.unrecorded)
}

/// Skill units: retired ones move to backups/, the rest are written; the lock records every unit now ours.
fn apply_skills(
    c: &Ctx,
    p: &Plan,
    todo: &[&Item],
    out: &mut [Item],
    lock: &mut Lock,
    adopt: bool,
) -> Res<()> {
    let stamp = utc_stamp();
    for i in todo.iter().filter(|i| i.area == "skills") {
        safe_rel("skill", &i.name)?;
        let unit_path = c.dest.join("skills").join(&i.name);
        if i.status == St::Retire {
            let tgt = c
                .dest
                .join(BACKUPS)
                .join(format!("retired-{stamp}"))
                .join(&i.name);
            fs::create_dir_all(tgt.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::rename(&unit_path, &tgt).map_err(|e| format!("retire {}: {e}", i.name))?;
            lock.skills.remove(&i.name);
            mark_done(out, "skills", &i.name, "moved to backups/");
            continue;
        }
        let files = &p.want.units[&i.name];
        if files.len() == 1 && files.contains_key("") {
            write_atomic(&unit_path, &files[""], Some(0o644))?;
        } else {
            if let Some(live) = live_unit(&c.dest, &i.name) {
                for rel in live.keys().filter(|r| !files.contains_key(*r)) {
                    fs::remove_file(unit_path.join(rel)).map_err(|e| e.to_string())?;
                }
            }
            for (rel, b) in files {
                safe_rel("skill file", rel)?;
                write_atomic(&unit_path.join(rel), b, Some(0o644))?;
            }
        }
        mark_done(out, "skills", &i.name, "written");
    }
    for (u, files) in &p.want.units {
        if out.iter().any(|i| {
            i.area == "skills" && i.name == *u && (i.status == St::InSync || should_write(i, adopt))
        }) {
            lock.skills.insert(u.clone(), tree_digest_files(files));
        }
    }
    Ok(())
}

/// SOUL.md: only the sections being changed are written (conflicting ones keep their live text).
fn apply_soul(
    c: &Ctx,
    p: &Plan,
    ws: &[(String, String)],
    todo: &[&Item],
    out: &mut [Item],
    lock: &mut Lock,
    adopt: bool,
) -> Res<()> {
    let sp = c.dest.join("SOUL.md");
    let live = fs::read_to_string(&sp).ok();
    let soul_todo: Vec<&&Item> = todo.iter().filter(|i| i.area == "soul").collect();
    if !soul_todo.is_empty() {
        let live_secs = live.as_deref().and_then(find_sections);
        let keep: Vec<(String, String)> = ws
            .iter()
            .map(|(n, wc)| {
                let changing = soul_todo
                    .iter()
                    .any(|i| i.name == *n || i.name == "SOUL.md");
                let live_c = live_secs
                    .as_ref()
                    .and_then(|s| s.iter().find(|x| x.0 == *n))
                    .map(|x| x.1.clone());
                let text = if changing {
                    wc.clone()
                } else {
                    live_c.unwrap_or(wc.clone())
                };
                (n.clone(), text)
            })
            .collect();
        let legacy = live_secs.is_none();
        let text = match (legacy, p.want.soul_append, live.as_deref()) {
            (true, true, Some(l)) if !l.trim().is_empty() => {
                let sep = if l.ends_with('\n') { "" } else { "\n" };
                format!("{l}{sep}{}", render_soul(None, &keep))
            }
            _ => render_soul(if legacy { None } else { live.as_deref() }, &keep),
        };
        write_atomic(&sp, text.as_bytes(), Some(0o600))?;
        for i in &soul_todo {
            mark_done(out, "soul", &i.name, "written");
        }
    }
    if let Ok(now) = fs::read_to_string(&sp) {
        record_soul(lock, &p.items, &now, adopt);
    }
    Ok(())
}

/// Record only what is ours now (in sync or just written). `__file__` (the whole SOUL.md) is recorded only when
/// no SOUL item conflicts, and removed when any does; a conflicting section keeps its old record (or none), so
/// a later plan still sees the Hermes-side edit as a conflict. A conflict `--adopt` takes over counts as ours.
fn record_soul(lock: &mut Lock, items: &[Item], now: &str, adopt: bool) {
    let soul_conflict = |n: &str| {
        items.iter().any(|i| {
            i.area == "soul" && (i.name == n || i.name == "SOUL.md") && kept_conflict(i, adopt)
        })
    };
    if items
        .iter()
        .any(|i| i.area == "soul" && kept_conflict(i, adopt))
    {
        lock.soul.remove("__file__");
    } else {
        lock.soul
            .insert("__file__".into(), sha256_hex(now.as_bytes()));
    }
    for (n, cnt) in find_sections(now)
        .unwrap_or_default()
        .into_iter()
        .map(|x| (x.0, x.1))
    {
        if !soul_conflict(&n) {
            lock.soul.insert(n, sha256_hex(cnt.as_bytes()));
        }
    }
}

/// config.yaml: the planned text, or (with conflicts) only the changing paths on top of the live file. True when
/// it was written (the gateway needs a restart).
fn apply_config(
    c: &Ctx,
    p: &Plan,
    todo: &[&Item],
    out: &mut [Item],
    lock: &mut Lock,
    adopt: bool,
) -> Res<bool> {
    let mut restart = false;
    if let Some(text) = &p.config_text {
        let cfg_todo: Vec<&&Item> = todo.iter().filter(|i| i.area == "config").collect();
        let has_conflict = p
            .items
            .iter()
            .any(|i| i.area == "config" && kept_conflict(i, adopt));
        if !cfg_todo.is_empty() || !c.dest.join("config.yaml").exists() {
            if has_conflict {
                // Rebuild without the conflicting paths: re-plan semantics, applied path by path.
                let mut new =
                    load_yaml(&c.dest.join("config.yaml"))?.unwrap_or(Y::Mapping(Mapping::new()));
                let full: Y = serde_yaml::from_str(&crate::core::yaml11::read(text))
                    .map_err(|e| e.to_string())?;
                for i in &cfg_todo {
                    let path: Vec<String> = split_dotted(&i.name, &full);
                    if let Some(v) = get_path(&full, &path) {
                        set_path(&mut new, &path, v.clone());
                    }
                }
                let head = &p.want.config.as_ref().unwrap().0;
                write_atomic(
                    &c.dest.join("config.yaml"),
                    format!("{head}\n{}", crate::core::yaml11::emit_checked(&new)?).as_bytes(),
                    Some(0o600),
                )?;
            } else {
                write_atomic(&c.dest.join("config.yaml"), text.as_bytes(), Some(0o600))?;
            }
            restart = true;
            for i in &cfg_todo {
                mark_done(out, "config", &i.name, "written");
            }
        }
    }
    for (k, h) in &p.config_owned {
        if !p
            .items
            .iter()
            .any(|i| i.area == "config" && i.name == *k && kept_conflict(i, adopt))
        {
            lock.config.insert(k.clone(), h.clone());
        }
    }
    Ok(restart)
}

/// USER.md: missing seed entries appended.
fn apply_user(c: &Ctx, p: &Plan, todo: &[&Item], out: &mut [Item]) -> Res<()> {
    let (Some(seed), true) = (&p.want.user_seed, todo.iter().any(|i| i.area == "user")) else {
        return Ok(());
    };
    let up = c.dest.join("memories/USER.md");
    let live = fs::read_to_string(&up).unwrap_or_default();
    let mut have = entries(&live);
    for e in entries(seed) {
        if !have.contains(&e) {
            have.push(e);
        }
    }
    write_atomic(&up, (have.join(ENTRY_DELIM) + "\n").as_bytes(), Some(0o600))?;
    mark_done(out, "user", "USER.md", "seed entries added");
    Ok(())
}

/// MCP entries (owned leaves only, on top of whatever config.yaml now holds). True when config.yaml was written.
fn apply_mcp(
    c: &Ctx,
    p: &Plan,
    todo: &[&Item],
    out: &mut [Item],
    lock: &mut Lock,
    adopt: bool,
) -> Res<bool> {
    let mcp_todo: Vec<&&Item> = todo.iter().filter(|i| i.area == "mcp").collect();
    if !mcp_todo.is_empty() {
        let cp = c.dest.join("config.yaml");
        let text = fs::read_to_string(&cp).unwrap_or_default();
        let head: String = text
            .lines()
            .take_while(|l| l.starts_with('#'))
            .map(|l| format!("{l}\n"))
            .collect();
        let mut new = load_yaml(&cp)?.unwrap_or(Y::Mapping(Mapping::new()));
        for i in &mcp_todo {
            let (id, want) = p
                .want
                .mcp
                .iter()
                .find(|(id, _)| format!("mcp_servers.{id}") == i.name)
                .unwrap();
            let mut ls = vec![];
            leaves(
                want,
                &mut vec!["mcp_servers".to_string(), id.clone()],
                &mut ls,
            );
            for (path, v) in ls {
                set_path(&mut new, &path, v);
            }
        }
        let body = crate::core::yaml11::emit_checked(&new)?;
        let out_t = if head.is_empty() {
            body
        } else {
            format!("{head}\n{body}")
        };
        write_atomic(&cp, out_t.as_bytes(), Some(0o600))?;
        for i in &mcp_todo {
            mark_done(out, "mcp", &i.name, "written");
        }
    }
    for (id, want) in &p.want.mcp {
        let name = format!("mcp_servers.{id}");
        let ours = out.iter().any(|i| {
            i.area == "mcp" && i.name == name && (i.status == St::InSync || should_write(i, adopt))
        });
        if ours {
            let mut ls = vec![];
            leaves(want, &mut vec![], &mut ls);
            lock.config.insert(
                name,
                yhash(&Y::Sequence(ls.into_iter().map(|(_, v)| v).collect())),
            );
        }
    }
    Ok(!mcp_todo.is_empty())
}

/// The manual checklist (ticks the user made on unchanged lines are kept).
fn apply_manual(c: &Ctx, p: &Plan, todo: &[&Item], out: &mut [Item]) -> Res<()> {
    if !todo.iter().any(|i| i.area == "manual") {
        return Ok(());
    }
    let mp = c.dest.join(MANUAL);
    let ticked: BTreeSet<String> = fs::read_to_string(&mp)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("- [x] ").or(l.strip_prefix("- [X] ")))
        .map(String::from)
        .collect();
    let t: String = manual_text(&p.want.manual)
        .lines()
        .map(|l| match l.strip_prefix("- [ ] ") {
            Some(r) if ticked.contains(r) => format!("- [x] {r}\n"),
            _ => format!("{l}\n"),
        })
        .collect();
    write_atomic(&mp, t.as_bytes(), Some(0o644))?;
    mark_done(out, "manual", MANUAL, "written");
    Ok(())
}

/// Record the bot's cron jobs that are ours: adopted live ones as they are, created/updated ones as wanted.
fn record_model_cron(c: &Ctx, model_cron: &[CronJob], out: &[Item], lock: &mut Lock) {
    for j in model_cron {
        let st = out
            .iter()
            .find(|i| i.area == "cron" && i.name == j.name)
            .map(|i| i.status);
        let live = live_jobs(&c.dest);
        let cur = live.iter().find(|x| x["name"].as_str() == Some(&j.name));
        match (st, cur) {
            (Some(St::InSync), Some(cur)) => {
                let d = job_digest(cur["prompt"].as_str().unwrap_or(""), &cron_expr(cur));
                lock.g2h_cron.insert(j.name.clone(), d);
            }
            (Some(St::Create | St::Update), _) => {
                lock.g2h_cron
                    .insert(j.name.clone(), job_digest(&j.prompt, &j.schedule));
            }
            _ => {}
        }
    }
}

/// Cron jobs through the hermes CLI (which owns jobs.json); skipped off ~/.hermes.
fn apply_cron(c: &Ctx, p: &Plan, todo: &[&Item], out: &mut [Item]) -> Res<()> {
    for i in todo.iter().filter(|i| i.area == "cron") {
        let j = p
            .want
            .cron
            .iter()
            .chain(&p.want.model_cron)
            .find(|j| j.name == i.name)
            .unwrap();
        if !c.real_home {
            for o in out
                .iter_mut()
                .filter(|o| o.area == "cron" && o.name == i.name)
            {
                o.status = St::Skip;
                o.detail = "cron is managed by the hermes CLI, which only serves ~/.hermes".into();
            }
            continue;
        }
        run_hermes_cron(c, j, i.status == St::Create)?;
        // Read back: an exit code of 0 is not proof the job exists.
        if live_jobs(&c.dest)
            .iter()
            .any(|x| x["name"].as_str() == Some(&j.name))
        {
            mark_done(out, "cron", &i.name, "written via hermes cron");
        } else {
            mark_done(
                out,
                "cron",
                &i.name,
                "hermes cron exited 0 but the job is not in cron/jobs.json; check `hermes cron list`",
            );
        }
    }
    Ok(())
}

/// `hermes cron create` (paused unless the job is explicitly enabled) or `hermes cron edit`.
fn run_hermes_cron(c: &Ctx, j: &CronJob, create: bool) -> Res<()> {
    // hermes writes its job store itself: every argument passes the write-boundary guard first
    let mut args: Vec<(&str, &str)> = vec![
        ("name", &j.name),
        ("deliver", &j.deliver),
        ("schedule", &j.schedule),
        ("prompt", &j.prompt),
    ];
    if let Some(r) = &j.paused_reason {
        args.push(("paused-reason", r));
    }
    secrets::guard_args("hermes cron", &args)?;
    let mut cmd = Command::new(&c.hermes_bin);
    if let Some(h) = &c.hermes_home {
        cmd.env("HERMES_HOME", h);
    }
    cmd.args(["-p", &c.profile, "cron"]);
    if create {
        cmd.args(["create", "--name", &j.name, "--deliver", &j.deliver]);
        if j.paused() {
            cmd.arg("--paused");
        }
        if let Some(r) = &j.paused_reason {
            cmd.args(["--paused-reason", r]);
        }
        cmd.args([&j.schedule, &j.prompt]);
    } else {
        let id = live_jobs(&c.dest)
            .into_iter()
            .find(|x| x["name"].as_str() == Some(&j.name))
            .and_then(|x| x["id"].as_str().map(String::from))
            .ok_or("cron job id not found")?;
        cmd.args([
            "edit",
            &id,
            "--schedule",
            &j.schedule,
            "--deliver",
            &j.deliver,
            "--prompt",
            &j.prompt,
        ]);
    }
    let o = cmd.output().map_err(|e| format!("{}: {e}", c.hermes_bin))?;
    if !o.status.success() {
        return err(format!(
            "hermes cron {} failed: {}",
            j.name,
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .last()
                .unwrap_or("")
        ));
    }
    Ok(())
}

/// The lock: source digest and files read, written when it changed; the pre-rename lock removed.
fn write_lock(c: &Ctx, lock: &mut Lock) -> Res<()> {
    lock.version = 1;
    lock.tool_version = crate::adapters::grok_bot::bundle::VERSION.into();
    lock.bundle_digest = c.source.digest();
    lock.grok_excluded.extend(c.source.excluded.iter().cloned());
    lock.files_read = c.source.files_read.clone();
    let lt = json_sorted(&*lock);
    if fs::read_to_string(c.dest.join(LOCK)).ok().as_deref() != Some(lt.as_str()) {
        write_atomic(&c.dest.join(LOCK), lt.as_bytes(), Some(0o644))?;
    }
    if c.dest.join(LEGACY_LOCK).is_file() {
        fs::remove_file(c.dest.join(LEGACY_LOCK)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Split a dotted path back into segments, matching keys that themselves contain dots (e.g. model names).
fn split_dotted(dotted: &str, v: &Y) -> Vec<String> {
    let mut all = vec![];
    leaves(v, &mut vec![], &mut all);
    all.into_iter()
        .map(|(p, _)| p)
        .find(|p| p.join(".") == dotted)
        .unwrap_or_else(|| dotted.split('.').map(String::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_become_paused_jobs_only_when_hermes_accepts_them() {
        use crate::core::model::{Schedule, ScheduleKind};
        let mut m = BotModel::from_json(
            r#"{"schema":"bot-model/1","persona":{"name":"Nova","instructions":""}}"#,
        )
        .unwrap();
        let sch = |name: &str, cron: Option<&str>, tz: &str| Schedule {
            name: name.into(),
            cron: cron.map(String::from),
            tz: tz.into(),
            kind: ScheduleKind::Scheduled,
            action: "go".into(),
            enabled: true,
        };
        m.schedules = vec![
            sch("Cron", Some("0 7 * * 1-5"), "America/Chicago"),
            sch("Phrase", Some("every monday 9am"), ""),
            sch("Every", Some("every 15 min"), "UTC"),
            sch("Hourly", Some("hourly"), "UTC"),
            sch("Fridays", Some("fridays 16:00"), "UTC"),
            sch("Seconds", Some("0 0 9 * * *"), "UTC"),
            sch("Once", Some("in 30m"), "UTC"),
            sch("None", None, "UTC"),
        ];
        let mut manual = vec![];
        let jobs = schedules_to_cron(&m, &[], &mut manual);
        let got: Vec<(&str, &str)> = jobs
            .iter()
            .map(|j| (j.name.as_str(), j.schedule.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("Nova: Cron", "0 7 * * 1-5"),
                ("Nova: Phrase", "0 9 * * 1"),
                ("Nova: Every", "every 15m")
            ]
        );
        assert!(jobs.iter().all(|j| j.paused()));
        // a zone is named only when the routine carried one; otherwise the profile's zone is stated
        assert!(jobs[0]
            .paused_reason
            .as_ref()
            .unwrap()
            .contains("schedule is in America/Chicago (check the host timezone)"));
        assert!(jobs[1]
            .paused_reason
            .as_ref()
            .unwrap()
            .contains("the Hermes profile's timezone"));
        let m: BTreeMap<_, _> = manual.into_iter().collect();
        for (n, t) in [
            ("Hourly", "hourly"),
            ("Fridays", "fridays 16:00"),
            ("Seconds", "0 0 9 * * *"),
        ] {
            assert_eq!(
                m[&format!("schedule {n}")],
                format!("not imported: '{t}' is not a schedule Hermes accepts; set it by hand")
            );
        }
        assert!(m["schedule Once"].contains("runs once"));
        assert!(m["schedule None"].starts_with("no schedule"));
    }

    #[test]
    fn a_schedule_with_no_zone_says_it_runs_in_the_profiles_zone() {
        use crate::core::model::{Schedule, ScheduleKind};
        let mut m = BotModel::from_json(
            r#"{"schema":"bot-model/1","persona":{"name":"Nova","instructions":""}}"#,
        )
        .unwrap();
        m.schedules = ["", "UTC"]
            .iter()
            .map(|tz| Schedule {
                name: format!("z{tz}"),
                cron: Some("every friday 16:00".into()),
                tz: tz.to_string(),
                kind: ScheduleKind::Scheduled,
                action: "go".into(),
                enabled: true,
            })
            .collect();
        let jobs = schedules_to_cron(&m, &[], &mut vec![]);
        let d = std::env::temp_dir().join(format!("botport-tz-{}", std::process::id()));
        let src = Source {
            path: d.clone(),
            id: "t".into(),
            files_read: BTreeMap::new(),
            excluded: BTreeSet::new(),
        };
        let o = super::super::Opts {
            spec: Spec::default(),
            profile: "t".into(),
            real_home: false,
            hermes_bin: "hermes".into(),
            hermes_home: None,
        };
        let c = Ctx::new(&m, &src, &d, &o);
        let items = plan_model_cron(&c, &jobs, &[], &Lock::default());
        let details: Vec<&str> = items.iter().map(|i| i.detail.as_str()).collect();
        assert_eq!(
            details,
            [
                "not managed here (--no-cron or not ~/.hermes); runs in the Hermes profile's timezone",
                "not managed here (--no-cron or not ~/.hermes)"
            ]
        );
    }

    #[test]
    fn unsafe_skill_names_and_paths_are_neither_read_nor_written() {
        let d = std::env::temp_dir().join(format!("botport-skillpath-{}", std::process::id()));
        let m: BotModel = serde_json::from_value(json!({
            "schema": "botport.model/v1",
            "persona": {"name": "Bot", "instructions": ""},
            "skills": [
                {"name": "ok", "files": {"SKILL.md": "fine\n"}},
                {"name": "../../evil", "files": {"SKILL.md": "x\n"}},
                {"name": ".hidden", "files": {"SKILL.md": "x\n"}},
                {"name": "deep", "files": {"SKILL.md": "x\n", "../../../outside.md": "x\n"}},
                {"name": "abs", "files": {"/tmp/abs.md": "x\n"}}
            ]
        }))
        .unwrap();
        let src = Source {
            path: d.clone(),
            id: "t".into(),
            files_read: BTreeMap::new(),
            excluded: BTreeSet::new(),
        };
        let o = super::super::Opts {
            spec: Spec::default(),
            profile: "t".into(),
            real_home: false,
            hermes_bin: "hermes".into(),
            hermes_home: None,
        };
        let c = Ctx::new(&m, &src, &d, &o);
        let (_, units, manual) = want_skills(&c, "cat");
        assert_eq!(units.keys().collect::<Vec<_>>(), ["cat/ok"]);
        assert_eq!(manual.len(), 4, "{manual:?}");
        assert!(manual
            .iter()
            .all(|(_, why)| why.starts_with("not imported:")));
        assert!(live_unit(&d, "../x").is_none());
    }

    #[test]
    fn restore_point_refuses_a_live_secret_and_writes_nothing() {
        let d = std::env::temp_dir().join(format!("botport-rp-{}", std::process::id()));
        fs::create_dir_all(d.join("skills/cat/s")).unwrap();
        fs::write(d.join("config.yaml"), "model: x\n").unwrap();
        fs::write(
            d.join("skills/cat/s/SKILL.md"),
            "PROVIDER=sk-0123456789abcdef0123456789\n",
        )
        .unwrap();
        let e = restore_point(&d, "cat", 5).unwrap_err();
        assert!(
            e.contains("skills/cat/s/SKILL.md") && e.contains("no restore point"),
            "{e}"
        );
        assert!(!e.contains("0123456789abcdef"), "{e}");
        assert!(
            !e.contains("refusing to write") && !e.contains("checklist"),
            "{e}"
        );
        // not even an empty backups directory is created
        assert!(!d.join("backups").exists());
        // without the secret it works
        fs::write(d.join("skills/cat/s/SKILL.md"), "hello\n").unwrap();
        assert!(restore_point(&d, "cat", 5).unwrap().is_file());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn soul_sections_roundtrip_keeps_outside_text() {
        let live =
            "mine above\n<!-- botport:begin soul -->\nold\n<!-- botport:end soul -->\nmine below\n";
        let out = render_soul(Some(live), &[("soul".into(), "new\n".into())]);
        assert_eq!(
            out,
            "mine above\n<!-- botport:begin soul -->\nnew\n<!-- botport:end soul -->\nmine below\n"
        );
        assert_eq!(find_sections(&out).unwrap()[0].1, "new\n");
        let fresh = render_soul(None, &[("a".into(), "x".into())]);
        assert_eq!(
            fresh,
            "<!-- botport:begin a -->\nx\n<!-- botport:end a -->\n"
        );
    }

    #[test]
    fn merge_list_keeps_live_extras_in_place() {
        let s = |v: &[&str]| {
            v.iter()
                .map(|x| Y::String(x.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            merge_list(&s(&["a", "b", "c"]), &s(&["a", "wiki", "b", "c"])),
            s(&["a", "wiki", "b", "c"])
        );
        assert_eq!(
            merge_list(&s(&["a", "b"]), &s(&["x", "b"])),
            s(&["x", "a", "b"])
        );
        assert_eq!(merge_list(&s(&["a"]), &s(&[])), s(&["a"]));
    }

    #[test]
    fn leaves_and_paths_with_dotted_keys() {
        let v: Y = serde_yaml::from_str("a:\n  b: 1\n  'x.y': 2\nc: []\n").unwrap();
        let mut l = vec![];
        leaves(&v, &mut vec![], &mut l);
        assert_eq!(l.len(), 3);
        assert_eq!(
            split_dotted("a.x.y", &v),
            vec!["a".to_string(), "x.y".to_string()]
        );
        let mut w = Y::Null;
        set_path(&mut w, &["p".into(), "q".into()], Y::Bool(true));
        assert_eq!(
            get_path(&w, &["p".into(), "q".into()]),
            Some(&Y::Bool(true))
        );
    }

    #[test]
    fn parse_list_accepts_strings() {
        assert_eq!(
            parse_list(Some(&Y::String("['a', \"b\"]".into()))),
            vec!["a", "b"]
        );
    }

    /// SOUL.md text with sections `a` and `b`, and a lock that recorded `old` for both and the whole file.
    fn soul_fixture() -> (String, Lock) {
        let now = render_soul(
            None,
            &[("a".into(), "A\n".into()), ("b".into(), "B\n".into())],
        );
        let mut lock = Lock::default();
        for k in ["__file__", "a", "b"] {
            lock.soul.insert(k.into(), "old".into());
        }
        (now, lock)
    }

    fn conflict(name: &str, unrecorded: bool) -> Item {
        Item {
            unrecorded,
            ..item("soul", name, St::Conflict, "")
        }
    }

    #[test]
    fn soul_lock_no_conflict_records_file_and_every_section() {
        let (now, mut lock) = soul_fixture();
        let items = [
            item("soul", "a", St::Update, ""),
            item("soul", "b", St::InSync, ""),
        ];
        record_soul(&mut lock, &items, &now, false);
        assert_eq!(lock.soul["__file__"], sha256_hex(now.as_bytes()));
        assert_eq!(lock.soul["a"], sha256_hex(b"A\n"));
        assert_eq!(lock.soul["b"], sha256_hex(b"B\n"));
    }

    #[test]
    fn soul_lock_any_section_conflict_removes_file_and_keeps_that_sections_old_record() {
        let (now, mut lock) = soul_fixture();
        let items = [item("soul", "a", St::InSync, ""), conflict("b", false)];
        record_soul(&mut lock, &items, &now, false);
        assert!(!lock.soul.contains_key("__file__"));
        assert_eq!(lock.soul["a"], sha256_hex(b"A\n"));
        assert_eq!(
            lock.soul["b"], "old",
            "a conflicting section keeps its old record"
        );
    }

    #[test]
    fn soul_lock_whole_file_conflict_removes_file_and_records_no_section() {
        let (now, mut lock) = soul_fixture();
        record_soul(&mut lock, &[conflict("SOUL.md", true)], &now, false);
        assert!(!lock.soul.contains_key("__file__"));
        assert_eq!(
            (lock.soul["a"].as_str(), lock.soul["b"].as_str()),
            ("old", "old")
        );
    }

    #[test]
    fn soul_lock_adopted_unrecorded_conflict_counts_as_ours() {
        let (now, mut lock) = soul_fixture();
        record_soul(&mut lock, &[conflict("b", true)], &now, true);
        assert_eq!(lock.soul["__file__"], sha256_hex(now.as_bytes()));
        assert_eq!(lock.soul["b"], sha256_hex(b"B\n"));
    }

    #[test]
    fn soul_lock_adopt_never_takes_over_a_recorded_conflict() {
        let (now, mut lock) = soul_fixture();
        record_soul(&mut lock, &[conflict("b", false)], &now, true);
        assert!(!lock.soul.contains_key("__file__"));
        assert_eq!(lock.soul["b"], "old");
    }

    #[test]
    fn soul_lock_ignores_conflicts_in_other_areas() {
        let (now, mut lock) = soul_fixture();
        let items = [Item {
            area: "config".into(),
            ..conflict("a", false)
        }];
        record_soul(&mut lock, &items, &now, false);
        assert_eq!(lock.soul["__file__"], sha256_hex(now.as_bytes()));
        assert_eq!(lock.soul["a"], sha256_hex(b"A\n"));
    }

    #[test]
    fn soul_lock_without_markers_records_only_the_file() {
        let (_, mut lock) = soul_fixture();
        let now = "A hand-written SOUL.md with no botport markers\n";
        record_soul(&mut lock, &[], now, false);
        assert_eq!(lock.soul["__file__"], sha256_hex(now.as_bytes()));
        assert_eq!(
            (lock.soul["a"].as_str(), lock.soul["b"].as_str()),
            ("old", "old"),
            "no sections found, so no section record changes"
        );
    }

    #[test]
    fn user_entries_split() {
        assert_eq!(entries("a\n§\nb\n"), vec!["a", "b"]);
        assert!(entries("  ").is_empty());
    }
}
