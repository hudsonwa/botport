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
            out.content.insert(id, c);
        }
        Err(e) => {
            out.unsupported.insert(raw, e);
        }
    }
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
    // memories and user facts are not part of the setup sync: counted under "not mirrored" so the owner sees it
    if !m.memory.is_empty() {
        out.unsupported.insert(
            "memory".into(),
            format!(
                "{} memories and user facts stay on this side (not part of the setup sync)",
                m.memory.len()
            ),
        );
    }
    out
}

/// Read the bot's setup from an export.
pub fn read_items(export: &Path) -> Res<GrokItems> {
    Ok(items_of(&GrokBot.read(export)?))
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

/// The newest readable state file in `out`; with a pair key only one signed with it counts, so nothing (such as the
/// applied generation carried forward) is taken from a file the key does not verify.
fn newest_state(out: &Path, pair: &str, key: Option<&[u8]>) -> Option<StateFile> {
    let mut best: Option<StateFile> = None;
    for e in fs::read_dir(out).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(n) = crate::adapters::mirror::state::state_seq(&name) else {
            continue;
        };
        if best.as_ref().is_some_and(|b| b.n >= n) {
            continue;
        }
        if let Ok(f) = fs::read(e.path())
            .map_err(|e| e.to_string())
            .and_then(|b| StateFile::parse_with(&b, pair, key))
        {
            best = Some(f);
        }
    }
    best
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
    let prev = newest_state(out, pair, key);
    let n = prev.as_ref().map_or(1, |p| p.n + 1);
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
    let mut applied: Vec<Applied> = log.to_vec();
    applied.sort_by_key(|a| a.gen);
    let skip = applied.len().saturating_sub(LOG_KEEP);
    let applied = applied.split_off(skip);
    let mut f = StateFile {
        schema: SCHEMA.into(),
        pair: pair.into(),
        n,
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
        unsupported: items
            .unsupported
            .iter()
            .filter_map(|(raw, why)| ItemId::parse(raw).ok().map(|id| (id, why.clone())))
            .collect(),
        applied,
        sha256: String::new(),
        mac: None,
    };
    let bytes = f.seal_with(key);
    write_private(out, &format!("state-{n}.json"), &bytes)?;
    Ok(f)
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
) -> Res<PackRun> {
    let p = head_pack(pair_dir, pair, key)?;
    let now = live.digests();
    let mut steps = vec![];
    let mut added = vec![];
    for it in &p.items {
        let cur = now
            .get(&it.item)
            .map_or(ABSENT.to_string(), |(_, n)| n.clone());
        let step = if cur == it.want {
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
        } else if key.is_none() {
            Step::Held
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
) -> Res<()> {
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
        if Content::Package(staged).neutral_digest(item) != it.want {
            return err(format!(
                "{item}: the staged copy changed since it was staged; run apply-pack again"
            ));
        }
    }
    Ok(())
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
        apply_pack(&d, "demo-bot", &live, &stage, &[], Some(&KEY)).unwrap();
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
        let run = apply_pack(&d, "demo-bot", &live, &stage, &[], None).unwrap();
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
        } = apply_pack(&d, "demo-bot", &live, &stage, &[], Some(&KEY)).unwrap();
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
            apply_pack(&d, "demo-bot", &after, &stage, &[], Some(&KEY)).unwrap();
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
        let again = apply_pack(&d, "demo-bot", &after, &stage, &read_log(&logp), Some(&KEY))
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
        assert!(apply_pack(&d, "demo-bot", &after, &stage, &[], Some(&KEY)).is_err());
        fs::remove_dir_all(&d).ok();
    }
}
