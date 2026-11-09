//! Change packs (`botport-pack/1`) and the `HEAD` line that names the current one. Spec: docs/design/MIRROR.md
//! section 4.4 and 6.3.
//!
//! A pack is one manifest file listing every item still in flight to the Grok side (packs are cumulative: only the
//! one `HEAD` names counts). Per item: the action, the neutral digest the Grok side should have now (`expect`), the
//! neutral digest it should have after (`want`), and the blob holding the new content. Like state files, a manifest
//! carries a sha256 of itself with that field empty.
use crate::core::mirror::{ItemId, ABSENT};
use crate::core::util::{err, json_sorted, sha256_hex, Res};
use serde::{Deserialize, Serialize};

/// Schema of a pack manifest.
pub const SCHEMA: &str = "botport-pack/1";

/// Blobs to publish with a pack: (sha256, bytes).
pub type Blobs = Vec<(String, Vec<u8>)>;

/// What a pack asks for one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackAction {
    /// install the blob's content
    Put,
    /// remove the item
    Retire,
}

/// One item of a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackItem {
    /// The item.
    pub item: ItemId,
    /// Install or remove.
    pub action: PackAction,
    /// Neutral digest the live copy must have for the install to go ahead ([`ABSENT`]: must not exist).
    pub expect: String,
    /// Neutral digest after the install ([`ABSENT`] for a retire).
    pub want: String,
    /// sha256 of the blob with the new content (`Put` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// A pack manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// The pair.
    pub pair: String,
    /// Generation (monotonic, never reused).
    pub gen: u64,
    /// The neutral form the digests were made with ([`crate::core::package::NEUTRAL_FORM`]).
    #[serde(default = "crate::core::package::neutral_form_v1")]
    pub neutral_form: u32,
    /// Random id, so two packs of one generation can never be mistaken for each other.
    pub id: String,
    /// The machine that wrote it.
    pub machine: String,
    /// When it was written.
    pub written: String,
    /// Every item in flight.
    pub items: Vec<PackItem>,
    /// sha256 of the manifest with this field empty (and no `mac`).
    #[serde(default)]
    pub sha256: String,
    /// HMAC with the pair key over the manifest without this field (when the pair has a key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

impl Pack {
    /// A new pack; `id` should be random (see [`random_id`]).
    pub fn new(
        pair: &str,
        gen: u64,
        id: &str,
        machine: &str,
        written: &str,
        items: Vec<PackItem>,
    ) -> Res<Pack> {
        if !token(id) || !token(machine) {
            return err("pack id and machine must be 1-32 lowercase hex characters");
        }
        let mut seen = std::collections::BTreeSet::new();
        for it in &items {
            if !seen.insert(&it.item) {
                return err(format!("{}: listed twice in one pack", it.item));
            }
            if it
                .blob
                .as_ref()
                .is_some_and(|b| b.len() != 64 || !token(&b[..32]) || !token(&b[32..]))
            {
                return err(format!("{}: invalid blob name", it.item));
            }
            let ok = match it.action {
                PackAction::Put => it.blob.is_some() && it.want != ABSENT,
                PackAction::Retire => it.blob.is_none() && it.want == ABSENT,
            };
            if !ok {
                return err(format!("{}: inconsistent pack item", it.item));
            }
        }
        Ok(Pack {
            schema: SCHEMA.into(),
            pair: pair.into(),
            gen,
            neutral_form: crate::core::package::NEUTRAL_FORM,
            id: id.into(),
            machine: machine.into(),
            written: written.into(),
            items,
            sha256: String::new(),
            mac: None,
        })
    }

    /// The file name: `pack-<gen>-<id>.json`.
    pub fn file_name(&self) -> String {
        format!("pack-{}-{}.json", self.gen, self.id)
    }

    /// The manifest bytes with `sha256` filled in (unsigned).
    pub fn seal(&mut self) -> Vec<u8> {
        self.seal_with(None)
    }

    /// The manifest bytes with `sha256` filled in and, with a pair key, `mac`.
    pub fn seal_with(&mut self, key: Option<&[u8]>) -> Vec<u8> {
        self.mac = None;
        self.sha256 = String::new();
        self.sha256 = sha256_hex(json_sorted(self).as_bytes());
        if let Some(k) = key {
            let mut v = serde_json::to_value(&*self).expect("serializable");
            crate::core::auth::sign(&mut v, k);
            self.mac = v["mac"].as_str().map(String::from);
        }
        json_sorted(self).into_bytes()
    }

