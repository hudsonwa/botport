//! Helpers shared by the integration tests: a self-cleaning temp dir and small file shorthands.
#![allow(dead_code)]
use std::fs;
use std::path::{Path, PathBuf};

pub struct Tmp(pub PathBuf);
impl Tmp {
    /// A new private (0700) directory under the system temp directory with a random name, so no other process can
    /// predict it or have made it first.
    pub fn new(tag: &str) -> Tmp {
        use std::io::Read;
        use std::os::unix::fs::DirBuilderExt;
        for _ in 0..16 {
            let mut b = [0u8; 16];
            fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut b))
                .unwrap();
            let r: String = b.iter().map(|x| format!("{x:02x}")).collect();
            let p = std::env::temp_dir().join(format!("botport-test-{tag}-{r}"));
            if fs::DirBuilder::new().mode(0o700).create(&p).is_ok() {
                return Tmp(p);
            }
        }
        panic!("no private test directory for {tag}");
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
