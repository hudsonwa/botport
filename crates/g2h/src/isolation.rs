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

/// A path g2hd trusts must be owned by `own` (or root) and not writable by
/// group or others, so no client uid can change it.
fn trusted(path: &Path, own: u32, what: &str) -> Result<(), String> {
    let md = std::fs::metadata(path).map_err(|e| format!("{what} {}: {e}", path.display()))?;
    if md.uid() != own && md.uid() != 0 {
        return Err(format!(
            "{what} {} is owned by uid {}, not g2hd's uid {own} or root; fix: chown it to the g2h user",
            path.display(),
            md.uid()
        ));
    }
    if md.mode() & 0o022 != 0 {
        return Err(format!(
            "{what} {} has mode {:o}; must not be group/other-writable",
            path.display(),
            md.mode() & 0o777
        ));
    }
    Ok(())
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
    trusted(home, own, "g2h home")?;
    if std::fs::symlink_metadata(config).is_ok() {
        trusted(config, own, "config")?;
    }
    if let Some(argv) = approver.filter(|a| !a.is_empty()) {
        let bin = Path::new(&argv[0]);
        if !bin.is_absolute() {
            return Err(format!(
                "separate_user: [ops_approver] command {:?} must be an absolute path",
                argv[0]
            ));
        }
        trusted(bin, own, "approver")?;
        if let Some(dir) = bin.parent() {
            trusted(dir, own, "approver dir")?;
        }
    }
    Ok(())
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

    #[test]
    fn doctor_line_warns_in_same_user_mode() {
        assert!(describe(None, 501).contains("NOT a security boundary"));
        assert!(describe(Some(&su()), 300).starts_with("separate-user"));
    }
}
