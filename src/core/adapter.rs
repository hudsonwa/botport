//! The hub-and-spoke seam: every platform adapter reads its native layout into a `BotModel` and writes one back.
//! An adapter that can also keep a live install in step with a model implements `Target` (plan = diff, apply =
//! write behind a restore point). The CLI's plan/apply/sync/drift are: source `read_source` -> model -> `Target`.
use super::model::BotModel;
use super::util::{digest_pairs, Res};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub trait Adapter {
    /// Stable adapter id; also the adapter's key in `BotModel::extras`.
    fn id(&self) -> &'static str;
    /// Read the platform's native layout at `src` into the neutral model.
    fn read(&self, src: &Path) -> Res<BotModel>;
    /// `read`, plus what was read (for sync bookkeeping). Default: the model with no files recorded.
    fn read_source(&self, src: &Path) -> Res<(BotModel, Source)> {
        Ok((
            self.read(src)?,
            Source {
                path: src.to_path_buf(),
                ..Source::default()
            },
        ))
    }
    /// Write the model into the platform's native layout under `dest`; returns the files written (relative).
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>>;
}

/// An adapter that syncs a model into a live install: `plan` only reads, `apply` writes what `plan` reports as a
/// change (never a conflict, unless `adopt` takes over one with no sync record).
pub trait Target: Adapter {
    /// Target-specific options (which install, ownership templates, ...).
    type Opts;
    fn plan(&self, m: &BotModel, src: &Source, dest: &Path, o: &Self::Opts) -> Res<Plan>;
    fn apply(
        &self,
        m: &BotModel,
        src: &Source,
        dest: &Path,
        o: &Self::Opts,
        adopt: bool,
        dry: bool,
    ) -> Res<Applied>;
}

/// Where a model came from, for sync bookkeeping (not bot content): the source path, the source's own id, and
/// a sha256 per file read (the sync digest covers exactly these).
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub path: PathBuf,
    pub id: String,
    pub files_read: BTreeMap<String, String>,
    /// skills present at the source but left out by an allowlist
    pub excluded: BTreeSet<String>,
}

impl Source {
    pub fn digest(&self) -> String {
        digest_pairs(&self.files_read)
    }
}

/// Keep only the skills `allowed` accepts: dropped from the model and, with their files (`skills/<name>/...`),
/// from the source digest; their names go to `excluded`.
pub fn retain_skills(m: &mut BotModel, s: &mut Source, allowed: impl Fn(&str) -> bool) {
    let gone: Vec<String> = m
        .skills
        .iter()
        .filter(|k| !allowed(&k.name))
        .map(|k| k.name.clone())
        .collect();
    m.skills.retain(|k| !gone.contains(&k.name));
    let prefixes: Vec<String> = gone.iter().map(|n| format!("skills/{n}/")).collect();
    s.files_read
        .retain(|k, _| !prefixes.iter().any(|p| k.starts_with(p)));
    s.excluded.extend(gone);
}

/// What a plan says about one unit (a skill, a SOUL section, a config path, a job ...).
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum St {
    #[serde(rename = "in-sync")]
    InSync,
    Create,
    Update,
    Retire,
    Conflict,
    /// the same unit also present under another importer's name (left for a human)
    Duplicate,
    Skip,
    /// needs a human (listed in the target's checklist); never written, not a difference
    Manual,
}

impl St {
    pub fn word(self) -> &'static str {
        match self {
            St::InSync => "in-sync",
            St::Create => "create",
            St::Update => "update",
            St::Retire => "retire",
            St::Conflict => "conflict",
            St::Duplicate => "duplicate",
            St::Skip => "skip",
            St::Manual => "manual",
        }
    }
    pub fn is_change(self) -> bool {
        matches!(self, St::Create | St::Update | St::Retire)
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Item {
    pub area: String,
    pub name: String,
    pub status: St,
    pub detail: String,
    /// conflict with no record of what was written (`apply --adopt` may take it over)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unrecorded: bool,
}

pub fn item(area: &str, name: &str, status: St, detail: impl Into<String>) -> Item {
    Item {
        area: area.into(),
        name: name.into(),
        status,
        detail: detail.into(),
        unrecorded: false,
    }
}

pub struct Plan {
    pub items: Vec<Item>,
    /// the source digest recorded by the last apply (None: never applied)
    pub synced_digest: Option<String>,
}

pub struct Applied {
    pub items: Vec<Item>,
    pub restore_point: Option<PathBuf>,
    pub restart_needed: bool,
}
