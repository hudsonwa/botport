//! Helpers shared by the integration tests: a self-cleaning temp dir and small file shorthands.
#![allow(dead_code)]
use std::fs;
use std::path::{Path, PathBuf};

pub struct Tmp(pub PathBuf);
impl Tmp {
    pub fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("botport-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Write `s` to `p`, creating parent dirs.
pub fn w(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

pub fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}
