//! Wire envelope plus canonical JSON (sorted keys, compact) and its sha256.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Msg,
    End,
    Error,
    Timeout,
    Ack,
    /// One-way Grok -> Hermes message: its reply is not queued.
    Notice,
    /// Body: an [`crate::ops::OpRequest`] as JSON.
    Op,
    /// Body: an [`crate::ops::OpResult`] as JSON.
    #[serde(rename = "op_result")]
    OpResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    /// UUIDv7 string.
    pub id: String,
    pub conv_id: String,
    pub from: String,
    pub to: String,
    pub hop: u32,
    pub reply_to: Option<String>,
    /// RFC3339 UTC.
    pub ts: String,
    pub kind: Kind,
    pub body: String,
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Envelope {
    pub fn new(conv_id: &str, from: &str, to: &str, kind: Kind, body: &str) -> Envelope {
        Envelope {
            v: VERSION,
            id: new_id(),
            conv_id: conv_id.into(),
            from: from.into(),
            to: to.into(),
            hop: 0,
            reply_to: None,
            ts: now_ts(),
            kind,
            body: body.into(),
        }
    }
    /// A reply in the same conversation with hop + 1.
    pub fn reply(&self, kind: Kind, body: &str) -> Envelope {
        let mut e = Envelope::new(&self.conv_id, &self.to, &self.from, kind, body);
        e.hop = self.hop.saturating_add(1);
        e.reply_to = Some(self.id.clone());
        e
    }
    pub fn canonical_json(&self) -> String {
        canonical_json(&serde_json::to_value(self).expect("envelope serializes"))
    }
    pub fn sha256_hex(&self) -> String {
        sha256_hex(self.canonical_json().as_bytes())
    }
}

/// Compact JSON with object keys sorted recursively.
pub fn canonical_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical_json(&m[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> Envelope {
        Envelope {
            v: 1,
            id: "01".into(),
            conv_id: "c".into(),
            from: "a".into(),
            to: "b".into(),
            hop: 2,
            reply_to: None,
            ts: "2026-11-02T00:00:00.000Z".into(),
            kind: Kind::Msg,
            body: "hi \"x\"".into(),
        }
    }

    #[test]
    fn canonical_is_sorted_compact_and_stable() {
        let e = fixed();
        let c = e.canonical_json();
        assert_eq!(
            c,
            r#"{"body":"hi \"x\"","conv_id":"c","from":"a","hop":2,"id":"01","kind":"msg","reply_to":null,"to":"b","ts":"2026-11-02T00:00:00.000Z","v":1}"#
        );
        // reparse + recanonicalise is identical
        let back: Envelope = serde_json::from_str(&c).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.canonical_json(), c);
        assert_eq!(e.sha256_hex(), sha256_hex(c.as_bytes()));
        assert_eq!(e.sha256_hex().len(), 64);
    }

    #[test]
    fn nested_keys_sorted() {
        let v: serde_json::Value = serde_json::from_str(r#"{"b":[{"z":1,"a":2}],"a":1}"#).unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":1,"b":[{"a":2,"z":1}]}"#);
    }

    #[test]
    fn reply_increments_hop_and_ids_are_v7() {
        let e = Envelope::new("c", "a", "b", Kind::Msg, "x");
        assert_eq!(uuid::Uuid::parse_str(&e.id).unwrap().get_version_num(), 7);
        assert!(chrono::DateTime::parse_from_rfc3339(&e.ts).is_ok() && e.ts.ends_with('Z'));
        let r = e.reply(Kind::Ack, "");
        assert_eq!(
            (r.hop, r.from.as_str(), r.reply_to.as_deref()),
            (1, "b", Some(e.id.as_str()))
        );
    }

    #[test]
    fn kinds_roundtrip() {
        for k in ["msg", "end", "error", "timeout", "ack"] {
            let kk: Kind = serde_json::from_str(&format!("\"{k}\"")).unwrap();
            assert_eq!(serde_json::to_string(&kk).unwrap(), format!("\"{k}\""));
        }
    }

    #[test]
    fn reply_hop_saturates() {
        let mut e = Envelope::new("c", "a", "b", Kind::Msg, "x");
        e.hop = u32::MAX;
        assert_eq!(e.reply(Kind::Msg, "y").hop, u32::MAX);
    }
}
