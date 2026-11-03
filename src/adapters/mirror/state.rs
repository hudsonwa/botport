//! The Grok side's state file (`botport-mirror-state/1`) and the reader for a directory of them.
//!
//! The file is written by a program (`botport mirror digest-items` in the Grok box), never typed by a model: per item
//! the side digest, the neutral digest and, when the content is included, the name of its blob; coverage per kind;
//! the applied log; a sequence number `n`; and `sha256`, the digest of the file with that one field empty. A directory
//! holds `state-<n>.json` files and `blobs/<sha256>` (content addressed by the sha256 of the blob bytes).
//!
//! A reader takes the highest `n`, checks its `sha256` and pair, refuses to go back to a lower `n` than it accepted
//! before, and fetches blobs only when their bytes hash to their name and their content to the advertised digests.
use super::store::Store;
use super::PeerRead;
use crate::core::item::Content;
use crate::core::mirror::{Applied, Coverage, ItemId, Kind, SideRead, View};
use crate::core::util::{err, json_sorted, sha256_hex, Res};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema of the state file.
pub const SCHEMA: &str = "botport-mirror-state/1";

/// One item in a state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// side digest
    pub digest: String,
    /// neutral digest
    pub neutral: String,
    /// sha256 of the blob holding the content, when it is included
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// A state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFile {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// The pair it belongs to.
    pub pair: String,
    /// Sequence number, computed by the writer from its previous file.
    pub n: u64,
    /// When it was written (the writer's clock).
    pub written: String,
    /// Newest change pack generation applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_gen: Option<u64>,
    /// Coverage per kind.
    pub coverage: BTreeMap<Kind, Coverage>,
    /// Items.
    pub items: BTreeMap<ItemId, Entry>,
    /// Items the writer has but cannot offer, with the reason (held on both sides, never read as deleted).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unsupported: BTreeMap<ItemId, String>,
    /// The applied log (newest entries; the writer keeps a bounded tail).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<Applied>,
    /// sha256 of the file with this field empty (and no `mac`).
    #[serde(default)]
    pub sha256: String,
    /// HMAC with the pair key over the file without this field (when the pair has a key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

impl StateFile {
    /// The file's bytes with `sha256` filled in (unsigned).
    pub fn seal(&mut self) -> Vec<u8> {
        self.seal_with(None)
    }

    /// The file's bytes with `sha256` filled in and, with a pair key, `mac`.
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

    /// Parse and check a state file for `pair`.
    pub fn parse(b: &[u8], pair: &str) -> Res<StateFile> {
        let mut f: StateFile =
            serde_json::from_slice(b).map_err(|e| format!("state file is not valid: {e}"))?;
        if f.schema != SCHEMA {
            return err(format!(
                "state file has schema {:?}, expected {SCHEMA}",
                f.schema
            ));
        }
        if f.pair != pair {
            return err(format!("state file is for pair {:?}, not {pair:?}", f.pair));
        }
        let mac = f.mac.take();
        let claimed = std::mem::take(&mut f.sha256);
        if sha256_hex(json_sorted(&f).as_bytes()) != claimed {
            return err(format!("state file {} does not match its own sha256", f.n));
        }
        f.sha256 = claimed;
        f.mac = mac;
        Ok(f)
    }

    /// [`StateFile::parse`], and with a pair key the file must carry a valid `mac`.
    pub fn parse_with(b: &[u8], pair: &str, key: Option<&[u8]>) -> Res<StateFile> {
        let f = StateFile::parse(b, pair)?;
        if let Some(k) = key {
            crate::core::auth::verify(&serde_json::to_value(&f).expect("serializable"), k)?;
        }
        Ok(f)
    }

    /// As the merge engine sees it.
    pub fn to_read(&self) -> PeerRead {
        PeerRead {
            snapshot: format!(
                "state-{} {}",
                self.n,
                &self.sha256[..self.sha256.len().min(12)]
            ),
            written: self.written.clone(),
            side: SideRead {
                items: self
                    .items
                    .iter()
                    .map(|(k, e)| (k.clone(), View::new(e.digest.clone(), e.neutral.clone())))
                    .collect(),
                coverage: self.coverage.clone(),
                unsupported: self.unsupported.clone(),
            },
            applied: self.applied.clone(),
            applied_gen: self.applied_gen,
            // set by the reader that checked the mac (`StateDir::peer`), never from the file's say
            authenticated: false,
        }
    }
}

