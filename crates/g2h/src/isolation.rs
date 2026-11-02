//! Opt-in separate-user mode (`[separate_user]` in config.toml).
//!
//! Default (same-user): g2hd, both bots and the owner share one uid. The socket
//! is 0600 and the home 0700, which keeps other users out but is no boundary
//! against a bot that has a shell as that user: it can edit `config.toml`,
//! swap the approver, or read the state.
//!
//! Separate-user: g2hd runs as its own uid (e.g. `_g2h`). Its home, config,
//! state and approver are owned by that uid and not writable by the clients.
//! The home is 0750 and the socket 0660, both group `socket_gid`, so the
//! owner's user (and the bots running as it) can talk to g2hd; each connection's
//! peer uid (SO_PEERCRED / getpeereid) must be g2hd's own, root, or listed in
//! `allowed_uids`. `approve` is accepted only from g2hd's own uid or root, so a
//! bot running as the owner's user cannot approve its own op even if it learns
//! the code; the code itself goes out through the approver, which runs as g2hd.

use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeparateUser {
    /// Client uids allowed on the socket (the owner's user; the bots run as it).
    pub allowed_uids: Vec<u32>,
    /// Group of the home dir (0750) and socket (0660); the clients must be in it.
    pub socket_gid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    SameUser,
    SeparateUser,
}

impl Mode {
    pub fn of(s: Option<&SeparateUser>) -> Mode {
        if s.is_some() {
            Mode::SeparateUser
        } else {
            Mode::SameUser
        }
    }
    pub fn home_mode(self) -> u32 {
        match self {
            Mode::SameUser => 0o700,
            Mode::SeparateUser => 0o750,
        }
    }
    pub fn socket_mode(self) -> u32 {
        match self {
            Mode::SameUser => 0o600,
            Mode::SeparateUser => 0o660,
        }
    }
}

/// May a connection from `peer` talk to a g2hd running as `own`?
/// Same-user mode keeps the old behaviour (socket perms are the gate).
pub fn peer_allowed(s: Option<&SeparateUser>, own: u32, peer: Option<u32>) -> bool {
    let Some(s) = s else { return true };
    match peer {
        Some(u) => u == own || u == 0 || s.allowed_uids.contains(&u),
        None => false,
    }
}

/// May `peer` run `approve`? In separate-user mode only g2hd's uid or root
/// (`sudo -u _g2h g2h approve …`); the clients' uid is the bots' uid.
pub fn approve_allowed(s: Option<&SeparateUser>, own: u32, peer: Option<u32>) -> bool {
    match (s, peer) {
        (None, _) => true,
        (Some(_), Some(u)) => u == own || u == 0,
        (Some(_), None) => false,
    }
}

/// The effective uid of this process: who g2hd really runs as.
pub fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A path g2hd trusts must be owned by `own` (or root) and not writable by
/// group or others, so no client uid can change it.
fn trusted(path: &Path, own: u32, what: &str) -> Result<(), String> {
    let md = std::fs::metadata(path).map_err(|e| format!("{what} {}: {e}", path.display()))?;
    owned_not_writable(path, &md, own, what, false)
}

fn owned_not_writable(
    path: &Path,
    md: &std::fs::Metadata,
    own: u32,
    what: &str,
    sticky_ok: bool,
) -> Result<(), String> {
    if md.uid() != own && md.uid() != 0 {
        return Err(format!(
            "{what} {} is owned by uid {}, not g2hd's uid {own} or root; fix: chown it to the g2h user",
            path.display(),
            md.uid()
        ));
    }
    // a root-owned sticky dir (/tmp) lets no one else rename our entries
    let sticky = sticky_ok && md.uid() == 0 && md.mode() & 0o1000 != 0;
    if md.mode() & 0o022 != 0 && !sticky {
        return Err(format!(
            "{what} {} has mode {:o}; must not be group/other-writable",
            path.display(),
            md.mode() & 0o777
        ));
    }
    Ok(())
}

