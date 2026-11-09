//! Opaque packages (`botport-package/1`): a skill or custom tool carried byte for byte as a directory tree, never
//! parsed or rewritten. Spec: docs/design/MIRROR.md section 11.
//!
//! A package is the regular files under one directory, each with its bytes and executable bit. Its digest is sha256
//! over the sorted `path\0mode\0sha256\n` lines (`mode` is `x` or `-`), so two copies are the same package exactly when
//! every path, byte and executable bit matches. Empty directories are not part of a package. The manifest
//! ([`MANIFEST`], generated) sits at the package root and is never part of the digest.
//!
//! ```
//! use botport::core::package::Package;
//! use std::collections::BTreeMap;
//!
//! let mut files = BTreeMap::new();
//! files.insert("SKILL.md".to_string(), (b"# Demo skill\n".to_vec(), false));
//! let p = Package::from_files("demo-skill", files).unwrap();
//! assert_eq!(p.manifest().files.len(), 1);
//! assert_eq!(p.digest().len(), 64);
//! ```
#![warn(missing_docs)]

use super::util::{err, plain_name, rel_str, safe_rel, sha256_hex, write_atomic, Res};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Schema of the manifest.
pub const SCHEMA: &str = "botport-package/1";
/// The generated manifest's file name at the package root; skipped when a package is read.
pub const MANIFEST: &str = ".botport-package.json";

/// Size limits a package must stay within; anything larger is refused, not cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest single file, in bytes.
    pub max_file: u64,
    /// Largest package (sum of file sizes), in bytes.
    pub max_total: u64,
    /// Most files in one package.
    pub max_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file: 10 << 20,
            max_total: 50 << 20,
            max_files: 5000,
        }
    }
}

/// One file of a package as the manifest lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative path, `/`-separated.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Lowercase hex sha256 of the bytes.
    pub sha256: String,
    /// The executable bit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exec: bool,
}

/// The manifest: what a receiver checks a package against before it is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// The package name (a plain name: its directory name).
    pub name: String,
    /// The package digest.
    pub digest: String,
    /// Every file, sorted by path.
    pub files: Vec<Entry>,
}

/// A package in memory: relative path -> (bytes, executable bit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// The package name.
    pub name: String,
    /// Relative path -> (bytes, executable bit).
    pub files: BTreeMap<String, (Vec<u8>, bool)>,
}

impl Package {
    /// A package from files already in memory; checks the name and every path, not the limits.
    pub fn from_files(name: &str, files: BTreeMap<String, (Vec<u8>, bool)>) -> Res<Package> {
        plain_name("package name", name)?;
        for rel in files.keys() {
            safe_rel("package file", rel)?;
            if rel == MANIFEST {
                return err(format!("package {name}: {MANIFEST} is reserved"));
            }
        }
        no_collisions(name, files.keys())?;
        Ok(Package {
            name: name.to_string(),
            files,
        })
    }

    /// Read the package at `dir`. Refused (with the first offending relative path, never file content): a symlink
    /// or any entry that is neither a directory nor a regular file, a limit passed, an unreadable file.
    pub fn read(dir: &Path, name: &str, lim: &Limits) -> Res<Package> {
        plain_name("package name", name)?;
        let md = fs::symlink_metadata(dir).map_err(|e| format!("package {name}: {e}"))?;
        if !md.is_dir() {
            return err(format!("package {name}: not a directory (or a symlink)"));
        }
        // parent directories may be symlinks (a temp dir often is); the root and everything below may not
        let root = dir
            .canonicalize()
            .map_err(|e| format!("package {name}: {e}"))?;
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        walk(&root, &root, name, lim, &mut files, &mut total, &mut 0)?;
        no_collisions(name, files.keys())?;
        Ok(Package {
            name: name.to_string(),
            files,
        })
    }

    /// The digest the two sides compare (the item's neutral form): as [`Package::digest`], except that a root
    /// `SKILL.md` counts by its [`skill_md_neutral`] text, so formatting a platform's export adds on the way (quoted
    /// front-matter values, blank lines, trailing spaces) is not a change. Any other difference still is.
    pub fn neutral_digest(&self) -> String {
        let lines: String = self
            .entries()
            .iter()
            .map(|e| {
                let sha = match self.files.get(&e.path) {
                    // a SKILL.md that is not UTF-8 counts by its bytes: a lossy reading could make two files equal
                    Some((b, _)) if e.path == "SKILL.md" => match std::str::from_utf8(b) {
                        Ok(t) => sha256_hex(skill_md_neutral(t).as_bytes()),
                        Err(_) => e.sha256.clone(),
                    },
                    _ => e.sha256.clone(),
                };
                format!("{}\0{}\0{}\n", e.path, if e.exec { "x" } else { "-" }, sha)
            })
            .collect();
        sha256_hex(lines.as_bytes())
    }

    /// The package digest (see the module docs).
    pub fn digest(&self) -> String {
        let lines: String = self
            .entries()
            .iter()
            .map(|e| {
                format!(
                    "{}\0{}\0{}\n",
                    e.path,
                    if e.exec { "x" } else { "-" },
                    e.sha256
                )
            })
            .collect();
        sha256_hex(lines.as_bytes())
    }

    /// The manifest entries, sorted by path.
    pub fn entries(&self) -> Vec<Entry> {
        self.files
            .iter()
            .map(|(path, (b, exec))| Entry {
                path: path.clone(),
                size: b.len() as u64,
                sha256: sha256_hex(b),
                exec: *exec,
            })
            .collect()
    }

    /// The manifest.
    pub fn manifest(&self) -> Manifest {
        Manifest {
            schema: SCHEMA.into(),
            name: self.name.clone(),
            digest: self.digest(),
            files: self.entries(),
        }
    }

    /// The first file whose text looks like it holds a secret, as "<path>: <where>" (never the value); binary files
    /// are not text and are not checked. A package with a hit is not carried (the caller lists it as skipped).
    pub fn secret_at(&self) -> Option<String> {
        self.files.iter().find_map(|(rel, (b, _))| {
            super::secrets::find_secret(rel, b).map(|at| format!("{rel}: {at}"))
        })
    }

