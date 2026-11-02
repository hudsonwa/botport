//! Small shared helpers: hashing, sorted walks, atomic writes, deterministic JSON, argument parsing.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub type Res<T> = Result<T, String>;

pub fn err<T>(msg: impl Into<String>) -> Res<T> {
    Err(msg.into())
}

pub fn sha256_hex(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    h.finalize().iter().map(|x| format!("{x:02x}")).collect()
}

pub fn read(p: &Path) -> Res<Vec<u8>> {
    fs::read(p).map_err(|e| format!("read {}: {e}", p.display()))
}

pub fn read_str(p: &Path) -> Res<String> {
    String::from_utf8(read(p)?).map_err(|_| format!("{} is not UTF-8", p.display()))
}

/// Every regular file under `root` (no symlinks followed), as paths relative to `root`, sorted bytewise.
pub fn files_sorted(root: &Path) -> Vec<PathBuf> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                walk(base, &p, out);
            } else if ft.is_file() {
                if let Ok(rel) = p.strip_prefix(base) {
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Every entry under `root` that is neither a directory nor a regular file (symlinks included, never followed),
/// as relative paths, sorted.
pub fn special_entries(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            match fs::symlink_metadata(&p).map(|m| m.file_type()) {
                Ok(ft) if ft.is_dir() => walk(base, &p, out),
                Ok(ft) if ft.is_file() => {}
                _ => out.push(rel_str(p.strip_prefix(base).unwrap_or(&p))),
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Why [`read_confined`] refused a path.
#[derive(Debug, PartialEq)]
pub enum Confined {
    Missing,
    NotRegular,
    Outside,
    Io(String),
}

/// Read `dir/rel` only if it is a regular file reached through no symlink and inside `root` (the canonical
/// `dir`). The opened file must be the same inode `lstat` saw, so a swap to a symlink in between is refused.
pub fn read_confined(root: &Path, dir: &Path, rel: &str) -> Result<Vec<u8>, Confined> {
    if safe_rel("path", rel).is_err() {
        return Err(Confined::Outside);
    }
    let p = dir.join(rel);
    let md = match fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Confined::Missing),
        Err(e) => return Err(Confined::Io(e.to_string())),
    };
    if !md.file_type().is_file() {
        return Err(Confined::NotRegular);
    }
    if !p.canonicalize().is_ok_and(|c| c.starts_with(root)) {
        return Err(Confined::Outside);
    }
    let mut f = fs::File::open(&p).map_err(|e| Confined::Io(e.to_string()))?;
    let fmd = f.metadata().map_err(|e| Confined::Io(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (fmd.dev(), fmd.ino()) != (md.dev(), md.ino()) {
            return Err(Confined::NotRegular);
        }
    }
    if !fmd.is_file() {
        return Err(Confined::NotRegular);
    }
    let mut b = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut b).map_err(|e| Confined::Io(e.to_string()))?;
    Ok(b)
}

pub fn rel_str(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Digest over (relative path, sha256 of bytes) pairs in sorted order — agent-importers' tree digest.
pub fn digest_pairs<'a>(items: impl IntoIterator<Item = (&'a String, &'a String)>) -> String {
    let mut h = Sha256::new();
    for (rel, sha) in items {
        h.update(rel.as_bytes());
        h.update([0]);
        h.update(sha.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().map(|x| format!("{x:02x}")).collect()
}

pub fn tree_digest_files(files: &BTreeMap<String, Vec<u8>>) -> String {
    let hashed: BTreeMap<String, String> = files
        .iter()
        .map(|(k, v)| (k.clone(), sha256_hex(v)))
        .collect();
    digest_pairs(&hashed)
}

pub fn read_tree(dir: &Path) -> Res<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for rel in files_sorted(dir) {
        out.insert(rel_str(&rel), read(&dir.join(&rel))?);
    }
    Ok(out)
}

/// Temp file in the same directory + rename. An existing file keeps its permission bits; `mode` sets new ones.
/// Every output write: [`crate::core::secrets::guard`] (rule B) first, so a secret value never reaches disk.
pub fn write_atomic(p: &Path, data: &[u8], mode: Option<u32>) -> Res<()> {
    crate::core::secrets::guard(&p.display().to_string(), data)?;
    write_unguarded(p, data, mode)
}

fn write_unguarded(p: &Path, data: &[u8], mode: Option<u32>) -> Res<()> {
    let dir = p.parent().ok_or("no parent")?;
    fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    // An existing file keeps its permission bits; otherwise `mode`; set on the new file before any data is written.
    #[cfg(unix)]
    let m = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p)
            .ok()
            .map(|m| m.permissions().mode())
            .or(mode)
    };
    #[cfg(not(unix))]
    let _ = mode;
    // Unpredictable temp name, created exclusively (O_EXCL): never opens or follows a file or symlink already there.
    p.file_name().ok_or("no file name")?;
    let (tmp, mut f) = loop {
        let tmp = dir.join(format!(".botport-tmp.{}", random_hex()));
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(m.unwrap_or(0o666) & 0o7777);
        }
        match o.open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("write {}: {e}", tmp.display())),
        }
    };
    let done = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(m) = m {
            // the umask may have narrowed the creation mode; set it exactly, on the open file
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(m & 0o7777))?;
        }
        f.write_all(data)?;
        f.sync_all().ok();
        Ok(())
    })();
    drop(f);
    if let Err(e) = done
        .map_err(|e| format!("write {}: {e}", tmp.display()))
        .and_then(|_| fs::rename(&tmp, p).map_err(|e| format!("rename {}: {e}", p.display())))
    {
        fs::remove_file(&tmp).ok();
        return Err(e);
    }
    Ok(())
}

