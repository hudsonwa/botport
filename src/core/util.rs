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
    let tmp = dir.join(format!(
        ".{}.g2h-new",
        p.file_name().unwrap().to_string_lossy()
    ));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(data)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all().ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = fs::metadata(p)
            .ok()
            .map(|m| m.permissions().mode())
            .or(mode);
        if let Some(m) = m {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(m & 0o7777)).ok();
        }
    }
    #[cfg(not(unix))]
    let _ = mode;
    fs::rename(&tmp, p).map_err(|e| format!("rename {}: {e}", p.display()))
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