    /// Check the package against a manifest (name, file set, sizes, hashes, executable bits, digest).
    pub fn check(&self, m: &Manifest) -> Res<()> {
        if m.schema != SCHEMA {
            return err(format!(
                "package {}: unknown manifest schema {:?}",
                self.name, m.schema
            ));
        }
        if m.name != self.name {
            return err(format!(
                "package {}: manifest names {:?}",
                self.name, m.name
            ));
        }
        let mine = self.entries();
        if mine != m.files {
            let at = mine
                .iter()
                .zip(m.files.iter())
                .find(|(a, b)| a != b)
                .map(|(a, _)| a.path.clone())
                .or_else(|| mine.get(m.files.len()).map(|e| e.path.clone()))
                .or_else(|| m.files.get(mine.len()).map(|e| e.path.clone()))
                .unwrap_or_default();
            return err(format!(
                "package {}: does not match its manifest at {at:?}",
                self.name
            ));
        }
        if m.digest != self.digest() {
            return err(format!(
                "package {}: digest does not match its manifest",
                self.name
            ));
        }
        Ok(())
    }

    /// Write the package as `parent/<name>`, replacing what is there only once the new copy is complete and checked.
    /// Files go to a temp dir beside it (text through the secret write guard), are read back and checked against
    /// this package, then swapped in; on any failure the old copy stays as it was. A crash between the two renames of
    /// the swap leaves the old copy as `.botport-pkg-old.<name>`; [`recover`] (run by every write and by readers of
    /// a package folder) puts it back. With `manifest`, the manifest is written at the package root too. Mode: 0700
    /// for executable files, 0600 otherwise, inside a 0700 package dir. For a folder a platform loads skills from, use
    /// [`Package::write_via`] so nothing is staged inside it.
    pub fn write(&self, parent: &Path, manifest: bool) -> Res<()> {
        self.write_via(parent, parent, manifest)
    }

    /// [`Package::write`], with the new copy assembled (and the old one set aside) in `stage` instead of beside it, so
    /// nothing half-written ever sits in the live folder. `stage` must be on the same filesystem as `parent` (refused
    /// otherwise) and serve only this `parent`; [`recover_into`] puts back what a crash left there.
    pub fn write_via(&self, parent: &Path, stage: &Path, manifest: bool) -> Res<()> {
        // the fields are public: check the paths again before any of them is joined to a directory
        Package::from_files(&self.name, self.files.clone())?;
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let stage = if canon(stage) == canon(parent) {
            parent
        } else {
            super::util::ensure_private_dir(stage)?;
            stage_on_device(stage, parent, same_device(stage, parent))?
        };
        recover_into(stage, parent)?;
        let dest = parent.join(&self.name);
        if fs::symlink_metadata(&dest).is_ok_and(|m| !m.is_dir()) {
            return err(format!(
                "package {}: the destination is not a directory (or is a symlink)",
                self.name
            ));
        }
        let tmp = fresh_dir(stage, "new")?;
        let staged = (|| -> Res<()> {
            for (rel, (b, exec)) in &self.files {
                write_atomic(&tmp.join(rel), b, Some(if *exec { 0o700 } else { 0o600 }))?;
            }
            let back = Package::read(
                &tmp,
                &self.name,
                &Limits {
                    max_file: u64::MAX,
                    max_total: u64::MAX,
                    max_files: usize::MAX,
                },
            )?;
            if back != *self {
                return err(format!(
                    "package {}: the written copy differs from the package",
                    self.name
                ));
            }
            if manifest {
                write_atomic(
                    &tmp.join(MANIFEST),
                    super::util::json_sorted(&self.manifest()).as_bytes(),
                    Some(0o600),
                )?;
            }
            Ok(())
        })();
        if let Err(e) = staged {
            fs::remove_dir_all(&tmp).ok();
            return Err(e);
        }
        let old = if dest.exists() {
            let old = stage.join(format!(".botport-pkg-old.{}", self.name));
            if fs::symlink_metadata(&old).is_ok() {
                fs::remove_dir_all(&old).map_err(|e| format!("remove {}: {e}", old.display()))?;
            }
            if let Err(e) = fs::rename(&dest, &old) {
                fs::remove_dir_all(&tmp).ok();
                return err(format!("rename {}: {e}", dest.display()));
            }
            Some(old)
        } else {
            None
        };
        if let Err(e) = fs::rename(&tmp, &dest) {
            if let Some(old) = &old {
                fs::rename(old, &dest).ok();
            }
            fs::remove_dir_all(&tmp).ok();
            return err(format!("rename into {}: {e}", dest.display()));
        }
        if let Some(old) = old {
            fs::remove_dir_all(&old).ok();
        }
        Ok(())
    }
}

/// The version of the neutral form ([`Package::neutral_digest`], [`skill_md_neutral`]). State files and change packs
/// carry it, and a side refuses a file written with another one: the two sides' botport must agree on what counts as
/// the same copy.
pub const NEUTRAL_FORM: u32 = 2;

/// Version 1 (no `neutral_form` field in a file).
pub fn neutral_form_v1() -> u32 {
    1
}

/// The words every neutral-form refusal carries (a reader that skips unreadable files reports this one instead).
pub const NEUTRAL_FORM_MISMATCH: &str = "compares copies another way";

/// Refuses a state file or pack (`b`, its bytes) written with another neutral form `v`, but only when the file is
/// intact and signed with the pair key: a damaged, forged or (keyless pair) unverifiable file is refused as such, so a
/// reader skips it like any other bad file instead of stopping on it. The checks run on the raw JSON, so they hold for
/// a file whose release had fields this one lacks or the other way round.
pub fn check_form_of(
    b: &[u8],
    what: &str,
    v: u32,
    key: Option<&[u8]>,
) -> crate::core::util::Res<()> {
    if v == NEUTRAL_FORM {
        return Ok(());
    }
    // without a key the only check is the file's own sha256, which anyone who can write the folder can make: such a
    // file is no reason to stop a reader (or republish), only one more file to skip
    let Some(k) = key else {
        return Err(format!(
            "{what} is in another neutral form ({v}) and the pair has no key to verify it"
        ));
    };
    intact_and_signed(b, what, Some(k))?;
    check_neutral_form(what, v)
}

