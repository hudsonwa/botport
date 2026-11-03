//! Where mirror files live. A [`Store`] is a flat namespace of `/`-separated relative paths; [`FolderStore`] keeps
//! them under a directory on this machine (a local directory, or a folder that Google Drive for desktop syncs).
use super::IoStats;
use crate::core::util::{err, read_confined, safe_rel, write_private, Confined, Res};
use std::fs;
use std::path::{Path, PathBuf};

/// One file in a store listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name (no directory part).
    pub name: String,
    /// Size in bytes.
    pub size: u64,
}

/// A place mirror files are read from and written to.
pub trait Store {
    /// Regular files directly inside `dir` (a missing directory lists as empty), sorted by name.
    fn list(&mut self, dir: &str) -> Res<Vec<Entry>>;
    /// The bytes of `path`, or `None` if it does not exist.
    fn get(&mut self, path: &str) -> Res<Option<Vec<u8>>>;
    /// At most `max + 1` bytes of `path` (so a longer file shows as longer than `max`), or `None` if it does not
    /// exist: nothing past them is read.
    fn get_at_most(&mut self, path: &str, max: u64) -> Res<Option<Vec<u8>>>;
    /// Create or replace `path` atomically.
    fn put(&mut self, path: &str, data: &[u8]) -> Res<()>;
    /// Remove `path` (missing is fine).
    fn delete(&mut self, path: &str) -> Res<()>;
    /// Figures since the store was opened.
    fn io(&self) -> IoStats;
    /// Forget anything read ahead, so the next read sees the store as it is now (a cached listing, say).
    fn refresh(&mut self) {}
}

/// Files under a directory: written 0600 in 0700 directories, read only as regular files reached through no
/// symlink inside the root.
pub struct FolderStore {
    root: PathBuf,
    io: IoStats,
}

impl FolderStore {
    /// A store rooted at `root` (which must exist and be a real directory, not a symlink).
    pub fn open(root: &Path) -> Res<FolderStore> {
        let md = fs::symlink_metadata(root)
            .map_err(|e| format!("mirror folder {}: {e}", root.display()))?;
        if !md.is_dir() {
            return err(format!(
                "mirror folder {} is not a directory (or is a symlink)",
                root.display()
            ));
        }
        let root = root
            .canonicalize()
            .map_err(|e| format!("mirror folder {}: {e}", root.display()))?;
        Ok(FolderStore {
            root,
            io: IoStats::default(),
        })
    }
}

impl FolderStore {
    /// `dir` under the root when every part of it is a real directory (never a symlink), `None` when a part is
    /// missing, an error when a part is something else.
    fn real_dir(&self, dir: &str) -> Res<Option<PathBuf>> {
        let mut d = self.root.clone();
        if dir.is_empty() {
            return Ok(Some(d));
        }
        safe_rel("mirror folder", dir)?;
        for part in dir.split('/') {
            d.push(part);
            match fs::symlink_metadata(&d) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    return err(format!(
                        "mirror folder: {dir} is not a directory (or is a symlink)"
                    ))
                }
                Err(_) => return Ok(None),
            }
        }
        Ok(Some(d))
    }
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

