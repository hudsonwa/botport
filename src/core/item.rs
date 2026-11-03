//! The content of one mirror item and its digests. Spec: docs/design/MIRROR.md section 3.
//!
//! Each item has two digests: the **side digest** (sha256 of the item as one side stores it) and the **neutral
//! digest** (sha256 of the item in the neutral form, comparable across sides). Content travels between sides as a
//! **blob** (`botport-blob/1`, one JSON document; package files base64), addressed by the sha256 of its bytes.
//!
//! ```
//! use botport::core::item::Content;
//! use botport::core::mirror::ItemId;
//!
//! let id = ItemId::parse("persona.instructions").unwrap();
//! let a = Content::Text("Be brief.\r\n".into());
//! let b = Content::Text("Be brief.  \n\n".into());
//! assert_eq!(a.neutral_digest(&id), b.neutral_digest(&id)); // line endings and trailing blanks do not count
//! let blob = a.to_blob(&id);
//! assert_eq!(Content::from_blob(&id, &blob).unwrap(), a);
//! ```
#![warn(missing_docs)]

use super::mirror::{ItemId, Kind};
use super::model::{Schedule, ScheduleKind};
use super::package::Package;
use super::util::{b64, err, sha256_hex, unb64, Res};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Schema of a blob.
pub const BLOB_SCHEMA: &str = "botport-blob/1";
/// The connector fields a blob carries; anything else is dropped before it leaves a side.
pub const CONNECTOR_FIELDS: [&str; 5] = ["id", "name", "kind", "url", "tools"];

/// What one item holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    /// `persona.name` or `persona.instructions`: the exact text.
    Text(String),
    /// `skill:<name>`: the package, byte for byte.
    Package(Package),
    /// `routine:<name>`: the schedule in the neutral model.
    Routine(Schedule),
    /// `connector:<id>`: the allowlisted fields only.
    Connector(BTreeMap<String, Value>),
}

impl Content {
    /// The kind of item this content can be.
    pub fn kind(&self) -> Kind {
        match self {
            Content::Text(_) => Kind::Persona,
            Content::Package(_) => Kind::Skill,
            Content::Routine(_) => Kind::Routine,
            Content::Connector(_) => Kind::Connector,
        }
    }

    /// The neutral digest (section 3): text canonicalised, packages by their package digest, routines and connectors
    /// as sorted-key JSON of the neutral fields. A routine's neutral form includes `enabled`, so pausing or switching
    /// it on is a change carried like any other; a side that holds a copy paused by policy (awaiting approval) reads
    /// it as enabled, so that copy is in step (section 4.5).
    pub fn neutral_digest(&self, id: &ItemId) -> String {
        match self {
            Content::Text(t) => sha256_hex(canonical_text(id, t).as_bytes()),
            Content::Package(p) => p.neutral_digest(),
            Content::Routine(s) => {
                super::routines::neutral_digest(s, super::routines::DEFAULT_TZ, false)
            }
            Content::Connector(m) => sha256_hex(connector_neutral(m).to_string().as_bytes()),
        }
    }

    /// The blob: `{"schema": "botport-blob/1", "item": ..., <one content field>}`, keys sorted, no whitespace.
    pub fn to_blob(&self, id: &ItemId) -> Vec<u8> {
        let mut v = json!({"schema": BLOB_SCHEMA, "item": id.as_str()});
        match self {
            Content::Text(t) => v["text"] = json!(t),
            Content::Package(p) => {
                let files: Map<String, Value> = p
                    .files
                    .iter()
                    .map(|(k, (b, x))| {
                        let mut f = json!({"b64": b64(b)});
                        if *x {
                            f["x"] = json!(true);
                        }
                        (k.clone(), f)
                    })
                    .collect();
                v["package"] = json!({"name": p.name, "files": files});
            }
            Content::Routine(s) => {
                v["routine"] = serde_json::to_value(s).expect("schedule is serializable")
            }
            Content::Connector(m) => v["connector"] = connector_neutral(m),
        }
        serde_json::to_vec(&v).expect("json")
    }