/// Whether `b`, a state file or pack written in neutral form `v`, is in another form than this release's and is
/// intact and signed with the pair key: the one case a reader stops on instead of skipping the file (a keyless pair
/// never stops: see [`check_form_of`]). A typed answer, never read from an error's text (which can quote the file).
pub fn verified_other_form(b: &[u8], v: u32, key: Option<&[u8]>) -> bool {
    v != NEUTRAL_FORM && key.is_some() && intact_and_signed(b, "the file", key).is_ok()
}

/// `b` matches its own sha256 (the field left empty, `mac` left out) and, with a pair key, carries a valid `mac`.
fn intact_and_signed(b: &[u8], what: &str, key: Option<&[u8]>) -> crate::core::util::Res<()> {
    let raw: serde_json::Value =
        serde_json::from_slice(b).map_err(|e| super::util::json_invalid(what, &e))?;
    let mut c = raw.clone();
    let o = c.as_object_mut().ok_or(format!("{what} is not valid"))?;
    let claimed = o.insert("sha256".into(), serde_json::json!(""));
    o.remove("mac");
    if claimed.as_ref().and_then(|c| c.as_str())
        != Some(sha256_hex(super::util::json_sorted(&c).as_bytes()).as_str())
    {
        return Err(format!("{what} does not match its own sha256"));
    }
    if let Some(k) = key {
        crate::core::auth::verify(&raw, k)?;
    }
    Ok(())
}

/// Refuses a file written with another neutral form.
pub fn check_neutral_form(what: &str, v: u32) -> crate::core::util::Res<()> {
    if v == NEUTRAL_FORM {
        Ok(())
    } else {
        Err(format!(
            "{what} was written by a botport that {NEUTRAL_FORM_MISMATCH} (neutral form {v}, this one {NEUTRAL_FORM}): run the same botport release on the host and in the box"
        ))
    }
}

/// `SKILL.md` as the two sides compare it: the front matter as parsed YAML in a canonical JSON form (quoting and key
/// order do not matter; every value, list item and nesting level does); the body with trailing spaces dropped (a
/// Markdown hard break, two or more spaces, kept as two), runs of blank lines as one and no leading or trailing blank
/// lines, all outside fenced code blocks (``` or ~~~, closed only by the same character at least as long; kept exactly).
/// Front matter that does not parse to a plain mapping (or holds a comment, a document marker or a directive, which
/// YAML would drop or read across) is compared as it is, so no text is ever dropped.
pub fn skill_md_neutral(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let mut out = String::new();
    let mut body = text.as_str();
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest
            .find("\n---\n")
            .or_else(|| rest.strip_suffix("\n---").map(|r| r.len()))
        {
            let front = &rest[..end];
            // only a plain mapping is compared as data: anything YAML would drop or read across the delimiter (a
            // comment, a document marker or directive, a scalar that is really body text) is compared as written
            let plain = !front.lines().any(|l| {
                l.contains('#')
                    || l.starts_with("---")
                    || l.starts_with("...")
                    || l.starts_with('%')
            });
            match super::yaml11::parse::<serde_yaml::Value>(front) {
                Ok(v @ serde_yaml::Value::Mapping(_)) if plain && all_strings(&v) => {
                    let j = serde_json::to_value(&v).unwrap_or_default();
                    out.push_str(&super::util::json_sorted(&j))
                }
                _ => out.push_str(front),
            }
            out.push_str("\n---\n");
            body = rest.get(end + 5..).unwrap_or("");
        }
    }
    let mut blank = false;
    // the open code fence (its character and length): inside it every line is kept exactly
    let mut fenced: Option<(char, usize)> = None;
    let mut lines: Vec<String> = vec![];
    for raw in body.lines() {
        if let Some((c, n)) = fenced {
            lines.push(raw.to_string());
            let t = raw.trim();
            if t.len() >= n && t.chars().all(|x| x == c) {
                fenced = None;
            }
            continue;
        }
        // trailing whitespace is dropped, except a Markdown hard line break (two or more spaces), kept as two
        let trimmed = raw.trim_end();
        let hard_break = raw[trimmed.len()..].chars().filter(|c| *c == ' ').count() >= 2;
        let l = if hard_break && !trimmed.is_empty() {
            format!("{trimmed}  ")
        } else {
            trimmed.to_string()
        };
        let l = l.as_str();
        if l.is_empty() {
            blank = true;
            continue;
        }
        if blank && !lines.is_empty() {
            lines.push(String::new());
        }
        blank = false;
        if let Some(f) = fence(l) {
            fenced = Some(f);
        }
        lines.push(l.to_string());
    }
    out.push_str(&lines.join("\n"));
    out
}

/// Whether every key and value is a string that no YAML 1.1 or 1.2 reader would type: only then can quoting be
/// ignored without making two different values equal (`1.10` and `1.1`, `yes` and `'yes'` to PyYAML).
/// (serde_yaml refuses a mapping with a duplicate key, so such front matter is compared as written.)
fn all_strings(v: &serde_yaml::Value) -> bool {
    use serde_yaml::Value as Y;
    match v {
        Y::String(s) => !super::yaml11::typed11(s),
        Y::Sequence(xs) => xs.iter().all(all_strings),
        Y::Mapping(m) => m.iter().all(|(k, v)| all_strings(k) && all_strings(v)),
        _ => false,
    }
}

