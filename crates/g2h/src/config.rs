//! `config.toml` in the g2h home dir. Every field is optional in the file;
//! missing fields take the defaults below.
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `hop_limit` | 3 | refuse delivery/auto-reply at this hop |
//! | `max_messages_per_day` | 200 | daily message cap (local day) |
//! | `max_tokens_per_day` | 200000 | daily token cap |
//! | `max_wakes_per_day` | 10 | daily cap on waking an idle bot |
//! | `selftest_wakes_per_day` | 3 | daily cap on live `g2h selftest --live` webhook POSTs |
//! | `peers` | `[]` | allowed peer names |
//! | `debounce_secs` | 30 | wait this long after a message for Grok before waking (batches) |
//! | `active_window_minutes` | 5 | a Grok-side call within this counts as "Grok active" (no wake) |
//! | `queue_ttl_secs` | 86400 | queued inbound messages expire after this |
//! | `dry_run` | true | log the would-be wake POST, send nothing |
//! | `backend` | `"gateway"` | `"gateway"` (Hermes gateway WS) or `"a2a"` |
//! | `hermes_home` | `$HERMES_HOME` or `~/.hermes` | Hermes home for the gateway backend |
//! | `profile` | none | Hermes profile name (`<hermes_home>/profiles/<name>`) |
//! | `peer_toolsets` | `[]` | toolsets for peer sessions; empty = no tools |
//! | `peer_session_max_turns` | 8 | start a new peer session (with a short framed recap) after this many turns; 0 = unlimited |
//! | `peer_session_max_tokens` | 24000 | same, once a turn's context size (`usage.context_used`, else its share of `usage.total`) reaches this; 0 = unlimited |
//! | `a2a_url` | none | loopback A2A URL (required for `backend = "a2a"`) |
//! | `service_label` | `"io.g2h.daemon"` | launchd label / systemd unit name used by `g2h setup` |
//! | `op_approval_ttl_secs` | 86400 | a pending op approval expires after this |
//! | `[ops_approver]` | unset | `command = ["argv", …]`: owner-set program that gets each approve-tier op's one-time code on stdin; unset refuses every approve-tier op |
//! | `[separate_user]` | unset | opt-in: `allowed_uids = [uid, …]`, `socket_gid = gid`; g2hd runs as its own user (see `isolation`) |
//! | `[ops]` | empty | op name -> `"auto"` / `"approve"` / `"never"`; empty refuses every op |

use crate::guard::Limits;
use crate::hermes::{resolve_profile_home, A2aOptions, GatewayOptions};
use crate::ops::Tier;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum BackendOptions {
    Gateway(GatewayOptions),
    A2a(A2aOptions),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub hop_limit: u32,
    pub max_messages_per_day: u64,
    pub max_tokens_per_day: u64,
    pub max_wakes_per_day: u64,
    pub selftest_wakes_per_day: u64,
    pub peers: Vec<String>,
    pub debounce_secs: u64,
    pub active_window_minutes: u64,
    pub queue_ttl_secs: i64,
    pub dry_run: bool,
    pub backend: String,
    pub hermes_home: Option<PathBuf>,
    pub profile: Option<String>,
    pub peer_toolsets: Vec<String>,
    pub peer_session_max_turns: u64,
    pub peer_session_max_tokens: u64,
    pub a2a_url: Option<String>,
    pub service_label: String,
    pub op_approval_ttl_secs: i64,
    pub ops_approver: Option<OpsApprover>,
    pub separate_user: Option<crate::isolation::SeparateUser>,
    pub ops: BTreeMap<String, Tier>,
}