    /// Parse a blob for `id`; refuses a blob for another item, of the wrong kind, or with a malformed field.
    pub fn from_blob(id: &ItemId, b: &[u8]) -> Res<Content> {
        let v: Value =
            serde_json::from_slice(b).map_err(|e| format!("{id}: blob is not JSON: {e}"))?;
        if v["schema"] != BLOB_SCHEMA {
            return err(format!("{id}: blob has an unknown schema"));
        }
        if v["item"] != id.as_str() {
            return err(format!("{id}: blob is for another item"));
        }
        let c = match id.kind() {
            Kind::Persona => Content::Text(
                v["text"]
                    .as_str()
                    .ok_or_else(|| format!("{id}: blob has no text"))?
                    .to_string(),
            ),
            Kind::Skill => {
                let p = &v["package"];
                let mut files = BTreeMap::new();
                for (k, f) in p["files"]
                    .as_object()
                    .ok_or_else(|| format!("{id}: blob has no package files"))?
                {
                    let bytes = f["b64"]
                        .as_str()
                        .and_then(unb64)
                        .ok_or_else(|| format!("{id}: blob file {k:?} is not base64"))?;
                    files.insert(k.clone(), (bytes, f["x"] == true));
                }
                if p["name"] != id.name() {
                    return err(format!("{id}: blob package has another name"));
                }
                Content::Package(Package::from_files(id.name(), files)?)
            }
            Kind::Routine => {
                let s: Schedule = serde_json::from_value(v["routine"].clone())
                    .map_err(|e| format!("{id}: blob routine: {e}"))?;
                if s.name != id.name() {
                    return err(format!("{id}: blob routine has another name"));
                }
                Content::Routine(s)
            }
            Kind::Connector => {
                let m = v["connector"]
                    .as_object()
                    .ok_or_else(|| format!("{id}: blob has no connector"))?;
                // only the allowlisted fields ever leave a blob (the neutral digest covers no other)
                let m: BTreeMap<String, Value> =
                    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let Value::Object(n) = connector_neutral(&m) else {
                    return err(format!("{id}: blob connector is not an object"));
                };
                if n.get("id").is_some_and(|v| v != id.name()) {
                    return err(format!("{id}: blob connector has another id"));
                }
                Content::Connector(n.into_iter().collect())
            }
        };
        Ok(c)
    }
}

/// The text form hashed for a persona item: `persona.name` trimmed; instructions with CRLF -> LF, trailing whitespace
/// dropped per line, trailing blank lines dropped, one final LF (empty text stays empty).
pub fn canonical_text(id: &ItemId, t: &str) -> String {
    if id.as_str() == "persona.name" {
        return t.trim().to_string();
    }
    let lines: Vec<&str> = t
        .split('\n')
        .map(|l| l.trim_end_matches(['\r', ' ', '\t']))
        .collect();
    let body = lines.join("\n");
    let body = body.trim_end_matches('\n');
    if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    }
}

/// A simpler neutral form of a routine (schedule whitespace collapsed, timezone, kind, prompt and, unless
/// `paused_by_policy`, the enabled flag). Mirror digests use `core::routines::neutral_digest`, which also normalises
/// the schedule and timezone spellings.
pub fn routine_neutral(s: &Schedule, paused_by_policy: bool) -> Value {
    let mut v = json!({
        "schedule": s.cron.as_deref().map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")),
        "tz": s.tz,
        "kind": match s.kind { ScheduleKind::Scheduled => "scheduled", ScheduleKind::Event => "event" },
        "prompt": canonical_prompt(&s.action),
    });
    if !paused_by_policy {
        v["enabled"] = json!(s.enabled);
    }
    v
}

