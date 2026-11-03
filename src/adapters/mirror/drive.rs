//! The Drive transport: one folder per pair (`botport-mirror/<pair>/`), reached through a [`Store`] — the folder as
//! Google Drive for desktop syncs it ([`super::store::FolderStore`]) or the Drive API ([`super::api::ApiStore`]).
//!
//! ```text
//! HEAD                          gen=<n> pack=<file> sha=<sha256> machine=<id>      (botport)
//! to-grok/pack-<gen>-<id>.json  cumulative change pack manifest                    (botport)
//! to-grok/blobs/<sha256>        content of pack items                              (botport)
//! from-grok/state-<n>.json      Grok side's state file                             (Grok routine, via botport)
//! from-grok/blobs/<sha256>      content of items the Grok side changed             (Grok routine, via botport)
//! digest/hermes-<n>.md          Hermes activity digest                             (botport)
//! digest/grok-<n>.md            Grok activity digest                               (botport; not the Grok routine yet)
//! ```
//! Files are immutable once written except `HEAD`. Readers take the highest sequence number and check embedded
//! digests; old files are pruned so that the newest [`KEEP`] of each kind remain, and a blob only when no kept file
//! refers to it.
use super::pack::{Head, Pack};
use super::state::StateDir;
use super::store::Store;
use super::{IoStats, PeerRead, Sent, Transport};
use crate::core::item::Content;
use crate::core::mirror::{ItemId, View};
use crate::core::util::{err, sha256_hex, Res};
use std::collections::{BTreeMap, BTreeSet};

/// How many files of each kind are kept.
pub const KEEP: usize = 3;
/// A peer digest larger than this is ignored.
pub const DIGEST_MAX: u64 = 64 * 1024;