/// Out-of-band channel to the human owner (a notification, a DM). It must
/// reach the human, not either bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsApprover {
    pub command: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let l = Limits::default();
        Config {
            hop_limit: l.hop_limit,
            max_messages_per_day: l.max_messages,
            max_tokens_per_day: l.max_tokens,
            max_wakes_per_day: l.max_wakes,
            selftest_wakes_per_day: 3,
            peers: Vec::new(),
            debounce_secs: 30,
            active_window_minutes: 5,
            queue_ttl_secs: 86_400,
            dry_run: true,
            backend: "gateway".into(),
            hermes_home: None,
            profile: None,
            peer_toolsets: Vec::new(),
            peer_session_max_turns: 8,
            peer_session_max_tokens: 24_000,
            a2a_url: None,
            service_label: "io.g2h.daemon".into(),
            op_approval_ttl_secs: crate::ops::DEFAULT_APPROVAL_TTL_SECS,
            ops_approver: None,
            separate_user: None,
            ops: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    /// Missing file means defaults.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(t) => Config::parse(&t).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Peer backend options; `g2h_home` holds the persisted session map.
    pub fn backend_options(&self, g2h_home: &Path) -> Result<BackendOptions, String> {
        match self.backend.as_str() {
            "gateway" => {
                let home =
                    resolve_profile_home(self.hermes_home.as_deref(), self.profile.as_deref())
                        .ok_or("cannot resolve the Hermes home (set hermes_home)")?;
                let mut o = GatewayOptions::new(home);
                o.peer_toolsets = self.peer_toolsets.clone();
                o.max_turns = self.peer_session_max_turns;
                o.max_tokens = self.peer_session_max_tokens;
                o.sessions_file = Some(crate::paths::Paths::new(g2h_home).sessions());
                Ok(BackendOptions::Gateway(o))
            }
            "a2a" => {
                let url = self
                    .a2a_url
                    .as_deref()
                    .ok_or("backend = \"a2a\" needs a2a_url")?;
                Ok(BackendOptions::A2a(A2aOptions::new(url)))
            }
            other => Err(format!(
                "unknown backend {other:?} (use \"gateway\" or \"a2a\")"
            )),
        }
    }

    pub fn limits(&self) -> Limits {
        Limits {
            hop_limit: self.hop_limit,
            max_messages: self.max_messages_per_day,
            max_tokens: self.max_tokens_per_day,
            max_wakes: self.max_wakes_per_day,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_defaults_and_dry_run() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert!(c.dry_run);
        assert_eq!(c.limits(), Limits::default());
    }

    #[test]
    fn partial_override() {
        let c = Config::parse("hop_limit = 2\npeers = [\"a\", \"b\"]\ndry_run = false\n").unwrap();
        assert_eq!(
            (c.hop_limit, c.peers.len(), c.dry_run, c.debounce_secs),
            (2, 2, false, 30)
        );
    }

    #[test]
    fn separate_user_section_parses_and_is_off_by_default() {
        assert!(Config::default().separate_user.is_none());
        let c = Config::parse("[separate_user]\nallowed_uids = [501]\nsocket_gid = 20\n").unwrap();
        let s = c.separate_user.unwrap();
        assert_eq!((s.allowed_uids, s.socket_gid), (vec![501], 20));
        assert!(Config::parse("[separate_user]\nallowed_uids = []\n").is_err());
    }

    #[test]
    fn unknown_key_rejected_and_missing_file_ok() {
        assert!(Config::parse("hop_limt = 2").is_err());
        assert_eq!(
            Config::load(Path::new("/nonexistent/g2h/config.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn wake_defaults_and_backend_wiring() {
        let c = Config::default();
        assert_eq!(
            (
                c.debounce_secs,
                c.active_window_minutes,
                c.max_wakes_per_day
            ),
            (30, 5, 10)
        );
        assert_eq!(c.backend, "gateway");
        assert!(c.peer_toolsets.is_empty());
        let c = Config::parse(
            "hermes_home = \"/h\"\nprofile = \"demo\"\npeer_toolsets = [\"knowledge\"]\n",
        )
        .unwrap();
        match c.backend_options(Path::new("/g2h")).unwrap() {
            BackendOptions::Gateway(o) => {
                assert_eq!(o.profile_home, Path::new("/h/profiles/demo"));
                assert_eq!(o.peer_toolsets, vec!["knowledge".to_string()]);
                assert!(o.sessions_file.unwrap().starts_with("/g2h"));
            }
            _ => panic!("gateway expected"),
        }
        let c = Config::parse("backend = \"a2a\"\na2a_url = \"http://127.0.0.1:9/\"\n").unwrap();
        assert!(
            matches!(c.backend_options(Path::new("/g")).unwrap(), BackendOptions::A2a(o) if o.url == "http://127.0.0.1:9/")
        );
        assert!(Config::parse("backend = \"a2a\"\n")
            .unwrap()
            .backend_options(Path::new("/g"))
            .is_err());
        assert!(Config::parse("backend = \"tcp\"\n")
            .unwrap()
            .backend_options(Path::new("/g"))
            .is_err());
    }

    #[test]
    fn peer_session_policy_defaults_and_wiring() {
        let c = Config::default();
        assert_eq!(
            (c.peer_session_max_turns, c.peer_session_max_tokens),
            (8, 24_000)
        );
        let c = Config::parse(
            "hermes_home = \"/h\"\npeer_session_max_turns = 0\npeer_session_max_tokens = 0\n",
        )
        .unwrap();
        match c.backend_options(Path::new("/g2h")).unwrap() {
            BackendOptions::Gateway(o) => assert_eq!((o.max_turns, o.max_tokens), (0, 0)),
            _ => panic!("gateway expected"),
        }
        match Config::parse("hermes_home = \"/h\"\n")
            .unwrap()
            .backend_options(Path::new("/g2h"))
            .unwrap()
        {
            BackendOptions::Gateway(o) => assert_eq!((o.max_turns, o.max_tokens), (8, 24_000)),
            _ => panic!("gateway expected"),
        }
    }
}