/// A line that opens a code fence: three or more backticks or tildes (CommonMark).
fn fence(l: &str) -> Option<(char, usize)> {
    let t = l.trim_start();
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

/// Read and check a received package: `dir` must hold a manifest that matches the files beside it exactly.
pub fn read_verified(dir: &Path, name: &str, lim: &Limits) -> Res<Package> {
    // the folder's checks first (a real directory, no symlinks), then the manifest as a regular file inside it
    let p = Package::read(dir, name, lim)?;
    let root = dir
        .canonicalize()
        .map_err(|e| format!("package {name}: {e}"))?;
    // one byte past the cap is enough to refuse: a huge manifest is never read whole
    let raw = super::util::read_confined_capped(
        &root,
        &root,
        MANIFEST,
        |_| true,
        MANIFEST_MAX as u64 + 1,
    )
    .map_err(|c| format!("package {name}: manifest: {c:?}"))?;
    if raw.len() > MANIFEST_MAX {
        return err(format!(
            "package {name}: manifest is larger than {MANIFEST_MAX} bytes"
        ));
    }
    let m: Manifest = serde_json::from_slice(&raw)
        .map_err(|e| super::util::json_invalid(&format!("package {name}: manifest"), &e))?;
    p.check(&m)?;
    Ok(p)
}

/// The largest manifest read.
pub const MANIFEST_MAX: usize = 4 << 20;

/// A rename cannot cross filesystems, and falling back to staging beside the live folder would put half-written
/// copies where a platform loads them: a stage on another filesystem is refused.
fn stage_on_device<'a>(stage: &'a Path, parent: &Path, same: bool) -> Res<&'a Path> {
    if same {
        Ok(stage)
    } else {
        err(format!(
            "the staging folder {} is not on the same filesystem as {}",
            stage.display(),
            parent.display()
        ))
    }
}

/// Whether two existing paths are on the same filesystem (always true where that cannot be told).
fn same_device(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
}

/// Put back package copies a crash left aside in `parent`: `.botport-pkg-old.<name>` is renamed to `<name>` when
/// `<name>` is missing (the crash came between the two renames of a swap) and removed when it is not; staging dirs
/// (`.botport-pkg-new.*`) are removed once they are over 10 minutes old (a write may be running). Returns the names
/// put back.
pub fn recover(parent: &Path) -> Res<Vec<String>> {
    recover_into(parent, parent)
}

/// [`recover`] for packages of `parent` staged in `stage` (see [`Package::write_via`]).
pub fn recover_into(stage: &Path, parent: &Path) -> Res<Vec<String>> {
    let mut back = vec![];
    let Ok(rd) = fs::read_dir(stage) else {
        return Ok(back);
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let is_dir = fs::symlink_metadata(e.path()).is_ok_and(|m| m.is_dir());
        if !is_dir {
            continue;
        }
        if let Some(name) = n.strip_prefix(".botport-pkg-old.") {
            if plain_name("package name", name).is_err() {
                continue;
            }
            let dest = parent.join(name);
            if fs::symlink_metadata(&dest).is_err() {
                // one that cannot be put back is left for the owner, and does not block the others
                if fs::rename(e.path(), &dest).is_ok() {
                    back.push(name.to_string());
                }
            } else {
                fs::remove_dir_all(e.path()).ok();
            }
        } else if n.starts_with(".botport-pkg-new.") {
            // a staging folder is left over from a crash only once it is a while old (a write may be running now)
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok());
            if old.is_some_and(|a| a > std::time::Duration::from_secs(600)) {
                fs::remove_dir_all(e.path()).ok();
            }
        }
    }
    Ok(back)
}

fn walk(
    base: &Path,
    dir: &Path,
    name: &str,
    lim: &Limits,
    out: &mut BTreeMap<String, (Vec<u8>, bool)>,
    total: &mut u64,
    dirs: &mut usize,
) -> Res<()> {
    // stop listing once the entries left could not all fit (each is a file, a folder, or the manifest): a folder of
    // millions of entries is refused without being listed whole
    let room = (lim.max_files - out.len().min(lim.max_files))
        .saturating_add(lim.max_files - (*dirs).min(lim.max_files))
        .saturating_add(1);
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("package {name}: {e}"))?
        .take(room.saturating_add(1))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("package {name}: {e}"))?;
    if entries.len() > room {
        return err(format!("package {name}: more than {} files", lim.max_files));
    }
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let rel = rel_str(p.strip_prefix(base).unwrap_or(&p));
        if dir == base && rel == MANIFEST {
            continue;
        }
        let md = fs::symlink_metadata(&p).map_err(|e| format!("package {name}: {rel}: {e}"))?;
        let ft = md.file_type();
        if ft.is_dir() {
            // directories count towards the file limit too (a tree of empty folders is not free)
            *dirs += 1;
            if *dirs > lim.max_files {
                return err(format!(
                    "package {name}: more than {} folders",
                    lim.max_files
                ));
            }
            walk(base, &p, name, lim, out, total, dirs)?;
            continue;
        }
        if !ft.is_file() {
            return err(format!(
                "package {name}: {rel} is a symlink or special file"
            ));
        }
        if special_bits(&md) {
            return err(format!(
                "package {name}: {rel} has the setuid, setgid or sticky bit"
            ));
        }
        if p.to_str().is_none() {
            return err(format!("package {name}: a file name is not UTF-8"));
        }
        safe_rel("package file", &rel)?;
        if md.len() > lim.max_file {
            return err(format!(
                "package {name}: {rel} is larger than {} bytes",
                lim.max_file
            ));
        }
        *total += md.len();
        if *total > lim.max_total {
            return err(format!(
                "package {name}: larger than {} bytes",
                lim.max_total
            ));
        }
        if out.len() >= lim.max_files {
            return err(format!("package {name}: more than {} files", lim.max_files));
        }
        let b = super::util::read_confined_capped(
            base,
            dir,
            &e.file_name().to_string_lossy(),
            |_| true,
            lim.max_file.saturating_add(1),
        )
        .map_err(|c| format!("package {name}: {rel}: {c:?}"))?;
        // the file may have grown since it was measured (read up to one byte past the cap, never further)
        if b.len() as u64 > lim.max_file {
            return err(format!(
                "package {name}: {rel} is larger than {} bytes",
                lim.max_file
            ));
        }
        out.insert(rel, (b, exec_bit(&md)));
    }
    Ok(())
}

