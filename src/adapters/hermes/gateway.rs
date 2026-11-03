//! Can the owner chat with a Hermes twin? Read-only, from the files Hermes itself keeps (recent Hermes Agent versions;
//! see Hermes' multi-profile gateway guide): a profile is reached through a gateway, normally the one host gateway that serves every profile
//! (`gateway.multiplex_profiles`). botport never starts, stops or asks a gateway anything; it reads
//! - `<profile>/gateway.parked`: the owner took the profile offline (`hermes -p NAME gateway stop`);
//! - `gateway.standalone` in the profile's `config.yaml`: the profile runs its own gateway (`<profile>/gateway_state.json`);
//! - the host record (`$HERMES_GATEWAY_LOCK_DIR` or `$XDG_STATE_HOME/hermes/gateway-locks/host-gateway.json`) or the
//!   root `gateway_state.json`: whether a gateway runs, which profiles it serves (`served_profiles`), which it parked
//!   and why (`parked_profiles`), and why it serves only itself (`multiplex_standalone_reason`);
//! - the model the profile is set to use (`model`, `model.default`; fallbacks `fallback_providers`, `fallback_model`).
//!
//! Sync never needs any of this: a twin is kept in step with no gateway running. Chatting with it needs one.
use crate::core::util::{err, Res};
use serde_yaml::Value as Y;
use std::fs;
use std::path::{Path, PathBuf};

/// Whether the owner can chat with the twin now; when not, why and the one thing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chat {
    Ready(String),
    Not {
        reason: String,
        fix: String,
    },
    /// Hermes' own files could not tell (for example an older Hermes, or no way to check a process).
    Unknown(String),
}

/// What a profile is set to use for answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Model {
    /// `model` (a string) or `model.default` / `model.model` / `model.name`; empty when unset.
    pub model: String,
    /// `model.provider`, when set.
    pub provider: String,
    /// Whether `fallback_providers` or `fallback_model` names anything.
    pub fallback: bool,
}

fn yaml(p: &Path) -> Option<Y> {
    let t = crate::core::yaml11::read_yaml_file(p).ok()?;
    crate::core::yaml11::parse(&crate::core::yaml11::read(&t)).ok()
}

/// The model settings in a profile's `config.yaml`.
pub fn model(profile_dir: &Path) -> Model {
    let Some(y) = yaml(&profile_dir.join("config.yaml")) else {
        return Model::default();
    };
    let s = |v: Option<&Y>| v.and_then(Y::as_str).unwrap_or("").trim().to_string();
    let (model, provider) = match y.get("model") {
        Some(Y::String(m)) => (m.trim().to_string(), String::new()),
        Some(m) => (
            [m.get("default"), m.get("model"), m.get("name")]
                .into_iter()
                .map(s)
                .find(|v| !v.is_empty())
                .unwrap_or_default(),
            s(m.get("provider")),
        ),
        None => (String::new(), String::new()),
    };
    let named = |v: Option<&Y>| match v {
        Some(Y::Sequence(l)) => !l.is_empty(),
        Some(Y::Mapping(m)) => !m.is_empty(),
        Some(Y::String(t)) => !t.trim().is_empty(),
        _ => false,
    };
    Model {
        model,
        provider,
        fallback: named(y.get("fallback_providers")) || named(y.get("fallback_model")),
    }
}

/// A value a model copy may carry: a model id or provider name, one plain word (no spaces, no `@`, no `://`, nothing
/// that looks like a credential).
fn plain_setting(v: &Y) -> Option<Y> {
    let s = v.as_str()?.trim();
    let ok = !s.is_empty()
        && s.len() <= 200
        && !s
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '@')
        && !s.contains("://")
        && crate::core::secrets::scan_value(s).is_none();
    ok.then(|| Y::String(s.to_string()))
}

/// The allowlisted part of a model setting: a plain string, or a mapping kept to `keys` with plain values (a list, for
/// `fallback_providers`, of the same). Other keys (credentials, base URLs, headers) and anything nested are dropped;
/// values that look like credentials are dropped too. `None` when nothing is left.
fn allowed(v: &Y, keys: &[&str], list: bool) -> Option<Y> {
    match v {
        Y::String(_) => plain_setting(v),
        Y::Mapping(m) => {
            let mut out = serde_yaml::Mapping::new();
            for k in keys {
                if let Some(x) = m.get(*k).and_then(plain_setting) {
                    out.insert(Y::String((*k).into()), x);
                }
            }
            (!out.is_empty()).then_some(Y::Mapping(out))
        }
        Y::Sequence(l) if list => {
            let kept: Vec<Y> = l.iter().filter_map(|x| allowed(x, keys, false)).collect();
            (!kept.is_empty()).then_some(Y::Sequence(kept))
        }
        _ => None,
    }
}

