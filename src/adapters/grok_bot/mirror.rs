//! Mirror mode's Grok side, run in the Grok box (docs/design/MIRROR.md sections 4.4 and 6.3): the bot's setup read
//! into mirror items (`botport mirror digest-items`), and change packs checked and staged for the bot to install
//! (`botport mirror apply-pack`). Both are programs, so the digests, coverage and sequence numbers in the state file
//! are computed, never typed by a model.
//!
//! The setup is read from an export the bot made of itself (a bundle directory or a grok-bot-export file, through the
//! same reader as `plan`). Coverage is complete for every kind the export format lists in full.
use super::GrokBot;
use crate::adapters::mirror::pack::{Head, Pack, PackAction};
use crate::adapters::mirror::state::{Entry, StateFile, SCHEMA};
use crate::core::adapter::Adapter;
use crate::core::item::Content;
use crate::core::mirror::{Applied, Coverage, ItemId, Kind, ABSENT};
use crate::core::model::BotModel;
use crate::core::package::{Limits, Package};
use crate::core::util::{
    err, json_sorted, jsonl_line, sha256_hex, write_atomic, write_private, Res,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// The applied log keeps at most this many entries (newest).
pub const LOG_KEEP: usize = 200;

/// The Grok side's items: id -> content; plus items that cannot be mirrored, with the reason.
#[derive(Debug, Clone, Default)]
pub struct GrokItems {
    /// id -> content
    pub content: BTreeMap<ItemId, Content>,
    /// raw id -> reason
    pub unsupported: BTreeMap<String, String>,
    /// Read from a source that may leave items out (a model-written export): every kind is reported with partial
    /// coverage, so an item missing from it is never taken as removed.
    pub partial: bool,
    /// Kinds the source cannot read at all, with the reason (the box files hold no routines, for example): the other
    /// side carries none of them and says so, never a silent gap.
    pub unread: BTreeMap<Kind, String>,
    /// Kinds the source reads but the box never writes, with the reason (another bot's name, for the Mirror bot): a
    /// change bound for the box goes on the owner's list to apply in the app instead of into a pack.
    pub by_hand: BTreeMap<Kind, String>,
}

impl GrokItems {
    /// id -> (side digest, neutral digest). The side digest is the sha256 of the item's blob.
    pub fn digests(&self) -> BTreeMap<ItemId, (String, String)> {
        self.content
            .iter()
            .map(|(id, c)| {
                (
                    id.clone(),
                    (sha256_hex(&c.to_blob(id)), c.neutral_digest(id)),
                )
            })
            .collect()
    }
}

fn add(out: &mut GrokItems, raw: String, c: Content) {
    match ItemId::parse(&raw) {
        // two things that come out as one item id: neither is carried (which one is meant is the owner's call)
        Ok(id) if out.content.contains_key(&id) || out.unsupported.contains_key(&raw) => {
            out.content.remove(&id);
            out.unsupported
                .insert(raw, "two of the bot's items have this id".into());
        }
        Ok(id) => {
            // nothing that looks like a secret leaves the box (a skill's files are checked as a package)
            if !matches!(c, Content::Package(_)) {
                if let Some(at) = crate::core::secrets::find_secret(id.as_str(), &c.to_blob(&id)) {
                    out.unsupported.insert(raw, at);
                    return;
                }
            }
            out.content.insert(id, c);
        }
        Err(e) => {
            out.unsupported.insert(raw, e);
        }
    }
}

/// Set item `raw` to `c`, replacing whatever was there (for a source laid over another, e.g. the app's cache over an
/// export). The same checks as every other item: a secret-looking text is not carried and is reported with its location.
pub fn put(out: &mut GrokItems, raw: &str, c: Content) {
    if let Ok(id) = ItemId::parse(raw) {
        out.content.remove(&id);
    }
    out.unsupported.remove(raw);
    add(out, raw.to_string(), c);
}

/// The items of a model read from an export.
pub fn items_of(m: &BotModel) -> GrokItems {
    let mut out = GrokItems::default();
    if !m.persona.name.trim().is_empty() {
        add(
            &mut out,
            "persona.name".into(),
            Content::Text(m.persona.name.clone()),
        );
    }
    if !m.persona.instructions.trim().is_empty() {
        add(
            &mut out,
            "persona.instructions".into(),
            Content::Text(m.persona.instructions.clone()),
        );
    }
    for s in &m.skills {
        let files: BTreeMap<String, (Vec<u8>, bool)> = s
            .bytes()
            .into_iter()
            .map(|(k, v)| match (k.as_str(), String::from_utf8(v.clone())) {
                ("SKILL.md", Ok(t)) => (k, (without_export_heading(&t).into_bytes(), false)),
                _ => (k, (v, false)),
            })
            .collect();
        match Package::from_files(&s.name, files) {
            Ok(p) => match p.secret_at() {
                Some(at) => {
                    out.unsupported.insert(
                        format!("skill:{}", s.name),
                        format!("looks like it holds a secret ({at})"),
                    );
                }
                None => add(&mut out, format!("skill:{}", s.name), Content::Package(p)),
            },
            Err(e) => {
                out.unsupported.insert(format!("skill:{}", s.name), e);
            }
        }
    }
    for s in &m.schedules {
        add(
            &mut out,
            format!("routine:{}", s.name),
            Content::Routine(s.clone()),
        );
    }
    for c in &m.connectors {
        let mut f = BTreeMap::new();
        f.insert("id".to_string(), json!(c.name));
        f.insert("kind".to_string(), json!(c.kind));
        if let Some(u) = &c.url {
            f.insert("url".to_string(), json!(u));
        }
        if !c.tools.is_empty() {
            f.insert("tools".to_string(), json!(c.tools));
        }
        add(
            &mut out,
            format!("connector:{}", c.name),
            Content::Connector(f),
        );
    }
    // memories and user facts are not part of the setup sync: counted under "not mirrored" so the owner sees it (the
    // exporter's placeholder line for an empty memory drop is not a fact)
    let facts = m
        .memory
        .iter()
        .filter(|f| f.text.trim() != super::bundle::NO_FACTS)
        .count();
    if facts > 0 {
        out.unsupported.insert(
            "memory".into(),
            format!(
                "{facts} memories and user facts stay on this side (not part of the setup sync)"
            ),
        );
    }
    // apps with no url: a public connector maps through the table; anything else is carried as is (private)
    let apps = m
        .extras
        .get(super::ID)
        .and_then(|x| x.get("apps_without_url"))
        .and_then(|v| v.as_array());
    for name in apps.into_iter().flatten().filter_map(|v| v.as_str()) {
        use crate::adapters::connectors::{from_grok, Mapping};
        let mut f = BTreeMap::new();
        let id = match from_grok(&json!({"name": name}), &Default::default()) {
            Mapping::Mapped { row } => {
                f.insert("id".to_string(), json!(row.id));
                f.insert("kind".to_string(), json!("mcp-http"));
                if let Some(u) = row.mcp_url() {
                    f.insert("url".to_string(), json!(u));
                }
                row.id.clone()
            }
            _ => {
                let id = super::export_json::slug(name);
                f.insert("id".to_string(), json!(id));
                f.insert("name".to_string(), json!(name));
                f.insert("kind".to_string(), json!("app"));
                id
            }
        };
        add(&mut out, format!("connector:{id}"), Content::Connector(f));
    }
    out
}

/// Read the bot's setup from an export.
pub fn read_items(export: &Path) -> Res<GrokItems> {
    Ok(items_of(&GrokBot.read(export)?))
}

/// Why the box files cannot give an item kind or the instructions.
pub const NOT_IN_BOX: &str =
    "not in the box files (read from the box, the mirror carries the name and the digest)";

/// Why a box read carries no skills unless the owner opted that pair in.
pub const SHARED_SKILLS: &str = "the box's skills are shared by every bot in the box, not one bot's own (not mirrored unless the owner opts this pair in)";

/// Why the Mirror bot never writes what it reads of another bot.
pub const BY_HAND: &str =
    "the Mirror bot never changes another bot's setup: apply it in the Grok app by hand";

/// Read a bot's setup from the box files, with no request to the bot: its name from
/// `<agent_data>/agents/<id>/profile.json`, a regular file inside `agent_data`, reached through no link. The name is
/// read, never written (the Mirror bot changes no other bot's setup): a change bound for it is listed for the owner to
/// apply by hand. The box keeps skills for all its bots together, so none of them is one bot's own: they are read
/// (from `<agent_data>/workflows/<name>/`, managed and plugin skills left out) only with `with_skills`, the owner's
/// per-pair opt-in, and otherwise reported as not read, like the routines and connectors, which the box files do not
/// hold. The instructions are held as not mirrored. Neither side takes any of them as removed, and the owner sees the
/// gap.
pub fn read_box(agent_data: &Path, agent_id: &str, with_skills: bool) -> Res<GrokItems> {
    use crate::core::util::{files_sorted, read_confined};
    if agent_id.len() != 36 || !agent_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return err(format!(
            "{agent_id:?} is not an agent id (36 characters, hex and dashes)"
        ));
    }
    let root = agent_data
        .canonicalize()
        .map_err(|e| format!("{}: {e}", agent_data.display()))?;
    let agent = root.join("agents").join(agent_id);
    let profile = read_confined(&root, &agent, "profile.json")
        .map_err(|e| format!("{}/profile.json: not read ({e:?})", agent.display()))?;
    let profile: serde_json::Value =
        serde_json::from_slice(&profile).map_err(|e| format!("profile.json: {e}"))?;
    let mut m = BotModel::from_json(
        r#"{"schema": "bot-model/1", "persona": {"name": "", "instructions": ""}}"#,
    )?;
    m.persona.name = profile["name"].as_str().unwrap_or("").to_string();
    if with_skills {
        let wf = root.join("workflows");
        let mut skills: BTreeMap<String, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
        for rel in files_sorted(&wf) {
            let r = rel.to_string_lossy().replace('\\', "/");
            if r.starts_with("managed-skills/") || r.starts_with("plugin-skills/") {
                continue;
            }
            let Some((n, f)) = r.split_once('/') else {
                continue;
            };
            let b = read_confined(&root, &wf, &r)
                .map_err(|e| format!("workflows/{r}: not read ({e:?})"))?;
            // the box keeps a skill's Markdown with literal "\n" (as the export reads it)
            let b = if f.ends_with(".md") {
                super::bundle::unescape(&String::from_utf8_lossy(&b)).into_bytes()
            } else {
                b
            };
            skills
                .entry(n.to_string())
                .or_default()
                .insert(f.to_string(), b);
        }
        for (n, files) in skills {
            if files.contains_key("SKILL.md") {
                m.skills
                    .push(crate::core::model::Skill::from_bytes(&n, &files));
            }
        }
    }
    let mut out = items_of(&m);
    out.unsupported
        .insert("persona.instructions".into(), NOT_IN_BOX.into());
    out.partial = true;
    out.by_hand.insert(Kind::Persona, BY_HAND.into());
    if !with_skills {
        out.unread.insert(Kind::Skill, SHARED_SKILLS.into());
    }
    for k in [Kind::Routine, Kind::Connector] {
        out.unread.insert(k, NOT_IN_BOX.into());
    }
    Ok(out)
}