    /// Whether `b`, a manifest that failed [`Pack::parse_with`], is this pair's, intact and (with a key) signed, and
    /// written in another neutral form. Decided on the file's fields, after it verified, never on an error's text.
    pub fn other_form(b: &[u8], pair: &str, key: Option<&[u8]>) -> bool {
        let Ok(raw) = serde_json::from_slice::<serde_json::Value>(b) else {
            return false;
        };
        let v = raw
            .get("neutral_form")
            .map_or(Some(1), |v| v.as_u64().and_then(|v| u32::try_from(v).ok()));
        raw["schema"] == SCHEMA
            && raw["pair"] == pair
            && v.is_some_and(|v| crate::core::package::verified_other_form(b, v, key))
    }

    /// [`Pack::parse`], and with a pair key the manifest must carry a valid `mac`.
    pub fn parse_with(b: &[u8], pair: &str, key: Option<&[u8]>) -> Res<Pack> {
        let p = Pack::parse_keyed(b, pair, key)?;
        if let Some(k) = key {
            crate::core::auth::verify(&serde_json::to_value(&p).expect("serializable"), k)?;
        }
        Ok(p)
    }

    /// Parse and check a manifest for `pair`.
    pub fn parse(b: &[u8], pair: &str) -> Res<Pack> {
        Pack::parse_keyed(b, pair, None)
    }

    /// [`Pack::parse`]; a file in another neutral form is reported only when it verifies with `key`.
    fn parse_keyed(b: &[u8], pair: &str, key: Option<&[u8]>) -> Res<Pack> {
        let mut p: Pack =
            serde_json::from_slice(b).map_err(|e| crate::core::util::json_invalid("pack", &e))?;
        if p.schema != SCHEMA || p.pair != pair {
            return err("pack is for another pair or schema");
        }
        crate::core::package::check_form_of(b, "the change pack", p.neutral_form, key)?;
        let mac = p.mac.take();
        let claimed = std::mem::take(&mut p.sha256);
        if sha256_hex(json_sorted(&p).as_bytes()) != claimed {
            return err(format!("pack {} does not match its own sha256", p.gen));
        }
        p.sha256 = claimed;
        p.mac = mac;
        Pack::new(
            &p.pair,
            p.gen,
            &p.id,
            &p.machine,
            &p.written,
            p.items.clone(),
        )?;
        Ok(p)
    }

    /// The `HEAD` that names this (sealed) pack.
    pub fn head(&self) -> Head {
        Head {
            gen: self.gen,
            pack: self.file_name(),
            sha: self.sha256.clone(),
            machine: self.machine.clone(),
        }
    }
}

/// The `HEAD` line: `gen=<n> pack=<file> sha=<sha256 of the manifest> machine=<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// Generation.
    pub gen: u64,
    /// Manifest file name.
    pub pack: String,
    /// Manifest sha256.
    pub sha: String,
    /// Writer machine.
    pub machine: String,
}

impl Head {
    /// The line, with a trailing newline.
    pub fn to_line(&self) -> String {
        format!(
            "gen={} pack={} sha={} machine={}\n",
            self.gen, self.pack, self.sha, self.machine
        )
    }

    /// Parse a `HEAD` file.
    pub fn parse(b: &[u8]) -> Res<Head> {
        let t = std::str::from_utf8(b).map_err(|_| "HEAD is not text".to_string())?;
        let mut h = Head {
            gen: 0,
            pack: String::new(),
            sha: String::new(),
            machine: String::new(),
        };
        let mut gen = None;
        for kv in t.split_whitespace() {
            match kv.split_once('=') {
                Some(("gen", v)) => gen = v.parse().ok(),
                Some(("pack", v)) => h.pack = v.into(),
                Some(("sha", v)) => h.sha = v.into(),
                Some(("machine", v)) => h.machine = v.into(),
                _ => return err("HEAD has an unknown field"),
            }
        }
        h.gen = gen.ok_or("HEAD has no gen")?;
        if h.pack.is_empty() || h.sha.is_empty() || h.machine.is_empty() || h.pack.contains('/') {
            return err("HEAD is incomplete");
        }
        // the machine is printed (a second writer is named in an error): held to the pack's form, never free text
        if !token(&h.machine) {
            return err("HEAD has an invalid machine id");
        }
        Ok(h)
    }
}