/// `pack-<gen>-<id>.json` (id: 1-32 of `[0-9a-f]`) -> gen.
pub fn pack_gen(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("pack-")?.strip_suffix(".json")?;
    let (g, id) = rest.split_once('-')?;
    let ok = !g.is_empty()
        && g.len() <= 13
        && g.bytes().all(|b| b.is_ascii_digit())
        && (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    ok.then(|| g.parse().ok()).flatten()
}

/// The Drive transport over a store rooted at the pair's folder.
pub struct DriveTransport<S: Store> {
    from: StateDir<S>,
    pair: String,
    key: Option<Vec<u8>>,
}

impl<S: Store> DriveTransport<S> {
    /// `store` is rooted at `botport-mirror/<pair>/`; `floor` is the highest Grok state sequence number accepted
    /// before.
    pub fn new(store: S, pair: &str, floor: Option<u64>) -> DriveTransport<S> {
        DriveTransport {
            from: StateDir::new(store, "from-grok", pair, floor),
            pair: pair.to_string(),
            key: None,
        }
    }

    /// With a pair key: only signed Grok state files count, and `HEAD`'s pack must be signed.
    pub fn with_key(mut self, key: Option<Vec<u8>>) -> DriveTransport<S> {
        self.from.key = key.clone();
        self.key = key;
        self
    }

    fn store(&mut self) -> &mut S {
        &mut self.from.store
    }

    /// The pair folder's store, for the connection check (`mirror check`): files outside the sync layout only.
    pub fn probe_store(&mut self) -> &mut dyn Store {
        &mut self.from.store
    }

    /// The highest Grok state sequence number accepted (to keep in the mirror state).
    pub fn floor(&self) -> Option<u64> {
        self.from.floor
    }

    /// The current `HEAD`, if any. An unreadable `HEAD` is an error (never silently replaced).
    pub fn head(&mut self) -> Res<Option<Head>> {
        match self.store().get("HEAD")? {
            None => Ok(None),
            Some(b) => Head::parse(&b).map(Some),
        }
    }

    /// The pack `HEAD` names, checked against `HEAD`'s sha256.
    pub fn head_pack(&mut self) -> Res<Option<Pack>> {
        let Some(h) = self.head()? else {
            return Ok(None);
        };
        let b = self
            .store()
            .get(&format!("to-grok/{}", h.pack))?
            .ok_or("HEAD names a pack that is missing")?;
        let p = Pack::parse_with(&b, &self.pair.clone(), self.key.as_deref())?;
        if p.sha256 != h.sha || p.gen != h.gen {
            return err("HEAD does not match the pack it names");
        }
        Ok(Some(p))
    }

    /// Publish a sealed pack and its blobs, then move `HEAD` to it. `expect` is the `HEAD` the journal says was
    /// current (None: no pack yet). The move is refused when `HEAD` is not exactly `expect` (another run or machine
    /// moved it) or when the pack's generation is not higher: `HEAD` only moves forward, and only from the
    /// predecessor this run planned against.
    pub fn send_pack(
        &mut self,
        pack: &Pack,
        blobs: &[(String, Vec<u8>)],
        expect: Option<&Head>,
    ) -> Res<Sent> {
        if pack.sha256.is_empty() {
            return err("pack is not sealed");
        }
        let check = |cur: Option<Head>| -> Res<()> {
            if cur.as_ref() != expect {
                return err(format!(
                    "HEAD moved since this run planned ({}); run botport mirror status",
                    cur.map_or("none".into(), |h| format!("gen {} by {}", h.gen, h.machine))
                ));
            }
            if cur.is_some_and(|h| h.gen >= pack.gen) {
                return err("refusing to move HEAD backwards or sideways");
            }
            Ok(())
        };
        let cur = self.head()?;
        // a retry after HEAD already moved to this very pack (a failure after the HEAD write): nothing left to do
        if cur.as_ref() == Some(&pack.head()) {
            return Ok(Sent {
                to: format!("to-grok/{} (already sent)", pack.file_name()),
                bytes: 0,
            });
        }
        check(cur)?;
        let mut bytes = 0u64;
        let have: BTreeSet<String> = self
            .store()
            .list("to-grok/blobs")?
            .into_iter()
            .map(|e| e.name)
            .collect();
        // every blob the pack names must be published with it or already there
        for it in &pack.items {
            if let Some(b) = &it.blob {
                if !have.contains(b) && !blobs.iter().any(|(n, _)| n == b) {
                    return err(format!(
                        "{}: the pack names a blob that is not being sent",
                        it.item
                    ));
                }
            }
        }
        for (name, b) in blobs {
            if sha256_hex(b) != *name {
                return err("a pack blob does not match its name");
            }
            if !have.contains(name) {
                self.store().put(&format!("to-grok/blobs/{name}"), b)?;
                bytes += b.len() as u64;
            }
        }
        // written as sealed (and signed, with a pair key): checked, never re-sealed
        let manifest = crate::core::util::json_sorted(pack).into_bytes();
        if Pack::parse_with(&manifest, &self.pair.clone(), self.key.as_deref())?.sha256
            != pack.sha256
        {
            return err("pack changed after it was sealed");
        }
        self.store()
            .put(&format!("to-grok/{}", pack.file_name()), &manifest)?;
        // re-read right before replacing, from the store as it is now (no backend has a conditional write)
        self.store().refresh();
        check(self.head()?)?;
        let line = pack.head().to_line();
        self.store().put("HEAD", line.as_bytes())?;
        bytes += (manifest.len() + line.len()) as u64;
        // HEAD has moved: pruning is housekeeping, and its failure must not make a sent pack look unsent
        if let Err(e) = self.prune_packs(&pack.head()) {
            eprintln!("botport: note: old packs not pruned this run ({e})");
        }
        Ok(Sent {
            to: format!("to-grok/{}", pack.file_name()),
            bytes,
        })
    }

    /// Keep the pack `head` names and the newest [`KEEP`] packs below it; remove older packs (only files named
    /// exactly `pack-<gen>-<id>.json` with a lower gen) and blobs no kept pack refers to. If a kept pack cannot be read,
    /// no blob is removed.
    fn prune_packs(&mut self, head: &Head) -> Res<()> {
        let mut packs: Vec<(u64, String)> = self
            .store()
            .list("to-grok")?
            .into_iter()
            .filter_map(|e| pack_gen(&e.name).map(|g| (g, e.name)))
            .filter(|(g, name)| *g < head.gen && *name != head.pack)
            .collect();
        packs.sort();
        let keep_from = packs.len().saturating_sub(KEEP - 1);
        let mut kept: Vec<String> = packs[keep_from..].iter().map(|(_, n)| n.clone()).collect();
        kept.push(head.pack.clone());
        let mut referenced = BTreeSet::new();
        let mut all_read = true;
        for name in &kept {
            match self
                .store()
                .get(&format!("to-grok/{name}"))?
                .map(|b| Pack::parse_with(&b, &self.pair.clone(), self.key.as_deref()))
            {
                Some(Ok(p)) => referenced.extend(p.items.into_iter().filter_map(|i| i.blob)),
                _ => all_read = false,
            }
        }
        for (_, name) in &packs[..keep_from] {
            self.store().delete(&format!("to-grok/{name}"))?;
        }
        if all_read {
            for e in self.store().list("to-grok/blobs")? {
                if !referenced.contains(&e.name) {
                    self.store().delete(&format!("to-grok/blobs/{}", e.name))?;
                }
            }
        }
        Ok(())
    }

    fn digests(&mut self, side: &str) -> Res<Vec<(u64, String)>> {
        let prefix = format!("{side}-");
        let mut v: Vec<(u64, String)> = self
            .store()
            .list("digest")?
            .into_iter()
            .filter_map(|e| {
                let n: u64 = e
                    .name
                    .strip_prefix(&prefix)?
                    .strip_suffix(".md")?
                    .parse()
                    .ok()?;
                (n <= crate::adapters::mirror::state::MAX_SEQ).then_some((n, e.name))
            })
            .collect();
        v.sort();
        Ok(v)
    }
}

impl<S: Store> Transport for DriveTransport<S> {
    fn id(&self) -> &'static str {
        "drive"
    }

    fn read_peer(&mut self) -> Res<Option<PeerRead>> {
        self.from.peer()
    }

    fn fetch(&mut self, id: &ItemId, view: &View) -> Res<Content> {
        self.from.fetch(id, view)
    }

    fn read_peer_digest(&mut self) -> Res<Option<Vec<u8>>> {
        /// How many of the newest Grok digests are checked, newest first, for one that verifies.
        const DIGEST_SCAN: usize = 64;
        // text placed in a model's context must come from the pair: only a digest signed with the pair key is read
        // (a pair without a key reads none from the folder); the newest one that verifies counts
        let Some(key) = self.key.clone() else {
            return Ok(None);
        };
        let pair = self.pair.clone();
        let sizes: BTreeMap<String, u64> = self
            .store()
            .list("digest")?
            .into_iter()
            .map(|e| (e.name, e.size))
            .collect();
        let mut names = self.digests("grok")?;
        names.reverse();
        // newest first, each checked before any is passed over: the newest one that verifies counts (the scan is
        // bounded so a folder filled with unsigned files cannot cost unbounded reads)
        for (_, name) in names.into_iter().take(DIGEST_SCAN) {
            // a digest is capped at a few kilobytes; anything far larger is not one
            if sizes.get(&name).is_some_and(|s| *s > DIGEST_MAX) {
                continue;
            }
            let Some(text) = self.store().get(&format!("digest/{name}"))? else {
                continue;
            };
            let sig = self
                .store()
                .get(&format!("digest/{name}.mac"))?
                .unwrap_or_default();
            if crate::core::auth::digest_mac_ok(&key, &pair, &name, &text, &sig) {
                return Ok(Some(text));
            }
        }
        Ok(None)
    }

    fn send_digest(&mut self, text: &[u8]) -> Res<Sent> {
        let mut have = self.digests("hermes")?;
        if let Some((_, last)) = have.last() {
            if self.store().get(&format!("digest/{last}"))?.as_deref() == Some(text) {
                // unchanged, but written before the pair had this key (or any): its signature is put right
                if let Some(key) = self.key.clone() {
                    let (pair, last) = (self.pair.clone(), last.clone());
                    let sig = self
                        .store()
                        .get(&format!("digest/{last}.mac"))?
                        .unwrap_or_default();
                    if !crate::core::auth::digest_mac_ok(&key, &pair, &last, text, &sig) {
                        let good = crate::core::auth::digest_mac(&key, &pair, &last, text);
                        self.store()
                            .put(&format!("digest/{last}.mac"), good.as_bytes())?;
                    }
                }
                return Ok(Sent {
                    to: format!("digest/{last} (unchanged)"),
                    bytes: 0,
                });
            }
        }
        let n = have
            .last()
            .map_or(Some(1), |(n, _)| n.checked_add(1))
            .ok_or("digest sequence number out of range")?;
        let name = format!("hermes-{n}.md");
        self.store().put(&format!("digest/{name}"), text)?;
        // with a pair key the reader can tell a digest from the pair from text someone else placed in the folder
        if let Some(key) = self.key.clone() {
            let pair = self.pair.clone();
            let sig = crate::core::auth::digest_mac(&key, &pair, &name, text);
            self.store()
                .put(&format!("digest/{name}.mac"), sig.as_bytes())?;
        }
        have.push((n, name.clone()));
        for (_, old) in &have[..have.len().saturating_sub(KEEP)] {
            self.store().delete(&format!("digest/{old}"))?;
            if self.key.is_some() {
                self.store().delete(&format!("digest/{old}.mac")).ok();
            }
        }
        Ok(Sent {
            to: format!("digest/{name}"),
            bytes: text.len() as u64,
        })
    }

    fn io(&self) -> IoStats {
        self.from.store.io()
    }
}