/// A skill as the export rebuilds it has a `# <label>` heading above its instructions. When the instructions already
/// start with that same heading (a skill installed from the other side, whose own text begins with it), the added
/// heading is not part of the skill and is dropped, so a skill installed exactly as staged reads back as the same
/// copy. Nothing else in the text is touched.
pub fn without_export_heading(t: &str) -> String {
    let Some(rest) = t.strip_prefix("---\n") else {
        return t.to_string();
    };
    let Some(end) = rest.find("\n---\n") else {
        return t.to_string();
    };
    let (front, body) = rest.split_at(end + 5);
    let Some(after) = body.strip_prefix('\n') else {
        return t.to_string();
    };
    let Some((heading, more)) = after.split_once('\n') else {
        return t.to_string();
    };
    let Some(instructions) = more.strip_prefix('\n') else {
        return t.to_string();
    };
    let repeats = heading.starts_with("# ")
        && (instructions == heading || instructions.starts_with(&format!("{heading}\n")));
    if repeats {
        format!("---\n{front}\n{instructions}")
    } else {
        t.to_string()
    }
}

/// The applied log in a box file (JSON lines; unreadable lines skipped).
pub fn read_log(p: &Path) -> Vec<Applied> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The highest state file number in `out`, by file name, whether or not that file parses (a file written by an older
/// release, with another neutral form, still holds its number: the next one never goes below it).
fn highest_seq(out: &Path) -> Option<u64> {
    fs::read_dir(out)
        .ok()?
        .flatten()
        .filter_map(|e| crate::adapters::mirror::state::state_seq(&e.file_name().to_string_lossy()))
        .max()
}

/// The newest readable state file in `out`; with a pair key only one signed with it counts, so nothing (such as the
/// applied generation carried forward) is taken from a file the key does not verify.
fn newest_state(out: &Path, pair: &str, key: Option<&[u8]>) -> Option<StateFile> {
    // the highest-numbered file that verifies, whatever order the folder lists them in
    let mut all: Vec<(u64, std::path::PathBuf)> = fs::read_dir(out)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = crate::adapters::mirror::state::state_seq(&e.file_name().to_string_lossy())?;
            Some((n, e.path()))
        })
        .collect();
    all.sort_by_key(|y| std::cmp::Reverse(y.0));
    all.into_iter().find_map(|(_, p)| {
        fs::read(&p)
            .ok()
            .and_then(|b| StateFile::parse_with(&b, pair, key).ok())
    })
}