/// Two paths that differ only by case or Unicode normalisation would land on one file on a case-insensitive or
/// normalising disk (the default on macOS): refused rather than letting one silently replace the other.
fn no_collisions<'a>(name: &str, paths: impl Iterator<Item = &'a String>) -> Res<()> {
    use unicode_normalization::UnicodeNormalization;
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut files: Vec<String> = vec![];
    for p in paths {
        files.push(p.nfc().collect::<String>().to_lowercase());
        // every folder on the way counts too: `a/x` and `A/y`, or a file `a` and a folder `A/`, collide
        let parts: Vec<&str> = p.split('/').collect();
        for i in 1..=parts.len() {
            let prefix = parts[..i].join("/");
            let key: String = prefix.nfc().collect::<String>().to_lowercase();
            match seen.get(&key) {
                Some(other) if *other != prefix => {
                    return err(format!(
                        "package {name}: {other} and {prefix} differ only by case or Unicode normalisation"
                    ));
                }
                Some(_) => {}
                None => {
                    seen.insert(key, prefix);
                }
            }
        }
    }
    // a file cannot also be a folder (`a` next to `a/b`): the write could never succeed
    let folders: std::collections::BTreeSet<&str> = files
        .iter()
        .filter_map(|f| f.rsplit_once('/').map(|(d, _)| d))
        .flat_map(|d| {
            let parts: Vec<&str> = d.split('/').collect();
            (1..=parts.len())
                .map(move |i| &d[..parts[..i].iter().map(|s| s.len() + 1).sum::<usize>() - 1])
        })
        .collect();
    if let Some(f) = files.iter().find(|f| folders.contains(f.as_str())) {
        return err(format!("package {name}: {f} is both a file and a folder"));
    }
    Ok(())
}

/// The setuid, setgid and sticky bits: never carried.
#[cfg(unix)]
fn special_bits(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o7000 != 0
}