/// 128 random bits as hex (the OS RNG; a time/pid/counter hash only if /dev/urandom is unreadable).
fn random_hex() -> String {
    let mut b = [0u8; 16];
    let ok = fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_ok();
    if !ok {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let h = Sha256::digest(format!("{t}:{}:{n}:{:p}", std::process::id(), &b).as_bytes());
        b.copy_from_slice(&h[..16]);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A plain name for one path component (a profile, a skill category or name): not empty, no `/` or `\`, no
/// leading `.` (so neither `.` nor `..`), no NUL.
pub fn plain_name(what: &str, s: &str) -> Res<()> {
    if s.is_empty() || s.starts_with('.') || s.contains(['/', '\\', '\0']) {
        return err(format!(
            "invalid {what} {s:?}: must be a plain name (no '/', no leading '.', not empty)"
        ));
    }
    Ok(())
}

/// A relative path that stays below its root: not empty, not absolute, every `/`-separated component non-empty
/// and not `.` or `..`, no `\` or NUL.
pub fn safe_rel(what: &str, rel: &str) -> Res<()> {
    if rel.is_empty()
        || rel.contains(['\\', '\0'])
        || rel
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return err(format!(
            "unsafe {what} path {rel:?}: must stay below its directory"
        ));
    }
    Ok(())
}

/// Pretty JSON with sorted keys (serde_json::Map is a BTreeMap here) and a trailing newline.
pub fn json_sorted<T: serde::Serialize>(v: &T) -> String {
    let v = serde_json::to_value(v).expect("serializable");
    serde_json::to_string_pretty(&v).expect("json") + "\n"
}

/// One JSON object per line, keys sorted.
pub fn jsonl_line(v: &serde_json::Value) -> String {
    serde_json::to_string(v).expect("json") + "\n"
}

pub fn short(d: &str) -> &str {
    &d[..d.len().min(12)]
}

/// UTC "YYYYMMDDTHHMMSSZ" (only used for restore-point file names; never in hashed content).
pub fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    // civil-from-days (Howard Hinnant)
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Minimal flag parser: `--key value`, `--flag`, positionals. Unknown flags are an error.
pub struct Args {
    pub pos: Vec<String>,
    vals: BTreeMap<String, Vec<String>>,
    flags: Vec<String>,
}

impl Args {
    pub fn parse(raw: &[String], valued: &[&str], flags: &[&str]) -> Res<Args> {
        let mut a = Args {
            pos: vec![],
            vals: BTreeMap::new(),
            flags: vec![],
        };
        let mut i = 0;
        while i < raw.len() {
            let s = &raw[i];
            if let Some(k) = s.strip_prefix("--") {
                let (k, inline) = match k.split_once('=') {
                    Some((k, v)) => (k, Some(v.to_string())),
                    None => (k, None),
                };
                if valued.contains(&k) {
                    let v = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            raw.get(i).cloned().ok_or(format!("--{k} needs a value"))?
                        }
                    };
                    a.vals.entry(k.to_string()).or_default().push(v);
                } else if flags.contains(&k) {
                    a.flags.push(k.to_string());
                } else {
                    return err(format!("unknown option --{k}"));
                }
            } else if s == "-p" && valued.contains(&"profile") {
                i += 1;
                a.vals
                    .entry("profile".into())
                    .or_default()
                    .push(raw.get(i).cloned().ok_or("-p needs a value")?);
            } else {
                a.pos.push(s.clone());
            }
            i += 1;
        }
        Ok(a)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.vals.get(k).and_then(|v| v.last()).map(|s| s.as_str())
    }
    pub fn path(&self, k: &str) -> Option<PathBuf> {
        self.get(k).map(expand)
    }
    pub fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|f| f == k)
    }
}

pub fn expand(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(r) => home().join(r),
        None => PathBuf::from(s),
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Lowercase hex of `b`.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Bytes of a hex string (None when it is not hex).
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn plain_names_and_safe_rels() {
        for ok in ["demo", "my-bot_2", "a.b"] {
            assert!(super::plain_name("profile", ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "a\\b"] {
            assert!(super::plain_name("profile", bad).is_err(), "{bad}");
        }
        for ok in ["SKILL.md", "references/x.md"] {
            assert!(super::safe_rel("skill file", ok).is_ok(), "{ok}");
        }
        for bad in ["", "/etc/x", "../x", "a/../../x", "a//b", "./a", "a\\..\\b"] {
            assert!(super::safe_rel("skill file", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn write_atomic_handles_a_212_byte_file_name() {
        let d = std::env::temp_dir().join(format!("botport-longname-{}", std::process::id()));
        let p = d.join(format!("{}.md", "r".repeat(212)));
        super::write_atomic(&p, b"long", None).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"long");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_never_follows_a_planted_temp_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("botport-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let outside = d.join("outside.txt");
        std::fs::write(&outside, "keep").unwrap();
        // the old predictable temp name, and any name an attacker might guess
        for t in [".f.txt.g2h-new", ".f.txt.botport-tmp"] {
            std::os::unix::fs::symlink(&outside, d.join(t)).unwrap();
        }
        super::write_atomic(&d.join("f.txt"), b"new", Some(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "new");
        assert!(!std::fs::symlink_metadata(d.join("f.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
        let m = std::fs::metadata(d.join("f.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
        std::fs::remove_dir_all(&d).unwrap();
    }

    use super::*;

    #[test]
    fn digest_is_order_independent_of_insertion() {
        let mut a = BTreeMap::new();
        a.insert("b".to_string(), b"2".to_vec());
        a.insert("a".to_string(), b"1".to_vec());
        let mut b = BTreeMap::new();
        b.insert("a".to_string(), b"1".to_vec());
        b.insert("b".to_string(), b"2".to_vec());
        assert_eq!(tree_digest_files(&a), tree_digest_files(&b));
    }

    #[test]
    fn stamp_shape() {
        let s = utc_stamp();
        assert_eq!(s.len(), 16);
        assert!(s.ends_with('Z'));
    }

    #[test]
    fn args_parse() {
        let raw: Vec<String> = ["x", "--profile", "demo", "--json", "--out=o"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = Args::parse(&raw, &["profile", "out"], &["json"]).unwrap();
        assert_eq!(a.get("profile"), Some("demo"));
        assert_eq!(a.get("out"), Some("o"));
        assert!(a.has("json"));
        assert_eq!(a.pos, vec!["x"]);
        assert!(Args::parse(&["--nope".to_string()], &[], &[]).is_err());
    }
}