/// `path` and every directory above it, up to `/`, must be trusted: anyone
/// who can write a parent can swap the child after the startup check. Checks
/// the path as written (each symlink and its directory) and the resolved path.
fn trusted_chain(path: &Path, own: u32, what: &str) -> Result<(), String> {
    trusted(path, own, what)?;
    // the leaf itself, if a symlink: its owner could re-point it later
    let leaf =
        std::fs::symlink_metadata(path).map_err(|e| format!("{what} {}: {e}", path.display()))?;
    if leaf.file_type().is_symlink() && leaf.uid() != own && leaf.uid() != 0 {
        return Err(format!(
            "{what} {} is a symlink owned by uid {}; must be g2hd's or root's",
            path.display(),
            leaf.uid()
        ));
    }
    let real =
        std::fs::canonicalize(path).map_err(|e| format!("{what} {}: {e}", path.display()))?;
    for p in [path, real.as_path()] {
        let mut cur = p.parent();
        while let Some(dir) = cur.filter(|d| !d.as_os_str().is_empty()) {
            let md = std::fs::symlink_metadata(dir)
                .map_err(|e| format!("{what} parent {}: {e}", dir.display()))?;
            let what = format!("{what} parent dir");
            if md.file_type().is_symlink() {
                // a symlink's own mode is meaningless; its owner and its dir count
                if md.uid() != own && md.uid() != 0 {
                    return Err(format!(
                        "{what} {} is a symlink owned by uid {}; must be g2hd's or root's",
                        dir.display(),
                        md.uid()
                    ));
                }
            } else {
                owned_not_writable(dir, &md, own, &what, true)?;
            }
            cur = dir.parent();
        }
    }
    Ok(())
}

/// argv[0] names that run a second, unchecked file (a script argument, or
/// code read from elsewhere). The approver must be one trusted program.
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "csh",
    "tcsh",
    "env",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "deno",
    "bun",
    "php",
    "lua",
    "tclsh",
    "osascript",
    "pwsh",
    "busybox",
];

fn is_interpreter(bin: &Path) -> bool {
    let name = bin.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    INTERPRETERS.contains(&name) || INTERPRETERS.contains(&base)
}

/// Files in the home g2hd reads or writes; each must be trusted if present,
/// or a client uid could rewrite credentials (re-read on every wake) or the
/// pending-op book.
pub fn home_files(home: &Path) -> [std::path::PathBuf; 8] {
    let p = crate::paths::Paths::new(home);
    [
        p.config(),
        p.state(),
        p.credentials(),
        p.inbox_credentials(),
        p.log(),
        p.lock(),
        p.sessions(),
        home.join("approvals.txt"),
    ]
}

/// Startup checks for separate-user mode. `config` may be missing (defaults).
pub fn check_startup(
    s: &SeparateUser,
    own: u32,
    home: &Path,
    config: &Path,
    approver: Option<&[String]>,
) -> Result<(), String> {
    if own == 0 {
        return Err("separate_user: g2hd must run as its own user, not root".into());
    }
    if s.allowed_uids.contains(&own) {
        return Err(format!(
            "separate_user: g2hd's uid {own} is in allowed_uids; run g2hd as a dedicated user (see the README recipe)"
        ));
    }
    trusted_chain(home, own, "g2h home")?;
    let mut files = home_files(home).to_vec();
    files[0] = config.to_path_buf();
    for f in files {
        if let Ok(md) = std::fs::symlink_metadata(&f) {
            if md.file_type().is_symlink() {
                return Err(format!(
                    "separate_user: {} is a symlink; must be a regular file",
                    f.display()
                ));
            }
            owned_not_writable(&f, &md, own, "home file", false)?;
            if md.uid() != own {
                return Err(format!(
                    "separate_user: {} is owned by uid {}, not g2hd's uid {own}; fix: sudo chown {own} {}",
                    f.display(),
                    md.uid(),
                    f.display()
                ));
            }
        }
    }
    if let Some(argv) = approver.filter(|a| !a.is_empty()) {
        // one element only: any argument could be a script or a program for a
        // launcher (nice, xargs, awk -f, …) that no name list can cover
        if argv.len() != 1 {
            return Err(format!(
                "separate_user: [ops_approver] command must be one element (the program itself, no arguments), got {argv:?}; put the arguments in a root-owned wrapper script"
            ));
        }
        let bin = Path::new(&argv[0]);
        if !bin.is_absolute() {
            return Err(format!(
                "separate_user: [ops_approver] command {:?} must be an absolute path",
                argv[0]
            ));
        }
        // by name as written, then (after the chain check) by what it resolves to
        let real = if is_interpreter(bin) {
            bin.to_path_buf()
        } else {
            trusted_chain(bin, own, "approver")?;
            std::fs::canonicalize(bin).map_err(|e| format!("approver {}: {e}", bin.display()))?
        };
        if is_interpreter(&real) {
            return Err(format!(
                "separate_user: [ops_approver] command {:?} is an interpreter; its script or code is not checked. Point command at the script itself (root-owned, with a #! line)",
                argv[0]
            ));
        }
    }
    Ok(())
}

