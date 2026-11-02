//! Home-directory layout. `$G2H_HOME` wins; otherwise `~/.g2h` on every
//! platform (CLI, daemon and setup agree with no env set). A home left at the
//! old platform default (macOS `~/Library/Application Support/g2h`, other Unix
//! `$XDG_STATE_HOME/g2h` or `~/.local/state/g2h`) is moved there once.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
}

/// Resolve the home dir from explicit env values (testable).
pub fn resolve_home(g2h_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(h) = g2h_home.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(h));
    }
    home.filter(|s| !s.is_empty())
        .map(|h| Path::new(h).join(".g2h"))
}

/// The older per-platform default, migrated from on first run.
pub fn legacy_home(home: Option<&str>, xdg_state: Option<&str>, macos: bool) -> Option<PathBuf> {
    if macos {
        return home.map(|h| Path::new(h).join("Library/Application Support/g2h"));
    }
    if let Some(x) = xdg_state.filter(|s| !s.is_empty()) {
        return Some(Path::new(x).join("g2h"));
    }
    home.map(|h| Path::new(h).join(".local/state/g2h"))
}

/// Move `legacy` to `new` by atomic rename when `new` does not exist and
/// `legacy` is a real directory (not a symlink). Returns whether it moved.
pub fn migrate_legacy(legacy: &Path, new: &Path) -> io::Result<bool> {
    if std::fs::symlink_metadata(new).is_ok() {
        return Ok(false);
    }
    match std::fs::symlink_metadata(legacy) {
        Ok(m) if m.is_dir() => {}
        _ => return Ok(false),
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(legacy, new)?;
    Ok(true)
}

/// The canonical form of `home` (symlinks resolved); unchanged if it does not exist yet.
pub fn canonical(home: &Path) -> PathBuf {
    std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf())
}

/// Like `canonical`, but resolves the parent when `p` does not exist yet, so a fresh
/// `$HOME/.g2h` under a symlinked temp dir (macOS `/var` → `/private/var`) still compares equal.
fn canonical_parent(p: &Path) -> PathBuf {
    match (std::fs::canonicalize(p), p.parent(), p.file_name()) {
        (Ok(c), _, _) => c,
        (Err(_), Some(parent), Some(name)) => canonical(parent).join(name),
        _ => p.to_path_buf(),
    }
}

impl Paths {
    pub fn from_env() -> Option<Paths> {
        let g = std::env::var("G2H_HOME").ok().filter(|s| !s.is_empty());
        let h = std::env::var("HOME").ok();
        let home = resolve_home(g.as_deref(), h.as_deref())?;
        // An exported `G2H_HOME=$HOME/.g2h` survives `export HOME=$(mktemp -d)`: say
        // so, or a try-out silently talks to the real daemon.
        if let (Some(g), Some(h)) = (g.as_deref(), h.as_deref().filter(|s| !s.is_empty())) {
            let other_default = Path::new(g).file_name() == Some(".g2h".as_ref());
            if other_default && !canonical_parent(Path::new(g)).starts_with(canonical(Path::new(h)))
            {
                eprintln!("g2h: warning: G2H_HOME={g} is outside HOME={h}; using G2H_HOME (set G2H_HOME=\"$HOME/.g2h\" to isolate)");
            }
        }
        if g.is_none() {
            let x = std::env::var("XDG_STATE_HOME").ok();
            if let Some(old) = legacy_home(h.as_deref(), x.as_deref(), cfg!(target_os = "macos")) {
                match migrate_legacy(&old, &home) {
                    Ok(true) => {
                        eprintln!(
                            "g2h: moved the home {} -> {}",
                            old.display(),
                            home.display()
                        );
                        let _ = crate::log::Logger::new(home.join("g2h.log.jsonl"), 16 << 20, 5)
                            .write(&serde_json::json!({"event": "home_migrated", "from": old, "to": home}));
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!(
                        "g2h: could not move the old home {} -> {}: {e}",
                        old.display(),
                        home.display()
                    ),
                }
            }
        }
        Some(Paths {
            home: canonical(&home),
        })
    }
    pub fn new(home: impl Into<PathBuf>) -> Paths {
        Paths { home: home.into() }
    }
    pub fn socket(&self) -> PathBuf {
        self.home.join("g2hd.sock")
    }
    pub fn log(&self) -> PathBuf {
        self.home.join("g2h.log.jsonl")
    }
    pub fn state(&self) -> PathBuf {
        self.home.join("state.json")
    }
    pub fn config(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn credentials(&self) -> PathBuf {
        self.home.join("credentials")
    }
    /// Production-routine wake credentials; preferred over `credentials` when present.
    pub fn inbox_credentials(&self) -> PathBuf {
        self.home.join("inbox.credentials")
    }
    /// The file `wake` uses: `inbox.credentials` if it exists (even if its
    /// mode is wrong, so a bad file is refused, not skipped), else `credentials`.
    pub fn wake_credentials(&self) -> PathBuf {
        let inbox = self.inbox_credentials();
        if std::fs::symlink_metadata(&inbox).is_ok() {
            inbox
        } else {
            self.credentials()
        }
    }
    pub fn lock(&self) -> PathBuf {
        self.home.join("g2hd.lock")
    }
    pub fn sessions(&self) -> PathBuf {
        self.home.join("sessions.json")
    }
}

/// Create the home dir 0700 if missing; refuse one that is world-writable
/// (or not a directory). An existing dir with looser group bits is tightened to 0700.
pub fn ensure_home(home: &Path) -> io::Result<()> {
    ensure_home_mode(home, 0o700)
}

/// `ensure_home` with another target mode (0750 in separate-user mode).
pub fn ensure_home_mode(home: &Path, want: u32) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !home.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(want)
            .create(home)?;
    }
    // metadata() follows symlinks: a symlinked home is fine.
    let md = std::fs::metadata(home)?;
    if !md.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            home.display()
        )));
    }
    let mode = md.permissions().mode() & 0o777;
    if mode & 0o002 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is world-writable (mode {mode:o}); fix: chmod 700",
                home.display()
            ),
        ));
    }
    if mode != want {
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(want))?;
    }
    Ok(())
}