impl Store for FolderStore {
    fn list(&mut self, dir: &str) -> Res<Vec<Entry>> {
        let Some(d) = self.real_dir(dir)? else {
            return Ok(vec![]);
        };
        let mut out = vec![];
        for e in fs::read_dir(&d)
            .map_err(|e| format!("mirror folder: {dir}: {e}"))?
            .flatten()
        {
            let Ok(m) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            if !m.is_file() {
                continue;
            }
            if let Some(name) = e.file_name().to_str() {
                out.push(Entry {
                    name: name.to_string(),
                    size: m.len(),
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn get(&mut self, path: &str) -> Res<Option<Vec<u8>>> {
        safe_rel("mirror file", path)?;
        let (dir, name) = split(path);
        match read_confined(&self.root, &self.root.join(dir), name) {
            Ok(b) => {
                self.io.read_bytes += b.len() as u64;
                Ok(Some(b))
            }
            Err(Confined::Missing) => Ok(None),
            Err(e) => err(format!("mirror file {path}: refused ({e:?})")),
        }
    }

    fn get_at_most(&mut self, path: &str, max: u64) -> Res<Option<Vec<u8>>> {
        safe_rel("mirror file", path)?;
        let (dir, name) = split(path);
        let cap = max.saturating_add(1);
        match crate::core::util::read_confined_capped(
            &self.root,
            &self.root.join(dir),
            name,
            |_| true,
            cap,
        ) {
            Ok(b) => {
                self.io.read_bytes += b.len() as u64;
                Ok(Some(b))
            }
            Err(Confined::Missing) => Ok(None),
            Err(e) => err(format!("mirror file {path}: refused ({e:?})")),
        }
    }

    fn put(&mut self, path: &str, data: &[u8]) -> Res<()> {
        write_private(&self.root, path, data)?;
        self.io.written_bytes += data.len() as u64;
        Ok(())
    }

    fn delete(&mut self, path: &str) -> Res<()> {
        safe_rel("mirror file", path)?;
        let (dir, name) = split(path);
        let Some(d) = self.real_dir(dir)? else {
            return Ok(());
        };
        let p = d.join(name);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_file() => fs::remove_file(&p).map_err(|e| format!("remove {path}: {e}")),
            Ok(_) => err(format!(
                "mirror file {path} is not a regular file; not removed"
            )),
            Err(_) => Ok(()),
        }
    }

    fn io(&self) -> IoStats {
        self.io
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("botport-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_bounded_get_reads_no_more_than_asked() {
        let d = crate::core::util::test_dir("store-capped");
        let mut s = FolderStore::open(&d).unwrap();
        s.put("probe/bot.txt", &vec![b'x'; 10_000]).unwrap();
        let before = s.io().read_bytes;
        let b = s.get_at_most("probe/bot.txt", 4096).unwrap().unwrap();
        assert_eq!(
            b.len(),
            4097,
            "max + 1 bytes, so a longer file shows as longer"
        );
        assert_eq!(s.io().read_bytes - before, 4097);
        s.put("probe/small.txt", b"ok").unwrap();
        assert_eq!(
            s.get_at_most("probe/small.txt", 4096).unwrap().unwrap(),
            b"ok"
        );
        assert_eq!(s.get_at_most("probe/none.txt", 4096).unwrap(), None);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn put_get_list_delete() {
        let d = tmp("basic");
        let mut s = FolderStore::open(&d).unwrap();
        assert_eq!(s.list("from-grok").unwrap(), vec![]);
        assert_eq!(s.get("from-grok/state-1.json").unwrap(), None);
        s.put("from-grok/state-1.json", b"{}").unwrap();
        s.put("from-grok/blobs/abc", b"xyz").unwrap();
        assert_eq!(s.get("from-grok/state-1.json").unwrap().unwrap(), b"{}");
        let names: Vec<String> = s
            .list("from-grok")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            ["state-1.json"],
            "directories are not listed as files"
        );
        assert_eq!(s.io().written_bytes, 5);
        assert_eq!(s.io().read_bytes, 2);
        s.delete("from-grok/state-1.json").unwrap();
        s.delete("from-grok/state-1.json").unwrap();
        assert_eq!(s.get("from-grok/state-1.json").unwrap(), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = |p: &str| fs::metadata(d.join(p)).unwrap().permissions().mode() & 0o777;
            assert_eq!(m("from-grok/blobs/abc"), 0o600);
            assert_eq!(m("from-grok/blobs"), 0o700);
        }
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn paths_stay_inside_and_links_are_refused() {
        let d = tmp("conf");
        let mut s = FolderStore::open(&d).unwrap();
        for bad in ["../x", "/etc/hosts", "a//b", "a/../b"] {
            assert!(s.get(bad).is_err(), "{bad}");
            assert!(s.put(bad, b"x").is_err(), "{bad}");
        }
        #[cfg(unix)]
        {
            let outside = tmp("conf-outside");
            fs::write(outside.join("secret.txt"), b"not for the store").unwrap();
            std::os::unix::fs::symlink(outside.join("secret.txt"), d.join("link.json")).unwrap();
            assert!(s.get("link.json").is_err(), "a symlink is never followed");
            assert!(s.list("").unwrap().is_empty(), "nor listed");
            assert!(s.delete("link.json").is_err());
            assert!(outside.join("secret.txt").exists());
            std::os::unix::fs::symlink(&outside, d.join("dirlink")).unwrap();
            assert!(s.list("dirlink").is_err());
            assert!(s.get("dirlink/secret.txt").is_err());
            // a symlinked folder further up is never followed either, not even to list or delete
            fs::create_dir_all(outside.join("blobs")).unwrap();
            fs::write(outside.join("blobs/victim"), b"keep me").unwrap();
            assert!(s.list("dirlink/blobs").is_err());
            assert!(s.delete("dirlink/blobs/victim").is_err());
            assert!(outside.join("blobs/victim").exists());
            fs::remove_dir_all(&outside).ok();
            // the root itself must not be a symlink
            std::os::unix::fs::symlink(&d, d.with_extension("alias")).unwrap();
            assert!(FolderStore::open(&d.with_extension("alias")).is_err());
            fs::remove_file(d.with_extension("alias")).ok();
        }
        fs::remove_dir_all(&d).ok();
    }
}