/// Give a profile with no model the default profile's model settings: `model`, `fallback_providers` and
/// `fallback_model`, keeping only the default/model/name/provider fields and dropping values that look like credentials
/// (through [`allowed`]; keys live in `.env`, which is never read). Returns
/// what was copied, or `None` when the profile has a model already or the default profile has none. The file is written again (its other settings kept,
/// its leading comment lines kept, other comments and formatting not).
pub fn copy_default_model(profile_dir: &Path, hermes_home: &Path) -> Res<Option<String>> {
    if profile_dir == hermes_home || !model(profile_dir).model.is_empty() {
        return Ok(None);
    }
    let from = hermes_home.join("config.yaml");
    if model(hermes_home).model.is_empty() {
        return Ok(None);
    }
    let Some(src) = yaml(&from) else {
        return Ok(None);
    };
    let p = profile_dir.join("config.yaml");
    // read with the YAML size cap; only a missing file starts empty (one that cannot be read stops the write)
    let text = if fs::symlink_metadata(&p).is_ok() {
        Some(crate::core::yaml11::read_yaml_file(&p)?)
    } else {
        None
    };
    let mut y = match &text {
        Some(t) => crate::core::yaml11::parse(&crate::core::yaml11::read(t))
            .map_err(|e| format!("{}: {e}", p.display()))?,
        None => Y::Mapping(Default::default()),
    };
    let Some(m) = y.as_mapping_mut() else {
        return err(format!("{}: not a mapping", p.display()));
    };
    let mut copied = vec![];
    const MODEL: &[&str] = &["default", "model", "name", "provider"];
    const FALLBACK: &[&str] = &["provider", "model"];
    for (k, keys, list) in [
        ("model", MODEL, false),
        ("fallback_providers", FALLBACK, true),
        ("fallback_model", FALLBACK, false),
    ] {
        if let Some(v) = src.get(k).and_then(|v| allowed(v, keys, list)) {
            m.insert(Y::String(k.into()), v);
            copied.push(k);
        }
    }
    if copied.is_empty() {
        return Ok(None);
    }
    // the same writer as every config.yaml change: YAML 1.1-safe scalars, the file's leading comment lines kept
    let head: String = text
        .unwrap_or_default()
        .lines()
        .take_while(|l| l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    let body = crate::core::yaml11::emit_checked(&y)?;
    let text = if head.is_empty() {
        body
    } else {
        format!("{head}\n{body}")
    };
    crate::core::util::write_atomic(&p, text.as_bytes(), Some(0o600))?;
    Ok(Some(copied.join(", ")))
}

/// Whether process `pid` is alive, by `kill -0` (found as every external program is). `None`: cannot tell (no `kill`,
/// or a process of another user). 0 and 1 are never a gateway (a process group, the init process).
fn alive(pid: u64) -> Option<bool> {
    if pid <= 1 || pid > i32::MAX as u64 {
        return Some(false);
    }
    let kill = crate::core::util::program("kill").ok()?;
    let o = std::process::Command::new(kill)
        .args(["-0", &pid.to_string()])
        // its message in one language, so a permission error is recognised
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;
    if o.status.success() {
        return Some(true);
    }
    // a process that exists but belongs to someone else: alive, but not this user's to judge
    let e = String::from_utf8_lossy(&o.stderr).to_ascii_lowercase();
    if e.contains("not permitted") || e.contains("permission") {
        return None;
    }
    Some(false)
}

fn json(p: &Path) -> Option<serde_json::Value> {
    let b = fs::read(p).ok()?;
    serde_json::from_slice(&b).ok()
}

/// A gateway record that is live: a living pid and a state that is not stopped or failed.
fn live(v: &serde_json::Value) -> Option<bool> {
    let pid = v["pid"].as_u64()?;
    if matches!(
        v["gateway_state"].as_str(),
        Some("stopped") | Some("startup_failed")
    ) {
        return Some(false);
    }
    alive(pid)
}

fn host_record() -> Option<PathBuf> {
    let dir = match std::env::var_os("HERMES_GATEWAY_LOCK_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => {
            let state = match std::env::var_os("XDG_STATE_HOME") {
                Some(s) if !s.is_empty() => PathBuf::from(s),
                _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
            };
            state.join("hermes/gateway-locks")
        }
    };
    Some(dir.join("host-gateway.json"))
}

