//! Platform-neutral bot model (`bot-model/1`). Spec: docs/MODEL.md; JSON Schema: docs/schema/botmodel-v1.json
//! (tests/schema.rs keeps the two in step).
//!
//! ```
//! use botport::core::model::BotModel;
//!
//! let m = BotModel::from_json(r#"{"schema": "bot-model/1", "persona": {"name": "helper", "instructions": ""}}"#)
//!     .unwrap();
//! assert_eq!(m.persona.name, "helper");
//! assert!(m.to_json().starts_with("{\n  \"schema\": \"bot-model/1\""));
//! ```
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema family; the version after `/` is `MAJOR[.MINOR]`.
pub const SCHEMA_FAMILY: &str = "bot-model";
/// Major version this build understands. Minors are additive and accepted.
pub const SCHEMA_MAJOR: u32 = 1;
/// `extras` key that holds top-level fields this build does not know (from a newer minor), kept, not dropped.
pub const UNKNOWN_KEY: &str = "_unknown";
const KNOWN: [&str; 11] = [
    "schema",
    "persona",
    "memory",
    "memory_text",
    "skills",
    "schedules",
    "connectors",
    "credentials",
    "history",
    "manual",
    "extras",
];

/// One bot, platform-neutral: what an adapter reads from a platform and writes to one. Parse with
/// [`BotModel::from_json`], write the canonical form with [`BotModel::to_json`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotModel {
    /// `bot-model/MAJOR[.MINOR]`; see [`check_schema`].
    pub schema: String,
    /// Name and instructions.
    pub persona: Persona,
    /// Remembered facts, in origin order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory: Vec<Fact>,
    /// The origin's memory as one document, verbatim (e.g. a MEMORY.md with its headings), when it keeps one.
    /// `memory` holds the same facts one by one; a writer that keeps a document prefers this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_text: Option<String>,
    /// Reusable instruction packs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Skill>,
    /// Timed or event-triggered jobs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<Schedule>,
    /// Tool servers (MCP etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connectors: Vec<Connector>,
    /// Names of the secrets the bot needs, never values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<CredentialRef>,
    /// Conversation history stored beside the model (referenced, not embedded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryRef>,
    /// What the reader saw but could not carry (a schedule that runs elsewhere, an app with no url, an entry
    /// that looked like it held a secret): names and reasons only, never a value. Writers list them for a human.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manual: Vec<ManualItem>,
    /// Per-adapter area for data the model cannot express, keyed by adapter
    /// id. Adapters put unmappable input here instead of dropping it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// Who the bot is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persona {
    /// Display name.
    pub name: String,
    /// Role / system prompt / persona text (may be empty).
    pub instructions: String,
}

/// One remembered fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    /// The fact itself.
    pub text: String,
    /// e.g. `core` (always loaded) or `recall` (searchable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// e.g. `user`, `agent`, `project`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Where on the origin platform the fact came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// ISO-8601 date or timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Which side of a two-way sync first wrote it (opaque adapter id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_side: Option<String>,
}

/// A reusable instruction pack: a named set of files (usually a `SKILL.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    /// Skill name (also its directory name on most platforms).
    pub name: String,
    /// One-line summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Relative path -> UTF-8 content.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    /// Relative path -> lowercase hex of a file that is not UTF-8 (images, archives).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binary_files: BTreeMap<String, String>,
}

impl Skill {
    /// Every file as bytes (text and binary), by relative path.
    pub fn bytes(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out: BTreeMap<String, Vec<u8>> = self
            .files
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().into_bytes()))
            .collect();
        for (k, h) in &self.binary_files {
            if let Some(b) = super::util::unhex(h) {
                out.insert(k.clone(), b);
            }
        }
        out
    }

    /// A skill from raw files: UTF-8 ones as text, the rest hex-encoded.
    pub fn from_bytes(name: &str, files: &BTreeMap<String, Vec<u8>>) -> Skill {
        let mut s = Skill {
            name: name.to_string(),
            description: None,
            files: BTreeMap::new(),
            binary_files: BTreeMap::new(),
        };
        for (k, b) in files {
            match String::from_utf8(b.clone()) {
                Ok(t) => {
                    s.files.insert(k.clone(), t);
                }
                Err(_) => {
                    s.binary_files.insert(k.clone(), super::util::hex(b));
                }
            }
        }
        s
    }
}

/// Something only a human can set up; `item` names it (e.g. `schedule Nightly`), `todo` says what to do.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ManualItem {
    /// What it is, e.g. `schedule Nightly`.
    pub item: String,
    /// What to do by hand.
    pub todo: String,
    /// ticked off by a human (`- [x]` in a checklist)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

impl ManualItem {
    /// An open (not done) item.
    pub fn new(item: impl Into<String>, todo: impl Into<String>) -> Self {
        ManualItem {
            item: item.into(),
            todo: todo.into(),
            done: false,
        }
    }
}

/// How a [`Schedule`] is triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    /// By time: `cron` is set.
    Scheduled,
    /// By something outside (mail, a webhook, ...): no `cron`.
    Event,
}

/// A timed or event-triggered job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    /// Job name.
    pub name: String,
    /// Five-field cron; absent for `event` schedules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// IANA timezone.
    pub tz: String,
    /// Time or event trigger.
    pub kind: ScheduleKind,
    /// Natural-language prompt to run.
    pub action: String,
    /// False for a paused job.
    pub enabled: bool,
}

