//! Presets (careful | balanced | fast; default fast) and the hard floors 1-6 that no setting reaches.
//!
//! [`Policy::decide`] checks the floors first, always. The floors are also plain functions in [`floors`], called
//! again at the transport edge (IPC allowlist, port bind, delete).
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Careful,
    Balanced,
    #[default]
    Fast,
}

impl Preset {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "careful" => Some(Preset::Careful),
            "balanced" => Some(Preset::Balanced),
            "fast" => Some(Preset::Fast),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Careful => "careful",
            Preset::Balanced => "balanced",
            Preset::Fast => "fast",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotsScope {
    All,
    Allowlist(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirm {
    Typed,
    YesFlag,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortMode {
    Off,
    AskOnce,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wrap {
    Note,
    Data,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretMode {
    ByRef,
    ValuesWithFlag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofLevel {
    Full,
    Fast,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCheck {
    Refuse(f64),
    Warn(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionGate {
    Blocked,
    AfterCanary,
    AfterCanaryOrTrust,
}

/// Transport order and on/off. Order is fixed: debug-port -> ax -> bot-shell -> webhook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportSet {
    pub debug_port: bool,
    pub ax: bool,
    pub bot_shell: bool,
    pub webhook: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub preset: Preset,
    pub bots_scope: BotsScope,
    pub confirm_destructive: Confirm,
    pub debug_port: PortMode,
    pub port_ttl_s: u64,
    pub reply_wrapping: Wrap,
    pub action_log: bool,
    pub secrets_to_grok: SecretMode,
    pub proof: ProofLevel,
    pub usage_check: UsageCheck,
    pub unknown_version_writes: VersionGate,
    pub mcp_lifecycle_tools: bool,
    pub transports: TransportSet,
    /// balanced: the owner answered "yes" once to an automatic launch.
    #[serde(default)]
    pub launch_remembered: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy::preset(Preset::Fast)
    }
}

/// Who is calling. MCP servers a Grok bot can call never get stop/delete/lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    Cli { tty: bool },
    Mcp { bot_callable: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action<'a> {
    Read,
    Send,
    Stop,
    Watch,
    RoutineChange,
    /// Delete a bot or routine; `exact` is the name or id the caller typed.
    Delete {
        exact: &'a str,
        confirm: Option<&'a str>,
        yes: bool,
    },
    /// `launch` / `restore` run by the user (allowed in every preset).
    Launch,
    /// An automatic restart of the app to open the port (no explicit `launch`).
    AutoLaunch,
    SecretValue {
        flag: bool,
    },
    /// Any IPC method over the debug port (floor 6).
    Ipc(&'a str),
    /// Answering one of Grok's approval cards (floor 2): never.
    AnswerApproval,
    /// Binding / connecting to an address (floor 1).
    Bind(&'a str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Ask the user (a TTY prompt). Not answerable over MCP: refuse there.
    Ask(String),
    /// exit 4; names the setting (or floor) that decided, and what would allow it.
    Refuse {
        setting: &'static str,
        hint: String,
    },
}

/// The bot an action is about (selector already resolved).
pub struct Target<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub main: bool,
}

pub mod floors {
    //! Hard floors. Plain functions; no preset or setting reaches them.

    /// Floor 1: only loopback addresses are ever bound or connected to by the debug-port transport.
    pub fn loopback_only(addr: &str) -> bool {
        let host = addr.rsplit_once(':').map(|x| x.0).unwrap_or(addr);
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host == "127.0.0.1" || host == "::1"
    }

    /// Floor 6 (review H2): an attachment path the bot's transcript names may be read directly (through
    /// `readAttachmentBytes`) only when it is the shape the app itself writes: a `file://` URL to one file in this
    /// bot's asset store, `.../agent-data/agents/<bot id>/assets/<name>`, where `<name>` is one hash-like component
    /// (ASCII letters, digits, `-`, `_`, one optional `.ext`). No `..`, percent-encoding, query, fragment or other
    /// root. Anything else goes to the app's own preview route.
    pub fn attachment_url_ok(path: &str, bot_id: &str) -> bool {
        let Some(rest) = path.strip_prefix("file:///") else {
            return false;
        };
        if bot_id.is_empty()
            || !bot_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || rest
                .bytes()
                .any(|c| c.is_ascii_control() || b"%?#\\ ".contains(&c))
            || rest
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return false;
        }
        let marker = format!("/agent-data/agents/{bot_id}/assets/");
        let Some(i) = rest.find(&marker) else {
            return false;
        };
        let name = &rest[i + marker.len()..];
        let (stem, ext) = name.split_once('.').unwrap_or((name, ""));
        let word = |s: &str| {
            s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        };
        !stem.is_empty()
            && stem.len() <= 128
            && word(stem)
            && ext.len() <= 16
            && ext.bytes().all(|c| c.is_ascii_alphanumeric())
            && !(name.contains('.') && ext.is_empty())
    }

    /// Floor 6: the `desktop.main` IPC methods botport may call (app 1.1.0). Everything else is denied.
    pub const IPC_READ: &[&str] = &[
        "getUpdateStatus",
        "getCursorWeeklyUsage",
        "getCursorUsageSummary",
        "listTeamBotAutomations",
        "listGrokBotAgentFiles",
        "listGrokBotUserStoreFiles",
        "getLocalToolPermission",
        "readAttachmentBytes",
        "readAttachmentText",
    ];
    pub const IPC_WRITE: &[&str] = &[
        "setTeamBotAutomationEnabled",
        "uploadGrokBotAgentFile",
        "deleteGrokBotAgentFile",
    ];

    /// Words that mark a method as payment, vault, secret, account, team, install or approval: denied even if
    /// someone adds it to the allowlist by mistake.
    const DENY_WORDS: &[&str] = &[
        "approval",
        "approve",
        "permission",
        "grant",
        "purchase",
        "checkout",
        "cart",
        "payment",
        "billing",
        "vault",
        "secret",
        "credential",
        "token",
        "account",
        "team",
        "install",
        "update",
        "mcp",
    ];

    pub fn ipc_allowed(method: &str) -> bool {
        let listed = IPC_READ.contains(&method) || IPC_WRITE.contains(&method);
        let lower = method.to_ascii_lowercase();
        // reads that only look at these words (getLocalToolPermission, getUpdateStatus,
        // listTeamBotAutomations, setTeamBotAutomationEnabled) are listed by exact name and are not writes of
        // the denied kind; any other method containing a deny word is refused.
        let exact_exempt = [
            "getlocaltoolpermission",
            "getupdatestatus",
            "listteambotautomations",
            "setteambotautomationenabled",
        ];
        let denied =
            DENY_WORDS.iter().any(|w| lower.contains(w)) && !exact_exempt.contains(&lower.as_str());
        listed && !denied
    }

    /// Floor 4: a bot or routine is deleted only when named exactly (id, or the exact name, case-sensitive).
    pub fn delete_exact(typed: &str, id: &str, name: &str) -> bool {
        !typed.is_empty() && (typed == id || typed == name)
    }

    /// Floor 3: something that looks like a secret value (for logs and proofs).
    pub fn looks_secret(s: &str) -> bool {
        let pats = ["crsr_", "sk-", "xai-", "Bearer ", "ghp_", "-----BEGIN"];
        pats.iter().any(|p| s.contains(p))
    }

    /// Floor 3: replace anything secret-shaped before a line reaches a log or a proof.
    pub fn scrub(s: &str) -> String {
        s.split(' ')
            .map(|w| if looks_secret(w) { "[secret]" } else { w })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Policy {
    pub fn preset(p: Preset) -> Self {
        let all = TransportSet {
            debug_port: true,
            ax: true,
            bot_shell: true,
            webhook: false,
        };
        match p {
            Preset::Careful => Policy {
                preset: p,
                bots_scope: BotsScope::Allowlist(vec![]),
                confirm_destructive: Confirm::Typed,
                debug_port: PortMode::Off,
                port_ttl_s: 15 * 60,
                reply_wrapping: Wrap::Note,
                action_log: true,
                secrets_to_grok: SecretMode::ByRef,
                proof: ProofLevel::Full,
                usage_check: UsageCheck::Refuse(90.0),
                unknown_version_writes: VersionGate::Blocked,
                mcp_lifecycle_tools: false,
                transports: all,
                launch_remembered: false,
            },
            Preset::Balanced => Policy {
                preset: p,
                bots_scope: BotsScope::All,
                confirm_destructive: Confirm::YesFlag,
                debug_port: PortMode::AskOnce,
                port_ttl_s: 30 * 60,
                reply_wrapping: Wrap::Data,
                action_log: true,
                secrets_to_grok: SecretMode::ByRef,
                proof: ProofLevel::Fast,
                usage_check: UsageCheck::Warn(80.0),
                unknown_version_writes: VersionGate::AfterCanary,
                mcp_lifecycle_tools: true,
                transports: all,
                launch_remembered: false,
            },
            Preset::Fast => Policy {
                preset: p,
                bots_scope: BotsScope::All,
                confirm_destructive: Confirm::None,
                debug_port: PortMode::Auto,
                port_ttl_s: 60 * 60,
                reply_wrapping: Wrap::Raw,
                action_log: true,
                secrets_to_grok: SecretMode::ValuesWithFlag,
                proof: ProofLevel::Fast,
                usage_check: UsageCheck::Warn(90.0),
                unknown_version_writes: VersionGate::AfterCanaryOrTrust,
                mcp_lifecycle_tools: true,
                transports: all,
                launch_remembered: false,
            },
        }
    }

    /// Floors first, then the preset's settings.
    pub fn decide(&self, caller: &Caller, action: &Action, bot: Option<&Target>) -> Decision {
        let refuse = |setting: &'static str, hint: &str| Decision::Refuse {
            setting,
            hint: hint.to_string(),
        };
        // ---- floors (no setting reaches these)
        match action {
            Action::AnswerApproval => {
                return refuse("floor 2", "botport never answers Grok's approval cards; answer it in the app, or use its \"Always allow\"")
            }
            Action::Ipc(m) if !floors::ipc_allowed(m) => {
                return refuse("floor 6", "this app function is not on botport's allowlist")
            }
            Action::Bind(a) if !floors::loopback_only(a) => {
                return refuse("floor 1", "the debug port binds to 127.0.0.1 only")
            }
            Action::Delete { exact, confirm, .. } => match bot {
                // over MCP the `confirm` argument must repeat the exact name too
                Some(_) if matches!(caller, Caller::Mcp { .. }) && *confirm != Some(*exact) => {
                    return refuse("floor 4", "over MCP, confirm must repeat the exact name or id")
                }
                Some(t) if floors::delete_exact(exact, t.id, t.name) => {}
                _ => return refuse("floor 4", "name the bot or routine exactly (its id or exact name)"),
            },
            _ => {}
        }
        if let Caller::Mcp { bot_callable: true } = caller {
            if matches!(
                action,
                Action::Stop | Action::Delete { .. } | Action::Launch | Action::AutoLaunch
            ) {
                return refuse(
                    "mcp",
                    "stop, delete and lifecycle tools are never on a server a Grok bot can call",
                );
            }
        }
        // ---- preset settings
        if let (BotsScope::Allowlist(list), Some(t)) = (&self.bots_scope, bot) {
            if !matches!(action, Action::Launch)
                && !list
                    .iter()
                    .any(|x| x == t.id || x.eq_ignore_ascii_case(t.name))
            {
                return refuse("bots_scope", "add the bot: botport grok policy set bots_scope \"<name>,...\" (or preset balanced)");
            }
        }
        match action {
            Action::Delete { exact, confirm, yes } => {
                if bot.is_some_and(|t| t.main) {
                    return refuse("main bot", "the main bot cannot be deleted");
                }
                match (self.confirm_destructive, caller) {
                    (Confirm::Typed, Caller::Mcp { .. }) => {
                        refuse("confirm_destructive", "careful: typed confirm works on the CLI only")
                    }
                    (Confirm::Typed, Caller::Cli { .. }) => Decision::Ask(format!("type {exact} to confirm")),
                    (Confirm::YesFlag, Caller::Mcp { .. }) if *confirm != Some(*exact) => {
                        refuse("confirm_destructive", "pass confirm = the exact name")
                    }
                    (Confirm::YesFlag, Caller::Cli { .. }) if !*yes => {
                        refuse("confirm_destructive", "add --yes (or preset fast)")
                    }
                    _ => Decision::Allow,
                }
            }
            Action::AutoLaunch => match self.debug_port {
                PortMode::Off => refuse("debug_port", "run `botport grok launch` yourself, or: botport grok policy set debug_port auto"),
                PortMode::AskOnce if self.launch_remembered => Decision::Allow,
                PortMode::AskOnce => match caller {
                    Caller::Cli { tty: true } => Decision::Ask("Restart Grok Bot with the local debug port open?".into()),
                    _ => refuse("debug_port", "balanced asks once on a terminal; run `botport grok launch` once"),
                },
                PortMode::Auto => Decision::Allow,
            },
            Action::SecretValue { flag } => match self.secrets_to_grok {
                SecretMode::ValuesWithFlag if *flag => Decision::Allow,
                SecretMode::ValuesWithFlag => refuse("secrets_to_grok", "add --allow-secret-values"),
                SecretMode::ByRef => refuse("secrets_to_grok", "secrets go by reference under this preset"),
            },
            Action::Launch if matches!(caller, Caller::Mcp { .. }) && !self.mcp_lifecycle_tools => {
                refuse("mcp_lifecycle_tools", "lifecycle tools are off over MCP under this preset")
            }
            _ => Decision::Allow,
        }
    }

    // ---------------------------------------------------------------- persistence
    pub fn path(home: &Path) -> PathBuf {
        home.join("grok-policy.toml")
    }

    /// The saved policy, or the default (fast) when there is none.
    pub fn load(home: &Path) -> Result<Self, String> {
        let p = Self::path(home);
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }

    pub fn save(&self, home: &Path) -> Result<(), String> {
        let s = toml::to_string(self).map_err(|e| e.to_string())?;
        crate::util::write_private(&Self::path(home), s.as_bytes()).map_err(|e| e.to_string())
    }

    /// `policy set KEY VALUE`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let bad = || format!("bad value {value:?} for {key}");
        let onoff = |v: &str| match v {
            "on" | "true" => Ok(true),
            "off" | "false" => Ok(false),
            _ => Err(bad()),
        };
        match key {
            "bots_scope" => {
                self.bots_scope = if value == "all" {
                    BotsScope::All
                } else {
                    BotsScope::Allowlist(
                        value
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    )
                }
            }
            "confirm_destructive" => {
                self.confirm_destructive = match value {
                    "typed" => Confirm::Typed,
                    "yes" => Confirm::YesFlag,
                    "none" => Confirm::None,
                    _ => return Err(bad()),
                }
            }
            "debug_port" => {
                self.debug_port = match value {
                    "off" => PortMode::Off,
                    "ask-once" => PortMode::AskOnce,
                    "auto" => PortMode::Auto,
                    _ => return Err(bad()),
                }
            }
            "port_ttl" => {
                self.port_ttl_s = crate::util::parse_duration_s(value)
                    .filter(|s| *s > 0 && *s <= crate::lifecycle::MAX_PORT_TTL_S)
                    .ok_or_else(bad)?
            }
            "reply_wrapping" => {
                self.reply_wrapping = match value {
                    "note" => Wrap::Note,
                    "data" => Wrap::Data,
                    "raw" => Wrap::Raw,
                    _ => return Err(bad()),
                }
            }
            "action_log" => self.action_log = onoff(value)?,
            "mcp_lifecycle_tools" => self.mcp_lifecycle_tools = onoff(value)?,
            "transports.debug-port" | "transports.debug_port" => {
                self.transports.debug_port = onoff(value)?
            }
            "transports.ax" => self.transports.ax = onoff(value)?,
            "transports.bot-shell" | "transports.bot_shell" => {
                self.transports.bot_shell = onoff(value)?
            }
            "transports.webhook" => self.transports.webhook = onoff(value)?,
            _ => return Err(format!("unknown setting {key:?}")),
        }
        Ok(())
    }
}