/// `state-<n>.json` -> n.
/// The highest state file number botport accepts or writes: far beyond any real pair, and far from overflowing.
pub const MAX_SEQ: u64 = 1_000_000_000_000;

pub fn state_seq(name: &str) -> Option<u64> {
    let n = name.strip_prefix("state-")?.strip_suffix(".json")?;
    (!n.is_empty()
        && n.len() <= 13
        && n.bytes().all(|b| b.is_ascii_digit())
        && !(n.len() > 1 && n.starts_with('0')))
    .then(|| n.parse().ok())
    .flatten()
    .filter(|n| *n <= MAX_SEQ)
}

/// Reads the newest state file in one directory of a [`Store`] and fetches blobs next to it.
pub struct StateDir<S: Store> {
    /// The store.
    pub store: S,
    dir: String,
    pair: String,
    /// The highest `n` accepted so far (from the mirror state, so a writer that lost its count is noticed).
    pub floor: Option<u64>,
    /// The pair key: when set, only files with a valid `mac` are accepted.
    pub key: Option<Vec<u8>>,
    current: Option<StateFile>,
}

impl<S: Store> StateDir<S> {
    /// A reader for `dir` (e.g. `from-grok`, or `""` for the store root).
    pub fn new(store: S, dir: &str, pair: &str, floor: Option<u64>) -> StateDir<S> {
        StateDir {
            store,
            dir: dir.trim_end_matches('/').to_string(),
            pair: pair.to_string(),
            floor,
            key: None,
            current: None,
        }
    }

    /// Accept only files signed with `key`.
    pub fn with_key(mut self, key: Option<Vec<u8>>) -> StateDir<S> {
        self.key = key;
        self
    }