#[cfg(test)]
mod tests {
    use super::super::pack::{random_id, PackAction, PackItem};
    use super::super::state::tests::{publish, tmp};
    use super::super::store::FolderStore;
    use super::*;
    use crate::core::mirror::ABSENT;

    fn pack(gen: u64, blob: &[u8]) -> (Pack, Vec<(String, Vec<u8>)>) {
        let sha = sha256_hex(blob);
        let mut p = Pack::new(
            "demo-bot",
            gen,
            &random_id(),
            "a1",
            "2000-01-01T00:00:00Z",
            vec![PackItem {
                item: ItemId::parse("skill:demo-skill").unwrap(),
                action: PackAction::Put,
                expect: ABSENT.into(),
                want: "w".into(),
                blob: Some(sha.clone()),
            }],
        )
        .unwrap();
        p.seal();
        (p, vec![(sha, blob.to_vec())])
    }

    #[test]
    fn head_moves_forward_from_the_planned_predecessor_only() {
        let d = tmp("drive-head");
        let mut t = DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None);
        assert_eq!(t.head().unwrap(), None);
        let (p1, b1) = pack(1, b"one");
        t.send_pack(&p1, &b1, None).unwrap();
        let h1 = t.head().unwrap().unwrap();
        assert_eq!(h1, p1.head());
        assert_eq!(t.head_pack().unwrap().unwrap(), p1);
        // a run that planned against "no HEAD" is refused now
        let (p2, b2) = pack(2, b"two");
        assert!(t
            .send_pack(&p2, &b2, None)
            .unwrap_err()
            .contains("HEAD moved"));
        // same or lower generation is refused even from the right predecessor
        let (p1b, b1b) = pack(1, b"one-b");
        assert!(t
            .send_pack(&p1b, &b1b, Some(&h1))
            .unwrap_err()
            .contains("backwards"));
        t.send_pack(&p2, &b2, Some(&h1)).unwrap();
        assert_eq!(t.head().unwrap().unwrap().gen, 2);
        // a tampered manifest no longer matches HEAD
        let mut s = FolderStore::open(&d).unwrap();
        s.put(&format!("to-grok/{}", p2.file_name()), b"{}")
            .unwrap();
        assert!(t.head_pack().is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_signed_pair_publishes_signed_packs_only() {
        let d = tmp("drive-signed");
        let key = vec![5u8; 32];
        let mut t = DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None)
            .with_key(Some(key.clone()));
        let (p, b) = pack(1, b"one");
        assert!(t
            .send_pack(&p, &b, None)
            .unwrap_err()
            .contains("not signed"));
        let (mut p, b) = pack(1, b"one");
        p.seal_with(Some(&key));
        t.send_pack(&p, &b, None).unwrap();
        assert_eq!(t.head_pack().unwrap().unwrap().mac, p.mac);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn heads_pack_survives_stray_packs_and_a_retry_is_already_sent() {
        let d = tmp("drive-stray");
        let mut t = DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None);
        let (p1, b1) = pack(1, b"one");
        // planted files that sort higher than the real pack
        let mut s = FolderStore::open(&d).unwrap();
        for n in [
            "pack-999999-aa.json",
            "pack-999998-bb.json",
            "pack-999997-cc.json",
            "pack-1-zz.json",
        ] {
            s.put(&format!("to-grok/{n}"), b"{}").unwrap();
        }
        t.send_pack(&p1, &b1, None).unwrap();
        assert_eq!(
            t.head_pack().unwrap().unwrap(),
            p1,
            "HEAD's pack and its blob are never pruned"
        );
        assert_eq!(s.list("to-grok/blobs").unwrap().len(), 1);
        // a retry of the same pack after HEAD moved: success, nothing written again
        assert_eq!(t.send_pack(&p1, &b1, None).unwrap().bytes, 0);
        // a pack naming a blob that is neither sent nor present is refused
        let (p2, _) = pack(2, b"two");
        assert!(t
            .send_pack(&p2, &[], Some(&p1.head()))
            .unwrap_err()
            .contains("not being sent"));
        assert_eq!(pack_gen("pack-12-8f3a2c.json"), Some(12));
        for bad in [
            "pack-12-8F3A.json",
            "pack-x-ab.json",
            "pack-1-.json",
            "pack-1-ab.json.tmp",
        ] {
            assert_eq!(pack_gen(bad), None, "{bad}");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn old_packs_and_unreferenced_blobs_are_pruned() {
        let d = tmp("drive-prune");
        let mut t = DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None);
        let mut prev = None;
        for g in 1..=5u64 {
            let (p, b) = pack(g, format!("blob {g}").as_bytes());
            t.send_pack(&p, &b, prev.as_ref()).unwrap();
            prev = Some(p.head());
        }
        let mut s = FolderStore::open(&d).unwrap();
        let packs: Vec<String> = s
            .list("to-grok")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(packs.len(), KEEP);
        assert!(packs
            .iter()
            .all(|n| !n.starts_with("pack-1-") && !n.starts_with("pack-2-")));
        assert_eq!(s.list("to-grok/blobs").unwrap().len(), KEEP);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn digests_rotate_and_unchanged_is_not_rewritten() {
        let d = tmp("drive-digest");
        let mut t = DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None);
        assert_eq!(t.read_peer_digest().unwrap(), None);
        for i in 1..=5 {
            t.send_digest(format!("digest {i}").as_bytes()).unwrap();
        }
        assert_eq!(t.send_digest(b"digest 5").unwrap().bytes, 0);
        let mut s = FolderStore::open(&d).unwrap();
        let names: Vec<String> = s
            .list("digest")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["hermes-3.md", "hermes-4.md", "hermes-5.md"]);
        s.put("digest/grok-2.md", b"older").unwrap();
        s.put("digest/grok-10.md", b"newest").unwrap();
        // a pair without a key reads no digest from the folder: anyone who can write it could place text there
        assert_eq!(t.read_peer_digest().unwrap(), None);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn with_a_key_digests_are_signed_and_only_signed_ones_are_read() {
        let d = tmp("drive-digest-signed");
        let key = vec![7u8; 32];
        // a digest written while the pair had no key is signed once a keyed sync finds it unchanged
        DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None)
            .send_digest(b"digest one")
            .unwrap();
        let mut t = DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None)
            .with_key(Some(key.clone()));
        assert_eq!(t.send_digest(b"digest one").unwrap().bytes, 0);
        let mut s = FolderStore::open(&d).unwrap();
        let sig = s.get("digest/hermes-1.md.mac").unwrap().unwrap();
        assert!(crate::core::auth::digest_mac_ok(
            &key,
            "demo-bot",
            "hermes-1.md",
            b"digest one",
            &sig
        ));
        // another pair or another file name never verifies
        assert!(!crate::core::auth::digest_mac_ok(
            &key,
            "other-bot",
            "hermes-1.md",
            b"digest one",
            &sig
        ));
        assert!(!crate::core::auth::digest_mac_ok(
            &key,
            "demo-bot",
            "hermes-2.md",
            b"digest one",
            &sig
        ));
        // the Grok digest: unsigned or badly signed is ignored, signed is read
        s.put("digest/grok-1.md", b"from the pair").unwrap();
        assert_eq!(t.read_peer_digest().unwrap(), None);
        s.put("digest/grok-1.md.mac", b"0000").unwrap();
        assert_eq!(t.read_peer_digest().unwrap(), None);
        let good = crate::core::auth::digest_mac(&key, "demo-bot", "grok-1.md", b"from the pair");
        s.put("digest/grok-1.md.mac", good.as_bytes()).unwrap();
        assert_eq!(t.read_peer_digest().unwrap().unwrap(), b"from the pair");
        // a newer digest that does not verify is passed over for the newest one that does
        s.put("digest/grok-2.md", b"placed by someone else")
            .unwrap();
        assert_eq!(t.read_peer_digest().unwrap().unwrap(), b"from the pair");
        // rotation removes old signatures with their digests
        for i in 2..=5 {
            t.send_digest(format!("digest {i}").as_bytes()).unwrap();
        }
        assert!(s.get("digest/hermes-1.md.mac").unwrap().is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn reads_the_grok_state_from_its_folder() {
        let d = tmp("drive-read");
        let mut t = DriveTransport::new(FolderStore::open(&d).unwrap(), "demo-bot", None);
        assert_eq!(t.read_peer().unwrap(), None);
        publish(&d, "from-grok", 4);
        let r = t.read_peer().unwrap().unwrap();
        assert!(r.snapshot.starts_with("state-4 "));
        assert_eq!(t.floor(), Some(4));
        let (id, v) = r
            .side
            .items
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
            .unwrap();
        t.fetch(&id, &v).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }
}
