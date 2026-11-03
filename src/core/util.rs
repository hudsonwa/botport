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

/// The uid this process's files get (the running user), found once from a file it creates; None if that fails.
#[cfg(unix)]
pub fn my_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    static UID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    if let Some(u) = UID.get() {
        return Some(*u);
    }
    // only a found uid is kept: a temp dir that is briefly unwritable is tried again next time
    let found = (|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("botport-uid-{}-{nanos}", std::process::id()));
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .ok()?;
        let uid = f.metadata().ok().map(|m| m.uid());
        drop(f);
        fs::remove_file(&p).ok();
        uid
    })();
    found.map(|u| *UID.get_or_init(|| u))
}

/// Read `dir/rel` only if it is a regular file reached through no symlink and inside `root` (the canonical
/// `dir`). The opened file must be the same inode `lstat` saw, so a swap to a symlink in between is refused.
pub fn read_confined(root: &Path, dir: &Path, rel: &str) -> Result<Vec<u8>, Confined> {
    read_confined_if(root, dir, rel, |_| true)
}

/// [`read_confined`], also refusing a file whose metadata (of the file actually opened) fails `ok`.
pub fn read_confined_if(
    root: &Path,
    dir: &Path,
    rel: &str,
    ok: impl Fn(&fs::Metadata) -> bool,
) -> Result<Vec<u8>, Confined> {
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
    if !fmd.is_file() || !ok(&fmd) {
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

/// The per-profile operation lock file: commands that write into a profile hold it; plan/drift never touch it.
pub const OP_LOCK: &str = ".botport.lock";

/// Profile dirs this process holds the lock on: a nested acquire (e.g. the two halves of `sync --direction both`)
/// shares the outer lock instead of blocking on it.
static HELD: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
/// Set once: locks taken from now on are kept until the process exits (one lock across several steps).
static KEEP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// An exclusive per-profile lock: an OS file lock (`flock`) on `<dir>/.botport.lock`. The file is never deleted;
/// the kernel releases the lock when the holder exits, however it exits, so there is no stale-lock logic.
/// The holder's `pid=<pid> since=<unix secs>` is written into the file only to name it in the error message.
pub struct OpLock {
    dir: PathBuf,
    file: Option<fs::File>,
}

impl OpLock {
    /// Take the lock, or fail fast naming the holder. A symlink or non-regular file at the lock path is refused.
    pub fn acquire(dir: &Path) -> Res<OpLock> {
        Self::try_acquire(dir)?.ok_or_else(|| {
            format!(
                "{} is not writable (could not create the lock file {OP_LOCK})",
                dir.display()
            )
        })
    }

    /// Like `acquire`, but `None` when the lock file doesn't exist and the dir is read-only: a run that turns out
    /// to write nothing can go ahead, and one that does write fails on its first write.
    pub fn try_acquire(dir: &Path) -> Res<Option<OpLock>> {
        let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if held.contains(&key) {
            return Ok(Some(OpLock {
                dir: key,
                file: None,
            }));
        }
        let p = dir.join(OP_LOCK);
        let Some((f, writable)) = open_lock_file(&p)? else {
            eprintln!(
                "botport: note: {} is read-only; the operation lock {OP_LOCK} was not created",
                dir.display()
            );
            return Ok(None);
        };
        match f.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                let mut b = Vec::new();
                std::io::Read::take(&f, 256).read_to_end(&mut b).ok();
                let who = String::from_utf8_lossy(&b);
                let who = who.split_whitespace().collect::<Vec<_>>().join(" ");
                return err(format!(
                    "another botport run holds the lock {} ({}); wait for it to finish (don't delete the file: the lock is released when that run exits)",
                    p.display(),
                    if who.is_empty() { "holder unknown" } else { &who }
                ));
            }
            Err(fs::TryLockError::Error(e)) => return err(format!("lock {}: {e}", p.display())),
        }
        // write the pid line only into a plain file of our own: a hard link planted at the lock path (nlink > 1) or a
        // file owned by someone other than the profile dir's owner is locked but never written through
        let own_plain_file = {
            use std::os::unix::fs::MetadataExt;
            match (f.metadata(), fs::metadata(dir)) {
                (Ok(m), Ok(d)) => m.nlink() == 1 && m.uid() == d.uid(),
                _ => false,
            }
        };
        if writable && own_plain_file {
            let since = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // only for the message another run prints; a failure here doesn't weaken the lock
            f.set_len(0).ok();
            std::io::Seek::seek(&mut &f, std::io::SeekFrom::Start(0)).ok();
            writeln!(&f, "pid={} since={since}", std::process::id()).ok();
        }
        held.push(key.clone());
        Ok(Some(OpLock {
            dir: key,
            file: Some(f),
        }))
    }

    /// Keep every lock taken from now on until the process exits (the kernel releases it then).
    pub fn keep_until_exit() {
        KEEP.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

use std::io::Read as _;

impl Drop for OpLock {
    fn drop(&mut self) {
        let Some(f) = self.file.take() else { return };
        if KEEP.load(std::sync::atomic::Ordering::SeqCst) {
            std::mem::forget(f);
            return;
        }
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        held.retain(|d| d != &self.dir);
        f.unlock().ok();
    }
}

/// Open (or create, 0600) the lock file without following a symlink: refuse anything but a regular file, and
/// check the opened file is the one that was inspected. `None`: missing and the dir can't take a new file.
/// The bool says whether it opened for writing (a lock file owned by another user opens read-only).
fn open_lock_file(p: &Path) -> Res<Option<(fs::File, bool)>> {
    let refuse = |what: &str| {
        format!(
            "{} is {what}, not a lock file; refusing to use it (move it aside)",
            p.display()
        )
    };
    for _ in 0..3 {
        let m = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut o = fs::OpenOptions::new();
                o.read(true).write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
                match o.open(p) {
                    Ok(f) => return Ok(Some((f, true))),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::ReadOnlyFilesystem
                        ) =>
                    {
                        return Ok(None);
                    }
                    Err(e) => return err(format!("lock {}: {e}", p.display())),
                }
            }
            Err(e) => return err(format!("lock {}: {e}", p.display())),
        };
        if m.file_type().is_symlink() {
            return Err(refuse("a symlink"));
        }
        if !m.is_file() {
            return Err(refuse("not a regular file"));
        }
        let (f, w) = match fs::OpenOptions::new().read(true).write(true).open(p) {
            Ok(f) => (f, true),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => (
                fs::File::open(p).map_err(|e| format!("lock {}: {e}", p.display()))?,
                false,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return err(format!("lock {}: {e}", p.display())),
        };
        let fm = f
            .metadata()
            .map_err(|e| format!("lock {}: {e}", p.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if (fm.dev(), fm.ino()) != (m.dev(), m.ino()) {
                continue; // swapped between the check and the open: look again
            }
        }
        if !fm.is_file() {
            return Err(refuse("not a regular file"));
        }
        return Ok(Some((f, w)));
    }
    err(format!(
        "lock {}: it keeps changing; try again",
        p.display()
    ))
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

/// Write a bundle file that may hold personal data (memories, facts, chat history, the bot's description): the file
/// 0600, and `root` plus every directory below it on the way to the file 0700. A directory on the way that is a
/// symlink or not a directory is refused.
pub fn write_private(root: &Path, rel: &str, data: &[u8]) -> Res<()> {
    safe_rel("bundle file", rel)?;
    fs::create_dir_all(root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
    private_dir(root, fs::metadata(root).ok())?;
    let mut d = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for c in &parts[..parts.len() - 1] {
        d.push(c);
        let m = fs::symlink_metadata(&d).ok();
        if m.is_none() {
            fs::create_dir(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
        }
        private_dir(&d, m.or_else(|| fs::symlink_metadata(&d).ok()))?;
    }
    write_atomic(&root.join(rel), data, Some(0o600))
}

/// Create `d` (and its parents) if needed and make it a private directory (0700); a symlink or non-directory is
/// refused.
pub fn ensure_private_dir(d: &Path) -> Res<()> {
    fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
    private_dir(d, fs::symlink_metadata(d).ok())
}

fn private_dir(d: &Path, m: Option<fs::Metadata>) -> Res<()> {
    if !m.is_some_and(|m| m.is_dir()) {
        return err(format!(
            "{} is not a directory (or is a symlink)",
            d.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod {}: {e}", d.display()))?;
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
    let (y, m, d, hh, mm, ss) = utc_civil(now_secs());
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Seconds since the Unix epoch, now (0 if the clock is before it).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix seconds -> UTC (year, month, day, hour, minute, second).
pub fn utc_civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let r = rem as u32;
    (y, m as u32, d as u32, r / 3600, r % 3600 / 60, r % 60)
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" now.
pub fn utc_iso() -> String {
    iso_utc(now_secs())
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" of unix seconds `secs`.
pub fn utc_iso_at(secs: u64) -> String {
    iso_utc(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Unix seconds of a time in exactly the form "YYYY-MM-DDTHH:MM:SSZ" (None for any other form).
pub fn parse_utc_iso(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    if b.len() != 20 || !digits.iter().all(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    // strict: the time must print back exactly as given (no 31 February, no second 60)
    let secs = parse_iso(s).and_then(|x| u64::try_from(x).ok())?;
    (utc_iso_at(secs) == s).then_some(secs)
}

/// UTC date -> days since the Unix epoch (days-from-civil, Howard Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds -> "YYYY-MM-DDTHH:MM:SSZ".
pub fn iso_utc(secs: i64) -> String {
    let (y, m, d, hh, mm, ss) = utc_civil(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// "YYYY-MM-DD[(T| )HH:MM[:SS[.f]]][Z|±HH:MM|±HHMM]" -> Unix seconds; no offset is read as UTC.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |a: usize, b: usize| -> Option<i64> {
        let t = s.get(a..b)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if s.get(4..5)? != "-"
        || s.get(7..8)? != "-"
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
    {
        return None;
    }
    let mut secs = days_from_civil(y, mo as u32, d as u32) * 86400;
    let mut rest = &s[10..];
    if let Some(t) = rest.strip_prefix(['T', ' ']) {
        let hh = t.get(0..2)?.parse::<i64>().ok()?;
        let mm = t.get(3..5)?.parse::<i64>().ok()?;
        if t.get(2..3)? != ":" || hh > 23 || mm > 59 {
            return None;
        }
        secs += hh * 3600 + mm * 60;
        rest = &t[5..];
        if let Some(r) = rest.strip_prefix(':') {
            let ss = r.get(0..2)?.parse::<i64>().ok()?;
            if ss > 60 {
                return None;
            }
            secs += ss;
            rest = r[2..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        }
    }
    match rest {
        "" | "Z" | "z" => Some(secs),
        o if o.starts_with(['+', '-']) => {
            let digits: String = o[1..].chars().filter(|c| *c != ':').collect();
            if digits.len() != 4 || !digits.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let off =
                digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60;
            Some(if o.starts_with('+') {
                secs - off
            } else {
                secs + off
            })
        }
        _ => None,
    }
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
    /// Every value given for the valued option `k`, in order.
    pub fn all(&self, k: &str) -> &[String] {
        self.vals.get(k).map_or(&[], Vec::as_slice)
    }
    pub fn path(&self, k: &str) -> Option<PathBuf> {
        self.get(k).map(expand)
    }
    pub fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|f| f == k)
    }
}

/// The origin of a url (scheme, host and port, lowercased): `https://mcp.example.test:8443`. Credentials kept for one
/// origin never go to another. `None`, "no origin", for anything a browser-style parser could read differently from a
/// plain one: a scheme other than http or https, userinfo (an `@` in the authority), a backslash, percent-encoding, or
/// a host that is not ASCII letters, digits and `-` in non-empty labels (or a bracketed IPv6 literal). Every
/// comparison treats "no origin" as a move to another origin.
pub fn url_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if rest.contains('\\') {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    if auth.is_empty() || auth.contains(['@', '%']) || !auth.is_ascii() {
        return None;
    }
    let hp = auth.to_ascii_lowercase();
    // host and port; an IPv6 host is in brackets
    let (host, port) = match hp.rfind(':') {
        Some(i)
            if !hp[i + 1..].is_empty()
                && hp[i + 1..].bytes().all(|b| b.is_ascii_digit())
                && (!hp.starts_with('[') || hp[..i].ends_with(']')) =>
        {
            (hp[..i].to_string(), hp[i + 1..].to_string())
        }
        _ => (hp.clone(), String::new()),
    };
    // spellings of one origin: the scheme's default port, a trailing dot on the host
    let host = host.strip_suffix('.').unwrap_or(&host);
    // a host is a bracketed IPv6 literal, or labels of ASCII letters, digits and '-' (none empty)
    let ok = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => {
            !v6.is_empty()
                && v6
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        }
        None => {
            !host.is_empty()
                && host.split('.').all(|l| {
                    !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
        }
    };
    if !ok {
        return None;
    }
    let default = matches!(
        (scheme.as_str(), port.as_str()),
        ("https", "443") | ("http", "80")
    );
    Some(if port.is_empty() || default {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    })
}

/// Whether two urls share one origin; "no origin" on either side never does.
pub fn same_origin(a: &str, b: &str) -> bool {
    matches!((url_origin(a), url_origin(b)), (Some(x), Some(y)) if x == y)
}

/// An external program (`sqlite3`, `curl`, a token command) to run by absolute path: a name with a `/` is taken as
/// given; a bare name is looked up in `PATH`, skipping entries that are empty or relative (".") and directories anyone
/// else can write to, so a planted file in the working directory or a shared folder is never run.
pub fn program(name: &str) -> Res<PathBuf> {
    program_in(name, &std::env::var_os("PATH").unwrap_or_default())
}

/// A tool botport depends on, found as [`program`] finds it and at least the given version (`sqlite3` 3.33 for
/// `-json`, `curl` 7.52); the version is read once per process.
pub fn tool(name: &str) -> Res<PathBuf> {
    static SEEN: std::sync::Mutex<Vec<(String, PathBuf)>> = std::sync::Mutex::new(Vec::new());
    if let Some((_, p)) = SEEN
        .lock()
        .ok()
        .and_then(|v| v.iter().find(|(n, _)| n == name).cloned())
    {
        return Ok(p);
    }
    // by its file name, so a configured full path to curl or sqlite3 gets the same version check
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name);
    let (arg, min): (&str, (u32, u32)) = match base {
        "sqlite3" => ("-version", (3, 33)),
        "curl" => ("--version", (7, 52)),
        // another program: the same file and folder checks, no version check
        _ => return program(name),
    };
    let p = program(name)?;
    let out = std::process::Command::new(&p)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{name}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let have = version_in(&text, name).ok_or(format!("{name}: could not read its version"))?;
    if have < min {
        return err(format!(
            "{name} {}.{} is too old: botport needs {}.{} or later",
            have.0, have.1, min.0, min.1
        ));
    }
    if let Ok(mut v) = SEEN.lock() {
        v.push((name.to_string(), p.clone()));
    }
    Ok(p)
}

/// The major and minor version in a tool's version output (`3.99.0 2000-01-01 ...` or `curl 9.9.9 (...)`).
fn version_in(text: &str, name: &str) -> Option<(u32, u32)> {
    let word = text
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))?;
    let _ = name;
    let mut parts = word.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

/// [`program`] with the search path given.
pub fn program_in(name: &str, path: &std::ffi::OsStr) -> Res<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        if !p.is_absolute() {
            return err(format!(
                "{name}: a program given with a path must be absolute"
            ));
        }
        let dir = p.parent().ok_or(format!("{name}: no folder"))?;
        return if safe_dir(dir) && safe_program(&p) {
            Ok(p)
        } else {
            err(format!("{name}: refused (its folder or the file is writable by others or owned by someone else)"))
        };
    }
    let mut skipped = vec![];
    for dir in std::env::split_paths(path) {
        // a leading `~/` is the home folder, as a shell would read it
        let dir = match dir.to_str().and_then(|d| d.strip_prefix("~/")) {
            Some(rest) => home().join(rest),
            None => dir,
        };
        // a PATH entry that is itself a link is not followed
        if !dir.is_absolute()
            || fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink())
        {
            continue;
        }
        let p = dir.join(name);
        if !fs::metadata(&p).is_ok_and(|m| m.is_file()) {
            continue;
        }
        if safe_dir(&dir) && safe_program(&p) {
            return Ok(p);
        }
        skipped.push(dir.display().to_string());
    }
    if skipped.is_empty() {
        err(format!("{name} not found in PATH"))
    } else {
        err(format!(
            "{name} not found in PATH (skipped, writable by others or owned by someone else: {})",
            skipped.join(", ")
        ))
    }
}

/// A folder to take a program from: every component of its path, from the root down and without following links,
/// is a real folder owned by root or this user that no one else can write to (a sticky folder such as /tmp counts as
/// writable: it is not trusted). A link on the way is followed only when root made it (merged `/usr`, system
/// profiles), and its target is then checked the same way.
fn safe_dir(dir: &Path) -> bool {
    safe_dir_at(dir, 0)
}

fn safe_dir_at(dir: &Path, depth: u8) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if depth > 8 || !dir.is_absolute() {
            return false;
        }
        let parts: Vec<std::path::Component> = dir.components().collect();
        let mut at = PathBuf::new();
        for (i, c) in parts.iter().enumerate() {
            at.push(c);
            let Ok(m) = fs::symlink_metadata(&at) else {
                return false;
            };
            if m.file_type().is_symlink() {
                if m.uid() != 0 {
                    return false;
                }
                let Ok(t) = fs::read_link(&at) else {
                    return false;
                };
                let mut target = if t.is_absolute() {
                    t
                } else {
                    at.parent().unwrap_or(Path::new("/")).join(t)
                };
                for rest in &parts[i + 1..] {
                    target.push(rest);
                }
                return safe_dir_at(&target, depth + 1);
            }
            let ok = m.is_dir()
                && m.permissions().mode() & 0o022 == 0
                && (m.uid() == 0 || Some(m.uid()) == my_uid());
            if !ok {
                return false;
            }
        }
        true
    }
    #[cfg(not(unix))]
    {
        let _ = depth;
        dir.is_dir()
    }
}

/// A program file: a regular file owned by root or this user that no one else can write to; one reached through a
/// link must also sit in a safe folder (see [`safe_dir`]) at its real place.
fn safe_program(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let file_ok = fs::metadata(p).is_ok_and(|m| {
            m.is_file()
                && m.permissions().mode() & 0o022 == 0
                && (m.uid() == 0 || Some(m.uid()) == my_uid())
        });
        let linked = fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
        file_ok
            && (!linked
                || fs::canonicalize(p)
                    .ok()
                    .and_then(|r| r.parent().map(Path::to_path_buf))
                    .is_some_and(|d| safe_dir(&d)))
    }
    #[cfg(not(unix))]
    {
        p.is_file()
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

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648, with padding) of `b`.
pub fn b64(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Bytes of standard padded base64 (None when it is not exactly that).
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let val = |c: u8| B64.iter().position(|&x| x == c).map(|p| p as u32);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (k, q) in s.chunks(4).enumerate() {
        let last = k == s.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = n << 6 | val(c)?;
        }
        n <<= 6 * pad as u32;
        // canonical only: the bits a padded group leaves over must be zero
        if (pad == 1 && n & 0xff != 0) || (pad == 2 && n & 0xffff != 0) {
            return None;
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_versions_are_read_from_their_output() {
        assert_eq!(
            super::version_in("3.99.0 2000-01-01 00:00:00 example (64-bit)", "sqlite3"),
            Some((3, 99))
        );
        assert_eq!(
            super::version_in(
                "curl 9.9.9 (example-target) libcurl/9.9.9\nRelease-Date: 2000-01-01",
                "curl"
            ),
            Some((9, 9))
        );
        assert_eq!(super::version_in("no version here", "curl"), None);
        assert!((3, 32) < (3, 33) && (7, 52) >= (7, 52));
    }

    #[cfg(unix)]
    #[test]
    fn programs_come_only_from_absolute_path_entries_nobody_else_can_write() {
        use std::os::unix::fs::PermissionsExt;
        // under the crate, not a temp folder: /tmp is sticky and world-writable, which the rule does not trust
        let base = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
            .unwrap()
            .join(format!(".botport-prog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !super::safe_dir(&base) {
            // a checkout under a folder others can write to cannot host a trusted program folder at all
            eprintln!(
                "skipped: {} is not under folders only root or this user can write",
                base.display()
            );
            std::fs::remove_dir_all(&base).ok();
            return;
        }
        let (open, good) = (base.join("open"), base.join("good"));
        for d in [&open, &good] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("demo-tool"), "#!/bin/sh\n").unwrap();
            // explicit modes: the result must not depend on the umask
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(d.join("demo-tool"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let path =
            std::env::join_paths([std::path::PathBuf::from("."), open.clone(), good.clone()])
                .unwrap();
        assert_eq!(
            super::program_in("demo-tool", &path).unwrap(),
            good.join("demo-tool")
        );
        let only_open = std::env::join_paths([open.clone()]).unwrap();
        // a PATH entry that is a link is not followed, even to a safe folder
        let linked = base.join("linked");
        std::os::unix::fs::symlink(&good, &linked).unwrap();
        assert!(super::program_in("demo-tool", &std::env::join_paths([linked]).unwrap()).is_err());
        assert!(super::program_in("demo-tool", &only_open).is_err());
        // a program given with a path gets the same checks, and must be absolute
        assert_eq!(
            super::program_in(good.join("demo-tool").to_str().unwrap(), &only_open).unwrap(),
            good.join("demo-tool")
        );
        assert!(super::program_in(open.join("demo-tool").to_str().unwrap(), &only_open).is_err());
        assert!(super::program_in("bin/demo-tool", &only_open).is_err());
        // a link in a program's path is followed only when root made it (run as root, every link is root's)
        if super::my_uid() != Some(0) {
            let user_link = base.join("user-link");
            std::os::unix::fs::symlink(&good, &user_link).unwrap();
            assert!(
                super::program_in(user_link.join("demo-tool").to_str().unwrap(), &only_open)
                    .is_err()
            );
        }
        // a program file others can write is skipped like its folder would be
        std::fs::set_permissions(
            good.join("demo-tool"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(
            super::program_in("demo-tool", &std::env::join_paths([good.clone()]).unwrap()).is_err()
        );
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn base64_round_trips() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(super::b64(raw.as_bytes()), enc);
            assert_eq!(super::unb64(enc).unwrap(), raw.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(super::unb64(&super::b64(&all)).unwrap(), all);
        // not exactly canonical padded base64
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9!", "Zm 9", "Zh==", "Zm9="] {
            assert!(super::unb64(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn strict_utc_times() {
        assert_eq!(
            super::parse_utc_iso("2000-01-01T00:00:00Z"),
            Some(946_684_800)
        );
        for bad in [
            "2000-02-31T00:00:00Z",
            "2000-01-01T00:00:60Z",
            "2000-01-01T+5:00:00Z",
            "2000-01-01 00:00:00Z",
        ] {
            assert_eq!(super::parse_utc_iso(bad), None, "{bad}");
        }
    }

    #[test]
    fn utc_dates_round_trip() {
        for t in [0, 951_782_400, 946_684_800, 4_102_444_799] {
            assert_eq!(super::parse_iso(&super::iso_utc(t)), Some(t), "{t}");
        }
        assert_eq!(super::iso_utc(946_684_800), "2000-01-01T00:00:00Z");
        let p = super::parse_iso;
        assert_eq!(p("2000-01-01"), p("2000-01-01T00:00:00Z"));
        assert_eq!(
            p("2000-01-01T07:30:00.123456+02:00"),
            p("2000-01-01T05:30:00Z")
        );
        assert_eq!(p("2000-01-01 05:30"), p("2000-01-01T05:30:00Z"));
        for bad in [
            "",
            "2000-13-01",
            "00-01-01",
            "2000-01-01T5:30",
            "2000-01-01Tx",
            "2000-01-01T05:30 later",
        ] {
            assert_eq!(p(bad), None, "{bad}");
        }
    }

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
    fn a_url_origin_is_scheme_host_and_port() {
        let o = |u: &str| super::url_origin(u);
        assert_eq!(
            o("https://MCP.Example.test/v2?x=1").as_deref(),
            Some("https://mcp.example.test")
        );
        assert_eq!(
            o("https://mcp.example.test:8443/mcp").as_deref(),
            Some("https://mcp.example.test:8443")
        );
        assert_ne!(
            o("http://mcp.example.test/"),
            o("https://mcp.example.test/")
        );
        // spellings of one origin
        for u in [
            "https://mcp.example.test:443/",
            "https://mcp.example.test./x",
        ] {
            assert_eq!(o(u).as_deref(), Some("https://mcp.example.test"), "{u}");
        }
        assert_eq!(
            o("http://[::1]:8080/").as_deref(),
            Some("http://[::1]:8080")
        );
        assert_eq!(o("http://[::1]/").as_deref(), Some("http://[::1]"));
        // what a browser-style parser could read as another host: no origin, never the same origin as anything
        for u in [
            "https://attacker.example\\@mcp.example.test/mcp",
            "https://u@mcp.example.test/",
            "https://u:p@mcp.example.test/",
            "https://mcp.example%2Etest/",
            "https://mcp.ex\u{e4}mple.test/",
            "https:///mcp",
            "mcp.example.test/mcp",
            "ftp://mcp.example.test/",
            "https://mcp..example.test/",
            "https://mcp_example.test/",
            "https://[zz]/",
            "https://[]/",
        ] {
            assert_eq!(o(u), None, "{u}");
            assert!(!super::same_origin(u, u), "{u}");
        }
        assert!(super::same_origin(
            "https://mcp.example.test/a",
            "https://MCP.example.test:443/b"
        ));
    }

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
