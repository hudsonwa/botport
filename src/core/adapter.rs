//! The hub-and-spoke seam: every platform adapter reads its native layout into a `BotModel` and writes one back.
//! An adapter that can also keep a live install in step with a model implements `Target` (plan = diff, apply =
//! write behind a restore point). The CLI's plan/apply/sync/drift are: source `read_source` -> model -> `Target`.
#![warn(missing_docs)]
use super::model::BotModel;
use super::util::{digest_pairs, Res};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A platform: reads its native layout into a [`BotModel`] and writes one back. Every adapter passes the shared
/// conformance suite (`src/core/conformance.rs`, run by `src/conformance_tests.rs`; see docs/ADAPTERS.md).
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
    /// What `apply` would do to the install at `dest`, unit by unit; reads only.
    fn plan(&self, m: &BotModel, src: &Source, dest: &Path, o: &Self::Opts) -> Res<Plan>;
    /// Write the changes `plan` reports (behind a restore point) and record what was written; `dry` writes nothing.
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
    /// The path that was read.
    pub path: PathBuf,
    /// The source's own id (e.g. a bot id), if it has one.
    pub id: String,
    /// Relative path -> sha256 of every file read.
    pub files_read: BTreeMap<String, String>,
    /// skills present at the source but left out by an allowlist
    pub excluded: BTreeSet<String>,
}

impl Source {
    /// The sync digest over `files_read`.
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
    /// already as the model says
    #[serde(rename = "in-sync")]
    InSync,
    /// new at the target
    Create,
    /// changed at the source since the last apply
    Update,
    /// gone from the source; removed at the target
    Retire,
    /// changed at the target since botport last wrote it: left alone
    Conflict,
    /// the same unit also present under another importer's name (left for a human)
    Duplicate,
    /// not handled (e.g. excluded by an allowlist)
    Skip,
    /// needs a human (listed in the target's checklist); never written, not a difference
    Manual,
}

impl St {
    /// The status as plan output prints it.
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
    /// Create, update or retire: something `apply` writes.
    pub fn is_change(self) -> bool {
        matches!(self, St::Create | St::Update | St::Retire)
    }
}

/// One line of a plan: a unit in an area and what happens to it.
#[derive(Serialize, Clone, Debug)]
pub struct Item {
    /// e.g. `skills`, `soul`.
    pub area: String,
    /// The unit within the area.
    pub name: String,
    /// What happens to it.
    pub status: St,
    /// Human-readable reason or summary.
    pub detail: String,
    /// conflict with no record of what was written (`apply --adopt` may take it over)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unrecorded: bool,
}

/// An [`Item`] with `unrecorded: false`.
pub fn item(area: &str, name: &str, status: St, detail: impl Into<String>) -> Item {
    Item {
        area: area.into(),
        name: name.into(),
        status,
        detail: detail.into(),
        unrecorded: false,
    }
}

/// What [`Target::plan`] found.
pub struct Plan {
    /// One entry per unit.
    pub items: Vec<Item>,
    /// the source digest recorded by the last apply (None: never applied)
    pub synced_digest: Option<String>,
}

/// What [`Target::apply`] did.
pub struct Applied {
    /// One entry per unit, as planned.
    pub items: Vec<Item>,
    /// Backup of the install taken before writing (None when nothing was written).
    pub restore_point: Option<PathBuf>,
    /// The platform must be restarted to pick the change up.
    pub restart_needed: bool,
}