#[cfg(not(unix))]
fn special_bits(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn exec_bit(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn exec_bit(_: &fs::Metadata) -> bool {
    false
}

/// A new, empty, uniquely named directory in `parent` (created exclusively, so never an existing one).
fn fresh_dir(parent: &Path, tag: &str) -> Res<PathBuf> {
    for n in 0..1000u32 {
        let d = parent.join(format!(".botport-pkg-{tag}.{}.{n}", std::process::id()));
        match fs::create_dir(&d) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&d, fs::Permissions::from_mode(0o700))
                        .map_err(|e| format!("chmod {}: {e}", d.display()))?;
                }
                return Ok(d);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return err(format!("mkdir {}: {e}", d.display())),
        }
    }
    err(format!("no free temp dir in {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = crate::core::util::test_dir(&format!("pkg-{tag}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(p: &Path, b: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b).unwrap();
    }

    #[cfg(unix)]
    fn chmod(p: &Path, m: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
    }

    /// An invented skill tree: text, a binary file, an executable script, an empty file, a nested dir.
    fn sample(root: &Path) -> PathBuf {
        let d = root.join("demo-skill");
        put(
            &d.join("SKILL.md"),
            b"# Demo skill\n\nExample instructions.\n",
        );
        put(&d.join("assets/logo.bin"), &[0xff, 0xfe, 0x00, 0x80, 0x0a]);
        put(&d.join("scripts/run.sh"), b"#!/bin/sh\necho example\n");
        put(&d.join("notes/empty.txt"), b"");
        #[cfg(unix)]
        chmod(&d.join("scripts/run.sh"), 0o755);
        d
    }

    #[test]
    fn round_trip_is_byte_identical() {
        let w = tmp("rt");
        let src = sample(&w);
        let p = Package::read(&src, "demo-skill", &Limits::default()).unwrap();
        assert_eq!(p.files.len(), 4);
        #[cfg(unix)]
        assert!(p.files["scripts/run.sh"].1);
        assert!(!p.files["SKILL.md"].1);
        p.write(&w.join("out"), true).unwrap();
        let back =
            read_verified(&w.join("out/demo-skill"), "demo-skill", &Limits::default()).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.digest(), p.digest());
        for rel in p.files.keys() {
            assert_eq!(
                fs::read(src.join(rel)).unwrap(),
                fs::read(w.join("out/demo-skill").join(rel)).unwrap()
            );
        }
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn a_staged_write_leaves_nothing_in_the_live_folder_and_recovers_from_its_stage() {
        let w = tmp("stage");
        let p = Package::read(&sample(&w), "demo-skill", &Limits::default()).unwrap();
        let (live, stage) = (w.join("skills/cat"), w.join("botport/stage/cat"));
        p.write_via(&live, &stage, false).unwrap();
        p.write_via(&live, &stage, false).unwrap(); // a replacement too
        let names = |d: &Path| -> Vec<String> {
            fs::read_dir(d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        };
        assert_eq!(names(&live), vec!["demo-skill".to_string()]);
        assert!(names(&stage).is_empty(), "{:?}", names(&stage));
        // a crash between the two renames: the old copy sits in the stage, and goes back into the live folder
        fs::rename(
            live.join("demo-skill"),
            stage.join(".botport-pkg-old.demo-skill"),
        )
        .unwrap();
        assert_eq!(recover_into(&stage, &live).unwrap(), vec!["demo-skill"]);
        assert_eq!(
            Package::read(&live.join("demo-skill"), "demo-skill", &Limits::default()).unwrap(),
            p
        );
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn an_export_reformatting_skill_md_is_not_a_change() {
        let installed = "---\nname: demo-skill\ndescription: Example skill\n---\n# Demo skill\n\nExample steps.\n";
        let exported = "---\nname: demo-skill\ndescription: \"Example skill\"\n---\n\n# Demo skill \n\n\nExample steps.\n\n";
        assert_eq!(skill_md_neutral(installed), skill_md_neutral(exported));
        let pkg = |t: &str| {
            Package::from_files(
                "demo-skill",
                BTreeMap::from([("SKILL.md".to_string(), (t.as_bytes().to_vec(), false))]),
            )
            .unwrap()
        };
        assert_eq!(
            pkg(installed).neutral_digest(),
            pkg(exported).neutral_digest()
        );
        assert_ne!(pkg(installed).digest(), pkg(exported).digest());
        // a real change still is one
        let changed = installed.replace("Example steps.", "Other steps.");
        assert_ne!(
            pkg(installed).neutral_digest(),
            pkg(&changed).neutral_digest()
        );
        let renamed = installed.replace("description: Example skill", "description: Another skill");
        assert_ne!(
            pkg(installed).neutral_digest(),
            pkg(&renamed).neutral_digest()
        );
        // every front-matter value counts: list items, block scalars, nesting
        let base = "---\nname: demo-skill\nallowed-tools:\n  - read\nmeta:\n  author: a\nnotes: |\n  one\n---\nBody.\n";
        for other in [
            base.replace("  - read\n", "  - read\n  - write\n"),
            base.replace("  one\n", "  two\n"),
            base.replace("meta:\n  author: a\n", "author: a\n"),
        ] {
            assert_ne!(skill_md_neutral(base), skill_md_neutral(&other), "{other}");
        }
        // fenced code is kept exactly
        let code = "# T\n\n```\na\n\n\nb\n```\n";
        assert_ne!(
            skill_md_neutral(code),
            skill_md_neutral(&code.replace("a\n\n\nb", "a\nb"))
        );
        // front matter that does not parse is compared as it is
        let bad = "---\nname: [unclosed\n---\nBody.\n";
        assert_ne!(
            skill_md_neutral(bad),
            skill_md_neutral(&bad.replace("name: [unclosed", "name:  [unclosed"))
        );
    }

    #[test]
    fn the_neutral_skill_md_never_drops_text() {
        // two files that differ in text never compare equal, whatever the delimiters look like
        let pairs = [
            // the closing delimiter is the last line: what sits before it is front matter only if it is a mapping
            (
                "---\nIntro line\nmore intro\n---",
                "---\nIntro line more intro\n---",
            ),
            (
                "---\nname: x\n---body one\n---",
                "---\nname: x\n---body two\n---",
            ),
            // YAML would drop a comment or read past a document marker
            (
                "---\nname: x # one\n---\nBody.\n",
                "---\nname: x # two\n---\nBody.\n",
            ),
            (
                "---\nname: x\n...\nkept one\n---\nBody.\n",
                "---\nname: x\n...\nkept two\n---\nBody.\n",
            ),
            // a longer fence holds a shorter one; tildes are fences too
            ("````\n```\na\n\n\nb\n````\n", "````\n```\na\nb\n````\n"),
            ("~~~\na  \n\n\nb\n~~~\n", "~~~\na\nb\n~~~\n"),
        ];
        for (a, b) in pairs {
            assert_ne!(skill_md_neutral(a), skill_md_neutral(b), "{a:?} vs {b:?}");
        }
        // the formatting rules still hold outside fences
        assert_eq!(
            skill_md_neutral("---\nname: x\n---\n# T\n\n\nBody. \n"),
            skill_md_neutral("---\nname: 'x'\n---\n# T\n\nBody.\n")
        );
        // front matter compares as data only when no value could be read two ways
        for (a, b) in [
            ("---\nversion: 1.10\n---\n", "---\nversion: 1.1\n---\n"),
            ("---\nx: 0123\n---\n", "---\nx: 123\n---\n"),
            ("---\nflag: yes\n---\n", "---\nflag: 'yes'\n---\n"),
            ("---\nd: a\nd: b\n---\n", "---\nd: b\n---\n"),
        ] {
            assert_ne!(skill_md_neutral(a), skill_md_neutral(b), "{a:?} vs {b:?}");
        }
        // what changes Markdown's meaning is a change: a repeated heading, a hard line break
        assert_ne!(
            skill_md_neutral("# T\n# T\nBody.\n"),
            skill_md_neutral("# T\nBody.\n")
        );
        assert_ne!(
            skill_md_neutral("Line one  \nline two\n"),
            skill_md_neutral("Line one\nline two\n")
        );
        assert_eq!(
            skill_md_neutral("Line one   \nline two\n"),
            skill_md_neutral("Line one  \nline two\n")
        );
        // a SKILL.md that is not UTF-8 counts by its bytes
        let raw = |b: &[u8]| {
            let d = crate::core::util::test_dir("skill-raw");
            fs::write(d.join("SKILL.md"), b).unwrap();
            let p = Package::read(&d, "demo-skill", &Limits::default()).unwrap();
            fs::remove_dir_all(&d).ok();
            p.neutral_digest()
        };
        assert_ne!(raw(b"\xffBody.\n"), raw(b"\xfeBody.\n"));
    }

    #[test]
    fn digest_covers_path_bytes_and_exec_bit_only() {
        let mk = |rel: &str, b: &[u8], x: bool| {
            Package::from_files("p", BTreeMap::from([(rel.to_string(), (b.to_vec(), x))])).unwrap()
        };
        let a = mk("a.md", b"hi\n", false);
        assert_eq!(a.digest(), mk("a.md", b"hi\n", false).digest());
        assert_ne!(a.digest(), mk("b.md", b"hi\n", false).digest());
        assert_ne!(a.digest(), mk("a.md", b"hi \n", false).digest());
        assert_ne!(a.digest(), mk("a.md", b"hi\n", true).digest());
        // the name is not part of the digest: a renamed package is the same content
        let renamed = Package::from_files("q", a.files.clone()).unwrap();
        assert_eq!(a.digest(), renamed.digest());
    }

    #[test]
    fn manifest_is_skipped_on_read_and_reserved_in_memory() {
        let w = tmp("man");
        let src = sample(&w);
        let before = Package::read(&src, "demo-skill", &Limits::default()).unwrap();
        put(&src.join(MANIFEST), b"{}");
        assert_eq!(
            Package::read(&src, "demo-skill", &Limits::default()).unwrap(),
            before
        );
        let e = Package::from_files(
            "p",
            BTreeMap::from([(MANIFEST.to_string(), (vec![], false))]),
        )
        .unwrap_err();
        assert!(e.contains("reserved"), "{e}");
        // only the root manifest is special: a nested file of that name is content
        put(&src.join("sub").join(MANIFEST), b"{}");
        assert_eq!(
            Package::read(&src, "demo-skill", &Limits::default())
                .unwrap()
                .files
                .len(),
            5
        );
        fs::remove_dir_all(&w).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_specials_are_refused() {
        let w = tmp("sym");
        let src = sample(&w);
        put(&w.join("outside.txt"), b"not part of the package\n");
        std::os::unix::fs::symlink(w.join("outside.txt"), src.join("link.txt")).unwrap();
        let e = Package::read(&src, "demo-skill", &Limits::default()).unwrap_err();
        assert!(e.contains("link.txt") && e.contains("symlink"), "{e}");
        fs::remove_file(src.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&w, src.join("dirlink")).unwrap();
        assert!(Package::read(&src, "demo-skill", &Limits::default()).is_err());
        fs::remove_file(src.join("dirlink")).unwrap();
        // the package root itself must not be a symlink
        std::os::unix::fs::symlink(&src, w.join("alias")).unwrap();
        assert!(Package::read(&w.join("alias"), "alias", &Limits::default()).is_err());
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn limits_refuse_instead_of_cutting() {
        let w = tmp("lim");
        let src = sample(&w);
        let small = |f, t, n| Limits {
            max_file: f,
            max_total: t,
            max_files: n,
        };
        assert!(Package::read(&src, "demo-skill", &small(10, 1000, 10))
            .unwrap_err()
            .contains("larger than 10"));
        assert!(Package::read(&src, "demo-skill", &small(1000, 50, 10))
            .unwrap_err()
            .contains("larger than 50"));
        assert!(Package::read(&src, "demo-skill", &small(1000, 1000, 3))
            .unwrap_err()
            .contains("more than 3"));
        assert!(Package::read(&src, "demo-skill", &small(1000, 1000, 4)).is_ok());
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn a_crowded_folder_and_a_huge_manifest_are_refused() {
        let w = tmp("crowd");
        let d = w.join("crowd");
        for i in 0..40 {
            put(&d.join(format!("f{i}.txt")), b"x");
        }
        let lim = Limits {
            max_file: 1000,
            max_total: 1 << 20,
            max_files: 3,
        };
        assert!(Package::read(&d, "crowd", &lim)
            .unwrap_err()
            .contains("more than 3 files"));
        let src = sample(&w);
        put(&src.join(MANIFEST), &vec![b' '; MANIFEST_MAX + 10]);
        assert!(read_verified(&src, "demo-skill", &Limits::default())
            .unwrap_err()
            .contains("manifest is larger than"));
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn a_stage_on_another_filesystem_is_refused_not_swapped_for_the_live_folder() {
        let (stage, live) = (Path::new("/stage"), Path::new("/live"));
        assert_eq!(stage_on_device(stage, live, true).unwrap(), stage);
        assert!(stage_on_device(stage, live, false)
            .unwrap_err()
            .contains("not on the same filesystem"));
    }

    #[test]
    fn secret_text_is_found_without_the_value() {
        let clean = Package::from_files(
            "p",
            BTreeMap::from([(
                "SKILL.md".to_string(),
                (b"Rotate the API token weekly.\n".to_vec(), false),
            )]),
        )
        .unwrap();
        assert_eq!(clean.secret_at(), None);
        let value = "sk-ABCDEFGHIJKLMNOPQRSTUVWX123";
        let hit = Package::from_files(
            "p",
            BTreeMap::from([
                ("SKILL.md".to_string(), (b"ok\n".to_vec(), false)),
                (
                    "conf/env.txt".to_string(),
                    (format!("api_key={value}\n").into_bytes(), false),
                ),
                ("blob.bin".to_string(), (vec![0xff, 0x00, 0xfe], false)),
            ]),
        )
        .unwrap();
        let at = hit.secret_at().unwrap();
        assert!(at.starts_with("conf/env.txt"), "{at}");
        assert!(!at.contains(value), "{at}");
    }

    #[test]
    fn case_and_normalisation_collisions_are_refused() {
        let two = |a: &str, b: &str| {
            Package::from_files(
                "p",
                BTreeMap::from([
                    (a.to_string(), (vec![], false)),
                    (b.to_string(), (vec![], false)),
                ]),
            )
        };
        assert!(two("SKILL.md", "skill.md")
            .unwrap_err()
            .contains("differ only"));
        // "é" precomposed vs "e" + combining acute
        assert!(two("caf\u{e9}.md", "cafe\u{301}.md")
            .unwrap_err()
            .contains("differ only"));
        assert!(two("a/SKILL.md", "b/SKILL.md").is_ok());
    }

    #[test]
    fn folder_prefixes_collide_by_case_too() {
        let two = |a: &str, b: &str| {
            Package::from_files(
                "p",
                BTreeMap::from([
                    (a.to_string(), (vec![], false)),
                    (b.to_string(), (vec![], false)),
                ]),
            )
        };
        assert!(two("a/x", "A/y").unwrap_err().contains("differ only"));
        assert!(two("a", "A/b").unwrap_err().contains("differ only"));
        assert!(two("a/x", "a/y").is_ok());
        assert!(two("a", "a/b")
            .unwrap_err()
            .contains("both a file and a folder"));
        assert!(two("x/a", "x/a/b").is_err());
    }

    #[test]
    fn a_crash_between_the_two_renames_is_recovered() {
        let w = tmp("crash");
        let out = w.join("out");
        let v1 = Package::from_files(
            "p",
            BTreeMap::from([("SKILL.md".to_string(), (b"v1\n".to_vec(), false))]),
        )
        .unwrap();
        v1.write(&out, false).unwrap();
        // what the first rename of a swap leaves behind when the process dies right after it
        fs::rename(out.join("p"), out.join(".botport-pkg-old.p")).unwrap();
        fs::create_dir_all(out.join(".botport-pkg-new.1.0")).unwrap();
        assert_eq!(recover(&out).unwrap(), vec!["p".to_string()]);
        assert_eq!(
            Package::read(&out.join("p"), "p", &Limits::default()).unwrap(),
            v1
        );
        // a fresh staging folder may belong to a write running now: left alone
        assert!(out.join(".botport-pkg-new.1.0").exists());
        // with the destination present, a leftover old copy is just removed
        fs::create_dir_all(out.join(".botport-pkg-old.p")).unwrap();
        assert!(recover(&out).unwrap().is_empty());
        assert!(!out.join(".botport-pkg-old.p").exists());
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn write_checks_paths_even_for_a_hand_built_package() {
        let w = tmp("handbuilt");
        let p = Package {
            name: "p".into(),
            files: BTreeMap::from([("../escape.txt".to_string(), (b"x".to_vec(), false))]),
        };
        assert!(p.write(&w.join("out"), false).is_err());
        assert!(!w.join("escape.txt").exists());
        fs::remove_dir_all(&w).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_manifest_is_refused() {
        let w = tmp("linkman");
        let src = sample(&w);
        let p = Package::read(&src, "demo-skill", &Limits::default()).unwrap();
        p.write(&w.join("out"), true).unwrap();
        let dir = w.join("out/demo-skill");
        fs::remove_file(dir.join(MANIFEST)).unwrap();
        fs::write(
            w.join("elsewhere.json"),
            super::super::util::json_sorted(&p.manifest()),
        )
        .unwrap();
        std::os::unix::fs::symlink(w.join("elsewhere.json"), dir.join(MANIFEST)).unwrap();
        assert!(read_verified(&dir, "demo-skill", &Limits::default()).is_err());
        fs::remove_dir_all(&w).ok();
    }

    #[cfg(unix)]
    #[test]
    fn written_modes_are_private_and_special_bits_refused() {
        use std::os::unix::fs::PermissionsExt;
        let w = tmp("modes");
        let src = sample(&w);
        let p = Package::read(&src, "demo-skill", &Limits::default()).unwrap();
        p.write(&w.join("out"), true).unwrap();
        let mode = |rel: &str| {
            fs::metadata(w.join("out/demo-skill").join(rel))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("SKILL.md"), 0o600);
        assert_eq!(mode("scripts/run.sh"), 0o700);
        assert_eq!(mode(MANIFEST), 0o600);
        assert_eq!(
            fs::metadata(w.join("out/demo-skill"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        chmod(&src.join("scripts/run.sh"), 0o4755);
        let e = Package::read(&src, "demo-skill", &Limits::default()).unwrap_err();
        assert!(e.contains("setuid"), "{e}");
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn names_and_paths_must_stay_inside() {
        for bad in ["", ".", "..", ".hidden", "a/b"] {
            assert!(
                Package::from_files(bad, BTreeMap::new()).is_err(),
                "{bad:?}"
            );
        }
        for bad in ["../x", "/abs", "a//b", "a/./b", "a\\b"] {
            let f = BTreeMap::from([(bad.to_string(), (vec![], false))]);
            assert!(Package::from_files("p", f).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn check_finds_any_tampering() {
        let w = tmp("chk");
        let src = sample(&w);
        let p = Package::read(&src, "demo-skill", &Limits::default()).unwrap();
        let m = p.manifest();
        assert!(p.check(&m).is_ok());
        let mut extra = p.clone();
        extra
            .files
            .insert("zz.txt".into(), (b"added\n".to_vec(), false));
        assert!(extra.check(&m).unwrap_err().contains("zz.txt"));
        let mut changed = p.clone();
        changed.files.get_mut("SKILL.md").unwrap().0.push(b'!');
        assert!(changed.check(&m).unwrap_err().contains("SKILL.md"));
        let mut missing = p.clone();
        missing.files.remove("notes/empty.txt");
        assert!(missing.check(&m).is_err());
        let mut wrong_digest = m.clone();
        wrong_digest.digest = "0".repeat(64);
        assert!(p.check(&wrong_digest).unwrap_err().contains("digest"));
        let mut other_name = m.clone();
        other_name.name = "other".into();
        assert!(p.check(&other_name).is_err());
        // on disk: a file changed after the manifest was written
        p.write(&w.join("out"), true).unwrap();
        put(&w.join("out/demo-skill/SKILL.md"), b"tampered\n");
        assert!(
            read_verified(&w.join("out/demo-skill"), "demo-skill", &Limits::default()).is_err()
        );
        fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn write_replaces_whole_and_keeps_old_copy_on_failure() {
        let w = tmp("swap");
        let out = w.join("out");
        let v1 = Package::from_files(
            "p",
            BTreeMap::from([
                ("SKILL.md".to_string(), (b"v1\n".to_vec(), false)),
                (
                    "old-only.txt".to_string(),
                    (b"gone in v2\n".to_vec(), false),
                ),
            ]),
        )
        .unwrap();
        v1.write(&out, false).unwrap();
        let v2 = Package::from_files(
            "p",
            BTreeMap::from([("SKILL.md".to_string(), (b"v2\n".to_vec(), false))]),
        )
        .unwrap();
        v2.write(&out, false).unwrap();
        assert_eq!(
            Package::read(&out.join("p"), "p", &Limits::default()).unwrap(),
            v2
        );
        // a package whose text trips the secret write guard is not written, and v2 stays
        let bad = Package::from_files(
            "p",
            BTreeMap::from([(
                "SKILL.md".to_string(),
                (b"api_key=sk-ABCDEFGHIJKLMNOPQRSTUVWX123\n".to_vec(), false),
            )]),
        )
        .unwrap();
        assert!(bad.write(&out, false).is_err());
        assert_eq!(
            Package::read(&out.join("p"), "p", &Limits::default()).unwrap(),
            v2
        );
        // no temp or old dirs are left behind
        let left: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("p")]);
        fs::remove_dir_all(&w).ok();
    }
}