/// A pack id or machine id: 1-32 lowercase hex characters.
fn token(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// 48 random bits as hex, for pack ids and machine ids; an error without a random source (never the time or pid,
/// which another writer could repeat or guess).
pub fn try_random_id() -> Res<String> {
    let mut b = [0u8; 6];
    crate::core::util::os_random(&mut b)?;
    Ok(crate::core::util::hex(&b))
}

/// [`try_random_id`] for callers that cannot return an error: without a random source it panics rather than make
/// a guessable id.
pub fn random_id() -> String {
    try_random_id().expect("no random source for an id")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, action: PackAction) -> PackItem {
        PackItem {
            item: ItemId::parse(name).unwrap(),
            action,
            expect: "e".into(),
            want: if action == PackAction::Put {
                "w".into()
            } else {
                ABSENT.into()
            },
            blob: (action == PackAction::Put).then(|| "b".repeat(64)),
        }
    }

    #[test]
    fn seal_parse_and_head() {
        let mut p = Pack::new(
            "demo-bot",
            7,
            "a1b2c3",
            "a1",
            "2000-01-01T00:00:00Z",
            vec![
                item("skill:demo-skill", PackAction::Put),
                item("routine:demo-routine", PackAction::Retire),
            ],
        )
        .unwrap();
        let b = p.seal();
        assert_eq!(Pack::parse(&b, "demo-bot").unwrap(), p);
        assert!(Pack::parse(&b, "other").is_err());
        let t = String::from_utf8(b).unwrap();
        assert!(Pack::parse(t.replace("\"gen\": 7", "\"gen\": 8").as_bytes(), "demo-bot").is_err());
        let h = p.head();
        assert_eq!(h.pack, "pack-7-a1b2c3.json");
        assert_eq!(Head::parse(h.to_line().as_bytes()).unwrap(), h);
        assert!(Head::parse(b"gen=1 pack=../x sha=s machine=m").is_err());
        assert!(Head::parse(b"gen=x pack=p sha=s machine=m").is_err());
        assert!(Head::parse(b"gen=1 pack=p sha=s machine=m extra=1").is_err());
        // the machine id is held to the pack's form: no free text to print
        assert!(Head::parse(b"gen=1 pack=p sha=s machine=a1").is_ok());
        for m in ["m", "A1", "a1\u{202e}", &"a".repeat(33)] {
            let line = format!("gen=1 pack=p sha=s machine={m}");
            assert!(Head::parse(line.as_bytes()).is_err(), "{m}");
        }
    }

    #[test]
    fn signed_packs_need_the_pair_key() {
        let key = [3u8; 32];
        let mut p = Pack::new(
            "demo-bot",
            2,
            "ab",
            "cd",
            "t",
            vec![item("skill:demo-skill", PackAction::Put)],
        )
        .unwrap();
        let b = p.seal_with(Some(&key));
        assert!(Pack::parse_with(&b, "demo-bot", Some(&key)).is_ok());
        assert!(Pack::parse_with(&b, "demo-bot", Some(&[4u8; 32])).is_err());
        let mut u = p.clone();
        let ub = u.seal();
        assert!(Pack::parse_with(&ub, "demo-bot", Some(&key)).is_err());
        assert!(Pack::parse_with(&ub, "demo-bot", None).is_ok());
        assert_eq!(u.sha256, p.sha256, "the mac is not part of the sha256");
    }

    #[test]
    fn parse_errors_never_quote_the_file() {
        let mut p = Pack::new(
            "demo-bot",
            1,
            "ab",
            "cd",
            "t",
            vec![item("skill:demo-skill", PackAction::Put)],
        )
        .unwrap();
        let t = String::from_utf8(p.seal()).unwrap();
        let bad = t.replace("\"put\"", "\"example-secret-word\"");
        let e = Pack::parse(bad.as_bytes(), "demo-bot").unwrap_err();
        assert!(
            e.contains("line") && !e.contains("example-secret-word"),
            "{e}"
        );
        let e = crate::adapters::mirror::state::StateFile::parse(
            b"{\"schema\": [\"example-secret-word\"]}",
            "demo-bot",
        )
        .unwrap_err();
        assert!(!e.contains("example-secret-word"), "{e}");
    }

    #[test]
    fn a_pack_in_another_neutral_form_stops_only_when_signed() {
        let key = [3u8; 32];
        let mut p = Pack::new(
            "demo-bot",
            2,
            "ab",
            "cd",
            "t",
            vec![item("skill:demo-skill", PackAction::Put)],
        )
        .unwrap();
        p.neutral_form = 1;
        let mismatch = crate::core::package::NEUTRAL_FORM_MISMATCH;
        let e = Pack::parse_with(&p.seal_with(Some(&key)), "demo-bot", Some(&key)).unwrap_err();
        assert!(e.contains(mismatch), "{e}");
        // keyless: only its own sha256 vouches for it, so it is an unreadable pack, not a reason to republish
        let e = Pack::parse_with(&p.seal(), "demo-bot", None).unwrap_err();
        assert!(!e.contains(mismatch), "{e}");
    }

    #[test]
    fn inconsistent_items_are_refused() {
        let mut bad = item("skill:demo-skill", PackAction::Put);
        bad.blob = None;
        assert!(Pack::new("demo-bot", 1, "ab", "cd", "t", vec![bad]).is_err());
        let mut bad = item("skill:demo-skill", PackAction::Retire);
        bad.want = "w".into();
        assert!(Pack::new("demo-bot", 1, "ab", "cd", "t", vec![bad]).is_err());
        assert_eq!(random_id().len(), 12);
        assert_ne!(random_id(), random_id());
    }
}
