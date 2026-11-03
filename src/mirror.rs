//! `botport mirror ...`: mirror mode's commands (docs/design/MIRROR.md section 4.8), as library functions the CLI
//! (src/main.rs) calls. Each returns an [`Out`]: an exit code, a few summary lines and a JSON report.
//!
//! A pair has two files: the mirror state in the profile (`botport.mirror.json`: digests, names, numbers) and the pair
//! config outside it (`$XDG_CONFIG_HOME/botport/mirror/<pair>.toml`: which profile, which transport, the Drive token
//! command), so the config is never mirrored or put in a restore point.
use crate::adapters::grok_bot::mirror as gmirror;
use crate::adapters::hermes::mirror::{HermesSide, LocalItems};
use crate::adapters::hermes::sync;
use crate::adapters::mirror::api::{ApiConfig, ApiStore};
use crate::adapters::mirror::drive::DriveTransport;
use crate::adapters::mirror::local::{LocalTransport, StateDirLayout};
use crate::adapters::mirror::pack::{random_id, Blobs, Head, Pack, PackAction, PackItem};
use crate::adapters::mirror::store::FolderStore;
use crate::adapters::mirror::{IoStats, PeerRead, Transport};
use crate::core::item::{canonical_text, Content};
use crate::core::metrics;
use crate::core::mirror::{
    self as engine, Action, Approval, Coverage, Decision, ItemId, Kind, Outcome, Side, SideRead,
    State, View, ABSENT,
};
use crate::core::util::{
    err, now_secs, sha256_hex, utc_iso, write_atomic, write_private, Args, OpLock, Res,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The mirror state file in the profile.
pub const STATE_FILE: &str = "botport.mirror.json";
/// The conflicts and approvals report in the profile (written every run, never read back).
pub const REPORT_FILE: &str = "botport-mirror-conflicts.md";
/// Where kept copies live in the profile.
pub const STORE_DIR: &str = "botport/mirror-store";
/// An item in flight longer than this (seconds) makes `mirror status` warn and exit 1.
pub const IN_FLIGHT_TIMEOUT: u64 = 7 * 86400;
/// Instructions may drift this much (percent of changed lines) from the last approved text without an approval.
pub const DRIFT_PCT: usize = 25;

/// Unix seconds now, unsigned (0 before 1970).
fn now_unix() -> u64 {
    u64::try_from(now_secs()).unwrap_or(0)
}

/// What a mirror command reports.
pub struct Out {
    /// Exit code.
    pub code: u8,
    /// Summary lines for stdout.
    pub lines: Vec<String>,
    /// The JSON report.
    pub json: Value,
}

/// The pair config (`<pair>.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// The pair's name.
    pub pair: String,
    /// The Hermes profile directory.
    pub profile_dir: PathBuf,
    /// The profile name (for the hermes CLI).
    pub profile: String,
    /// HERMES_HOME for the hermes CLI, when not the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hermes_home: Option<PathBuf>,
    /// The hermes binary.
    pub hermes_bin: String,
    /// Whether routines are written (a real profile through the hermes CLI).
    pub cron: bool,
    /// `local`, `drive` or `hybrid`.
    pub transport: String,
    /// This machine's id (written into HEAD).
    pub machine: String,
    /// Mirrored skill categories (the first gets new skills).
    pub categories: Vec<String>,
    /// `all` (the default: every behaviour-changing edit waits for approval) or `runnable` (the owner's opt-in).
    pub approve: String,
    /// Local transport: the local records directory (`<local-dir>/<bot-key>/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_dir: Option<PathBuf>,
    /// The bot's key in the local records directory (default: the pair name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_key: Option<String>,
    /// The pair key file (64 hex characters, 0600): signs packs and checks Grok state files and host triggers.
    /// (Named for what it does: the secret write guard refuses a config line that looks like `key_file = ...`.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_file: Option<PathBuf>,
    /// Drive transport, synced-folder backend: the pair's folder on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_dir: Option<PathBuf>,
    /// Drive transport, API backend: the pair folder's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_folder_id: Option<String>,
    /// Drive API backend: the command that prints an access token (argv, no shell).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_cmd: Vec<String>,
    /// Kept so an older config still reads; never used: the Drive API base is fixed to Google's endpoint.
    #[serde(default, skip_serializing)]
    pub drive_api_base: Option<String>,
}

/// The directory pair configs live in.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::core::util::home().join(".config"))
        .join("botport/mirror")
}

fn config_path(a: &Args, pair: &str) -> PathBuf {
    a.path("mirror-config")
        .unwrap_or_else(|| config_dir().join(format!("{pair}.toml")))
}

fn load_config(a: &Args) -> Res<Config> {
    let pair = a.get("pair").ok_or("--pair NAME is required")?;
    engine::valid_name(pair)?;
    let p = config_path(a, pair);
    let t = fs::read_to_string(&p).map_err(|_| {
        format!(
            "no mirror config for {pair:?} at {}: run botport mirror init",
            p.display()
        )
    })?;
    let c: Config = toml::from_str(&t).map_err(|e| format!("{}: {e}", p.display()))?;
    if c.pair != pair {
        return err(format!("{} is for pair {:?}", p.display(), c.pair));
    }
    Ok(c)
}

/// The pair key, when the pair has one. The file must not be readable by group or others.
pub fn pair_key(c: &Config) -> Res<Option<Vec<u8>>> {
    let Some(p) = &c.signing_file else {
        return Ok(None);
    };
    read_key_file(p).map(Some)
}

/// A pair key file: a regular file, 0600, holding the key (as `digest show --key` reads it in the box).
pub fn read_key(p: &Path) -> Res<Vec<u8>> {
    read_key_file(p)
}