/// Text from Hermes' own files, safe to print (one line, no control characters, bounded).
fn said(v: &serde_json::Value) -> String {
    let t: String = v
        .as_str()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let t = t.trim();
    match t.char_indices().nth(200) {
        Some((i, _)) => format!("{}...", &t[..i]),
        None => t.to_string(),
    }
}

/// A printed `hermes -p ... <rest>` command: the profile name single-quoted for the shell when it is a plain profile name
/// (letters, digits, `.`, `_`, `-`; not starting with `.` or `-`; at most 64 characters); any other name is never
/// printed: the command reads `"$PROFILE"` and says to set PROFILE to the profile's name first.
pub fn hermes_cmd(name: &str, rest: &str) -> String {
    let plain = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if plain {
        format!("hermes -p '{name}' {rest}")
    } else {
        format!("hermes -p \"$PROFILE\" {rest} (first set PROFILE to the profile's name)")
    }
}

/// Can the owner chat with the profile at `profile_dir` (named `name`, or the default profile when it is the Hermes home)?
pub fn chat(hermes_home: &Path, profile_dir: &Path, name: &str) -> Chat {
    chat_at(hermes_home, profile_dir, name, host_record().as_deref())
}

/// [`chat`] with the host gateway record at `host` (none: there is no record).
pub fn chat_at(hermes_home: &Path, profile_dir: &Path, name: &str, host: Option<&Path>) -> Chat {
    let default = profile_dir == hermes_home;
    let p = if default { "" } else { name };
    let cmd = |rest: &str| -> String {
        if default {
            format!("hermes {rest}")
        } else {
            hermes_cmd(name, rest)
        }
    };
    if !default && profile_dir.join("gateway.parked").exists() {
        return Chat::Not {
            reason: "the profile is parked (taken offline with `gateway stop`)".into(),
            fix: cmd("gateway start"),
        };
    }
    let own = |dir: &Path| -> Option<bool> {
        let v = json(&dir.join("gateway_state.json")).or_else(|| json(&dir.join("gateway.pid")))?;
        live(&v)
    };
    let standalone = !default
        && yaml(&profile_dir.join("config.yaml"))
            .and_then(|y| y.get("gateway")?.get("standalone")?.as_bool())
            .unwrap_or(false);
    if standalone {
        return match own(profile_dir) {
            Some(true) => Chat::Ready("its own gateway is running (gateway.standalone)".into()),
            Some(false) | None => Chat::Not {
                reason: "it runs its own gateway (gateway.standalone: true), which is not running"
                    .into(),
                fix: cmd("gateway start"),
            },
        };
    }
    // a profile that still runs a gateway of its own (from before one gateway served every profile)
    if !default && own(profile_dir) == Some(true) {
        return Chat::Ready("its own gateway is running".into());
    }
    let host = host.and_then(json);
    let state = json(&hermes_home.join("gateway_state.json"));
    let rec = match (&host, &state) {
        (Some(h), _) if live(h) == Some(true) => h,
        (_, Some(s)) if live(s) == Some(true) => s,
        (None, None) => {
            return Chat::Not {
                reason: "no Hermes gateway record was found (no gateway has run, or this Hermes version keeps none)".into(),
                fix: "hermes gateway start (from the default profile; `hermes gateway install` keeps it running)".into(),
            }
        }
        _ => {
            if host.iter().chain(state.iter()).any(|v| live(v).is_none()) {
                return Chat::Unknown("could not check whether the Hermes gateway process is alive".into());
            }
            return Chat::Not {
                reason: "no Hermes gateway is running".into(),
                fix: "hermes gateway start (from the default profile; `hermes gateway install` keeps it running)".into(),
            };
        }
    };
    if default {
        return Chat::Ready("the gateway is running".into());
    }
    // the live record's own fields first: a root state file left by an older gateway never overrides them
    let full = if rec.get("served_profiles").is_some() || rec.get("parked_profiles").is_some() {
        rec
    } else {
        state.as_ref().unwrap_or(rec)
    };
    if let Some(why) = full["parked_profiles"].get(p) {
        return Chat::Not {
            reason: format!("the gateway parked this profile: {}", said(why)),
            fix: format!(
                "{} (then fix what it names and restart the gateway)",
                cmd("gateway status")
            ),
        };
    }
    let served = |v: &serde_json::Value, k: &str| {
        v[k].as_array()
            .is_some_and(|l| l.iter().any(|x| x.as_str() == Some(p)))
    };
    if served(rec, "profiles") || served(full, "served_profiles") {
        return Chat::Ready("served by the Hermes gateway".into());
    }
    match full["multiplex_standalone_reason"].as_str() {
        Some(why) if !why.trim().is_empty() => Chat::Not {
            reason: format!(
                "the Hermes gateway serves only its own profile ({})",
                said(&full["multiplex_standalone_reason"])
            ),
            fix: "hermes gateway migrate --multiplex --dry-run, then without --dry-run (read \"Chatting with your mirrored bots\" in docs/USER-GUIDE.md first)".into(),
        },
        _ if full.get("served_profiles").is_none() && rec.get("profiles").is_none() => {
            Chat::Unknown("this Hermes does not record which profiles its gateway serves".into())
        }
        _ => Chat::Not {
            reason: "the Hermes gateway does not serve this profile yet (it picks up new profiles periodically)".into(),
            fix: cmd("gateway status"),
        },
    }
}