/// The approve command the approval message shows. In separate-user mode a
/// bare `g2h approve` from the owner's shell gets `not_owner` and would run a
/// PATH-resolved `g2h` in the shell the bots control, so show the recipe's form.
pub fn approve_hint(
    s: Option<&SeparateUser>,
    own: u32,
    home: &Path,
    id: &str,
    code: &str,
) -> String {
    match s {
        None => format!("To allow it once, run on your machine:\n  g2h approve {id} {code}"),
        Some(_) => format!(
            "To allow it once, run from a separate admin account's own login session (never a shell opened from your everyday account; see docs/G2H-SEPARATE-USER.md):\n  /usr/bin/sudo -u '#{own}' /usr/bin/env G2H_HOME={} /opt/g2h/bin/g2h approve {id} {code}",
            home.display()
        ),
    }
}

/// One line for `g2h doctor`.
pub fn describe(s: Option<&SeparateUser>, own: u32) -> String {
    match s {
        None => format!("same-user (uid {own}): NOT a security boundary; a bot with a shell as this user can edit config.toml, change the approver or skip the socket. See [separate_user] in the README"),
        Some(s) => format!(
            "separate-user: g2hd uid {own}; clients {:?} via group {}; approve only as uid {own} or root",
            s.allowed_uids, s.socket_gid
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn su() -> SeparateUser {
        SeparateUser {
            allowed_uids: vec![501],
            socket_gid: 20,
        }
    }

    #[test]
    fn same_user_mode_allows_everything_as_before() {
        assert!(peer_allowed(None, 501, Some(777)));
        assert!(peer_allowed(None, 501, None));
        assert!(approve_allowed(None, 501, Some(501)));
        assert_eq!(Mode::of(None).home_mode(), 0o700);
        assert_eq!(Mode::of(None).socket_mode(), 0o600);
    }

    #[test]
    fn separate_user_peer_and_approve_policy() {
        let s = su();
        let own = 300;
        assert!(peer_allowed(Some(&s), own, Some(501)));
        assert!(peer_allowed(Some(&s), own, Some(own)));
        assert!(peer_allowed(Some(&s), own, Some(0)));
        assert!(!peer_allowed(Some(&s), own, Some(502)));
        assert!(!peer_allowed(Some(&s), own, None));
        assert!(
            !approve_allowed(Some(&s), own, Some(501)),
            "the bots' uid must not approve"
        );
        assert!(approve_allowed(Some(&s), own, Some(own)));
        assert!(approve_allowed(Some(&s), own, Some(0)));
        assert!(!approve_allowed(Some(&s), own, None));
        assert_eq!(Mode::of(Some(&s)).home_mode(), 0o750);
        assert_eq!(Mode::of(Some(&s)).socket_mode(), 0o660);
    }

    #[test]
    fn startup_refuses_same_uid_root_and_loose_files() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("h");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o750)).unwrap();
        let own = std::fs::metadata(&home).unwrap().uid();
        let cfg = home.join("config.toml");
        let mut s = su();
        s.allowed_uids = vec![own];
        assert!(check_startup(&s, own, &home, &cfg, None)
            .unwrap_err()
            .contains("allowed_uids"));
        s.allowed_uids = vec![own + 1];
        assert!(check_startup(&s, 0, &home, &cfg, None)
            .unwrap_err()
            .contains("root"));
        assert!(
            check_startup(&s, own, &home, &cfg, None).is_ok(),
            "missing config is fine"
        );
        // a home owned by someone else (simulated: g2hd claims another uid)
        assert!(check_startup(&s, own + 7, &home, &cfg, None)
            .unwrap_err()
            .contains("owned by"));
        std::fs::write(&cfg, "").unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(check_startup(&s, own, &home, &cfg, None)
            .unwrap_err()
            .contains("writable"));
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_startup(&s, own, &home, &cfg, None).is_ok());
        let rel = vec!["notify".to_string()];
        assert!(check_startup(&s, own, &home, &cfg, Some(&rel))
            .unwrap_err()
            .contains("absolute"));
        let bin = home.join("approver");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o777)).unwrap();
        let abs = vec![bin.display().to_string()];
        assert!(check_startup(&s, own, &home, &cfg, Some(&abs))
            .unwrap_err()
            .contains("approver"));
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_startup(&s, own, &home, &cfg, Some(&abs)).is_ok());
    }

    fn sep_home() -> (tempfile::TempDir, std::path::PathBuf, u32) {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("h");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o750)).unwrap();
        let own = std::fs::metadata(&home).unwrap().uid();
        (t, home, own)
    }

    #[test]
    fn startup_checks_every_home_file() {
        let (_t, home, own) = sep_home();
        let cfg = home.join("config.toml");
        for f in home_files(&home).iter().skip(1) {
            std::fs::write(f, "").unwrap();
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o666)).unwrap();
            let e = check_startup(&su(), own, &home, &cfg, None).unwrap_err();
            assert!(e.contains("writable"), "{}: {e}", f.display());
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(check_startup(&su(), own, &home, &cfg, None).is_ok());
        }
        let st = home.join("state.json");
        std::fs::remove_file(&st).unwrap();
        std::os::unix::fs::symlink("/dev/null", &st).unwrap();
        assert!(check_startup(&su(), own, &home, &cfg, None)
            .unwrap_err()
            .contains("symlink"));
    }

    #[test]
    fn startup_walks_every_parent_of_home_and_approver() {
        let (t, _, own) = sep_home();
        let a = t.path().join("a");
        let home = a.join("b/h");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o777)).unwrap();
        let cfg = home.join("config.toml");
        assert!(check_startup(&su(), own, &home, &cfg, None)
            .unwrap_err()
            .contains("parent dir"));
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_startup(&su(), own, &home, &cfg, None).is_ok());
        // approver at a/b/c/approver with a loose grandparent
        let c = a.join("b/c");
        std::fs::create_dir(&c).unwrap();
        let bin = c.join("approver");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let argv = vec![bin.display().to_string()];
        assert!(check_startup(&su(), own, &home, &cfg, Some(&argv)).is_ok());
        std::fs::set_permissions(a.join("b"), std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(check_startup(&su(), own, &home, &cfg, Some(&argv))
            .unwrap_err()
            .contains("parent dir"));
    }

    #[test]
    fn startup_refuses_interpreter_approvers() {
        let (_t, home, own) = sep_home();
        let cfg = home.join("config.toml");
        for i in [
            "/bin/sh",
            "/usr/bin/env",
            "/usr/bin/python3.12",
            "/usr/bin/perl",
        ] {
            let argv = vec![i.to_string()];
            assert!(
                check_startup(&su(), own, &home, &cfg, Some(&argv))
                    .unwrap_err()
                    .contains("interpreter"),
                "{i}"
            );
        }
        assert!(!is_interpreter(Path::new("/opt/g2h/bin/g2h-approver")));
    }

    #[test]
    fn startup_refuses_approver_arguments_and_interpreter_symlinks() {
        let (t, home, own) = sep_home();
        let cfg = home.join("config.toml");
        // the audit's bypasses: launchers and interpreters with arguments
        for argv in [
            &["/usr/bin/nice", "/bin/sh", "/tmp/evil.sh"][..],
            &["/usr/bin/awk", "-f", "/tmp/evil.sh"],
            &["/usr/bin/arch", "-arm64", "/bin/sh", "/tmp/evil.sh"],
            &["/usr/bin/xargs", "-n1", "/bin/sh", "/tmp/evil.sh"],
        ] {
            let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
            assert!(
                check_startup(&su(), own, &home, &cfg, Some(&argv))
                    .unwrap_err()
                    .contains("one element"),
                "{argv:?}"
            );
        }
        // a trusted-looking name that links to a shell
        let d = t.path().join("bin");
        std::fs::create_dir(&d).unwrap();
        let notify = d.join("notify");
        std::os::unix::fs::symlink("/bin/sh", &notify).unwrap();
        let argv = vec![notify.display().to_string()];
        assert!(check_startup(&su(), own, &home, &cfg, Some(&argv))
            .unwrap_err()
            .contains("interpreter"));
    }

    #[test]
    fn startup_checks_the_leaf_symlink_owner() {
        let (t, home, own) = sep_home();
        let cfg = home.join("config.toml");
        let link = t.path().join("approver");
        std::os::unix::fs::symlink("/usr/bin/true", &link).unwrap();
        let argv = vec![link.display().to_string()];
        // our own link passes; the same link judged for another g2hd uid fails
        assert!(check_startup(&su(), own, &home, &cfg, Some(&argv)).is_ok());
        let e = trusted_chain(&link, own + 1, "approver").unwrap_err();
        assert!(e.contains("is a symlink owned by uid"), "{e}");
    }

    #[test]
    fn approve_hint_shows_the_separate_user_form() {
        let h = Path::new("/var/lib/g2h");
        assert!(approve_hint(None, 7, h, "r1", "c").ends_with("\n  g2h approve r1 c"));
        let m = approve_hint(Some(&su()), 7, h, "r1", "c");
        assert!(m.contains("/usr/bin/sudo -u '#7' /usr/bin/env G2H_HOME=/var/lib/g2h /opt/g2h/bin/g2h approve r1 c"));
        assert!(m.contains("docs/G2H-SEPARATE-USER.md"));
    }

    #[test]
    fn euid_is_the_process_uid() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(euid(), std::fs::metadata(t.path()).unwrap().uid());
    }

    #[test]
    fn doctor_line_warns_in_same_user_mode() {
        assert!(describe(None, 501).contains("NOT a security boundary"));
        assert!(describe(Some(&su()), 300).starts_with("separate-user"));
    }
}