fn read_key_file(p: &Path) -> Res<Vec<u8>> {
    let md =
        fs::symlink_metadata(p).map_err(|_| format!("pair key file {} not found", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !md.is_file() || md.permissions().mode() & 0o077 != 0 {
            return err(format!(
                "pair key file {} must be a regular file with mode 0600",
                p.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = md;
    crate::core::auth::parse_key(&fs::read_to_string(p).map_err(|e| e.to_string())?)
}

/// A restore point of everything mirror mode may write: every mirrored skill category and the mirror state.
fn restore_point(c: &Config) -> Res<PathBuf> {
    let cats: Vec<&str> = c.categories.iter().map(String::as_str).collect();
    sync::restore_point_of(&c.profile_dir, &cats, &[STATE_FILE], 5)
}

fn side(c: &Config) -> HermesSide {
    HermesSide {
        dest: c.profile_dir.clone(),
        profile: c.profile.clone(),
        hermes_bin: c.hermes_bin.clone(),
        hermes_home: c.hermes_home.clone(),
        categories: c.categories.clone(),
        cron: c.cron,
        held: Default::default(),
    }
}

/// The Hermes side as the mirror state knows it: with the routines held paused until the owner approves them.
fn hside(c: &Config, st: &State) -> HermesSide {
    let mut h = side(c);
    h.held = st
        .held_paused
        .iter()
        .filter(|(i, _)| i.kind() == Kind::Routine)
        .map(|(i, d)| (i.name().to_string(), (!d.is_empty()).then(|| d.clone())))
        .collect();
    h
}

/// Hold a routine paused (on the Hermes side, awaiting approval) or release it, in both the side and the state.
/// `Some(digest)`: held as the copy with that side digest (empty while it is being written); `None`: not held.
fn set_held(hs: &mut HermesSide, st: &mut State, id: &ItemId, held: Option<String>) {
    if id.kind() != Kind::Routine {
        return;
    }
    match held {
        Some(d) => {
            hs.held
                .insert(id.name().to_string(), (!d.is_empty()).then(|| d.clone()));
            st.held_paused.insert(id.clone(), d);
        }
        None => {
            hs.held.remove(id.name());
            st.held_paused.remove(id);
        }
    }
}

fn load_state(c: &Config) -> Res<State> {
    let p = c.profile_dir.join(STATE_FILE);
    let t = fs::read_to_string(&p).map_err(|_| {
        format!(
            "no mirror state in {}: run botport mirror init",
            c.profile_dir.display()
        )
    })?;
    let mut st = State::from_json(&t, &c.pair)?;
    // a hold ends once the job is no longer the paused copy botport wrote (the owner resumed, paused again, edited or
    // removed it): from then on the job is read as it is, by every command
    let hs = hside(c, &st);
    st.held_paused
        .retain(|id, _| hs.paused_by_policy(id).unwrap_or(true));
    Ok(st)
}

fn save_state(c: &Config, st: &State) -> Res<()> {
    write_atomic(
        &c.profile_dir.join(STATE_FILE),
        st.to_json().as_bytes(),
        Some(0o600),
    )
}

// ---------------------------------------------------------------- transports

/// The Drive transport for a config, over the synced folder or the API.
pub enum Drive {
    /// Google Drive for desktop's folder on disk.
    Folder(DriveTransport<FolderStore>),
    /// The Drive API.
    Api(DriveTransport<ApiStore>),
}

impl Drive {
    fn open(c: &Config, floor: Option<u64>) -> Res<Option<Drive>> {
        let key = pair_key(c)?;
        if let Some(d) = &c.drive_dir {
            return Ok(Some(Drive::Folder(
                DriveTransport::new(FolderStore::open(d)?, &c.pair, floor).with_key(key),
            )));
        }
        if let Some(id) = &c.drive_folder_id {
            // the API base is always Google's endpoint (only the API's own unit tests use a stub)
            let mut cfg = ApiConfig::new(id, c.token_cmd.clone());
            // unchanged files are kept between runs, so a quiet sync downloads nothing
            cfg.cache_dir = Some(c.profile_dir.join("botport/drive-cache"));
            return Ok(Some(Drive::Api(
                DriveTransport::new(ApiStore::new(cfg), &c.pair, floor).with_key(key),
            )));
        }
        Ok(None)
    }

    fn t(&mut self) -> &mut dyn Transport {
        match self {
            Drive::Folder(t) => t,
            Drive::Api(t) => t,
        }
    }

    fn head(&mut self) -> Res<Option<Head>> {
        match self {
            Drive::Folder(t) => t.head(),
            Drive::Api(t) => t.head(),
        }
    }

    fn send_pack(
        &mut self,
        p: &Pack,
        blobs: &[(String, Vec<u8>)],
        expect: Option<&Head>,
    ) -> Res<()> {
        match self {
            Drive::Folder(t) => t.send_pack(p, blobs, expect).map(|_| ()),
            Drive::Api(t) => t.send_pack(p, blobs, expect).map(|_| ()),
        }
    }

    fn floor(&self) -> Option<u64> {
        match self {
            Drive::Folder(t) => t.floor(),
            Drive::Api(t) => t.floor(),
        }
    }
}

/// The transports a config names: local (read only) and/or Drive.
pub struct Link {
    local: Option<LocalTransport>,
    drive: Option<Drive>,
    /// which transport each item kind was read from
    pub from: BTreeMap<Kind, &'static str>,
}

impl Link {
    fn open(c: &Config, st: Option<&State>) -> Res<Link> {
        let floor = st.and_then(|s| s.remote_seq);
        let local = match (&c.local_dir, c.transport.as_str()) {
            (Some(d), "local" | "hybrid") => {
                Some(LocalTransport::new(Box::new(StateDirLayout::open(
                    d,
                    c.bot_key.as_deref().unwrap_or(&c.pair),
                    &c.pair,
                    floor,
                    pair_key(c)?,
                )?)))
            }
            _ => None,
        };
        let drive = match c.transport.as_str() {
            "drive" | "hybrid" => Drive::open(c, floor)?,
            _ => None,
        };
        if local.is_none() && drive.is_none() {
            return err(format!(
                "transport {:?} has nothing configured (see mirror init --help)",
                c.transport
            ));
        }
        Ok(Link {
            local,
            drive,
            from: BTreeMap::new(),
        })
    }

    /// Read the remote side: kind by kind from the local transport where its read is complete, else from Drive.
    fn read(&mut self) -> Res<Option<PeerRead>> {
        let l = match &mut self.local {
            Some(t) => t.read_peer()?,
            None => None,
        };
        let d = match &mut self.drive {
            Some(t) => t.t().read_peer()?,
            None => None,
        };
        let kinds = [Kind::Persona, Kind::Skill, Kind::Routine, Kind::Connector];
        let (base, other, bname, oname) = match (l, d) {
            (None, None) => return Ok(None),
            (Some(l), None) => {
                self.from = kinds.iter().map(|k| (*k, "local")).collect();
                return Ok(Some(l));
            }
            (None, Some(d)) => {
                self.from = kinds.iter().map(|k| (*k, "drive")).collect();
                return Ok(Some(d));
            }
            (Some(l), Some(d)) => (l, d, "local", "drive"),
        };
        let mut out = base.clone();
        out.side = SideRead::default();
        // both complete: the newer snapshot wins, so a stale local one never masks a newer Drive state
        let now = now_unix();
        let when = |w: &str| crate::core::util::parse_utc_iso(w).map(|t| t.min(now));
        let base_newer = when(&base.written) >= when(&other.written);
        for k in kinds {
            let (bc, oc) = (
                base.side.coverage.get(&k) == Some(&Coverage::Complete),
                other.side.coverage.get(&k) == Some(&Coverage::Complete),
            );
            let use_base = (bc && (!oc || base_newer)) || !oc;
            let src = if use_base { &base } else { &other };
            self.from.insert(k, if use_base { bname } else { oname });
            if let Some(cv) = src.side.coverage.get(&k) {
                out.side.coverage.insert(k, *cv);
            }
            for (id, v) in src.side.items.iter().filter(|(id, _)| id.kind() == k) {
                out.side.items.insert(id.clone(), v.clone());
            }
            for (id, why) in src.side.unsupported.iter().filter(|(id, _)| id.kind() == k) {
                out.side.unsupported.insert(id.clone(), why.clone());
            }
        }
        // the applied log and generation come from the Drive state file (the Grok routine writes it)
        out.applied = other.applied.clone();
        out.applied_gen = other.applied_gen.or(base.applied_gen);
        out.authenticated = base.authenticated && other.authenticated;
        Ok(Some(out))
    }

    fn fetch(&mut self, id: &ItemId, v: &View) -> Res<Content> {
        match self.from.get(&id.kind()).copied() {
            Some("local") => self
                .local
                .as_mut()
                .ok_or("no local transport")?
                .fetch(id, v),
            _ => self
                .drive
                .as_mut()
                .ok_or("no Drive transport")?
                .t()
                .fetch(id, v),
        }
    }

    fn io(&self) -> IoStats {
        let mut s = IoStats::default();
        if let Some(t) = &self.local {
            s.add(t.io());
        }
        if let Some(Drive::Folder(t)) = &self.drive {
            s.add(t.io());
        }
        if let Some(Drive::Api(t)) = &self.drive {
            s.add(t.io());
        }
        s
    }

    /// Figures of the Drive transport alone (what the Grok routine reads and writes for botport).
    fn drive_io(&self) -> IoStats {
        match &self.drive {
            Some(Drive::Folder(t)) => t.io(),
            Some(Drive::Api(t)) => t.io(),
            None => IoStats::default(),
        }
    }

    fn path(&self) -> String {
        let mut v: Vec<&str> = self.from.values().copied().collect();
        v.sort();
        v.dedup();
        if v.is_empty() {
            "none".into()
        } else {
            v.join("+")
        }
    }
}

// ---------------------------------------------------------------- store of kept copies

/// The store folder of an item: its id in hex, so no two ids can share one.
fn slug(id: &ItemId) -> String {
    crate::core::util::hex(id.as_str().as_bytes())
}

/// Keep a copy of `c` (content-addressed; the newest 5 per item).
fn keep(dest: &Path, id: &ItemId, c: &Content) -> Res<()> {
    let b = c.to_blob(id);
    let dir = format!("{STORE_DIR}/{}", slug(id));
    let name = sha256_hex(&b);
    write_private(dest, &format!("{dir}/{name}"), &b)?;
    let d = dest.join(&dir);
    let mut all: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&d)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    all.sort();
    while all.len() > 5 {
        let (_, p) = all.remove(0);
        fs::remove_file(p).ok();
    }
    Ok(())
}

fn kept(dest: &Path, id: &ItemId, short: &str) -> Res<Content> {
    // a prefix of at least 12 hex characters, matching exactly one kept copy
    if short.len() < 12 || !short.bytes().all(|b| b.is_ascii_hexdigit()) {
        return err(format!(
            "{id}: give at least 12 hex characters of the kept copy's digest"
        ));
    }
    let d = dest.join(STORE_DIR).join(slug(id));
    let matches: Vec<std::path::PathBuf> = fs::read_dir(&d)
        .map_err(|_| format!("{id}: no kept copies"))?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(short))
        .map(|e| e.path())
        .collect();
    match matches.as_slice() {
        [one] => {
            // a regular file whose bytes still hash to its name: a copy changed in the store is never restored
            if !fs::symlink_metadata(one).is_ok_and(|m| m.is_file()) {
                return err(format!("{id}: the kept copy {short} is not a regular file"));
            }
            let b = fs::read(one).map_err(|e| e.to_string())?;
            let name = one.file_name().map(|n| n.to_string_lossy().to_string());
            if name.as_deref() != Some(sha256_hex(&b).as_str()) {
                return err(format!(
                    "{id}: the kept copy {short} does not match its digest (changed in the store): not restored"
                ));
            }
            Content::from_blob(id, &b)
        }
        [] => err(format!("{id}: no kept copy {short}")),
        _ => err(format!(
            "{id}: {short} matches more than one kept copy; give more of the digest"
        )),
    }
}

const APPROVED_TEXT: &str = "approved-instructions";

fn approved_instructions(dest: &Path) -> Option<String> {
    fs::read_to_string(dest.join(STORE_DIR).join(APPROVED_TEXT)).ok()
}

fn set_approved_instructions(dest: &Path, t: &str) -> Res<()> {
    write_private(dest, &format!("{STORE_DIR}/{APPROVED_TEXT}"), t.as_bytes())
}

/// Percent of lines that differ between two texts (by line, order-insensitive multiset difference).
pub fn drift_pct(a: &str, b: &str) -> usize {
    let mut count: BTreeMap<&str, i64> = BTreeMap::new();
    for l in a.lines() {
        *count.entry(l).or_default() += 1;
    }
    for l in b.lines() {
        *count.entry(l).or_default() -= 1;
    }
    let changed: i64 = count.values().map(|v| v.abs()).sum();
    let total = a.lines().count().max(b.lines().count()).max(1) as i64;
    ((changed * 100) / (2 * total)).min(100) as usize
}

// ---------------------------------------------------------------- approvals

fn is_text_file(path: &str, b: &[u8], exec: bool) -> bool {
    !exec && std::str::from_utf8(b).is_ok() && (path.ends_with(".md") || path.ends_with(".txt"))
}

/// Why a change needs the owner's approval (None: it may flow). `from` is the copy that would be written, `to` the
/// copy it replaces (None: new on the receiving side).
pub fn needs_approval(
    policy: &str,
    id: &ItemId,
    to_side: Side,
    from: Option<&Content>,
    to: Option<&Content>,
    approved_text: Option<&str>,
) -> Option<String> {
    let from = from?; // a retire never makes anything run
                      // a connector url moving to another host: the owner is told what happens to kept credentials
    if let (Content::Connector(n), Some(Content::Connector(o))) = (from, to) {
        if to_side == Side::Local && moves_origin(n, o) {
            return Some(CROSS_ORIGIN.into());
        }
    }
    // the name lands in the system prompt as well: under "all" it waits like everything else
    if policy == "all" {
        return Some("approve = all: every change waits for the owner".into());
    }
    match (from, to) {
        (Content::Connector(_), _) => Some("a new or changed connector".into()),
        (Content::Package(_), None) => Some("a new skill".into()),
        (Content::Package(n), Some(Content::Package(o))) => n
            .files
            .iter()
            .find(|(p, (b, x))| {
                !is_text_file(p, b, *x) && o.files.get(*p) != Some(&(b.clone(), *x))
            })
            .map(|(p, _)| {
                format!("a new or changed file that is not Markdown or plain text ({p})")
            }),
        (Content::Routine(n), Some(Content::Routine(o))) => {
            // bound for Hermes, an enabled routine is written held paused and listed until the owner approves it
            if n.enabled && !o.enabled && to_side == Side::Remote {
                Some("enabling a routine".into())
            } else if n.enabled && !o.enabled {
                None
            } else if o.enabled && (n.cron != o.cron || n.action != o.action || n.tz != o.tz) {
                Some("a prompt or schedule change to an enabled routine".into())
            } else {
                None
            }
        }
        // on the Hermes side a new routine is written paused by policy; the Grok side cannot be told to, so it waits
        (Content::Routine(n), None) if n.enabled && to_side == Side::Remote => {
            Some("a new routine arriving enabled".into())
        }
        (Content::Routine(_), None) => None,
        (Content::Text(t), _) if id.as_str() == "persona.instructions" => {
            let base = approved_text.unwrap_or("");
            let pct = drift_pct(&canonical_text(id, base), &canonical_text(id, t));
            (pct > DRIFT_PCT)
                .then(|| format!("instructions drifted {pct}% from the last approved text"))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- commands

/// Run `botport mirror <sub>`.
pub fn run(a: &Args) -> Res<Out> {
    match a.pos.get(1).map(String::as_str) {
        Some("init") => init(a),
        Some("plan") => plan_cmd(a),
        Some("sync") => sync_cmd(a),
        Some("status") => status(a),
        Some("show") => show(a),
        Some("pick") => pick(a),
        Some("approve") => approve(a),
        Some("restore") => restore(a),
        Some("stats") => stats(a),
        Some("digest-items") => digest_items(a),
        Some("apply-pack") => apply_pack(a),
        Some(s) => err(format!("unknown mirror command {s:?} (try --help)")),
        None => err("mirror needs a command (try --help)"),
    }
}

/// `mirror init --pair P (--profile NAME [--hermes-home DIR] | --profile-dir DIR) --transport local|drive|hybrid
/// [--local-dir D] [--drive-dir D | --drive-folder-id ID --token-cmd "CMD ARGS"] [--category C]... [--approve all]`
fn init(a: &Args) -> Res<Out> {
    let pair = a.get("pair").ok_or("--pair NAME is required")?.to_string();
    engine::valid_name(&pair)?;
    if a.has("undo") {
        return undo(a);
    }
    let transport = a
        .get("transport")
        .ok_or("--transport local|drive|hybrid is required")?
        .to_string();
    if !matches!(transport.as_str(), "local" | "drive" | "hybrid") {
        return err("--transport must be local, drive or hybrid");
    }
    let (profile, profile_dir, hermes_home, cron) = match (a.get("profile"), a.path("profile-dir"))
    {
        (Some(p), None) => {
            crate::core::util::plain_name("profile", p)?;
            let home = a
                .path("hermes-home")
                .or_else(|| {
                    std::env::var_os("HERMES_HOME")
                        .filter(|v| !v.is_empty())
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| crate::core::util::home().join(".hermes"));
            // the default profile is the Hermes home itself, every other one lives under profiles/
            let dir = if p == "default" {
                home.clone()
            } else {
                home.join("profiles").join(p)
            };
            (p.to_string(), dir, a.path("hermes-home"), true)
        }
        (None, Some(d)) => (pair.clone(), d, None, a.get("hermes-bin").is_some()),
        _ => return err("give one of --profile NAME or --profile-dir DIR"),
    };
    if !profile_dir.is_dir() {
        return err(format!(
            "profile dir {} does not exist",
            profile_dir.display()
        ));
    }
    let _lock = OpLock::try_acquire(&profile_dir)?;
    if profile_dir.join(STATE_FILE).exists() && !a.has("force") {
        return err(format!(
            "{} already has a mirror state (mirror init --force replaces it)",
            profile_dir.display()
        ));
    }
    let categories: Vec<String> = if a.all("category").is_empty() {
        vec!["mirror".into()]
    } else {
        a.all("category").to_vec()
    };
    // a category is a folder name: the same fixed alphabet as item names (no separators, no '..', not absolute)
    for c in &categories {
        engine::valid_name(c).map_err(|_| format!("invalid skill category {c:?}: 1-64 of letters, digits, '.', '_', '-', space, starting with a letter or digit"))?;
    }
    let approve = a.get("approve").unwrap_or("all").to_string();
    if !matches!(approve.as_str(), "runnable" | "all") {
        return err("--approve must be runnable or all");
    }
    let cfg = Config {
        pair: pair.clone(),
        profile_dir: profile_dir.clone(),
        profile,
        hermes_home,
        hermes_bin: a.get("hermes-bin").unwrap_or("hermes").to_string(),
        cron,
        transport: transport.clone(),
        machine: random_id(),
        categories,
        approve,
        local_dir: a.path("local-dir"),
        bot_key: a.get("bot-key").map(String::from),
        signing_file: None,
        drive_dir: a.path("drive-dir"),
        drive_folder_id: a.get("drive-folder-id").map(String::from),
        token_cmd: a
            .get("token-cmd")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default(),
        drive_api_base: None,
    };
    let mut cfg = cfg;
    if let Some(k) = &cfg.bot_key {
        crate::core::local_records::valid_bot_key(k)?;
    }
    let mut new_key = false;
    if a.has("no-pair-key") && a.path("key-file").is_some() {
        return err("give one of --key-file F or --no-pair-key");
    }
    if let Some(kf) = a.path("key-file") {
        read_key_file(&kf)?;
        cfg.signing_file = Some(kf);
    } else if !a.has("no-pair-key") {
        // every pair is signed unless the owner opts out: files in the folder are authenticated, not only hashed
        let kp = config_path(a, &pair).with_extension("key");
        let dir = kp.parent().ok_or("key path has no directory")?;
        if !kp.exists() {
            write_private(
                dir,
                &kp.file_name().unwrap().to_string_lossy(),
                crate::core::auth::new_key()?.as_bytes(),
            )?;
            new_key = true;
        }
        read_key_file(&kp)?;
        cfg.signing_file = Some(kp);
    }
    let mut link = Link::open(&cfg, None)?;
    let remote = match link.read()? {
        Some(r) => r,
        None => {
            let mut why = nothing_yet(&cfg);
            // the first init of a signed pair makes its key: the Grok side signs its first state file with it
            if let (true, Some(k)) = (new_key, &cfg.signing_file) {
                why.push_str(&format!(
                    "; a pair key was made at {}: copy that file to the Grok box (never paste it into a chat), run digest-items there with --key, then run mirror init again",
                    k.display()
                ));
            }
            return err(why);
        }
    };
    let local = side(&cfg).read()?;
    let mut st = State::new(&pair, "hermes", "grok", &transport);
    st.remote_seq = remote_seq(&link);
    let now = utc_iso();
    let mut equal = 0;
    let copy_one_sided = a.has("copy-one-sided");
    // equal items get bases; every other item, one-sided ones too, goes to the pick-one list (or, with
    // --copy-one-sided, a one-sided item stays untracked so the first sync copies it, under the approval rules)
    for d in engine::plan(&st, &local.read, &remote.side, &[]) {
        match (&d.local, &d.remote) {
            (Some(l), Some(r)) if l.neutral == r.neutral => {
                if let Ok(c) = link.fetch(&d.item, r) {
                    keep(&profile_dir, &d.item, &c)?;
                }
                st.record(&d, Outcome::Kept, &now);
                equal += 1;
            }
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) if copy_one_sided => {}
            _ => {
                if let Some(r) = &d.remote {
                    if let Ok(c) = link.fetch(&d.item, r) {
                        keep(&profile_dir, &d.item, &c)?;
                    }
                }
                let mut c = d.clone();
                c.action = Action::Conflict;
                st.record(&c, Outcome::Kept, &now);
            }
        }
    }
    if let Some(Content::Text(t)) = local.content.get(&ItemId::parse("persona.instructions")?) {
        set_approved_instructions(&profile_dir, t)?;
    }
    let cp = config_path(a, &pair);
    let toml_text = toml::to_string(&cfg).map_err(|e| e.to_string())?;
    let dir = cp.parent().ok_or("config path has no directory")?;
    write_private(
        dir,
        &cp.file_name().unwrap().to_string_lossy(),
        toml_text.as_bytes(),
    )?;
    save_state(&cfg, &st)?;
    write_report(&cfg, &st, &[])?;
    let lines = vec![
        format!("mirror init {pair}: {equal} items in step, {} to pick (botport mirror status --pair {})", st.conflicts.len(), shell_word(&pair)),
                format!("config: {}", cp.display()),
        match &cfg.signing_file {
                        Some(k) => format!("the pair's signing file is {} (copy it to the Grok box; never print or paste it)", k.display()),
            None => "warning: unsigned pair (--no-pair-key): nothing is sent to the Grok side, and every change seen there needs approval before it is applied here".into(),
        },
        "add the digest hooks to the profile's config.yaml: see botport-manual.md".into(),
    ];
    write_hook_manual(&cfg)?;
    Ok(Out {
        code: 0,
        lines,
        json: json!({"pair": pair, "in_step": equal, "to_pick": st.conflicts.len(), "config": cp.display().to_string()}),
    })
}

/// `mirror init --undo --pair P`: stop mirroring the profile; its mirror state moves to `backups/botport/`.
fn undo(a: &Args) -> Res<Out> {
    let c = load_config(a)?;
    let from = c.profile_dir.join(STATE_FILE);
    if !from.is_file() {
        return err(format!("{} is not mirrored", c.profile_dir.display()));
    }
    let _lock = OpLock::try_acquire(&c.profile_dir)?;
    let dir = c.profile_dir.join(sync::BACKUPS);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let to = dir.join(format!("mirror-{}.json", crate::core::util::utc_stamp()));
    fs::rename(&from, &to).map_err(|e| format!("move {}: {e}", from.display()))?;
    Ok(Out {
        code: 0,
        lines: vec![format!(
            "mirror state of {} moved to {}; v0.1 commands work on it again",
            c.pair,
            to.display()
        )],
        json: json!({"pair": c.pair, "moved_to": to.display().to_string()}),
    })
}

/// Why init found nothing from the Grok side, as specific as the transport allows.
fn nothing_yet(c: &Config) -> String {
    if c.transport == "local" {
        return "the local records hold no setup (records that carry activity only cannot start a pair): use --transport hybrid or drive".into();
    }
    if let Some(d) = &c.drive_dir {
        let from = d.join("from-grok");
        // the pairs the state files there were written for
        let pairs: std::collections::BTreeSet<String> = fs::read_dir(&from)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                crate::adapters::mirror::state::state_seq(&e.file_name().to_string_lossy())
                    .is_some()
            })
            .filter_map(|e| fs::read(e.path()).ok())
            .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter_map(|v| v["pair"].as_str().map(String::from))
            // read from files nothing has verified yet: shown only as a valid pair name, else escaped
            .map(|p| {
                if engine::valid_name(&p).is_ok() {
                    p
                } else {
                    term_safe(&format!("{p:?}"))
                }
            })
            .collect();
        if !pairs.is_empty() && !pairs.contains(&c.pair) {
            return format!(
                "the state files in {} belong to pair {}, not {}",
                from.display(),
                pairs.into_iter().collect::<Vec<_>>().join(", "),
                c.pair
            );
        }
        if !pairs.is_empty() {
            return format!(
                "nothing from the Grok side yet that this pair can use in {}{}",
                from.display(),
                if c.signing_file.is_some() {
                    " (with a signing file only state files signed with it count): run digest-items there with --key"
                } else {
                    ": no valid state file for this pair there"
                }
            );
        }
        let nested = d.join(&c.pair).join("from-grok");
        let hint = if nested.is_dir() {
            format!(
                " (give --drive-dir {}: the pair's own folder)",
                d.join(&c.pair).display()
            )
        } else {
            String::new()
        };
        return format!(
            "nothing from the Grok side yet: no from-grok/state-*.json under {}{hint}; run digest-items there first (see docs)",
            d.display()
        );
    }
    "nothing from the Grok side yet: run digest-items there first (see docs)".into()
}

fn remote_seq(link: &Link) -> Option<u64> {
    link.drive.as_ref().and_then(Drive::floor)
}

fn write_hook_manual(c: &Config) -> Res<()> {
    // shell words inside YAML double quotes
    let word = |s: &str| {
        shell_word(s)
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    };
    let bin = word(
        &std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "botport".into()),
    );
    let dir = word(&c.profile_dir.display().to_string());
    let text = format!(
        "\n## botport mirror: activity digest hooks (add to config.yaml, then approve them once in hermes)\n\n```yaml\nhooks:\n  on_session_end:\n    - command: \"{bin} digest write --hermes-home {dir}\"\n      timeout: 10\n  pre_llm_call:\n    - command: \"{bin} digest inject --hermes-home {dir}\"\n      timeout: 5\n```\n"
    );
    let p = c.profile_dir.join(sync::MANUAL);
    let live = fs::read_to_string(&p).unwrap_or_default();
    if live.contains("botport mirror: activity digest hooks") {
        return Ok(());
    }
    write_atomic(&p, format!("{live}{text}").as_bytes(), Some(0o600))
}

struct Planned {
    st: State,
    local: LocalItems,
    remote: PeerRead,
    plan: Vec<Decision>,
    /// Hermes-bound content already fetched while planning.
    incoming: BTreeMap<ItemId, Content>,
}

fn planned(c: &Config, link: &mut Link) -> Res<Planned> {
    let st = load_state(c)?;
    let remote = link.read()?.ok_or("nothing from the Grok side yet")?;
    let local = hside(c, &st).read()?;
    // installs the box reports are taken in only from a read authenticated by the pair key
    let applied: &[engine::Applied] = if remote.authenticated {
        &remote.applied
    } else {
        &[]
    };
    let mut plan = engine::plan(&st, &local.read, &remote.side, applied);
    let signed = pair_key(c)?.is_some();
    for d in plan.iter_mut() {
        if let Some(why) = unsigned_package(signed, &d.item, &d.action) {
            d.action = Action::Unsupported(why);
        }
    }
    // a Grok side read only in part (from its export) is add-only: new items may go there, but replacing or removing
    // an item it has is the owner's to do by hand (design 4.4)
    for d in plan.iter_mut() {
        if let Some(why) = add_only(
            signed,
            &remote.side,
            d.item.kind(),
            &d.action,
            d.remote.is_some(),
        ) {
            d.action = Action::Unsupported(why);
        }
    }
    // approvals need the copies: the local one is at hand, the remote one is fetched only when needed
    let approved = approved_instructions(&c.profile_dir);
    let mut reasons: BTreeMap<ItemId, String> = BTreeMap::new();
    let mut unsupported: BTreeMap<ItemId, String> = BTreeMap::new();
    let mut incoming: BTreeMap<ItemId, Content> = BTreeMap::new();
    for d in &plan {
        let (to_side, from, to): (Side, Option<Content>, Option<Content>) = match d.action {
            Action::ToRemote => {
                if let Some(why) = persona_left_out(&remote.side, &d.item) {
                    unsupported.insert(d.item.clone(), why);
                    continue;
                }
                let to = remote_copy(link, d);
                if let Some(why) = local
                    .content
                    .get(&d.item)
                    .and_then(|cn| secret_in(&d.item, cn))
                {
                    unsupported.insert(d.item.clone(), why);
                    continue;
                }
                // the copy it would replace exists but cannot be read to compare: hold it rather than guess
                if d.remote.is_some() && to.is_none() {
                    reasons.insert(
                        d.item.clone(),
                        "the Grok copy it replaces could not be read to compare".into(),
                    );
                    continue;
                }
                if let Some(why) = local
                    .content
                    .get(&d.item)
                    .and_then(|cn| url_refused(cn, to.as_ref()))
                {
                    unsupported.insert(d.item.clone(), why);
                    continue;
                }
                (Side::Remote, local.content.get(&d.item).cloned(), to)
            }
            Action::ToLocal => match d.remote.as_ref().map(|v| link.fetch(&d.item, v)) {
                Some(Ok(cn)) => {
                    incoming.insert(d.item.clone(), cn.clone());
                    (Side::Local, Some(cn), local.content.get(&d.item).cloned())
                }
                Some(Err(e)) => {
                    unsupported.insert(d.item.clone(), format!("not fetched this run: {e}"));
                    continue;
                }
                None => continue,
            },
            _ => continue,
        };
        // nothing that looks like a secret is carried, whichever way it goes, and a name is only a name
        if let Some(why) = from
            .as_ref()
            .and_then(|cn| secret_in(&d.item, cn).or_else(|| not_a_name(&d.item, cn)))
        {
            unsupported.insert(d.item.clone(), why);
            continue;
        }
        // a change to a routine held paused here is written held again: nothing runs before the owner approves it
        if to_side == Side::Local && st.held_paused.contains_key(&d.item) {
            continue;
        }
        let why = needs_approval(
            &c.approve,
            &d.item,
            to_side,
            from.as_ref(),
            to.as_ref(),
            approved.as_deref(),
        );
        // an add to a Grok side read from its export: the export may have left out an item of that name
        let add =
            (to_side == Side::Remote && d.remote.is_none() && !remote.side.sees_all(d.item.kind()))
                .then_some(ADD_TO_EXPORT_SIDE);
        match (add, why) {
            (Some(a), Some(w)) => {
                reasons.insert(d.item.clone(), format!("{a}; also {w}"));
            }
            (Some(a), None) => {
                reasons.insert(d.item.clone(), a.into());
            }
            (None, Some(w)) => {
                reasons.insert(d.item.clone(), w);
            }
            (None, None) => {}
        }
    }
    for d in plan.iter_mut() {
        if let Some(why) = unsupported.remove(&d.item) {
            d.action = Action::Unsupported(why);
        }
    }
    // a removal or overwrite seen only in a read the pair key does not vouch for never happens on its own
    if !remote.authenticated {
        for d in plan.iter() {
            let why = match d.action {
                Action::RetireLocal => {
                    "removed on the Grok side, in a read not signed with the pair key"
                }
                // every change, additions included: nothing from an unauthenticated read is applied on its own
                Action::ToLocal => {
                    "changed on the Grok side, in a read not signed with the pair key"
                }
                _ => continue,
            };
            reasons
                .entry(d.item.clone())
                .and_modify(|r| {
                    r.push_str("; ");
                    r.push_str(why);
                })
                .or_insert_with(|| why.into());
        }
    }
    engine::gate(&mut plan, &st, |d, _| reasons.get(&d.item).cloned());
    Ok(Planned {
        st,
        local,
        remote,
        plan,
        incoming,
    })
}

/// Why a skill is not carried: packages travel only between signed pairs, never on an unauthenticated read or send.
fn unsigned_package(signed: bool, id: &ItemId, a: &Action) -> Option<String> {
    (!signed && id.kind() == Kind::Skill && matches!(a, Action::ToLocal | Action::ToRemote)).then(
        || "skills travel only in a signed pair (this one was made with --no-pair-key)".to_string(),
    )
}

/// Why an add to a Grok side read from its export waits for the owner.
const ADD_TO_EXPORT_SIDE: &str =
    "new for the Grok side, whose export may have left out an item of this name: approve once you know it has none";

/// Why a `persona.*` item is never sent as an add to a Grok side whose export lacks it: the bot always has one.
fn persona_left_out(remote: &SideRead, id: &ItemId) -> Option<String> {
    (id.kind() == Kind::Persona && !remote.items.contains_key(id) && !remote.sees_all(id.kind()))
        .then(|| {
            "the Grok side always has its persona; its export left it out, so nothing is sent"
                .to_string()
        })
}

/// Why a Grok-bound action is left to the owner: the pair is unsigned (the box applies nothing from an unsigned pack),
/// or it would replace or remove an item the Grok side has while that side's read of the kind is partial (add-only).
fn add_only(signed: bool, remote: &SideRead, k: Kind, a: &Action, exists: bool) -> Option<String> {
    let sends = matches!(a, Action::RetireRemote | Action::ToRemote);
    let replaces = matches!(a, Action::RetireRemote) || (*a == Action::ToRemote && exists);
    if sends && !signed {
        Some(
            "an unsigned pair sends nothing to the Grok side: make this change there by hand"
                .into(),
        )
    } else if replaces && !remote.sees_all(k) {
        Some(
            "add-only on the Grok side (read from its export): make this change there by hand"
                .into(),
        )
    } else {
        None
    }
}

/// Why a `persona.name` is not carried: a name is one line of at most [`NAME_MAX`] characters (anything longer would
/// be text placed in the system prompt without the instructions' approval rules).
fn not_a_name(id: &ItemId, cn: &Content) -> Option<String> {
    match cn {
        Content::Text(t) if id.as_str() == "persona.name" => {
            let t = t.trim();
            (t.contains(['\n', '\r']) || t.chars().count() > NAME_MAX)
                .then(|| format!("a name is one line of at most {NAME_MAX} characters"))
        }
        _ => None,
    }
}

/// The longest `persona.name` carried.
const NAME_MAX: usize = 128;

/// Why an item's text may not be carried (it looks like it holds a secret); packages are checked when read.
fn secret_in(id: &ItemId, cn: &Content) -> Option<String> {
    if matches!(cn, Content::Package(_)) {
        return None;
    }
    crate::core::secrets::find_secret(id.as_str(), &cn.to_blob(id))
        .map(|at| format!("looks like it holds a secret ({at})"))
}

/// Why a Grok-bound connector's url may not be carried: the same url policy as on the Hermes side (https, no
/// credentials, a private host only if the Grok copy already has that url).
fn url_refused(cn: &Content, grok: Option<&Content>) -> Option<String> {
    let url_of = |c: &Content| match c {
        Content::Connector(f) => f.get("url").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    };
    let url = url_of(cn)?;
    let configured: std::collections::BTreeSet<String> =
        grok.and_then(url_of).into_iter().collect();
    crate::core::secrets::connector_url(&url)
        .map(|r| r.to_string())
        .or_else(|| crate::adapters::connectors::check_url(&url, &configured).err())
        .map(|r| format!("url refused ({r})"))
}

fn remote_copy(link: &mut Link, d: &Decision) -> Option<Content> {
    d.remote.as_ref().and_then(|v| link.fetch(&d.item, v).ok())
}

fn plan_lines(p: &Planned) -> (Vec<String>, Value) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut items = vec![];
    for d in &p.plan {
        *counts.entry(d.action.word()).or_default() += 1;
        let why = match &d.action {
            Action::NeedsApproval(w) | Action::Unsupported(w) => Some(w.clone()),
            _ => None,
        };
        items.push(json!({"item": d.item.as_str(), "action": d.action.word(), "why": why}));
    }
    // ids that parse are planned (as unsupported) above
    for (raw, why) in p
        .local
        .unsupported
        .iter()
        .filter(|(raw, _)| ItemId::parse(raw).is_err())
    {
        *counts.entry("unsupported").or_default() += 1;
        items.push(json!({"item": raw, "action": "unsupported", "why": why}));
    }
    let summary = counts
        .iter()
        .map(|(k, v)| format!("{v} {k}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut lines = vec![format!("mirror {}: {summary}", p.st.pair)];
    for d in p.plan.iter().filter(|d| d.action != Action::InSync).take(8) {
        lines.push(format!(
            "  {:<14} {}{}",
            d.action.word(),
            d.item,
            why_suffix(&d.action)
        ));
    }
    (
        lines,
        json!({"pair": p.st.pair, "snapshot": p.remote.snapshot, "items": items}),
    )
}

fn plan_cmd(a: &Args) -> Res<Out> {
    let c = load_config(a)?;
    let mut link = Link::open(&c, load_state(&c).ok().as_ref())?;
    let p = planned(&c, &mut link)?;
    let (lines, json) = plan_lines(&p);
    let differs = p.plan.iter().any(|d| !matches!(d.action, Action::InSync));
    Ok(Out {
        code: u8::from(differs),
        lines,
        json,
    })
}

/// `mirror sync --pair P [--allow-retire]`
fn sync_cmd(a: &Args) -> Res<Out> {
    let t0 = Instant::now();
    let c = load_config(a)?;
    // one run at a time per profile, from the first read of the state to its last write
    let _lock = OpLock::try_acquire(&c.profile_dir)?;
    let mut link = Link::open(&c, load_state(&c).ok().as_ref())?;
    let head0 = match &mut link.drive {
        Some(d) => d.head()?,
        None => None,
    };
    if let Some(h) = &head0 {
        if h.machine != c.machine && !a.has("take-over") {
            return err(format!(
                "HEAD was written by another machine ({}); one writer per pair (see docs)",
                h.machine
            ));
        }
    }
    let Planned {
        mut st,
        local,
        remote,
        mut plan,
        mut incoming,
    } = planned(&c, &mut link)?;
    if let Some(tp) = a.path("trigger") {
        // a run something on the host asked for: only with a fresh trigger signed with the pair key
        let key =
            pair_key(&c)?.ok_or("a triggered run needs a pair key (an unsigned pair, made with --no-pair-key, refuses triggers)")?;
        let tr: crate::core::auth::Trigger =
            serde_json::from_slice(&fs::read(&tp).map_err(|_| "trigger not readable".to_string())?)
                .map_err(|_| "trigger is not valid".to_string())?;
        tr.verify(&key, &c.pair, "sync", now_unix(), &mut st.nonces)?;
        save_state(&c, &st)?;
    }
    if !a.has("allow-retire") {
        if let Some(why) = engine::retire_guard(&plan, &st) {
            return err(format!(
                "{why}: stopping before any write; rerun with --allow-retire if that is intended"
            ));
        }
    }
    let now = utc_iso();
    let mut hs = hside(&c, &st);
    let writes_local = plan
        .iter()
        .any(|d| matches!(d.action, Action::ToLocal | Action::RetireLocal));
    let mut changed = 0u64;
    if writes_local {
        restore_point(&c)?;
    }
    for d in plan.iter_mut() {
        let outcome = match d.action.clone() {
            Action::ToLocal => {
                match write_local(&c, &mut hs, &mut st, d, incoming.remove(&d.item), &local) {
                    Ok(o) => {
                        if matches!(o, Outcome::Wrote { .. }) {
                            changed += 1;
                        }
                        o
                    }
                    Err(e) => {
                        d.action = Action::Unsupported(e);
                        Outcome::Skipped
                    }
                }
            }
            Action::RetireLocal => match d.local.as_ref().map(|v| hs.retire(&d.item, &v.digest)) {
                Some(Ok(true)) => {
                    changed += 1;
                    set_held(&mut hs, &mut st, &d.item, None);
                    Outcome::Retired
                }
                Some(Err(e)) => {
                    d.action = Action::Unsupported(e);
                    Outcome::Skipped
                }
                _ => Outcome::Skipped,
            },
            _ => Outcome::Kept,
        };
        if !matches!(d.action, Action::ToRemote | Action::RetireRemote) {
            st.record(d, outcome, &now);
        }
    }
    // what was written is recorded before anything else can fail
    st.remote_seq = remote_seq(&link).or(st.remote_seq);
    save_state(&c, &st)?;
    // the cumulative pack: every item in flight, plus this run's sends
    let sends: Vec<&Decision> = plan
        .iter()
        .filter(|d| matches!(d.action, Action::ToRemote | Action::RetireRemote))
        .collect();
    // republish when HEAD does not name the last pack the journal recorded, or an in-flight item (a pick) has not
    // been published yet
    let head_gen = head0.as_ref().map_or(0, |h| h.gen);
    let unpublished = st
        .items
        .values()
        .any(|e| !e.sent.is_empty() && (e.gen.unwrap_or(0) > head_gen || head_gen != st.gen));
    let mut sent_gen = None;
    // a pick that kept the Grok copy withdrew an item from the pack in flight: publish one that supersedes it
    if !sends.is_empty() || unpublished || st.republish {
        if link.drive.is_none() {
            write_report(&c, &st, &plan)?;
            return err("there are changes for the Grok side, but the transport cannot send (use drive or hybrid)");
        }
        let gen = st.gen.max(head0.as_ref().map_or(0, |h| h.gen)) + 1;
        for d in &sends {
            st.record(d, Outcome::Sent { gen }, &now);
            changed += 1;
        }
        let (pack, blobs) = build_pack(&c, &st, &local, gen, &now)?;
        st.gen = gen;
        // journal first: the state says what is being sent before HEAD moves
        save_state(&c, &st)?;
        let drive = link.drive.as_mut().ok_or("no Drive transport")?;
        drive.send_pack(&pack, &blobs, head0.as_ref())?;
        st.republish = false;
        sent_gen = Some(gen);
    }
    // the activity digests: never fatal (the setup sync above is what matters)
    if let Err(e) = exchange_digests(&c, &mut link) {
        eprintln!("botport: note: digests not exchanged this run ({e})");
    }
    st.remote_seq = remote_seq(&link).or(st.remote_seq);
    save_state(&c, &st)?;
    write_report(&c, &st, &plan)?;
    let in_flight = st.items.values().filter(|e| !e.sent.is_empty()).count() as u64;
    let io = link.io();
    let dio = link.drive_io();
    let wall_ms = t0.elapsed().as_millis() as u64;
    let row = metrics::Row {
        ts: now.clone(),
        pair: c.pair.clone(),
        cmd: "mirror-sync".into(),
        transport: c.transport.clone(),
        path: link.path(),
        items: plan.len() as u64,
        changed,
        conflicts: st.conflicts.len() as u64,
        in_flight,
        read_bytes: io.read_bytes,
        written_bytes: io.written_bytes,
        wall_ms,
        net_ms: io.net_ms,
        local_ms: wall_ms.saturating_sub(io.net_ms),
        local_tokens: 0,
        // what the Grok routine reads (written to Drive here) and wrote (read from Drive here), as an estimate
        remote_payload_tokens_est: link
            .drive
            .is_some()
            .then(|| metrics::payload_tokens(dio.read_bytes + dio.written_bytes)),
    };
    let mpath = a.path("metrics").unwrap_or_else(metrics::default_path);
    if let Err(e) = metrics::append(&mpath, &row) {
        eprintln!("botport: warning: metrics not written: {e}");
    }
    // changes held back, and routines held paused on the Hermes side, both wait for the owner
    let held = plan
        .iter()
        .filter(|d| matches!(d.action, Action::NeedsApproval(_)))
        .count()
        + st.held_paused.len();
    let not_carried = plan
        .iter()
        .filter(|d| matches!(d.action, Action::Unsupported(_)))
        .count();
    let mut lines = vec![format!(
        "mirror sync {}: {changed} changed, {} conflicts, {held} waiting for approval, {in_flight} in flight, {not_carried} not carried ({} ms, {})",
        c.pair,
        st.conflicts.len(),
        row.wall_ms,
        row.path
    )];
    if let Some(g) = sent_gen {
        lines.push(format!("  change pack gen {g} published for the Grok side"));
    }
    for d in plan
        .iter()
        .filter(|d| !matches!(d.action, Action::InSync | Action::InFlight))
        .take(8)
    {
        lines.push(format!(
            "  {:<14} {}{}",
            d.action.word(),
            d.item,
            why_suffix(&d.action)
        ));
    }
    let code = if !st.conflicts.is_empty() || held > 0 {
        3
    } else {
        0
    };
    Ok(Out {
        code,
        lines,
        json: json!({"pair": c.pair, "changed": changed, "conflicts": st.conflicts.len(), "waiting_for_approval": held,
                     "in_flight": in_flight, "not_carried": not_carried, "gen": sent_gen, "wall_ms": row.wall_ms, "path": row.path,
                     "snapshot": remote.snapshot,
                     "items": plan.iter().filter(|d| !matches!(d.action, Action::InSync)).map(|d| json!({
                         "item": d.item.as_str(), "action": d.action.word(), "why": action_why(&d.action)})).collect::<Vec<_>>()}),
    })
}

/// Send the Hermes digest to Drive, and take in the Grok one: built from the local records when there are any, else
/// the newest Grok digest on Drive that verifies with the pair key (none for a pair without a key), stored only if its
/// header names the Grok side of this pair.
fn exchange_digests(c: &Config, link: &mut Link) -> Res<()> {
    if let Some(drive) = link.drive.as_mut() {
        if let Ok(b) = fs::read(c.profile_dir.join("botport/digest-hermes.md")) {
            drive.t().send_digest(&b)?;
        }
    }
    if let Some(local) = link.local.as_mut() {
        if let Ok(Some(r)) = local.records() {
            let d = crate::core::digest::from_records(&r, "grok", &c.pair, &utc_iso());
            crate::core::digest::write_if_changed(
                &c.profile_dir,
                "botport/digest-grok.md",
                &d.render(),
            )?;
            return Ok(());
        }
    }
    if let Some(drive) = link.drive.as_mut() {
        if let Some(b) = drive.t().read_peer_digest()? {
            let text =
                std::str::from_utf8(&b).map_err(|_| "the Grok digest is not text".to_string())?;
            let d = crate::core::digest::parse(text)?;
            if d.side != "grok"
                || d.pair != c.pair
                || crate::core::util::parse_iso(&d.written).is_none()
            {
                return err("the Grok digest's header does not match this pair");
            }
            crate::core::digest::write_if_changed(
                &c.profile_dir,
                "botport/digest-grok.md",
                &d.render(),
            )?;
        }
    }
    Ok(())
}

/// Write one Grok copy into the Hermes profile (compare-and-swap on the live copy), check it reads back as the same
/// item, and record whether a routine is held paused. A routine arriving enabled where it was not running is written
/// held paused until `mirror approve`; a write never switches on a routine that was not running. On a failed check the
/// previous copy is put back the same way (or a new item removed) and the item is reported unsupported.
fn write_local(
    c: &Config,
    hs: &mut HermesSide,
    st: &mut State,
    d: &mut Decision,
    cn: Option<Content>,
    local: &LocalItems,
) -> Res<Outcome> {
    let cn = cn.ok_or("not fetched this run")?;
    // a connector's host is resolved and checked right before it is written (unless Hermes already has that url)
    if let Content::Connector(f) = &cn {
        if let Some(url) = f.get("url").and_then(|v| v.as_str()) {
            let have: std::collections::BTreeSet<String> = local
                .content
                .get(&d.item)
                .and_then(|c| match c {
                    Content::Connector(o) => {
                        o.get("url").and_then(|v| v.as_str()).map(String::from)
                    }
                    _ => None,
                })
                .into_iter()
                .collect();
            crate::adapters::connectors::check_resolved(
                url,
                &have,
                crate::adapters::connectors::system_resolve,
            )
            .map_err(|r| format!("{}: url refused ({r})", d.item))?;
        }
    }
    let expect = d.local.as_ref().map(|v| v.digest.clone());
    let was_running = d.item.kind() == Kind::Routine && hs.running(&d.item)?;
    let was_held = st.held_paused.get(&d.item).cloned();
    let paused = matches!(&cn, Content::Routine(s) if s.enabled) && !was_running;
    // the read-back must see the hold this write sets
    set_held(hs, st, &d.item, paused.then(String::new));
    let reread = match hs.write(&d.item, &cn, expect.as_deref(), paused) {
        Ok(Some(r)) => r,
        Ok(None) => {
            set_held(hs, st, &d.item, was_held);
            return Ok(Outcome::Skipped);
        }
        Err(e) => {
            set_held(hs, st, &d.item, was_held);
            return Err(e);
        }
    };
    if paused {
        set_held(hs, st, &d.item, Some(reread.clone()));
    }
    let now_v = hs.current(&d.item).ok().flatten();
    if now_v.as_ref().map(|v| &v.neutral) == d.remote.as_ref().map(|v| &v.neutral) {
        if let Err(e) = keep(&c.profile_dir, &d.item, &cn) {
            eprintln!("botport: note: {} not kept in the store ({e})", d.item);
        }
        return Ok(Outcome::Wrote { reread });
    }
    // the write changed the item's meaning: undo it as it was (never switching a routine on) and report it
    set_held(hs, st, &d.item, was_held.clone());
    match local.content.get(&d.item) {
        Some(old) => {
            // a hold that was there before follows the copy put back
            if let Ok(Some(r)) = hs.write(&d.item, old, Some(&reread), !was_running) {
                if was_held.is_some() {
                    set_held(hs, st, &d.item, Some(r));
                }
            }
        }
        None => {
            hs.retire(&d.item, &reread).ok();
        }
    }
    d.action = Action::Unsupported("the Hermes side cannot hold this copy as it is".into());
    Ok(Outcome::Skipped)
}

fn build_pack(
    c: &Config,
    st: &State,
    local: &LocalItems,
    gen: u64,
    now: &str,
) -> Res<(Pack, Blobs)> {
    let mut items = vec![];
    let mut blobs = vec![];
    for (id, e) in &st.items {
        let Some(want) = e.sent.last() else { continue };
        let expect = e.neutral_remote.clone().unwrap_or_else(|| ABSENT.into());
        if want == ABSENT {
            items.push(PackItem {
                item: id.clone(),
                action: PackAction::Retire,
                expect,
                want: want.clone(),
                blob: None,
            });
            continue;
        }
        let Some(cn) = local
            .content
            .get(id)
            .filter(|cn| cn.neutral_digest(id) == *want)
        else {
            continue; // the local copy moved on: the next plan sends the new one
        };
        if let Some(why) = secret_in(id, cn) {
            eprintln!("botport: note: {id} left out of the pack ({why})");
            continue;
        }
        let b = cn.to_blob(id);
        let sha = sha256_hex(&b);
        items.push(PackItem {
            item: id.clone(),
            action: PackAction::Put,
            expect,
            want: want.clone(),
            blob: Some(sha.clone()),
        });
        blobs.push((sha, b));
    }
    let mut p = Pack::new(&c.pair, gen, &random_id(), &c.machine, now, items)?;
    p.seal_with(pair_key(c)?.as_deref());
    Ok((p, blobs))
}

fn write_report(c: &Config, st: &State, plan: &[Decision]) -> Res<()> {
    let mut t = format!(
        "# botport mirror: {} (report; botport rewrites this file every run and never reads it back)\n\n",
        c.pair
    );
    if st.conflicts.is_empty()
        && st.held_paused.is_empty()
        && !plan
            .iter()
            .any(|d| matches!(d.action, Action::NeedsApproval(_)))
    {
        t.push_str("Nothing to pick or approve.\n");
    }
    let pair = shell_word(&c.pair);
    for id in st.held_paused.keys() {
        t.push_str(&format!(
            "## {id} is paused on the Hermes side until you approve it\nApprove: botport mirror approve --pair {pair} {}\n\n",
            shell_word(id.as_str())
        ));
    }
    for k in &st.conflicts {
        let id = shell_word(k.item.as_str());
        // an item on one side only (as at init): copy it over, or remove it where it is
        let only = match (k.local == ABSENT, k.remote == ABSENT) {
            (true, false) => Some(("grok", "hermes")),
            (false, true) => Some(("hermes", "grok")),
            _ => None,
        };
        // the Grok side is usually read from its export, which may leave items out
        let check = match (only, k.item.kind()) {
            (Some((_, "grok")), Kind::Persona) => {
                " (refused while the Grok side is read from its export: the bot has its own)"
            }
            (Some((_, "grok")), _) => " (only if the Grok bot has nothing by this name)",
            _ => "",
        };
        match only {
            Some((on, other)) => t.push_str(&format!(
                "## {}  (only on the {on} side, {} {})\nCompare: botport mirror show --pair {pair} {id}\nCopy it to {other}{check}: botport mirror pick --pair {pair} {id} --keep {on}\nRemove it from {on}: botport mirror pick --pair {pair} {id} --keep {other}\n\n",
                k.item,
                on,
                short(if on == "grok" { &k.remote } else { &k.local })
            )),
            None => t.push_str(&format!(
                "## {}  (both sides changed since {})\n- grok   {}\n- hermes {}\nCompare: botport mirror show --pair {pair} {id}\nKeep grok: botport mirror pick --pair {pair} {id} --keep grok\nKeep hermes: botport mirror pick --pair {pair} {id} --keep hermes\n\n",
                k.item, k.since, short(&k.remote), short(&k.local)
            )),
        }
    }
    for d in plan {
        if let Action::Unsupported(why) = &d.action {
            t.push_str(&format!(
                "## {} is not carried: {why}\nCompare: botport mirror show --pair {pair} {}\n\n",
                d.item,
                shell_word(d.item.as_str())
            ));
        }
    }
    for d in plan {
        if let Action::NeedsApproval(why) = &d.action {
            t.push_str(&format!(
                "## {} needs approval: {why}\nApprove: botport mirror approve --pair {pair} {}\n\n",
                d.item,
                shell_word(d.item.as_str())
            ));
        }
    }
    write_atomic(&c.profile_dir.join(REPORT_FILE), t.as_bytes(), Some(0o600))
}

/// A word for a command line the owner may copy: kept as is when it is plain, else single-quoted (an embedded quote
/// as `'\''`), so no part of an item id can ever run as a command.
fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '=' | '+' | ',')
        });
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The reason an action carries, if any.
fn action_why(a: &Action) -> Option<&str> {
    match a {
        Action::Unsupported(w) | Action::NeedsApproval(w) => Some(w),
        _ => None,
    }
}

fn why_suffix(a: &Action) -> String {
    action_why(a).map(|w| format!(" ({w})")).unwrap_or_default()
}

/// The short form of a digest shown to the owner: long enough to name one kept copy (`mirror restore --from`).
fn short(d: &str) -> &str {
    &d[..d.len().min(12)]
}

fn status(a: &Args) -> Res<Out> {
    let c = load_config(a)?;
    let st = load_state(&c)?;
    let mut link = Link::open(&c, Some(&st))?;
    let head = match &mut link.drive {
        Some(d) => d.head()?,
        None => None,
    };
    let in_flight: Vec<String> = st
        .items
        .iter()
        .filter(|(_, e)| !e.sent.is_empty())
        .map(|(k, _)| k.to_string())
        .collect();
    let mut lines = vec![format!(
        "mirror {}: {} items, {} conflicts, {} in flight, pack gen {} (HEAD {})",
        c.pair,
        st.items.len(),
        st.conflicts.len(),
        in_flight.len(),
        st.gen,
        head.as_ref()
            .map_or("none".into(), |h| format!("gen {} by {}", h.gen, h.machine))
    )];
    for k in st.conflicts.iter().take(6) {
        lines.push(format!("  conflict  {} (since {})", k.item, k.since));
    }
    let stale_head = head.as_ref().is_some_and(|h| h.gen != st.gen);
    if stale_head {
        lines.push("  WARN HEAD does not name the last pack this machine sent".into());
    }
    // a pack unapplied for longer than the timeout needs a look (is the Grok routine running?)
    let now = now_unix();
    let old: Vec<&ItemId> = st
        .items
        .iter()
        .filter(|(_, e)| !e.sent.is_empty())
        .filter(|(_, e)| {
            crate::core::util::parse_utc_iso(&e.at)
                .is_some_and(|t| now.saturating_sub(t) > IN_FLIGHT_TIMEOUT)
        })
        .map(|(k, _)| k)
        .collect();
    if !old.is_empty() {
        lines.push(format!(
            "  WARN {} item(s) in flight for over 7 days (is the Grok routine running?)",
            old.len()
        ));
    }
    if !st.held_paused.is_empty() {
        lines.push(format!(
            "  {} routine(s) paused on the Hermes side until you approve them (see the report)",
            st.held_paused.len()
        ));
    }
    if !st.approvals.is_empty() {
        lines.push(format!(
            "  {} approval(s) given, carried by the next sync",
            st.approvals.len()
        ));
    }
    let code = u8::from(!st.conflicts.is_empty() || stale_head || !old.is_empty());
    Ok(Out {
        code,
        lines,
        json: json!({"pair": c.pair, "conflicts": st.conflicts, "in_flight": in_flight, "gen": st.gen,
                     "head_gen": head.map(|h| h.gen)}),
    })
}

fn item_arg(a: &Args) -> Res<ItemId> {
    ItemId::parse(a.pos.get(2).ok_or("name the item, e.g. skill:demo-skill")?)
}

/// Text for a terminal: control characters other than newline and tab, and the bidirectional overrides, are shown
/// escaped (`\u{1b}`), so content from the other side cannot move the cursor, rewrite what was shown or hide text.
fn term_safe(t: &str) -> String {
    t.chars()
        .map(|c| {
            let bidi = matches!(
                c,
                '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{061c}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{feff}'
            );
            if (c.is_control() && c != '\n' && c != '\t') || bidi {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A package file that is not shown as text: its size and sha256 only.
fn binary_line(k: &str, b: &[u8]) -> String {
    format!(
        "{} (binary, {} bytes, sha256 {}; binary content is not scanned for secrets or instructions)",
        term_safe(k),
        b.len(),
        crate::core::util::sha256_hex(b)
    )
}

fn render(c: &Content) -> String {
    match c {
        Content::Text(t) => term_safe(t),
        Content::Package(p) => p
            .files
            .iter()
            .map(|(k, (b, x))| match std::str::from_utf8(b) {
                Ok(t) => format!(
                    "--- {}{}\n{}\n",
                    term_safe(k),
                    if *x { " (executable)" } else { "" },
                    term_safe(t)
                ),
                _ => format!("--- {}\n", binary_line(k, b)),
            })
            .collect(),
        Content::Routine(s) => term_safe(&serde_json::to_string_pretty(s).unwrap_or_default()),
        Content::Connector(m) => term_safe(&serde_json::to_string_pretty(m).unwrap_or_default()),
    }
}

/// `mirror show --pair P ITEM`: both copies, on the owner's terminal.
fn show(a: &Args) -> Res<Out> {
    let c = load_config(a)?;
    let id = item_arg(a)?;
    let mut link = Link::open(&c, load_state(&c).ok().as_ref())?;
    let remote = link.read()?.ok_or("nothing from the Grok side yet")?;
    let local = side(&c).read()?;
    let mut lines = vec![];
    match local.content.get(&id) {
        Some(cn) => lines.push(format!(
            "=== hermes {}\n{}",
            short(&local.read.items[&id].digest),
            render(cn)
        )),
        None => lines.push("=== hermes: absent".into()),
    }
    match remote.side.items.get(&id) {
        Some(v) => match link.fetch(&id, v) {
            Ok(cn) => lines.push(format!("=== grok {}\n{}", short(&v.digest), render(&cn))),
            Err(e) => lines.push(format!(
                "=== grok {}: content not available ({e})",
                short(&v.digest)
            )),
        },
        None => lines.push("=== grok: absent".into()),
    }
    Ok(Out {
        code: 0,
        lines,
        json: json!({"item": id.as_str()}),
    })
}

/// The mass-retire guard for a pick or approval that retires `id`: refused when the plan already trips it, unless
/// `--allow-retire`.
fn retire_check(a: &Args, plan: &[Decision], st: &State, id: &ItemId, on: Side) -> Res<()> {
    if a.has("allow-retire") {
        return Ok(());
    }
    // the plan with this retire counted
    let retire = if on == Side::Local {
        Action::RetireLocal
    } else {
        Action::RetireRemote
    };
    let mut with: Vec<Decision> = plan.iter().filter(|d| d.item != *id).cloned().collect();
    if let Some(d) = plan.iter().find(|d| d.item == *id) {
        let mut d = d.clone();
        d.action = retire;
        with.push(d);
    }
    match engine::retire_guard(&with, st) {
        Some(why) => err(format!(
            "{why}, and this would retire {id} as well: stopping; rerun with --allow-retire if that is intended"
        )),
        None => Ok(()),
    }
}

fn confirm(a: &Args, what: &str) -> Res<()> {
    if a.has("yes") {
        return Ok(());
    }
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return err("confirmation needs a terminal (or pass --yes)");
    }
    eprint!("{what} [y/N] ");
    std::io::stderr().flush().ok();
    let mut l = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut l)
        .map_err(|e| e.to_string())?;
    if l.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        err("not confirmed")
    }
}

/// `mirror pick --pair P ITEM --keep grok|hermes [--yes]`
fn pick(a: &Args) -> Res<Out> {
    let c = load_config(a)?;
    let id = item_arg(a)?;
    let keep_side = match a.get("keep") {
        Some("grok") => Side::Remote,
        Some("hermes") => Side::Local,
        _ => return err("--keep grok|hermes is required"),
    };
    let _lock = OpLock::try_acquire(&c.profile_dir)?;
    let mut st = load_state(&c)?;
    let k = st
        .conflicts
        .iter()
        .find(|k| k.item == id)
        .cloned()
        .ok_or_else(|| format!("{id} has no open conflict"))?;
    let mut link = Link::open(&c, Some(&st))?;
    let remote = link.read()?.ok_or("nothing from the Grok side yet")?;
    let local = side(&c).read()?;
    // absent from a Grok read that may leave items out is not absent: keeping that "copy" would retire the Hermes one
    if keep_side == Side::Remote
        && !remote.side.items.contains_key(&id)
        && !remote.side.sees_all(id.kind())
    {
        return err(format!(
            "{id}: the Grok side is read from its export, which may leave items out; remove it on the Hermes side by hand if that is what you want"
        ));
    }
    // an item a side cannot offer looks absent there, but is not: no pick until it can be read again
    if local.read.unsupported.contains_key(&id) || remote.side.unsupported.contains_key(&id) {
        return err(format!(
            "{id}: one side cannot offer it as it is (see mirror plan); pick again once it can"
        ));
    }
    let (lv, rv) = (local.read.items.get(&id), remote.side.items.get(&id));
    let grok_copy = rv.and_then(|v| link.fetch(&id, v).ok());
    // what the 4.5 rules would hold back is shown before the owner confirms: the pick is their approval
    let (to_side, from, to) = match keep_side {
        Side::Remote => (
            Side::Local,
            grok_copy.clone(),
            local.content.get(&id).cloned(),
        ),
        Side::Local => (
            Side::Remote,
            local.content.get(&id).cloned(),
            grok_copy.clone(),
        ),
    };
    if keep_side == Side::Local {
        if let Some(why) = persona_left_out(&remote.side, &id) {
            return err(format!("{id}: {why}"));
        }
    }
    // keeping an absent copy retires the other one: the mass-retire guard applies, as in sync
    if from.is_none() {
        let plan = engine::plan(&st, &local.read, &remote.side, &[]);
        retire_check(a, &plan, &st, &id, to_side)?;
    }
    if let Some(why) = unsigned_package(pair_key(&c)?.is_some(), &id, &Action::ToLocal) {
        return err(format!("{id}: {why}"));
    }
    let add = if keep_side == Side::Local && rv.is_none() && !remote.side.sees_all(id.kind()) {
        format!(" Note: {ADD_TO_EXPORT_SIDE}.")
    } else {
        String::new()
    };
    let note = needs_approval(
        &c.approve,
        &id,
        to_side,
        from.as_ref(),
        to.as_ref(),
        approved_instructions(&c.profile_dir).as_deref(),
    )
    .map(|w| format!("{add} Note: {w}."))
    .unwrap_or(add);
    confirm(
        a,
        &format!(
            "keep the {} copy of {id} (grok {}, hermes {})?{note}",
            a.get("keep").unwrap(),
            short(&k.remote),
            short(&k.local)
        ),
    )?;
    let d = engine::pick(&st, &id, keep_side, lv, rv, (&k.local, &k.remote))?;
    let now = utc_iso();
    match d.action {
        Action::ToLocal => {
            let cn = grok_copy
                .clone()
                .ok_or("the Grok copy could not be read; try again after a sync")?;
            if let Some(why) = secret_in(&id, &cn).or_else(|| not_a_name(&id, &cn)) {
                return err(format!("{id}: {why}; not written"));
            }
            restore_point(&c)?;
            // the same write as a sync: a routine that was not running stays paused until `mirror approve`
            let mut hs = hside(&c, &st);
            let mut d = d;
            match write_local(&c, &mut hs, &mut st, &mut d, Some(cn), &local)? {
                Outcome::Wrote { reread } => st.record(&d, Outcome::Wrote { reread }, &now),
                _ => {
                    return err(format!(
                        "{id}: not written ({}); pick again after a sync",
                        d.action.word()
                    ))
                }
            }
        }
        Action::RetireLocal => {
            restore_point(&c)?;
            let mut hs = hside(&c, &st);
            if !hs.retire(&id, &lv.ok_or("nothing to retire")?.digest)? {
                return err("the Hermes copy changed meanwhile; pick again");
            }
            set_held(&mut hs, &mut st, &id, None);
            st.record(&d, Outcome::Retired, &now);
        }
        // a Grok-bound pick goes out with the next sync's pack (and is in flight from now)
        Action::ToRemote | Action::RetireRemote => {
            if let Some(why) = add_only(
                pair_key(&c)?.is_some(),
                &remote.side,
                id.kind(),
                &d.action,
                rv.is_some(),
            ) {
                return err(format!("{id}: {why}"));
            }

            if let Some(why) = local.content.get(&id).and_then(|cn| secret_in(&id, cn)) {
                return err(format!("{id}: {why}; not sent"));
            }
            if let Some(why) = local
                .content
                .get(&id)
                .and_then(|cn| url_refused(cn, grok_copy.as_ref()))
            {
                return err(format!("{id}: {why}; not sent"));
            }
            if let Some(cn) = &grok_copy {
                keep(&c.profile_dir, &id, cn)?;
            }
            let gen = st.gen;
            st.record(&d, Outcome::Sent { gen: gen + 1 }, &now);
            st.gen = gen; // not published yet: the next sync publishes a new generation
        }
        _ => {}
    }
    // the owner's pick of the instructions is also their approval of that text (once it is written)
    if id.as_str() == "persona.instructions" {
        let kept_text = match keep_side {
            Side::Local => local.content.get(&id).cloned(),
            Side::Remote => grok_copy,
        };
        if let Some(Content::Text(t)) = kept_text {
            set_approved_instructions(&c.profile_dir, &t)?;
        }
    }
    save_state(&c, &st)?;
    write_report(&c, &st, &[])?;
    Ok(Out {
        code: 0,
        lines: vec![format!(
            "picked the {} copy of {id}; run botport mirror sync --pair {}",
            a.get("keep").unwrap(),
            shell_word(&c.pair)
        )],
        json: json!({"item": id.as_str(), "keep": a.get("keep")}),
    })
}

/// `mirror approve --pair P ITEM [--yes]`: approve the change currently waiting for `ITEM`, bound to its digest; or
/// switch on a routine botport wrote paused awaiting approval.
fn approve(a: &Args) -> Res<Out> {
    let c = load_config(a)?;
    let id = item_arg(a)?;
    let _lock = OpLock::try_acquire(&c.profile_dir)?;
    let mut link = Link::open(&c, load_state(&c).ok().as_ref())?;
    let p = planned(&c, &mut link)?;
    let d = p
        .plan
        .iter()
        .find(|d| d.item == id)
        .ok_or_else(|| format!("{id} is not in the plan"))?;
    let mut hs = hside(&c, &p.st);
    let Action::NeedsApproval(why) = &d.action else {
        // a routine held paused on the Hermes side: approving it switches it on, as it was shown
        if id.kind() == Kind::Routine && hs.paused_by_policy(&id)? {
            // only a routine in step: a pause or change on its way from the Grok side comes first
            if d.action != Action::InSync {
                return err(format!(
                    "{id} is held paused but not in step ({}); run mirror sync, then approve",
                    d.action.word()
                ));
            }
            let shown = d
                .local
                .as_ref()
                .map(|v| v.neutral.clone())
                .unwrap_or_default();
            confirm(
                a,
                &format!("switch on {id} ({}) on the Hermes side?", short(&shown)),
            )?;
            if hs.current(&id)?.map(|v| v.neutral) != Some(shown) {
                return err(format!(
                    "{id} changed since it was shown; run mirror plan and approve again"
                ));
            }
            restore_point(&c)?;
            let reread = hs.resume(&id)?;
            let mut st = p.st.clone();
            set_held(&mut hs, &mut st, &id, None);
            if let Some(e) = st.items.get_mut(&id) {
                e.base_local = Some(reread);
            }
            save_state(&c, &st)?;
            write_report(&c, &st, &[])?;
            return Ok(Out {
                code: 0,
                lines: vec![format!("{id} switched on on the Hermes side")],
                json: json!({"item": id.as_str(), "resumed": true}),
            });
        }
        return err(format!(
            "{id} is not waiting for approval ({})",
            d.action.word()
        ));
    };
    let to = d.held.ok_or("no direction recorded for the held change")?;
    let v = if to == Side::Local {
        &d.remote
    } else {
        &d.local
    };
    let digest = v.as_ref().map_or(ABSENT.to_string(), |v| v.neutral.clone());
    // approving a retire: the mass-retire guard applies, as in sync
    if digest == ABSENT {
        retire_check(a, &p.plan, &p.st, &id, to)?;
    }
    // a package's files that are not text are listed with size and sha256 before the owner says yes
    let content = match (to, v) {
        (Side::Local, Some(v)) => link.fetch(&id, v).ok(),
        (_, Some(_)) => side(&c).read()?.content.get(&id).cloned(),
        _ => None,
    };
    let binaries: String = match &content {
        Some(Content::Package(p)) => p
            .files
            .iter()
            .filter(|(_, (b, _))| std::str::from_utf8(b).is_err())
            .map(|(k, (b, _))| format!("\n  {}", binary_line(k, b)))
            .collect(),
        _ => String::new(),
    };
    confirm(
        a,
        &format!(
            "approve {id} ({}), copy {} to the {} side?{binaries}",
            term_safe(why),
            short(&digest),
            if to == Side::Local { "Hermes" } else { "Grok" }
        ),
    )?;
    let mut st = p.st.clone();
    st.approvals.retain(|x| x.item != id);
    st.approvals.push(Approval {
        item: id.clone(),
        to,
        digest,
    });
    if id.as_str() == "persona.instructions" {
        let text = match to {
            Side::Remote => p.local.content.get(&id).cloned(),
            Side::Local => p.incoming.get(&id).cloned(),
        };
        if let Some(Content::Text(t)) = text {
            set_approved_instructions(&c.profile_dir, &t)?;
        }
    }
    save_state(&c, &st)?;
    Ok(Out {
        code: 0,
        lines: vec![format!(
            "approved {id}; it is carried by the next botport mirror sync --pair {}",
            shell_word(&c.pair)
        )],
        json: json!({"item": id.as_str(), "to": to}),
    })
}

/// `mirror restore --pair P ITEM --from SHORT`: write a kept copy back to the Hermes side (the next sync carries it
/// on to the Grok side like any other Hermes change).
fn restore(a: &Args) -> Res<Out> {
    let c = load_config(a)?;
    let id = item_arg(a)?;
    let short_d = a
        .get("from")
        .ok_or("--from SHORT_DIGEST is required (see the store)")?;
    let _lock = OpLock::try_acquire(&c.profile_dir)?;
    let cn = kept(&c.profile_dir, &id, short_d)?;
    confirm(
        a,
        &format!("write the kept copy {short_d} of {id} to the Hermes side?"),
    )?;
    let mut st = load_state(&c)?;
    let mut hs = hside(&c, &st);
    let cur = hs.current(&id)?;
    restore_point(&c)?;
    // a restored routine is held paused until `mirror approve`, like any routine that arrives enabled
    let held = matches!(&cn, Content::Routine(s) if s.enabled) && !hs.running(&id)?;
    set_held(&mut hs, &mut st, &id, held.then(String::new));
    // a running routine restored as enabled keeps running; one that was not running is written paused and held
    let reread = hs
        .write(&id, &cn, cur.as_ref().map(|v| v.digest.as_str()), held)?
        .ok_or("the Hermes copy changed meanwhile")?;
    set_held(&mut hs, &mut st, &id, held.then_some(reread));
    save_state(&c, &st)?;
    Ok(Out {
        code: 0,
        lines: vec![format!(
            "restored {id} from {short_d} on the Hermes side; run mirror sync to carry it on"
        )],
        json: json!({"item": id.as_str()}),
    })
}

/// `mirror stats [--since 7d] [--metrics FILE]`: count and p50/p95 time and payload per command and transport.
fn stats(a: &Args) -> Res<Out> {
    let file = a.path("metrics").unwrap_or_else(metrics::default_path);
    let since = match a.get("since") {
        Some(s) => Some(now_unix().saturating_sub(metrics::parse_since(s)?)),
        None => None,
    };
    let rows = metrics::read(&file)?;
    let groups = metrics::stats(&rows, since);
    let mut lines: Vec<String> = metrics::render(&groups).lines().map(String::from).collect();
    if groups.is_empty() {
        lines = vec![format!("no mirror runs recorded in {}", file.display())];
    }
    let json = json!({
        "file": file.display().to_string(),
        "groups": groups.iter().map(|g| json!({
                        "cmd": g.cmd, "transport": g.transport, "runs": g.runs, "p50_ms": g.p50_ms, "p95_ms": g.p95_ms,
            "p50_local_ms": g.p50_local_ms, "p95_local_ms": g.p95_local_ms,
            "p50_payload_tokens_est": g.p50_tokens, "p95_payload_tokens_est": g.p95_tokens,
            "local_tokens": g.local_tokens,
        })).collect::<Vec<_>>(),
    });
    Ok(Out {
        code: 0,
        lines,
        json,
    })
}

/// `mirror digest-items --pair P --from EXPORT --out DIR [--log FILE] [--all]` (in the Grok box).
fn digest_items(a: &Args) -> Res<Out> {
    let pair = a.get("pair").ok_or("--pair NAME is required")?;
    let from = a.path("from").ok_or("--from EXPORT is required")?;
    let out = a.path("out").ok_or("--out DIR is required")?;
    let mut items = gmirror::read_items(&from)?;
    // an export is written by the bot itself and may leave items out: partial unless a program read the live setup
    items.partial = match a.get("coverage") {
        None | Some("partial") => true,
        Some("complete") => false,
        Some(_) => return err("--coverage must be partial or complete"),
    };
    let log = a
        .path("log")
        .map(|p| gmirror::read_log(&p))
        .unwrap_or_default();
    let key = a.path("key").map(|p| read_key_file(&p)).transpose()?;
    let f = gmirror::digest_items(
        &items,
        &out,
        pair,
        &log,
        &utc_iso(),
        a.has("all"),
        key.as_deref(),
    )?;
    let with = f.items.values().filter(|e| e.blob.is_some()).count();
    Ok(Out {
        code: 0,
        lines: vec![format!(
            "state-{}.json: {} items ({with} with content), {} not mirrored",
            f.n,
            f.items.len(),
            items.unsupported.len()
        )],
        json: json!({"n": f.n, "items": f.items.len(), "with_content": with, "unsupported": items.unsupported.keys().collect::<Vec<_>>()}),
    })
}

/// `mirror apply-pack --pair P --dir PAIRDIR --from EXPORT --stage DIR [--log FILE] [--check] [--precheck ITEM --gen N]`
fn apply_pack(a: &Args) -> Res<Out> {
    let pair = a.get("pair").ok_or("--pair NAME is required")?;
    let dir = a
        .path("dir")
        .ok_or("--dir PAIRDIR (a copy of the pair's Drive folder) is required")?;
    let from = a.path("from").ok_or("--from EXPORT is required")?;
    let live = gmirror::read_items(&from)?;
    let key = a.path("key").map(|p| read_key_file(&p)).transpose()?;
    if let Some(item) = a.get("precheck") {
        let gen: u64 = a
            .get("gen")
            .and_then(|g| g.parse().ok())
            .ok_or("--precheck needs --gen N")?;
        gmirror::precheck(
            &dir,
            pair,
            gen,
            &live,
            &ItemId::parse(item)?,
            key.as_deref(),
            a.path("stage").as_deref(),
        )?;
        return Ok(Out {
            code: 0,
            lines: vec![format!("ok: install {item}")],
            json: json!({"ok": true}),
        });
    }
    let stage = a.path("stage").ok_or("--stage DIR is required")?;
    let logp = a.path("log");
    let log = logp.as_deref().map(gmirror::read_log).unwrap_or_default();
    let gmirror::PackRun {
        pack: p,
        steps,
        added,
    } = gmirror::apply_pack(&dir, pair, &live, &stage, &log, key.as_deref())?;
    // only a pack signed with the pair key is ever logged (apply_pack holds every item of an unsigned one)
    if let (Some(lp), Some(_)) = (&logp, &key) {
        gmirror::append_log(lp, &added)?;
    }
    let pending = steps
        .iter()
        .filter(|(_, s)| !matches!(s, gmirror::Step::Done))
        .count();
    let mut lines = vec![format!(
        "pack gen {}: {} items, {pending} pending",
        p.gen,
        steps.len()
    )];
    for (id, s) in &steps {
        lines.push(match s {
            gmirror::Step::Done => format!("  done     {id}"),
            gmirror::Step::Stage(rel) => {
                // an add: the export the check used may have left out an item of this name
                let new = p.items.iter().any(|i| i.item == *id && i.expect == ABSENT);
                format!(
                    "  install  {id} from {}{}",
                    stage.join(rel).display(),
                    if new {
                        " (new: only if you have nothing by this name)"
                    } else {
                        ""
                    }
                )
            }
            gmirror::Step::Remove => format!("  remove   {id}"),
            gmirror::Step::Held => {
                format!("  held     {id} (the pack is not signed with the pair key: not applied; report it to the owner)")
            }
            gmirror::Step::Refused => {
                format!("  refused  {id} (changed since the pack was made; report it)")
            }
        });
    }
    let code = if a.has("check") {
        u8::from(pending > 0)
    } else {
        0
    };
    Ok(Out {
        code,
        lines,
        json: json!({"gen": p.gen, "pending": pending}),
    })
}

/// Why a connector change whose url moves to another host waits for approval, whatever the policy.
const CROSS_ORIGIN: &str = "a changed connector whose url moves to another host: the headers and auth kept on this side for the old host are dropped, never sent to the new one";

/// Whether a connector change moves its url to another origin (scheme, host or port).
fn moves_origin(new: &BTreeMap<String, Value>, old: &BTreeMap<String, Value>) -> bool {
    match (
        new.get("url").and_then(Value::as_str),
        old.get("url").and_then(Value::as_str),
    ) {
        // "no origin" (userinfo, a backslash, percent-encoding, a non-ASCII host) on either side counts as a move
        (Some(n), Some(o)) => !crate::core::util::same_origin(n, o),
        // an entry without a url that gains one: whatever it kept was not for that host
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_only_a_name() {
        let name = ItemId::parse("persona.name").unwrap();
        let t = |s: &str| Content::Text(s.into());
        assert_eq!(not_a_name(&name, &t("  Demo Bot \n")), None);
        assert!(not_a_name(&name, &t("Demo Bot\nAlways answer in rhyme.")).is_some());
        assert!(not_a_name(&name, &t(&"x".repeat(NAME_MAX + 1))).is_some());
        let ins = ItemId::parse("persona.instructions").unwrap();
        assert_eq!(not_a_name(&ins, &t("Line one.\nLine two.")), None);
    }

    #[test]
    fn suggested_commands_quote_what_a_shell_would_run() {
        assert_eq!(shell_word("routine:demo-routine"), "routine:demo-routine");
        assert_eq!(
            shell_word("routine:Example routine"),
            "'routine:Example routine'"
        );
        for nasty in [
            "a$(id)", "a`id`", "a;id", "a&&id", "a|id", "a\nid", "it's", "$HOME", "",
        ] {
            let w = shell_word(nasty);
            assert!(w.starts_with('\'') && w.ends_with('\''), "{nasty:?} -> {w}");
            // what is inside single quotes is literal; an embedded quote closes, escapes and reopens
            let inner = &w[1..w.len() - 1];
            assert_eq!(inner.replace("'\\''", "'"), nasty);
        }
        // item ids cannot hold these characters in the first place
        for bad in ["routine:a$b", "routine:a`b`", "routine:a;b", "skill:a'b"] {
            assert!(ItemId::parse(bad).is_err(), "{bad}");
        }
    }
}