/// A tool server the bot uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connector {
    /// Short id.
    pub name: String,
    /// e.g. `mcp-http`, `mcp-stdio`.
    pub kind: String,
    /// Endpoint, for http connectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Allowed tool names (empty: all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

/// A credential by NAME only. `deny_unknown_fields` makes any attempt to
/// carry a value (`value`, `secret`, ...) a parse error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRef {
    /// e.g. `TELEGRAM_BOT_TOKEN`.
    pub name: String,
    /// `where` from older documents: accepted and dropped, never written (a location phrase can hold a value).
    #[serde(rename = "where", default, skip_serializing)]
    pub legacy_where: serde::de::IgnoredAny,
}

impl CredentialRef {
    /// A reference by name.
    pub fn new(name: impl Into<String>) -> Self {
        CredentialRef {
            name: name.into(),
            legacy_where: serde::de::IgnoredAny,
        }
    }
}

/// Where conversation history is stored beside the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRef {
    /// Format, e.g. `jsonl`, `sqlite`.
    pub kind: String,
    /// Relative to the model file.
    pub path: String,
}

/// Parse `bot-model/MAJOR[.MINOR]` and reject other families/majors.
pub fn check_schema(schema: &str) -> Result<(), String> {
    let ver = schema
        .strip_prefix(SCHEMA_FAMILY)
        .and_then(|r| r.strip_prefix('/'))
        .ok_or_else(|| format!("not a {SCHEMA_FAMILY} document: schema {schema:?}"))?;
    let major: u32 = ver
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| format!("bad schema version {schema:?}"))?;
    if major != SCHEMA_MAJOR {
        return Err(format!(
            "unsupported major version {major} in {schema:?} (this build reads {SCHEMA_FAMILY}/{SCHEMA_MAJOR})"
        ));
    }
    Ok(())
}

impl BotModel {
    /// Parse a document: rejects another family or major version and any credential field but `name`; keeps
    /// unknown top-level fields from a newer minor in `extras["_unknown"]`.
    pub fn from_json(s: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| e.to_string())?;
        let schema = v
            .get("schema")
            .and_then(|x| x.as_str())
            .ok_or("missing \"schema\" field")?;
        check_schema(schema)?;
        let mut v = v;
        let obj = v.as_object_mut().ok_or("model is not a JSON object")?;
        let unknown: serde_json::Map<String, serde_json::Value> = obj
            .keys()
            .filter(|k| !KNOWN.contains(&k.as_str()))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|k| obj.remove(&k).map(|x| (k, x)))
            .collect();
        let mut m: BotModel = serde_json::from_value(v).map_err(|e| e.to_string())?;
        if !unknown.is_empty() {
            match m
                .extras
                .get_mut(UNKNOWN_KEY)
                .and_then(|x| x.as_object_mut())
            {
                Some(have) => have.extend(unknown),
                None => {
                    m.extras.insert(UNKNOWN_KEY.into(), unknown.into());
                }
            }
        }
        Ok(m)
    }

    /// Canonical form: 2-space pretty JSON, trailing newline.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("model serialises") + "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let p = format!("{}/tests/fixtures/model/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn fixtures_round_trip_byte_identical() {
        for name in ["full.json", "minimal.json"] {
            let src = fixture(name);
            let m = BotModel::from_json(&src).unwrap();
            assert_eq!(m.to_json(), src, "{name}");
        }
    }

    #[test]
    fn unknown_major_version_rejected() {
        let src = fixture("minimal.json").replace("bot-model/1", "bot-model/2");
        let err = BotModel::from_json(&src).unwrap_err();
        assert!(err.contains("unsupported major version 2"), "{err}");
        let src = fixture("minimal.json").replace("bot-model/1", "other/1");
        assert!(BotModel::from_json(&src).is_err());
    }

    #[test]
    fn newer_minor_version_accepted() {
        let src = fixture("minimal.json").replace("bot-model/1", "bot-model/1.3");
        assert!(BotModel::from_json(&src).is_ok());
    }

    #[test]
    fn newer_minor_unknown_fields_kept_in_extras() {
        let src = fixture("minimal.json")
            .replace("bot-model/1", "bot-model/1.3")
            .replace("\"persona\"", "\"mood\": \"sunny\",\n  \"persona\"");
        let m = BotModel::from_json(&src).unwrap();
        assert_eq!(m.extras[UNKNOWN_KEY]["mood"], "sunny");
        assert!(m.to_json().contains("sunny"));
    }

    #[test]
    fn credential_cannot_hold_a_value() {
        let src = fixture("full.json").replace(
            "\"name\": \"DEMO_TOKEN\"",
            "\"name\": \"DEMO_TOKEN\", \"value\": \"s3cret\"",
        );
        let err = BotModel::from_json(&src).unwrap_err();
        assert!(err.contains("value"), "{err}");
    }

    #[test]
    fn a_legacy_credential_where_is_accepted_and_never_written() {
        let src = fixture("full.json").replace(
            "\"name\": \"DEMO_TOKEN\"",
            "\"name\": \"DEMO_TOKEN\", \"where\": \"connector:demo\"",
        );
        let m = BotModel::from_json(&src).unwrap();
        assert_eq!(m.to_json(), fixture("full.json"));
    }
}