    fn path(&self, name: &str) -> String {
        if self.dir.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.dir)
        }
    }

    /// The newest valid state file, or `None` when there is none. Files that fail their checks are skipped (a
    /// half-synced or tampered copy); a newest file below the floor is an error (the writer lost its count).
    pub fn read(&mut self) -> Res<Option<StateFile>> {
        let mut seqs: Vec<(u64, String)> = self
            .store
            .list(&self.dir)?
            .into_iter()
            .filter_map(|e| state_seq(&e.name).map(|n| (n, e.name)))
            .collect();
        seqs.sort();
        while let Some((n, name)) = seqs.pop() {
            let Some(b) = self.store.get(&self.path(&name))? else {
                continue;
            };
            let Ok(f) = StateFile::parse_with(&b, &self.pair, self.key.as_deref()) else {
                continue;
            };
            if f.n != n {
                continue;
            }
            if let Some(floor) = self.floor {
                if n < floor {
                    return err(format!(
                        "the newest state file is {n}, older than {floor} already accepted: the writer lost its count"
                    ));
                }
            }
            self.floor = Some(n);
            self.current = Some(f.clone());
            return Ok(Some(f));
        }
        Ok(None)
    }

    /// [`StateDir::read`] as the merge engine sees it; `authenticated` when the pair key verified the file.
    pub fn peer(&mut self) -> Res<Option<PeerRead>> {
        let keyed = self.key.is_some();
        Ok(self.read()?.map(|f| {
            let mut r = f.to_read();
            r.authenticated = keyed;
            r
        }))
    }

    /// The content of `id` from the current state file's blob, checked: the blob's bytes hash to its name, it parses
    /// for this item, and its neutral digest equals both the file's entry and `view`.
    pub fn fetch(&mut self, id: &ItemId, view: &View) -> Res<Content> {
        let Some(cur) = &self.current else {
            return err("no state file read yet");
        };
        let Some(e) = cur.items.get(id) else {
            return err(format!("{id}: not in the state file"));
        };
        let Some(blob) = e.blob.clone() else {
            return err(format!(
                "{id}: content not included in state file {}",
                cur.n
            ));
        };
        if e.neutral != view.neutral || e.digest != view.digest {
            return err(format!("{id}: changed since it was read"));
        }
        if blob.len() != 64
            || !blob
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return err(format!("{id}: invalid blob name"));
        }
        let b = self
            .store
            .get(&self.path(&format!("blobs/{blob}")))?
            .ok_or_else(|| format!("{id}: blob missing"))?;
        if sha256_hex(&b) != blob {
            return err(format!("{id}: blob does not match its name"));
        }
        let c = Content::from_blob(id, &b)?;
        if c.neutral_digest(id) != view.neutral {
            return err(format!(
                "{id}: blob content does not match the advertised digest"
            ));
        }
        Ok(c)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::store::FolderStore;
    use super::*;
    use crate::core::model::{Schedule, ScheduleKind};
    use std::path::{Path, PathBuf};

    pub fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("botport-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn id(s: &str) -> ItemId {
        ItemId::parse(s).unwrap()
    }

    /// An invented Grok-side snapshot: instructions and one routine, routines' coverage partial.
    pub fn sample(n: u64) -> (StateFile, Vec<(String, Vec<u8>)>) {
        let ins = id("persona.instructions");
        let r = id("routine:demo-routine");
        let ci = Content::Text("Example instructions for the demo bot.\n".into());
        let cr = Content::Routine(Schedule {
            name: "demo-routine".into(),
            cron: Some("0 9 * * 1".into()),
            tz: "UTC".into(),
            kind: ScheduleKind::Scheduled,
            action: "Example prompt.".into(),
            enabled: true,
        });
        let (bi, br) = (ci.to_blob(&ins), cr.to_blob(&r));
        let mut items = BTreeMap::new();
        items.insert(
            ins.clone(),
            Entry {
                digest: sha256_hex(b"side-ins"),
                neutral: ci.neutral_digest(&ins),
                blob: Some(sha256_hex(&bi)),
            },
        );
        items.insert(
            r.clone(),
            Entry {
                digest: sha256_hex(b"side-r"),
                neutral: cr.neutral_digest(&r),
                blob: Some(sha256_hex(&br)),
            },
        );
        let f = StateFile {
            schema: SCHEMA.into(),
            pair: "demo-bot".into(),
            n,
            written: "2000-01-01T00:00:00Z".into(),
            applied_gen: Some(4),
            coverage: [
                (Kind::Persona, Coverage::Complete),
                (Kind::Routine, Coverage::Partial),
            ]
            .into_iter()
            .collect(),
            items,
            unsupported: BTreeMap::new(),
            applied: vec![],
            sha256: String::new(),
            mac: None,
        };
        (f, vec![(sha256_hex(&bi), bi), (sha256_hex(&br), br)])
    }

    pub fn publish(root: &Path, dir: &str, n: u64) -> StateFile {
        let (mut f, blobs) = sample(n);
        let mut s = FolderStore::open(root).unwrap();
        let p = |x: &str| {
            if dir.is_empty() {
                x.to_string()
            } else {
                format!("{dir}/{x}")
            }
        };
        for (name, b) in blobs {
            s.put(&p(&format!("blobs/{name}")), &b).unwrap();
        }
        let bytes = f.seal();
        s.put(&p(&format!("state-{n}.json")), &bytes).unwrap();
        f
    }

    #[test]
    fn seal_and_parse() {
        let (mut f, _) = sample(3);
        let b = f.seal();
        assert_eq!(StateFile::parse(&b, "demo-bot").unwrap(), f);
        assert!(StateFile::parse(&b, "other").is_err());
        let t = String::from_utf8(b).unwrap();
        assert!(
            StateFile::parse(t.replace("\"n\": 3", "\"n\": 4").as_bytes(), "demo-bot").is_err()
        );
        assert!(StateFile::parse(
            t.replace(SCHEMA, "botport-mirror-state/2").as_bytes(),
            "demo-bot"
        )
        .is_err());
    }

    #[test]
    fn the_items_a_side_cannot_offer_are_sealed_with_the_rest() {
        let key = [9u8; 32];
        let rt = ItemId::parse("routine:demo-routine").unwrap();
        // a file without the list reads as before (and its sha256 does not change)
        let (mut f, _) = sample(3);
        let plain = f.seal();
        assert!(!String::from_utf8_lossy(&plain).contains("unsupported"));
        assert!(StateFile::parse(&plain, "demo-bot")
            .unwrap()
            .unsupported
            .is_empty());
        f.unsupported
            .insert(rt.clone(), "two of the bot's items have this id".into());
        let signed = f.seal_with(Some(&key));
        let p = StateFile::parse_with(&signed, "demo-bot", Some(&key)).unwrap();
        assert_eq!(
            p.to_read().side.unsupported[&rt],
            "two of the bot's items have this id"
        );
        // edited in place: the sha256 no longer matches
        let t = String::from_utf8(signed).unwrap();
        assert!(StateFile::parse(t.replace("two of", "one of").as_bytes(), "demo-bot").is_err());
        // re-sealed without the pair key: refused by a pair that has one
        let mut g = p.clone();
        g.unsupported.clear();
        let forged = g.seal_with(Some(&[1u8; 32]));
        assert!(StateFile::parse_with(&forged, "demo-bot", Some(&key)).is_err());
    }

    #[test]
    fn signed_files_need_the_pair_key() {
        let key = [9u8; 32];
        let (mut f, _) = sample(5);
        let signed = f.seal_with(Some(&key));
        assert!(f.mac.is_some());
        let p = StateFile::parse_with(&signed, "demo-bot", Some(&key)).unwrap();
        assert!(
            !p.to_read().authenticated,
            "only a reader that checked the mac says so"
        );
        assert!(StateFile::parse_with(&signed, "demo-bot", Some(&[1u8; 32])).is_err());
        let (mut g, _) = sample(5);
        let unsigned = g.seal();
        assert!(StateFile::parse_with(&unsigned, "demo-bot", Some(&key))
            .unwrap_err()
            .contains("not signed"));
        // without a key a signed file still reads (the mac is not part of its sha256)
        assert!(StateFile::parse(&signed, "demo-bot").is_ok());
        let d = tmp("signed");
        let mut s = FolderStore::open(&d).unwrap();
        s.put("state-5.json", &unsigned).unwrap();
        let mut r = StateDir::new(FolderStore::open(&d).unwrap(), "", "demo-bot", None)
            .with_key(Some(key.to_vec()));
        assert_eq!(
            r.read().unwrap(),
            None,
            "an unsigned file is ignored when the pair has a key"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn sequence_names() {
        assert_eq!(state_seq("state-12.json"), Some(12));
        assert_eq!(state_seq("state-0.json"), Some(0));
        for bad in [
            "state-.json",
            "state-01.json",
            "state-1.json.tmp",
            "state-a.json",
            "state-1",
        ] {
            assert_eq!(state_seq(bad), None, "{bad}");
        }
    }

    #[test]
    fn newest_valid_file_wins_and_no_going_back() {
        let d = tmp("seq");
        publish(&d, "from-grok", 1);
        publish(&d, "from-grok", 2);
        // a tampered newer file is skipped, not trusted
        let mut s = FolderStore::open(&d).unwrap();
        s.put(
            "from-grok/state-3.json",
            b"{\"schema\": \"botport-mirror-state/1\"}",
        )
        .unwrap();
        let mut r = StateDir::new(
            FolderStore::open(&d).unwrap(),
            "from-grok",
            "demo-bot",
            None,
        );
        assert_eq!(r.read().unwrap().unwrap().n, 2);
        assert_eq!(r.floor, Some(2));
        // a reader that already accepted 5 refuses a newest file of 2
        let mut r = StateDir::new(
            FolderStore::open(&d).unwrap(),
            "from-grok",
            "demo-bot",
            Some(5),
        );
        assert!(r.read().unwrap_err().contains("lost its count"));
        // a file whose name and n disagree is skipped
        let (mut f, _) = sample(9);
        s.put("from-grok/state-10.json", &f.seal()).unwrap();
        let mut r = StateDir::new(
            FolderStore::open(&d).unwrap(),
            "from-grok",
            "demo-bot",
            None,
        );
        assert_eq!(r.read().unwrap().unwrap().n, 2);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn fetch_checks_everything() {
        let d = tmp("fetch");
        let f = publish(&d, "", 1);
        let mut r = StateDir::new(FolderStore::open(&d).unwrap(), "", "demo-bot", None);
        let ins = id("persona.instructions");
        let e = f.items[&ins].clone();
        let v = View::new(e.digest.clone(), e.neutral.clone());
        assert!(r.fetch(&ins, &v).unwrap_err().contains("no state file"));
        let read = r.read().unwrap().unwrap().to_read();
        assert_eq!(read.side.items[&ins], v);
        assert_eq!(read.side.coverage[&Kind::Routine], Coverage::Partial);
        assert!(matches!(r.fetch(&ins, &v).unwrap(), Content::Text(_)));
        // the read advertised another digest: the side changed mid-run
        assert!(r
            .fetch(&ins, &View::new(e.digest.clone(), "x"))
            .unwrap_err()
            .contains("changed"));
        // a blob whose bytes do not match its name
        let mut s = FolderStore::open(&d).unwrap();
        s.put(&format!("blobs/{}", e.blob.as_ref().unwrap()), b"tampered")
            .unwrap();
        assert!(r
            .fetch(&ins, &v)
            .unwrap_err()
            .contains("does not match its name"));
        assert!(r.fetch(&id("skill:absent"), &v).is_err());
        std::fs::remove_dir_all(&d).ok();
    }
}