fn canonical_prompt(t: &str) -> String {
    t.split('\n')
        .map(|l| l.trim_end_matches(['\r', ' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// The neutral form of a connector: the allowlisted fields only, `tools` sorted.
pub fn connector_neutral(m: &BTreeMap<String, Value>) -> Value {
    let mut out = Map::new();
    for k in CONNECTOR_FIELDS {
        if let Some(v) = m.get(k) {
            let v = match (k, v) {
                ("tools", Value::Array(a)) => {
                    let mut t: Vec<String> = a
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect();
                    t.sort();
                    t.dedup();
                    json!(t)
                }
                _ => v.clone(),
            };
            out.insert(k.to_string(), v);
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ItemId {
        ItemId::parse(s).unwrap()
    }

    fn routine(cron: &str, enabled: bool) -> Schedule {
        Schedule {
            name: "demo-routine".into(),
            cron: Some(cron.into()),
            tz: "UTC".into(),
            kind: ScheduleKind::Scheduled,
            action: "Example prompt: summarise the demo log.".into(),
            enabled,
        }
    }

    #[test]
    fn text_canonical_forms() {
        let i = id("persona.instructions");
        assert_eq!(canonical_text(&i, "a  \r\nb\t\n\n\n"), "a\nb\n");
        assert_eq!(canonical_text(&i, ""), "");
        assert_eq!(canonical_text(&i, "\n\n"), "");
        // leading whitespace and inner blank lines are meaning, kept
        assert_ne!(canonical_text(&i, "  a\n\nb"), canonical_text(&i, "a\nb"));
        assert_eq!(
            canonical_text(&id("persona.name"), "  Demo Bot \n"),
            "Demo Bot"
        );
    }

    #[test]
    fn routine_neutral_form() {
        let a = Content::Routine(routine("30  7 * * *", true));
        let b = Content::Routine(routine("30 7 * * *", true));
        let r = id("routine:demo-routine");
        assert_eq!(a.neutral_digest(&r), b.neutral_digest(&r));
        // pausing is a change
        let paused = Content::Routine(routine("30 7 * * *", false));
        assert_ne!(a.neutral_digest(&r), paused.neutral_digest(&r));
        assert_ne!(
            routine_neutral(&routine("30 7 * * *", false), false),
            routine_neutral(&routine("30 7 * * *", true), false)
        );
        let other = Content::Routine(routine("30 8 * * *", true));
        assert_ne!(a.neutral_digest(&r), other.neutral_digest(&r));
        // one schedule spelled two ways is one routine
        let every = Content::Routine(routine("every 6h", true));
        let cron = Content::Routine(routine("0 */6 * * *", true));
        assert_eq!(every.neutral_digest(&r), cron.neutral_digest(&r));
    }

    #[test]
    fn connector_neutral_drops_other_fields() {
        let m: BTreeMap<String, Value> = [
            ("id".to_string(), json!("example-connector")),
            ("url".to_string(), json!("https://mcp.example.test/mcp")),
            ("tools".to_string(), json!(["b", "a", "a"])),
            ("headers".to_string(), json!({"x": "y"})),
        ]
        .into_iter()
        .collect();
        let n = connector_neutral(&m);
        assert_eq!(
            n,
            json!({"id": "example-connector", "tools": ["a", "b"], "url": "https://mcp.example.test/mcp"})
        );
        let blob = Content::Connector(m).to_blob(&id("connector:example-connector"));
        assert!(
            !String::from_utf8(blob).unwrap().contains("headers"),
            "only allowlisted fields leave a side"
        );
    }

    #[test]
    fn blobs_round_trip_and_are_checked() {
        let mut files = BTreeMap::new();
        files.insert("SKILL.md".to_string(), (b"# Demo skill\n".to_vec(), false));
        files.insert("bin/tool".to_string(), (vec![0, 159, 146, 150, 255], true));
        let p = Package::from_files("demo-skill", files).unwrap();
        let s = id("skill:demo-skill");
        let c = Content::Package(p.clone());
        let blob = c.to_blob(&s);
        assert_eq!(Content::from_blob(&s, &blob).unwrap(), c);
        assert_eq!(c.neutral_digest(&s), p.neutral_digest());
        // the wrong item, a renamed package, a broken file, an unknown schema
        assert!(Content::from_blob(&id("skill:other"), &blob).is_err());
        let t = String::from_utf8(blob.clone()).unwrap();
        assert!(Content::from_blob(
            &s,
            t.replace("\"name\":\"demo-skill\"", "\"name\":\"x\"")
                .as_bytes()
        )
        .is_err());
        assert!(
            Content::from_blob(&s, t.replacen("\"b64\":\"", "\"b64\":\"!", 1).as_bytes()).is_err()
        );
        assert!(
            Content::from_blob(&s, t.replace(BLOB_SCHEMA, "botport-blob/9").as_bytes()).is_err()
        );
        // a package path that tries to leave its directory is refused on the way in
        let evil = t.replace("bin/tool", "../tool");
        assert!(Content::from_blob(&s, evil.as_bytes()).is_err());
        let r = id("routine:demo-routine");
        let rc = Content::Routine(routine("*/15 * * * *", true));
        assert_eq!(Content::from_blob(&r, &rc.to_blob(&r)).unwrap(), rc);
        // a routine blob for another name, a connector blob with extra fields
        assert!(Content::from_blob(&id("routine:other"), &rc.to_blob(&r)).is_err());
        let c = id("connector:example-connector");
        let raw = br#"{"schema":"botport-blob/1","item":"connector:example-connector","connector":{"id":"example-connector","url":"https://mcp.example.test/mcp","command":"sh","env":{"X":"y"}}}"#;
        let Content::Connector(m) = Content::from_blob(&c, raw).unwrap() else {
            panic!()
        };
        assert_eq!(
            m.keys().map(String::as_str).collect::<Vec<_>>(),
            ["id", "url"]
        );
    }
}
