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
    /// nothing half-written ever sits in the live folder. `stage` must be on the same filesystem as `parent` and serve
    /// only this `parent`; [`recover_into`] puts back what a crash left there.
    pub fn write_via(&self, parent: &Path, stage: &Path, manifest: bool) -> Res<()> {
        // the fields are public: check the paths again before any of them is joined to a directory
        Package::from_files(&self.name, self.files.clone())?;
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let stage = if canon(stage) == canon(parent) {
            parent
        } else {
            super::util::ensure_private_dir(stage)?;
            // a rename cannot cross filesystems: a stage elsewhere falls back to staging beside the live folder
            if same_device(stage, parent) {
                stage
            } else {
                parent
            }
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

/// Read and check a received package: `dir` must hold a manifest that matches the files beside it exactly.
pub fn read_verified(dir: &Path, name: &str, lim: &Limits) -> Res<Package> {
    // the folder's checks first (a real directory, no symlinks), then the manifest as a regular file inside it
    let p = Package::read(dir, name, lim)?;
    let root = dir
        .canonicalize()
        .map_err(|e| format!("package {name}: {e}"))?;
    let raw = super::util::read_confined(&root, &root, MANIFEST)
        .map_err(|c| format!("package {name}: manifest: {c:?}"))?;
    if raw.len() > MANIFEST_MAX {
        return err(format!(
            "package {name}: manifest is larger than {MANIFEST_MAX} bytes"
        ));
    }
    let m: Manifest = serde_json::from_slice(&raw)
        .map_err(|e| format!("package {name}: manifest is not valid: {e}"))?;
    p.check(&m)?;
    Ok(p)
}

/// The largest manifest read.
pub const MANIFEST_MAX: usize = 4 << 20;

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
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("package {name}: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("package {name}: {e}"))?;
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
        let b = super::util::read_confined(base, dir, &e.file_name().to_string_lossy())
            .map_err(|c| format!("package {name}: {rel}: {c:?}"))?;
        // the file may have grown since it was measured
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
        let d = std::env::temp_dir().join(format!("botport-pkg-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
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