/// Credential files must be 0600 regular files owned by this process's uid
/// (or root reading as root); a symlink is refused.
pub fn check_perms(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let md = std::fs::symlink_metadata(path)?;
    let me = crate::isolation::euid();
    if md.uid() != me && me != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {}, not uid {me}; fix: chown it to the user g2hd runs as",
                path.display(),
                md.uid()
            ),
        ));
    }
    if md.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is a symlink; must be a regular 0600 file",
                path.display()
            ),
        ));
    }
    let mode = md.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} has mode {:o}; must be 0600", path.display(), mode),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn g2h_home_wins() {
        let p = resolve_home(Some("/x/g"), Some("/h")).unwrap();
        assert_eq!(p, PathBuf::from("/x/g"));
    }
    #[test]
    fn default_is_dot_g2h_on_every_platform() {
        assert_eq!(
            resolve_home(None, Some("/h")).unwrap(),
            PathBuf::from("/h/.g2h")
        );
        assert_eq!(
            resolve_home(Some(""), Some("/h")).unwrap(),
            PathBuf::from("/h/.g2h")
        );
        assert_eq!(resolve_home(None, None), None);
    }
    #[test]
    fn legacy_defaults() {
        assert_eq!(
            legacy_home(Some("/h"), Some("/s"), true).unwrap(),
            PathBuf::from("/h/Library/Application Support/g2h")
        );
        assert_eq!(
            legacy_home(Some("/h"), Some("/s"), false).unwrap(),
            PathBuf::from("/s/g2h")
        );
        assert_eq!(
            legacy_home(Some("/h"), None, false).unwrap(),
            PathBuf::from("/h/.local/state/g2h")
        );
    }
    #[test]
    fn migrates_a_real_legacy_dir_once() {
        let t = tempfile::tempdir().unwrap();
        let (old, new) = (t.path().join("old/g2h"), t.path().join(".g2h"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("config.toml"), "x").unwrap();
        assert!(migrate_legacy(&old, &new).unwrap());
        assert!(new.join("config.toml").exists());
        assert!(!old.exists());
        // new exists now: no-op
        std::fs::create_dir_all(&old).unwrap();
        assert!(!migrate_legacy(&old, &new).unwrap());
        assert!(old.exists());
    }
    #[test]
    fn symlinked_legacy_is_not_migrated() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let old = t.path().join("old");
        std::os::unix::fs::symlink(&real, &old).unwrap();
        let new = t.path().join(".g2h");
        assert!(!migrate_legacy(&old, &new).unwrap());
        assert!(!new.exists());
    }
    #[test]
    fn symlinked_home_works_and_is_canonicalized() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        ensure_home(&link).unwrap();
        assert_eq!(canonical(&link), std::fs::canonicalize(&real).unwrap());
        // a missing home stays as given
        let gone = t.path().join("nope");
        assert_eq!(canonical(&gone), gone);
    }
    #[test]
    fn symlinked_credentials_file_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::write(&real, "x").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = t.path().join("credentials");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(check_perms(&real).is_ok());
        assert!(check_perms(&link).is_err());
    }
    #[test]
    fn file_paths_under_home() {
        let p = Paths::new("/g");
        assert_eq!(p.credentials(), PathBuf::from("/g/credentials"));
        for f in [p.socket(), p.log(), p.state(), p.config()] {
            assert!(f.starts_with("/g"));
        }
    }
    #[test]
    fn credentials_perms() {
        let dir = std::env::temp_dir().join(format!("g2h-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("credentials");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_perms(&f).is_err());
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_perms(&f).is_ok());
        std::fs::remove_file(&f).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn ensure_home_0700_and_refuses_world_writable() {
        let base = std::env::temp_dir().join(format!("g2h-home-{}", std::process::id()));
        let h = base.join("g");
        ensure_home(&h).unwrap();
        assert_eq!(
            std::fs::metadata(&h).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_home(&h).is_err());
        std::fs::remove_dir(&h).unwrap();
        std::fs::remove_dir(&base).unwrap();
    }
}