/// `digest-items`: write the next state file (and the blobs of items that changed since the previous one, or of all
/// items with `all`) into `out`. Returns the state file. The sequence number continues from the newest file in `out`.
pub fn digest_items(
    items: &GrokItems,
    out: &Path,
    pair: &str,
    log: &[Applied],
    written: &str,
    all: bool,
    key: Option<&[u8]>,
) -> Res<StateFile> {
    // the previous file counts only when it is the newest one and readable; otherwise (another release's file, or a
    // damaged one) every blob is written again
    let top = highest_seq(out);
    let prev = newest_state(out, pair, key).filter(|p| Some(p.n) == top);
    let all = all || (top.is_some() && prev.is_none());
    let n = top
        .map_or(Some(1), |t| t.checked_add(1))
        .filter(|n| *n <= crate::adapters::mirror::state::MAX_SEQ)
        .ok_or("state file number out of range")?;
    let mut entries = BTreeMap::new();
    for (id, c) in &items.content {
        let blob = c.to_blob(id);
        let sha = sha256_hex(&blob);
        let neutral = c.neutral_digest(id);
        let changed = all
            || prev
                .as_ref()
                .and_then(|p| p.items.get(id))
                .is_none_or(|e| e.neutral != neutral);
        if changed {
            write_private(out, &format!("blobs/{sha}"), &blob)?;
        }
        // an unchanged item keeps pointing at the blob an earlier file wrote (still in `out`), so its content stays
        // fetchable however many state files come between two syncs
        let kept = prev
            .as_ref()
            .and_then(|p| p.items.get(id))
            .filter(|e| !changed && e.digest == sha)
            .and_then(|e| e.blob.clone())
            .filter(|b| out.join("blobs").join(b).is_file());
        entries.insert(
            id.clone(),
            Entry {
                digest: sha.clone(),
                neutral,
                blob: if changed { Some(sha) } else { kept },
            },
        );
    }
    let applied = applied_tail(log);
    let mut f = StateFile {
        schema: SCHEMA.into(),
        pair: pair.into(),
        n,
        neutral_form: crate::core::package::NEUTRAL_FORM,
        written: written.into(),
        applied_gen: applied
            .iter()
            .map(|a| a.gen)
            .max()
            .or(prev.and_then(|p| p.applied_gen)),
        coverage: [Kind::Persona, Kind::Skill, Kind::Routine, Kind::Connector]
            .into_iter()
            .map(|k| {
                (
                    k,
                    if items.partial {
                        Coverage::Partial
                    } else {
                        Coverage::Complete
                    },
                )
            })
            .collect(),
        items: entries,
        unsupported: unsupported_ids(items),
        unread: items.unread.clone(),
        by_hand: items.by_hand.clone(),
        applied,
        sha256: String::new(),
        mac: None,
    };
    let bytes = f.seal_with(key);
    write_private(out, &format!("state-{n}.json"), &bytes)?;
    Ok(f)
}

/// The file beside the applied log that holds the newest pack generation applied in full (one number).
pub const FLOOR_FILE: &str = "applied-floor";

/// The applied floor, if the file holds one number.
pub fn read_floor(p: &Path) -> Option<u64> {
    // a regular file, never through a link
    if !fs::symlink_metadata(p).ok()?.is_file() {
        return None;
    }
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// Raise the applied floor to `gen` (0600).
pub fn write_floor(p: &Path, gen: u64) -> Res<()> {
    write_atomic(p, format!("{gen}\n").as_bytes(), Some(0o600))
}

/// The newest entries of the applied log, oldest first: the bounded tail a state file carries.
fn applied_tail(log: &[Applied]) -> Vec<Applied> {
    let mut applied: Vec<Applied> = log.to_vec();
    applied.sort_by_key(|a| a.gen);
    let skip = applied.len().saturating_sub(LOG_KEEP);
    applied.split_off(skip)
}

/// The unsupported entries that name an item (other notes, such as left-out memories, are not listed).
fn unsupported_ids(items: &GrokItems) -> BTreeMap<ItemId, String> {
    items
        .unsupported
        .iter()
        .filter_map(|(raw, why)| ItemId::parse(raw).ok().map(|id| (id, why.clone())))
        .collect()
}

/// The number of the newest state file in `out`, by file name, if any.
pub fn newest_seq(out: &Path) -> Option<u64> {
    highest_seq(out)
}

/// The newest state file's number when a new one would say nothing new: the same items (by both digests), the same
/// unsupported items, coverage and applied log, and signed with a key exactly when one is given now. `digest-items
/// --uploaded N` then writes nothing when that file is the one uploaded, so a routine that runs often uploads nothing
/// while no setup changes.
pub fn unchanged(
    items: &GrokItems,
    out: &Path,
    pair: &str,
    log: &[Applied],
    key: Option<&[u8]>,
) -> Option<u64> {
    let p = newest_state(out, pair, key).filter(|p| Some(p.n) == highest_seq(out))?;
    // the newest file must also verify with the key given now (a rotated key needs a new file)
    let b = fs::read(out.join(format!("state-{}.json", p.n))).ok()?;
    StateFile::parse_with(&b, pair, key).ok()?;
    let cov = if items.partial {
        Coverage::Partial
    } else {
        Coverage::Complete
    };
    let same_items = p.items.len() == items.content.len()
        && items.content.iter().all(|(id, c)| {
            p.items.get(id).is_some_and(|e| {
                e.digest == sha256_hex(&c.to_blob(id)) && e.neutral == c.neutral_digest(id)
            })
        });
    (same_items
        && p.unsupported == unsupported_ids(items)
        && p.coverage.len() == 4
        && p.coverage.values().all(|c| *c == cov)
        && p.applied == applied_tail(log)
        && p.mac.is_some() == key.is_some())
    .then_some(p.n)
}

/// What `apply-pack` found for one pack item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// already as the pack wants
    Done,
    /// staged for the bot to install, at this path (relative to the stage dir)
    Stage(String),
    /// a retire for the bot to carry out
    Remove,
    /// the live copy is neither what the pack expects nor what it wants: someone changed it; not touched
    Refused,
    /// an item of a pack not signed with the pair key: an unsigned pack is never applied; not touched
    Held,
}

/// The result of `apply-pack`: the pack, one step per item, and the applied-log entries added.
#[derive(Debug, Clone)]
pub struct PackRun {
    /// The pack `HEAD` names.
    pub pack: Pack,
    /// One step per pack item.
    pub steps: Vec<(ItemId, Step)>,
    /// Log entries added (items found already at `want`).
    pub added: Vec<Applied>,
}

/// Read the pack `HEAD` names in a local copy of the `to-grok` folder (`HEAD` beside it, as in the Drive layout).
pub fn head_pack(pair_dir: &Path, pair: &str, key: Option<&[u8]>) -> Res<Pack> {
    let h = Head::parse(&fs::read(pair_dir.join("HEAD")).map_err(|e| format!("HEAD: {e}"))?)?;
    let b =
        fs::read(pair_dir.join("to-grok").join(&h.pack)).map_err(|e| format!("{}: {e}", h.pack))?;
    let p = Pack::parse_with(&b, pair, key)?;
    if p.sha256 != h.sha || p.gen != h.gen {
        return err("HEAD does not match the pack it names");
    }
    // a signed pack is checked or refused, never taken on trust
    if key.is_none() && p.mac.is_some() {
        return err("the pack is signed with the pair key: pass --key FILE");
    }
    Ok(p)
}

