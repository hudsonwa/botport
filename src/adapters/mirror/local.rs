//! The local transport: reads the Grok side from files on this machine, never writes to it.
//!
//! botport defines only the interface (docs/design/MIRROR.md section 6.2): a `local-dir` of botport-normalised
//! records, which any program may write. [`StateDirLayout`] reads one bot's folder, `<local-dir>/<bot-key>/`:
//! the activity records (`core::local_records`) and, when the reader can produce one, a setup snapshot
//! (`botport-mirror-state/1` files and blobs). Without a snapshot there is nothing to merge (`Ok(None)`); anything it
//! cannot parse makes it report "unavailable", never a guess.
use super::state::StateDir;
use super::store::FolderStore;
use super::{IoStats, PeerRead, Sent, Transport};
use crate::core::item::Content;
use crate::core::local_records::{self, valid_bot_key, BotRecords};
use crate::core::mirror::{ItemId, View};
use crate::core::util::{err, Res};
use std::path::{Path, PathBuf};

/// One way of finding the Grok side's state on this machine.
pub trait LocalLayout {
    /// Short id for logs (`state-dir`, ...).
    fn id(&self) -> &'static str;
    /// The newest snapshot, `None` when unavailable.
    fn read(&mut self) -> Res<Option<PeerRead>>;
    /// One item's content, checked against `view`.
    fn fetch(&mut self, id: &ItemId, view: &View) -> Res<Content>;
    /// The Grok side's digest file, if the layout has one.
    fn digest(&mut self) -> Res<Option<Vec<u8>>>;
    /// The reader's normalised activity records for the bot, if the layout has them.
    fn records(&mut self) -> Res<Option<BotRecords>> {
        Ok(None)
    }
    /// The highest state sequence accepted, for a layout that reads numbered state files.
    fn floor(&self) -> Option<u64> {
        None
    }
    /// Figures since opened.
    fn io(&self) -> IoStats;
}

/// One bot's folder in a local-dir: records, and optionally state files and blobs (`state-<n>.json`,
/// `blobs/<sha256>`, `digest-grok.md`).
pub struct StateDirLayout {
    dir: StateDir<FolderStore>,
    local_dir: PathBuf,
    bot_key: String,
}

impl StateDirLayout {
    /// Open `<local_dir>/<bot_key>/` for `pair`; `floor` is the highest state sequence number accepted before; with
    /// `key`, only state files signed with the pair key count.
    pub fn open(
        local_dir: &Path,
        bot_key: &str,
        pair: &str,
        floor: Option<u64>,
        key: Option<Vec<u8>>,
    ) -> Res<StateDirLayout> {
        valid_bot_key(bot_key)?;
        let root = local_dir.join(bot_key);
        Ok(StateDirLayout {
            dir: StateDir::new(FolderStore::open(&root)?, "", pair, floor).with_key(key),
            local_dir: local_dir.to_path_buf(),
            bot_key: bot_key.to_string(),
        })
    }
}

impl LocalLayout for StateDirLayout {
    fn id(&self) -> &'static str {
        "state-dir"
    }

    fn floor(&self) -> Option<u64> {
        self.dir.floor
    }

    fn read(&mut self) -> Res<Option<PeerRead>> {
        self.dir.peer()
    }

    fn fetch(&mut self, id: &ItemId, view: &View) -> Res<Content> {
        self.dir.fetch(id, view)
    }

    fn digest(&mut self) -> Res<Option<Vec<u8>>> {
        use super::store::Store;
        self.dir.store.get("digest-grok.md")
    }

    fn records(&mut self) -> Res<Option<BotRecords>> {
        local_records::read(&self.local_dir, &self.bot_key).map(Some)
    }

    fn io(&self) -> IoStats {
        use super::store::Store;
        self.dir.store.io()
    }
}

/// The read-only local transport over one layout.
pub struct LocalTransport {
    layout: Box<dyn LocalLayout>,
}

impl LocalTransport {
    /// A transport reading through `layout`.
    pub fn new(layout: Box<dyn LocalLayout>) -> LocalTransport {
        LocalTransport { layout }
    }

    /// The highest state sequence the layout accepted, if it reads state files.
    pub fn floor(&self) -> Option<u64> {
        self.layout.floor()
    }

    /// Which layout it reads.
    pub fn layout(&self) -> &'static str {
        self.layout.id()
    }

    /// The local reader's activity records for the bot (for the Grok side's digest and the routine check).
    pub fn records(&mut self) -> Res<Option<BotRecords>> {
        self.layout.records()
    }
}

impl Transport for LocalTransport {
    fn id(&self) -> &'static str {
        "local"
    }

    fn read_peer(&mut self) -> Res<Option<PeerRead>> {
        self.layout.read()
    }

    fn fetch(&mut self, id: &ItemId, view: &View) -> Res<Content> {
        self.layout.fetch(id, view)
    }

    fn read_peer_digest(&mut self) -> Res<Option<Vec<u8>>> {
        self.layout.digest()
    }

    fn send_digest(&mut self, _: &[u8]) -> Res<Sent> {
        err("the local transport is read-only: sends go through the Drive transport (hybrid mode)")
    }

    fn io(&self) -> IoStats {
        self.layout.io()
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::tests::{id, publish, tmp};
    use super::*;

    #[test]
    fn reads_through_the_state_dir_layout() {
        let base = tmp("local");
        let d = base.join("demo-bot");
        std::fs::create_dir_all(&d).unwrap();
        let mut t = LocalTransport::new(Box::new(
            StateDirLayout::open(&base, "demo-bot", "demo-bot", None, None).unwrap(),
        ));
        assert_eq!((t.id(), t.layout()), ("local", "state-dir"));
        // nothing there yet: unavailable, not an error
        assert_eq!(t.read_peer().unwrap(), None);
        let f = publish(&d, "", 1);
        let r = t.read_peer().unwrap().unwrap();
        assert_eq!(r.applied_gen, Some(4));
        let ins = id("persona.instructions");
        let v = r.side.items[&ins].clone();
        assert_eq!(v.neutral, f.items[&ins].neutral);
        assert!(matches!(t.fetch(&ins, &v).unwrap(), Content::Text(_)));
        assert_eq!(t.read_peer_digest().unwrap(), None);
        assert!(t.send_digest(b"x").unwrap_err().contains("read-only"));
        assert!(t.io().read_bytes > 0);
        assert_eq!(t.io().written_bytes, 0);
        // the reader's records sit beside the snapshot
        std::fs::write(
            d.join("activity.json"),
            r#"{"schema": "botport-local/1", "id": "x", "name": "Demo Bot", "last_activity": "2000-01-03T00:00:00Z"}"#,
        )
        .unwrap();
        crate::core::local_records::lock_down(&base);
        assert_eq!(
            t.records().unwrap().unwrap().activity.unwrap().name,
            "Demo Bot"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_missing_or_linked_root_is_refused() {
        let d = tmp("local-missing");
        assert!(StateDirLayout::open(&d, "nope", "demo-bot", None, None).is_err());
        assert!(StateDirLayout::open(&d, "../x", "demo-bot", None, None).is_err());
        std::fs::remove_dir_all(&d).ok();
    }
}