/// The owner-facing notice: `None` when the twin can be chatted with and has a model.
pub fn notice(hermes_home: &Path, profile_dir: &Path, name: &str) -> Option<String> {
    notice_of(chat(hermes_home, profile_dir, name), profile_dir, name)
}

/// The notice for a known [`Chat`] state.
pub fn notice_of(c: Chat, profile_dir: &Path, name: &str) -> Option<String> {
    let m = model(profile_dir);
    let no_model = m.model.is_empty() && m.provider.is_empty();
    match (c, no_model) {
        (Chat::Not { reason, fix }, _) => Some(format!(
            "Synced, but you can't chat with this bot in Hermes yet: {reason}. Fix: {fix}"
        )),
        (_, true) => Some(format!(
            "Synced, but you can't chat with this bot in Hermes yet: it has no model set. Fix: {} (or set model in its config.yaml)",
            hermes_cmd(name, "model")
        )),
        (Chat::Unknown(why), false) => Some(format!(
            "Synced; whether you can chat with this bot in Hermes is unknown: {why}. Check: {}",
            hermes_cmd(name, "gateway status")
        )),
        (Chat::Ready(_), false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        crate::core::util::test_dir(&format!("gw-{tag}"))
    }

    #[test]
    fn a_parked_profile_and_one_with_no_gateway_cannot_chat() {
        let home = tmp("parked");
        let p = home.join("profiles/demo-bot");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("gateway.parked"), "").unwrap();
        match chat_at(
            &home,
            &p,
            "demo-bot",
            Some(&home.join("no-locks/host-gateway.json")),
        ) {
            Chat::Not { fix, .. } => assert_eq!(fix, "hermes -p 'demo-bot' gateway start"),
            c => panic!("{c:?}"),
        }
        fs::remove_file(p.join("gateway.parked")).unwrap();
        // a gateway record whose state says stopped is no gateway
        fs::write(
            home.join("gateway_state.json"),
            r#"{"pid": 1, "gateway_state": "stopped", "served_profiles": ["demo-bot"]}"#,
        )
        .unwrap();
        match chat_at(
            &home,
            &p,
            "demo-bot",
            Some(&home.join("no-locks/host-gateway.json")),
        ) {
            Chat::Not { reason, .. } => assert!(reason.contains("no Hermes gateway"), "{reason}"),
            c => panic!("{c:?}"),
        }
        let c = chat_at(&home, &p, "demo-bot", None);
        // pid 0 or 1 is never a gateway
        assert_eq!(alive(0), Some(false));
        assert_eq!(alive(1), Some(false));
        let n = notice_of(c, &p, "demo-bot").unwrap();
        assert!(
            n.starts_with("Synced, but you can't chat with this bot in Hermes yet:"),
            "{n}"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_running_gateway_serves_the_profile_or_says_why_not() {
        let home = tmp("served");
        let p = home.join("profiles/demo-bot");
        fs::create_dir_all(&p).unwrap();
        let me = std::process::id();
        let state = |extra: &str| {
            fs::write(
                home.join("gateway_state.json"),
                format!(r#"{{"pid": {me}, "gateway_state": "running"{extra}}}"#),
            )
            .unwrap()
        };
        state(r#", "served_profiles": ["default", "demo-bot"]"#);
        if alive(me as u64) != Some(true) {
            return; // no `kill` to check a process with here
        }
        assert!(matches!(
            chat_at(
                &home,
                &p,
                "demo-bot",
                Some(&home.join("no-locks/host-gateway.json"))
            ),
            Chat::Ready(_)
        ));
        state(
            r#", "served_profiles": ["default"], "multiplex_standalone_reason": "profile 'other' still runs its own gateway""#,
        );
        match chat_at(
            &home,
            &p,
            "demo-bot",
            Some(&home.join("no-locks/host-gateway.json")),
        ) {
            Chat::Not { reason, fix } => {
                assert!(reason.contains("serves only its own profile"), "{reason}");
                assert!(
                    fix.starts_with("hermes gateway migrate --multiplex --dry-run"),
                    "{fix}"
                );
            }
            c => panic!("{c:?}"),
        }
        state(
            r#", "served_profiles": ["default"], "parked_profiles": {"demo-bot": "state.db would not open\u001b[2J"}"#,
        );
        match chat_at(
            &home,
            &p,
            "demo-bot",
            Some(&home.join("no-locks/host-gateway.json")),
        ) {
            Chat::Not { reason, .. } => {
                assert!(reason.contains("state.db would not open"), "{reason}");
                assert!(!reason.contains('\u{1b}'), "{reason}");
            }
            c => panic!("{c:?}"),
        }
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_twin_without_a_model_gets_the_default_profiles_model_but_not_its_env() {
        let home = tmp("model");
        let p = home.join("profiles/demo-bot");
        fs::create_dir_all(&p).unwrap();
        fs::write(
            home.join("config.yaml"),
            "model:\n  default: example-model\n  provider: example-provider\nfallback_providers:\n  - provider: example-fallback\n    model: example-small\n",
        )
        .unwrap();
        fs::write(home.join(".env"), "EXAMPLE_KEY=not-copied\n").unwrap();
        fs::write(p.join("config.yaml"), "# kept\nmcp_servers: {}\n").unwrap();
        assert!(model(&p).model.is_empty());
        let copied = copy_default_model(&p, &home).unwrap().unwrap();
        assert_eq!(copied, "model, fallback_providers");
        let m = model(&p);
        assert_eq!(
            (m.model.as_str(), m.provider.as_str(), m.fallback),
            ("example-model", "example-provider", true)
        );
        let t = fs::read_to_string(p.join("config.yaml")).unwrap();
        assert!(
            t.starts_with("# kept\n") && t.contains("mcp_servers"),
            "{t}"
        );
        assert!(!p.join(".env").exists());
        // a twin that has a model keeps it
        assert_eq!(copy_default_model(&p, &home).unwrap(), None);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn the_model_copy_keeps_only_ids_and_provider_names() {
        let home = tmp("model-allow");
        let p = home.join("profiles/demo-bot");
        fs::create_dir_all(&p).unwrap();
        fs::write(
            home.join("config.yaml"),
            "model:\n  default: example-model\n  provider: example-provider\n  api_key: example-not-a-real-key\n  \
             base_url: https://demo:example-pass@example.invalid/v1\nfallback_providers:\n  - provider: example-fallback\n    \
             model: example-small\n    extra:\n      token: example-nested-token\n  - https://demo:pw@example.invalid/x\n\
             fallback_model:\n  model: example-tiny\n  headers:\n    Authorization: Bearer example-header-value\n",
        )
        .unwrap();
        assert_eq!(
            copy_default_model(&p, &home).unwrap().unwrap(),
            "model, fallback_providers, fallback_model"
        );
        let t = fs::read_to_string(p.join("config.yaml")).unwrap();
        for kept in [
            "example-model",
            "example-provider",
            "example-fallback",
            "example-small",
            "example-tiny",
        ] {
            assert!(t.contains(kept), "{kept} missing: {t}");
        }
        for gone in [
            "api_key",
            "example-not-a-real-key",
            "base_url",
            "example-pass",
            "example.invalid",
            "extra",
            "token",
            "headers",
            "Authorization",
            "Bearer",
            "example-header-value",
        ] {
            assert!(!t.contains(gone), "{gone} copied: {t}");
        }
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn printed_commands_quote_a_plain_profile_name_and_nothing_else() {
        assert_eq!(
            hermes_cmd("demo-bot", "model"),
            "hermes -p 'demo-bot' model"
        );
        for odd in [
            "", "-x", ".hidden", "demo bot", "demo';x", "demo$(x)", "demo\nx",
        ] {
            assert_eq!(
                hermes_cmd(odd, "model"),
                "hermes -p \"$PROFILE\" model (first set PROFILE to the profile's name)",
                "{odd:?}"
            );
        }
        let home = tmp("odd-name");
        let p = home.join("profiles/odd");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("gateway.parked"), "").unwrap();
        match chat_at(&home, &p, "odd';x", None) {
            Chat::Not { fix, .. } => assert_eq!(
                fix,
                "hermes -p \"$PROFILE\" gateway start (first set PROFILE to the profile's name)"
            ),
            c => panic!("{c:?}"),
        }
        fs::remove_dir_all(&home).ok();
    }
}