fn stage_path(id: &ItemId) -> String {
    match id.kind() {
        Kind::Skill => format!("skills/{}", id.name()),
        Kind::Persona => format!("{}.txt", id.as_str()),
        Kind::Routine => format!("routines/{}.json", id.name()),
        Kind::Connector => format!("connectors/{}.json", id.name()),
    }
}

/// `apply-pack`: check each item of the pack `HEAD` names against the live items and stage what may be installed.
/// Items already at `want` are logged as applied (before = what the pack expected), so the log shows a pack that only
/// superseded a stale one as done. Returns one step per pack item and the log entries added.
pub fn apply_pack(
    pair_dir: &Path,
    pair: &str,
    live: &GrokItems,
    stage: &Path,
    log: &[Applied],
    key: Option<&[u8]>,
    floor: Option<u64>,
) -> Res<PackRun> {
    let p = head_pack(pair_dir, pair, key)?;
    // below the persisted floor (the newest generation applied in full): an older pack put back, refused. A pack at
    // the floor is not refused: it is that same pack read again, so the caller reports it done and stages nothing
    // (refusing it would turn every run after a full apply into an error until botport publishes again)
    if let Some(f) = floor {
        if p.gen < f {
            return err(format!(
                "the pack HEAD names (gen {}) is older than gen {f}, applied in full already: refused (report this to \
                 the owner; only if the owner says they set this pair up again from an empty folder, move this pair's \
                 applied log and {FLOOR_FILE} aside)",
                p.gen
            ));
        }
    }
    // an older signed pack put back in place of a newer one already applied would roll the bot back: refused (the same
    // generation again is fine: a later run finishes a pack)
    if let Some(top) = log.iter().map(|a| a.gen).max() {
        if p.gen < top {
            return err(format!(
                "the pack HEAD names (gen {}) is older than one already applied (gen {top}): refused (report this to the owner; \
                 only if the owner says they set this pair up again from an empty folder, move this pair's applied log \
                 and {FLOOR_FILE} aside)",
                p.gen
            ));
        }
    }
    let now = live.digests();
    let mut steps = vec![];
    let mut added = vec![];
    for it in &p.items {
        let cur = now
            .get(&it.item)
            .map_or(ABSENT.to_string(), |(_, n)| n.clone());
        // an item the box holds back (a secret, two of one id) is there but unknown: never staged over or retired
        let step = if live.unsupported.contains_key(it.item.as_str())
            || live.unread.contains_key(&it.item.kind())
            || live.by_hand.contains_key(&it.item.kind())
        {
            Step::Refused
        } else if key.is_none() {
            // an unsigned pack is never applied, logged or counted, whatever the live item is: held
            Step::Held
        } else if cur == it.want {
            if !log
                .iter()
                .chain(added.iter())
                .any(|a: &Applied| a.gen == p.gen && a.item == it.item)
            {
                added.push(Applied {
                    gen: p.gen,
                    item: it.item.clone(),
                    before: it.expect.clone(),
                    after: it.want.clone(),
                });
            }
            Step::Done
        } else if cur != it.expect {
            Step::Refused
        } else if it.action == PackAction::Retire {
            Step::Remove
        } else {
            let sha = it.blob.as_deref().ok_or("pack item without a blob")?;
            let b = fs::read(pair_dir.join("to-grok/blobs").join(sha))
                .map_err(|_| format!("{}: blob missing", it.item))?;
            if sha256_hex(&b) != sha {
                return err(format!("{}: blob does not match its name", it.item));
            }
            let c = Content::from_blob(&it.item, &b)?;
            if c.neutral_digest(&it.item) != it.want {
                return err(format!("{}: blob content does not match the pack", it.item));
            }
            let rel = stage_path(&it.item);
            match &c {
                Content::Package(pk) => {
                    let dir = stage.join("skills");
                    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                    pk.write(&dir, false)?;
                }
                Content::Text(t) => write_atomic(&stage.join(&rel), t.as_bytes(), Some(0o600))?,
                Content::Routine(s) => {
                    write_atomic(&stage.join(&rel), json_sorted(s).as_bytes(), Some(0o600))?
                }
                Content::Connector(m) => {
                    write_atomic(&stage.join(&rel), json_sorted(m).as_bytes(), Some(0o600))?
                }
            }
            Step::Stage(rel)
        };
        steps.push((it.item.clone(), step));
    }
    Ok(PackRun {
        pack: p,
        steps,
        added,
    })
}

/// Append entries to the box's applied log, keeping the newest [`LOG_KEEP`].
pub fn append_log(p: &Path, added: &[Applied]) -> Res<()> {
    if added.is_empty() {
        return Ok(());
    }
    let mut all = read_log(p);
    all.extend(added.iter().cloned());
    let skip = all.len().saturating_sub(LOG_KEEP);
    let text: String = all[skip..]
        .iter()
        .map(|a| jsonl_line(&serde_json::to_value(a).expect("serializable")))
        .collect();
    write_atomic(p, text.as_bytes(), Some(0o600))
}

/// `apply-pack --precheck ITEM`: right before the bot installs one item, the pack must still be the one `HEAD` names
/// (`gen`) and the live copy must still have the digest the pack expects.
pub fn precheck(
    pair_dir: &Path,
    pair: &str,
    gen: u64,
    live: &GrokItems,
    item: &ItemId,
    key: Option<&[u8]>,
    stage: Option<&Path>,
) -> Res<Option<Package>> {
    let p = head_pack(pair_dir, pair, key)?;
    if p.gen != gen {
        return err(format!(
            "the pack was replaced (now gen {}); run apply-pack again",
            p.gen
        ));
    }
    let it = p
        .items
        .iter()
        .find(|i| i.item == *item)
        .ok_or_else(|| format!("{item} is not in the pack"))?;
    if live.unsupported.contains_key(item.as_str()) {
        return err(format!(
            "{item} is held back in the box (it cannot be read as it is); do not install it"
        ));
    }
    if let Some(why) = live
        .unread
        .get(&item.kind())
        .or(live.by_hand.get(&item.kind()))
    {
        return err(format!("{item}: {why}; do not install it"));
    }
    let cur = live
        .digests()
        .get(item)
        .map_or(ABSENT.to_string(), |(_, n)| n.clone());
    if cur != it.expect {
        return err(format!(
            "{item} changed since the pack was made; do not install it"
        ));
    }
    if key.is_none() {
        return err(format!(
            "{item}: the pack is not signed with the pair key; do not install it"
        ));
    }
    // what the bot installs from the stage must still be exactly what the pack wants
    if item.kind() != Kind::Skill && it.action == PackAction::Put {
        let stage =
            stage.ok_or("a precheck needs --stage DIR (where apply-pack staged the item)")?;
        let sha = it.blob.as_deref().ok_or("pack item without a blob")?;
        let b = fs::read(pair_dir.join("to-grok/blobs").join(sha))
            .map_err(|_| format!("{item}: blob missing"))?;
        if sha256_hex(&b) != sha {
            return err(format!("{item}: blob does not match its name"));
        }
        let c = Content::from_blob(item, &b)?;
        if c.neutral_digest(item) != it.want {
            return err(format!("{item}: blob content does not match the pack"));
        }
        let want: Vec<u8> = match c {
            Content::Text(t) => t.into_bytes(),
            Content::Routine(s) => json_sorted(&s).into_bytes(),
            Content::Connector(m) => json_sorted(&m).into_bytes(),
            Content::Package(_) => vec![],
        };
        let staged = fs::read(stage.join(stage_path(item))).unwrap_or_default();
        if staged != want {
            return err(format!(
                "{item}: the staged copy changed since it was staged; run apply-pack again"
            ));
        }
    }
    if item.kind() == Kind::Skill && it.action == PackAction::Put {
        let stage =
            stage.ok_or("a skill's precheck needs --stage DIR (where apply-pack staged it)")?;
        let staged = Package::read(
            &stage.join("skills").join(item.name()),
            item.name(),
            &Limits::default(),
        )
        .map_err(|e| {
            format!("{item}: the staged copy cannot be read ({e}); run apply-pack again")
        })?;
        if Content::Package(staged.clone()).neutral_digest(item) != it.want {
            return err(format!(
                "{item}: the staged copy changed since it was staged; run apply-pack again"
            ));
        }
        // the verified copy, held in memory: installing it writes exactly these bytes
        return Ok(Some(staged));
    }
    Ok(None)
}

