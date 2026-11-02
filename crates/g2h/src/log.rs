//! Append-only JSONL log with size-based rotation (`log`, `log.1` … `log.N`).
//! Every string in a record passes through secret redaction before writing.

use crate::secrets::{redact, Fingerprints};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct Logger {
    pub path: PathBuf,
    pub max_bytes: u64,
    /// Rotated files kept besides the live one.
    pub keep: usize,
    pub fingerprints: Option<Fingerprints>,
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

impl Logger {
    pub fn new(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> Self {
        Logger {
            path: path.into(),
            max_bytes,
            keep,
            fingerprints: None,
        }
    }

    fn scrub(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = redact(s, self.fingerprints.as_ref()).0,
            Value::Array(a) => a.iter_mut().for_each(|x| self.scrub(x)),
            Value::Object(m) => {
                let old = std::mem::take(m);
                for (k, mut x) in old {
                    self.scrub(&mut x);
                    m.insert(redact(&k, self.fingerprints.as_ref()).0, x);
                }
            }
            _ => {}
        }
    }

    fn rotate(&self) -> io::Result<()> {
        if self.keep == 0 {
            return fs::remove_file(&self.path);
        }
        let oldest = rotated(&self.path, self.keep);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        for n in (1..self.keep).rev() {
            let from = rotated(&self.path, n);
            if from.exists() {
                fs::rename(&from, rotated(&self.path, n + 1))?;
            }
        }
        fs::rename(&self.path, rotated(&self.path, 1))
    }

    pub fn write(&self, record: &Value) -> io::Result<()> {
        let mut rec = record.clone();
        self.scrub(&mut rec);
        let mut line = serde_json::to_string(&rec)?;
        line.push('\n');
        if let Ok(md) = fs::metadata(&self.path) {
            if md.len() > 0 && md.len() + line.len() as u64 > self.max_bytes {
                self.rotate()?;
            }
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        f.write_all(line.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("g2h-log-{tag}-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn redacts_and_appends() {
        let d = tmpdir("r");
        let p = d.join("g.jsonl");
        let lg = Logger::new(&p, 1 << 20, 2);
        let key = format!("sk-{}", "a1B2".repeat(8));
        lg.write(&json!({"body": format!("my key {key}"), "n": 1}))
            .unwrap();
        lg.write(&json!({"body": "two"})).unwrap();
        let s = fs::read_to_string(&p).unwrap();
        assert_eq!(s.lines().count(), 2);
        assert!(!s.contains(&key) && s.contains("[REDACTED]"));
        fs::remove_file(&p).unwrap();
        fs::remove_dir(&d).unwrap();
    }

    #[test]
    fn rotates_keeping_n() {
        let d = tmpdir("rot");
        let p = d.join("g.jsonl");
        let lg = Logger::new(&p, 40, 2);
        for i in 0..10 {
            lg.write(&json!({"i": i, "pad": "xxxxxxxxxxxxxxxx"}))
                .unwrap();
        }
        assert!(p.exists() && rotated(&p, 1).exists() && rotated(&p, 2).exists());
        assert!(!rotated(&p, 3).exists());
        assert!(fs::read_to_string(&p).unwrap().contains("\"i\":9"));
        for f in [p.clone(), rotated(&p, 1), rotated(&p, 2)] {
            fs::remove_file(f).unwrap();
        }
        fs::remove_dir(&d).unwrap();
    }

    #[test]
    fn scrubs_object_keys_and_creates_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("k");
        let p = d.join("g.jsonl");
        let lg = Logger::new(&p, 1 << 20, 2);
        let key = format!("sk-{}", "a1B2".repeat(8));
        lg.write(&json!({ key.clone(): 1, "nested": { key.clone(): "v" } }))
            .unwrap();
        let s = fs::read_to_string(&p).unwrap();
        assert!(!s.contains(&key), "{s}");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&p).unwrap();
        fs::remove_dir(&d).unwrap();
    }
}