/// Read a package from a directory with its executable bits (for a skill folder the bot keeps on disk).
pub fn package_from_dir(dir: &Path, name: &str) -> Res<Package> {
    Package::read(dir, name, &Limits::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::mirror::pack::{random_id, PackItem};
    use crate::core::model::{Persona, Schedule, ScheduleKind, Skill};
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        crate::core::util::test_dir(&format!("gmirror-{tag}"))
    }

    fn model(prompt: &str) -> BotModel {
        let mut m = BotModel::from_json(
            r#"{"schema": "bot-model/1", "persona": {"name": "", "instructions": ""}}"#,
        )
        .unwrap();
        m.persona = Persona {
            name: "Demo Bot".into(),
            instructions: "Example instructions.\n".into(),
        };
        m.skills.push(Skill::from_bytes(
            "demo-skill",
            &BTreeMap::from([("SKILL.md".to_string(), b"# Demo skill\n".to_vec())]),
        ));
        m.schedules.push(Schedule {
            name: "demo-routine".into(),
            cron: Some("0 9 * * 1".into()),
            tz: "UTC".into(),
            kind: ScheduleKind::Scheduled,
            action: prompt.into(),
            enabled: true,
        });
        m
    }

    #[test]
    fn a_box_read_takes_the_name_and_names_what_it_cannot_read() {
        let d = tmp("box-read");
        let id = "0a1b2c3d-0000-4000-8000-00000000abcd";
        let w = |rel: &str, t: &str| {
            let p = d.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, t).unwrap();
        };
        w(
            &format!("agents/{id}/profile.json"),
            r#"{"name": "Demo Bot"}"#,
        );
        w("workflows/demo-skill/SKILL.md", "# Demo skill\n");
        let r = read_box(&d, id, false).unwrap();
        // the box's skills belong to every bot in it: none is read as this bot's own
        let ids: Vec<&str> = r.content.keys().map(|k| k.as_str()).collect();
        assert_eq!(ids, ["persona.name"]);
        assert!(r.partial);
        assert_eq!(r.unsupported["persona.instructions"], NOT_IN_BOX);
        assert_eq!(r.unread[&Kind::Skill], SHARED_SKILLS);
        // the name is read, never written: a change bound for it is the owner's to apply
        assert_eq!(r.by_hand[&Kind::Persona], BY_HAND);
        // with the owner's opt-in for this pair, the box's own skills are read (managed ones are not the bot's)
        w(
            "workflows/managed-skills/other/SKILL.md",
            "# Not the bot's own\n",
        );
        let with = read_box(&d, id, true).unwrap();
        let ids: Vec<&str> = with.content.keys().map(|k| k.as_str()).collect();
        assert_eq!(ids, ["persona.name", "skill:demo-skill"]);
        assert!(!with.unread.contains_key(&Kind::Skill));
        assert_eq!(
            r.unread.keys().copied().collect::<Vec<_>>(),
            [Kind::Skill, Kind::Routine, Kind::Connector]
        );
        // the state file carries what was not read
        let out = d.join("out");
        let f = digest_items(
            &r,
            &out,
            "demo-bot",
            &[],
            "2000-01-01T00:00:00Z",
            false,
            None,
        )
        .unwrap();
        assert_eq!(f.unread.len(), 3);
        assert_eq!(f.to_read().unread, f.unread);
        // a pack item of a kind the box cannot read (a send from before the pair read the box) is never staged
        let rt = ItemId::parse("routine:demo-routine").unwrap();
        let wanted = items_of(&model("Example prompt.")).content[&rt].clone();
        let pd = d.join("pair");
        publish(&pd, 2, &[(&rt, &wanted, ABSENT)]);
        let run = apply_pack(&pd, "demo-bot", &r, &d.join("stage"), &[], Some(&KEY), None).unwrap();
        assert_eq!(run.steps[0], (rt.clone(), Step::Refused));
        assert!(precheck(&pd, "demo-bot", 2, &r, &rt, Some(&KEY), None)
            .unwrap_err()
            .contains("not in the box files"));
        // an id that is not one is refused; a profile that is a link is never read
        assert!(read_box(&d, "../agents", false).is_err());
        #[cfg(unix)]
        {
            let other = "0a1b2c3d-0000-4000-8000-00000000abce";
            fs::create_dir_all(d.join(format!("agents/{other}"))).unwrap();
            std::os::unix::fs::symlink(
                d.join(format!("agents/{id}/profile.json")),
                d.join(format!("agents/{other}/profile.json")),
            )
            .unwrap();
            assert!(read_box(&d, other, false).is_err());
        }
    }

    #[test]
    fn apps_without_a_url_map_through_the_public_table_or_are_carried_as_is() {
        let mut m = model("Example prompt.");
        m.extras.insert(
            super::super::ID.into(),
            json!({"apps_without_url": ["GitHub", "Example Internal Tool"]}),
        );
        let items = items_of(&m);
        let Content::Connector(gh) = &items.content[&ItemId::parse("connector:github").unwrap()]
        else {
            panic!()
        };
        assert_eq!(gh["url"], "https://api.githubcopilot.com/mcp/");
        let opaque = items
            .content
            .iter()
            .find(|(k, _)| k.as_str().starts_with("connector:example"))
            .unwrap();
        let Content::Connector(o) = opaque.1 else {
            panic!()
        };
        assert_eq!((o["kind"].as_str(), o.get("url")), (Some("app"), None));
    }

    #[test]
    fn two_items_with_one_id_are_neither_carried() {
        let mut m = model("Example prompt.");
        m.schedules.push(m.schedules[0].clone());
        let items = items_of(&m);
        assert!(!items
            .content
            .keys()
            .any(|k| k.as_str() == "routine:demo-routine"));
        assert!(items.unsupported["routine:demo-routine"].contains("two"));
    }

    #[test]
    fn a_pack_never_touches_an_item_the_box_holds_back() {
        let d = tmp("held-back");
        let stage = d.join("stage");
        let mut live = items_of(&model("Example prompt."));
        let r = ItemId::parse("routine:demo-routine").unwrap();
        let wanted = live.content[&r].clone();
        // the box holds it back (as if its text tripped the secret checks): it is there, but unknown
        live.content.remove(&r);
        live.unsupported
            .insert(r.as_str().into(), "looks like it holds a secret".into());
        publish(&d, 2, &[(&r, &wanted, ABSENT)]);
        let run = apply_pack(&d, "demo-bot", &live, &stage, &[], Some(&KEY), None).unwrap();
        assert_eq!(run.steps[0], (r.clone(), Step::Refused));
        assert!(precheck(&d, "demo-bot", 2, &live, &r, Some(&KEY), None)
            .unwrap_err()
            .contains("held back"));
    }

    #[test]
    fn text_that_looks_like_a_secret_stays_in_the_box() {
        let mut m = model("Example prompt.");
        m.persona.instructions =
            "Use api_key=sk-ABCDEFGHIJKLMNOPQRSTUVWX123 for the example service.\n".into();
        let items = items_of(&m);
        let ins = ItemId::parse("persona.instructions").unwrap();
        assert!(!items.content.contains_key(&ins));
        assert!(items.unsupported["persona.instructions"].contains("secret"));
    }

    #[test]
    fn items_it_cannot_offer_are_listed_in_the_state_file() {
        let mut m = model("Example prompt.");
        m.schedules.push(m.schedules[0].clone());
        m.memory.push(crate::core::model::Fact {
            text: "Example fact.".into(),
            tier: None,
            scope: None,
            source: None,
            date: None,
            origin_side: None,
        });
        let items = items_of(&m);
        assert!(items.unsupported["memory"].starts_with("1 memories"));
        let f = digest_items(
            &items,
            &tmp("unoffered"),
            "demo-bot",
            &[],
            "t1",
            false,
            None,
        )
        .unwrap();
        let rt = ItemId::parse("routine:demo-routine").unwrap();
        assert!(f.unsupported[&rt].contains("two"));
        assert_eq!(f.unsupported.len(), 1, "only item ids are listed");
    }

    #[test]
    fn a_skill_installed_from_hermes_reads_back_as_the_same_copy() {
        use crate::core::package::skill_md_neutral;
        let hermes = "---\nname: demo-skill\ndescription: Example skill\n---\n# Demo skill\n\nExample steps.  \nNext line.\n";
        // the bot installs it as name, description and instructions; its export rebuilds SKILL.md around them
        let exported = super::super::export_json::skill_markdown(&json!({
            "name": "Demo skill",
            "description": "Example skill",
            "instructions": "# Demo skill\n\nExample steps.  \nNext line."
        }));
        assert_ne!(skill_md_neutral(hermes), skill_md_neutral(&exported));
        assert_eq!(
            skill_md_neutral(hermes),
            skill_md_neutral(&without_export_heading(&exported))
        );
        // a skill whose text does not start with that heading keeps the export's heading, and other text is untouched
        let own = super::super::export_json::skill_markdown(&json!({
            "name": "Demo skill", "description": "Example skill", "instructions": "Example steps."
        }));
        assert_eq!(without_export_heading(&own), own);
        let twice = "---\nname: x\n---\n\nBody\n\n# A\n# A\n";
        assert_eq!(without_export_heading(twice), twice);
    }

    #[test]
    fn numbering_continues_past_a_file_this_release_cannot_read() {
        let d = tmp("past-old");
        let items = items_of(&model("Example prompt."));
        digest_items(&items, &d, "demo-bot", &[], "t1", false, None).unwrap();
        // a newer file from another release (or a damaged one) holds its number
        write_private(
            &d,
            "state-7.json",
            b"{\"schema\": \"botport-mirror-state/1\"}",
        )
        .unwrap();
        for e in fs::read_dir(d.join("blobs")).unwrap() {
            fs::remove_file(e.unwrap().path()).unwrap();
        }
        assert_eq!(newest_seq(&d), Some(7));
        assert_eq!(unchanged(&items, &d, "demo-bot", &[], None), None);
        let f = digest_items(&items, &d, "demo-bot", &[], "t2", false, None).unwrap();
        assert_eq!(f.n, 8);
        // every item's content is written again, since the previous file could not be used
        assert!(f.items.values().all(|e| e
            .blob
            .as_ref()
            .is_some_and(|b| d.join("blobs").join(b).is_file())));
    }

    #[test]
    fn unchanged_needs_the_same_read_the_same_log_and_the_current_key() {
        let d = tmp("unchanged");
        let items = items_of(&model("Example prompt."));
        let (k1, k2) = (b"example-key-one".as_slice(), b"example-key-two".as_slice());
        assert_eq!(
            unchanged(&items, &d, "demo-bot", &[], Some(k1)),
            None,
            "no file yet"
        );
        let f = digest_items(&items, &d, "demo-bot", &[], "t1", false, Some(k1)).unwrap();
        assert_eq!(unchanged(&items, &d, "demo-bot", &[], Some(k1)), Some(f.n));
        // a rotated key, or no key at all: the newest file would not verify as the host now checks it
        assert_eq!(unchanged(&items, &d, "demo-bot", &[], Some(k2)), None);
        assert_eq!(unchanged(&items, &d, "demo-bot", &[], None), None);
        // another pair's files never count
        assert_eq!(unchanged(&items, &d, "other-bot", &[], Some(k1)), None);
        // a changed item, a new applied-log entry, or another coverage: a new file is needed
        let changed = items_of(&model("Changed prompt."));
        assert_eq!(unchanged(&changed, &d, "demo-bot", &[], Some(k1)), None);
        let log = [Applied {
            gen: 1,
            item: ItemId::parse("routine:demo-routine").unwrap(),
            before: ABSENT.into(),
            after: "x".into(),
        }];
        assert_eq!(unchanged(&items, &d, "demo-bot", &log, Some(k1)), None);
        let mut partial = items_of(&model("Example prompt."));
        partial.partial = !items.partial;
        assert_eq!(unchanged(&partial, &d, "demo-bot", &[], Some(k1)), None);
    }

    #[test]
    fn digest_items_writes_sequenced_state_with_changed_blobs_only() {
        let d = tmp("digest");
        let items = items_of(&model("Example prompt."));
        assert_eq!(items.content.len(), 4);
        let f1 = digest_items(&items, &d, "demo-bot", &[], "t1", false, None).unwrap();
        assert_eq!(f1.n, 1);
        assert!(
            f1.items.values().all(|e| e.blob.is_some()),
            "first file carries every item"
        );
        // nothing changed: no new blob is written, and every item still points at the blob it has
        let blobs = |d: &Path| fs::read_dir(d.join("blobs")).unwrap().count();
        let before = blobs(&d);
        let f2 = digest_items(&items, &d, "demo-bot", &[], "t2", false, None).unwrap();
        assert_eq!(f2.n, 2);
        assert_eq!(blobs(&d), before);
        assert!(
            f2.items.iter().all(|(k, e)| e.blob == f1.items[k].blob),
            "content stays fetchable"
        );
        // one routine changed: only its blob
        let f3 = digest_items(
            &items_of(&model("Changed prompt.")),
            &d,
            "demo-bot",
            &[],
            "t3",
            false,
            None,
        )
        .unwrap();
        let moved: Vec<&str> = f3
            .items
            .iter()
            .filter(|(k, e)| e.blob != f2.items[*k].blob)
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(
            moved,
            ["routine:demo-routine"],
            "only the changed item has a new blob"
        );
        assert_eq!(blobs(&d), before + 1);
        // what it wrote reads back through the shared state reader
        let b = fs::read(d.join("state-3.json")).unwrap();
        assert_eq!(StateFile::parse(&b, "demo-bot").unwrap(), f3);
        fs::remove_dir_all(&d).ok();
    }

    /// a pair folder with HEAD and one pack carrying `items` (content, expect)
    const KEY: [u8; 32] = [7; 32];

    fn publish(d: &Path, gen: u64, items: &[(&ItemId, &Content, &str)]) -> Pack {
        let mut pis = vec![];
        for (id, c, expect) in items {
            let b = c.to_blob(id);
            let sha = sha256_hex(&b);
            write_private(d, &format!("to-grok/blobs/{sha}"), &b).unwrap();
            pis.push(PackItem {
                item: (*id).clone(),
                action: PackAction::Put,
                expect: expect.to_string(),
                want: c.neutral_digest(id),
                blob: Some(sha),
            });
        }
        let mut p = Pack::new("demo-bot", gen, &random_id(), "a1", "t", pis).unwrap();
        let bytes = p.seal_with(Some(&KEY));
        write_private(d, &format!("to-grok/{}", p.file_name()), &bytes).unwrap();
        write_private(d, "HEAD", p.head().to_line().as_bytes()).unwrap();
        p
    }

    #[test]
    fn a_staged_skill_is_checked_again_right_before_it_is_installed() {
        let d = tmp("precheck-skill");
        let stage = d.join("stage");
        let live = items_of(&model("Example prompt."));
        let s = ItemId::parse("skill:new-skill").unwrap();
        let pkg = Content::Package(
            Package::from_files(
                "new-skill",
                BTreeMap::from([(
                    "SKILL.md".to_string(),
                    (
                        b"---\nname: new-skill\n---\nExample steps.\n".to_vec(),
                        false,
                    ),
                )]),
            )
            .unwrap(),
        );
        publish(&d, 3, &[(&s, &pkg, ABSENT)]);
        apply_pack(&d, "demo-bot", &live, &stage, &[], Some(&KEY), None).unwrap();
        precheck(&d, "demo-bot", 3, &live, &s, Some(&KEY), Some(&stage)).unwrap();
        assert!(precheck(&d, "demo-bot", 3, &live, &s, Some(&KEY), None)
            .unwrap_err()
            .contains("--stage"));
        // someone edits the staged copy before the bot copies it
        fs::write(
            stage.join("skills/new-skill/SKILL.md"),
            "---\nname: new-skill\n---\nOther steps.\n",
        )
        .unwrap();
        assert!(
            precheck(&d, "demo-bot", 3, &live, &s, Some(&KEY), Some(&stage))
                .unwrap_err()
                .contains("staged copy")
        );
    }

    #[test]
    fn an_unsigned_pack_is_never_applied() {
        let d = tmp("unsigned");
        let stage = d.join("stage");
        let live = items_of(&model("Example prompt."));
        let r = ItemId::parse("routine:demo-routine").unwrap();
        let new_r = ItemId::parse("routine:new-routine").unwrap();
        let changed = items_of(&model("New prompt."));
        let Content::Routine(mut sched) = changed.content[&r].clone() else {
            panic!()
        };
        sched.name = "new-routine".into();
        let added = Content::Routine(sched);
        let mut pis = vec![];
        for (id, c, expect) in [
            (
                &r,
                &changed.content[&r],
                live.content[&r].neutral_digest(&r),
            ),
            (&new_r, &added, ABSENT.to_string()),
        ] {
            let b = c.to_blob(id);
            let sha = sha256_hex(&b);
            write_private(&d, &format!("to-grok/blobs/{sha}"), &b).unwrap();
            pis.push(PackItem {
                item: id.clone(),
                action: PackAction::Put,
                expect,
                want: c.neutral_digest(id),
                blob: Some(sha),
            });
        }
        let mut p = Pack::new("demo-bot", 3, &random_id(), "a1", "t", pis).unwrap();
        let bytes = p.seal();
        write_private(&d, &format!("to-grok/{}", p.file_name()), &bytes).unwrap();
        write_private(&d, "HEAD", p.head().to_line().as_bytes()).unwrap();
        let run = apply_pack(&d, "demo-bot", &live, &stage, &[], None, None).unwrap();
        assert_eq!(run.steps[0], (r.clone(), Step::Held));
        assert_eq!(run.steps[1], (new_r.clone(), Step::Held));
        assert!(!stage.exists(), "nothing staged");
        for id in [&r, &new_r] {
            assert!(precheck(&d, "demo-bot", 3, &live, id, None, None)
                .unwrap_err()
                .contains("not signed"));
        }
    }

    #[test]
    fn an_unsigned_pack_that_already_matches_is_held_and_logs_nothing() {
        let d = tmp("unsigned-match");
        let stage = d.join("stage");
        let live = items_of(&model("Example prompt."));
        let r = ItemId::parse("routine:demo-routine").unwrap();
        let c = &live.content[&r];
        let b = c.to_blob(&r);
        let sha = sha256_hex(&b);
        write_private(&d, &format!("to-grok/blobs/{sha}"), &b).unwrap();
        let now = c.neutral_digest(&r);
        let pi = PackItem {
            item: r.clone(),
            action: PackAction::Put,
            expect: now.clone(),
            want: now,
            blob: Some(sha),
        };
        // a high generation from anyone who can write the folder: never logged, so it can never raise the floor
        let mut p = Pack::new("demo-bot", 9, &random_id(), "a1", "t", vec![pi]).unwrap();
        let bytes = p.seal();
        write_private(&d, &format!("to-grok/{}", p.file_name()), &bytes).unwrap();
        write_private(&d, "HEAD", p.head().to_line().as_bytes()).unwrap();
        let run = apply_pack(&d, "demo-bot", &live, &stage, &[], None, None).unwrap();
        assert_eq!(run.steps, vec![(r, Step::Held)]);
        assert!(run.added.is_empty(), "nothing logged without the pair key");
    }

    #[test]
    fn an_export_with_no_memories_reports_none() {
        // a setup-only export: the exporter's MEMORY.md says "(no facts)", which is not a memory
        let mut m = model("Example prompt.");
        m.memory =
            vec![serde_json::from_value(json!({"text": super::super::bundle::NO_FACTS})).unwrap()];
        assert!(!items_of(&m).unsupported.contains_key("memory"));
        m.memory
            .push(serde_json::from_value(json!({"text": "Example fact."})).unwrap());
        assert_eq!(
            items_of(&m).unsupported["memory"],
            "1 memories and user facts stay on this side (not part of the setup sync)"
        );
    }

    #[test]
    fn apply_pack_stages_refuses_and_logs() {
        let d = tmp("apply");
        let stage = d.join("stage");
        let live = items_of(&model("Example prompt."));
        let r = ItemId::parse("routine:demo-routine").unwrap();
        let ins = ItemId::parse("persona.instructions").unwrap();
        let cur_r = live.content[&r].neutral_digest(&r);
        let mut m2 = model("New prompt.");
        m2.persona.instructions = "New instructions.\n".into();
        let new = items_of(&m2);
        let p = publish(
            &d,
            5,
            &[
                (&r, &new.content[&r], &cur_r),
                (&ins, &new.content[&ins], "someone-else"),
            ],
        );
        let PackRun {
            pack: pk,
            steps,
            added,
        } = apply_pack(&d, "demo-bot", &live, &stage, &[], Some(&KEY), None).unwrap();
        assert_eq!(pk.gen, 5);
        assert_eq!(
            steps[0],
            (r.clone(), Step::Stage("routines/demo-routine.json".into()))
        );
        assert_eq!(
            steps[1],
            (ins.clone(), Step::Refused),
            "the live copy is not what the pack expects"
        );
        assert!(stage.join("routines/demo-routine.json").is_file());
        assert!(added.is_empty());
        // a pack older than one already applied is refused
        let newer = [Applied {
            gen: 6,
            item: r.clone(),
            before: ABSENT.into(),
            after: "x".into(),
        }];
        assert!(
            apply_pack(&d, "demo-bot", &live, &stage, &newer, Some(&KEY), None)
                .unwrap_err()
                .contains("older than one already applied")
        );
        // precheck right before installing, then the bot installs and runs apply-pack again
        precheck(&d, "demo-bot", 5, &live, &r, Some(&KEY), Some(&stage)).unwrap();
        assert!(precheck(&d, "demo-bot", 5, &live, &r, Some(&KEY), None)
            .unwrap_err()
            .contains("--stage"));
        // an edit to the staged routine before the bot installs it is caught
        let staged = stage.join("routines/demo-routine.json");
        let keep = fs::read(&staged).unwrap();
        fs::write(&staged, b"{}").unwrap();
        assert!(
            precheck(&d, "demo-bot", 5, &live, &r, Some(&KEY), Some(&stage))
                .unwrap_err()
                .contains("staged copy")
        );
        fs::write(&staged, keep).unwrap();
        assert!(precheck(&d, "demo-bot", 4, &live, &r, Some(&KEY), None)
            .unwrap_err()
            .contains("replaced"));
        let after = items_of(&model("New prompt."));
        assert!(precheck(&d, "demo-bot", 5, &after, &r, Some(&KEY), None)
            .unwrap_err()
            .contains("changed"));
        let PackRun { steps, added, .. } =
            apply_pack(&d, "demo-bot", &after, &stage, &[], Some(&KEY), None).unwrap();
        assert_eq!(steps[0].1, Step::Done);
        assert_eq!(
            added,
            vec![Applied {
                gen: 5,
                item: r.clone(),
                before: cur_r.clone(),
                after: new.content[&r].neutral_digest(&r)
            }]
        );
        // logged once, even when run again
        let logp = d.join("applied.jsonl");
        append_log(&logp, &added).unwrap();
        let again = apply_pack(
            &d,
            "demo-bot",
            &after,
            &stage,
            &read_log(&logp),
            Some(&KEY),
            None,
        )
        .unwrap()
        .added;
        assert!(again.is_empty());
        // a signed pack is refused when the box has no key to check it with
        let key = [6u8; 32];
        let mut sp = Pack::new("demo-bot", 9, "ab", "a1", "t", vec![]).unwrap();
        let sb = sp.seal_with(Some(&key));
        write_private(&d, &format!("to-grok/{}", sp.file_name()), &sb).unwrap();
        write_private(&d, "HEAD", sp.head().to_line().as_bytes()).unwrap();
        assert!(head_pack(&d, "demo-bot", None)
            .unwrap_err()
            .contains("--key"));
        assert!(head_pack(&d, "demo-bot", Some(&key)).is_ok());
        // a tampered HEAD is refused
        let mut h = p.head();
        h.sha = "0".repeat(64);
        write_private(&d, "HEAD", h.to_line().as_bytes()).unwrap();
        assert!(apply_pack(&d, "demo-bot", &after, &stage, &[], Some(&KEY), None).is_err());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_previous_state_is_the_highest_numbered_one_that_verifies() {
        let d = tmp("newest");
        let items = items_of(&model("Example prompt."));
        for _ in 0..4 {
            digest_items(
                &items,
                &d,
                "demo-bot",
                &[],
                "2000-01-01T00:00:00Z",
                false,
                Some(&KEY),
            )
            .unwrap();
        }
        // state-4 written with another key, state-3 damaged: state-2 is the newest that verifies
        let other = [9u8; 32];
        let f4 = fs::read(d.join("state-4.json")).unwrap();
        let mut sf = StateFile::parse_with(&f4, "demo-bot", Some(&KEY)).unwrap();
        fs::write(d.join("state-4.json"), sf.seal_with(Some(&other))).unwrap();
        fs::write(d.join("state-3.json"), b"{ not a state file").unwrap();
        // the folder lists them in no particular order; renaming shuffles the creation order too
        for n in [2, 1] {
            let p = d.join(format!("state-{n}.json"));
            let b = fs::read(&p).unwrap();
            fs::remove_file(&p).unwrap();
            fs::write(&p, b).unwrap();
        }
        assert_eq!(
            newest_state(&d, "demo-bot", Some(&KEY)).map(|f| f.n),
            Some(2)
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_pack_older_than_the_applied_floor_is_refused() {
        let d = tmp("floor");
        let stage = d.join("stage");
        let live = items_of(&model("Example prompt."));
        let r = ItemId::parse("routine:demo-routine").unwrap();
        let c = live.content[&r].clone();
        publish(&d, 4, &[(&r, &c, ABSENT)]);
        // the floor is kept beside the applied log, one number
        let fp = d.join(FLOOR_FILE);
        assert_eq!(read_floor(&fp), None);
        write_floor(&fp, 5).unwrap();
        assert_eq!(read_floor(&fp), Some(5));
        let e = apply_pack(
            &d,
            "demo-bot",
            &live,
            &stage,
            &[],
            Some(&KEY),
            read_floor(&fp),
        )
        .err()
        .unwrap();
        assert!(e.contains("applied in full already"), "{e}");
        // at or above the floor the pack is read as usual (at it, the caller stages nothing)
        write_floor(&fp, 4).unwrap();
        assert!(apply_pack(
            &d,
            "demo-bot",
            &live,
            &stage,
            &[],
            Some(&KEY),
            read_floor(&fp)
        )
        .is_ok());
        fs::remove_dir_all(&d).ok();
    }
}
