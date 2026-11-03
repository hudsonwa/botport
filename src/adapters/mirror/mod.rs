//! Mirror mode transports: how botport reads the Grok side and gets change packs and digests to it. Spec:
//! docs/design/MIRROR.md section 6.
//!
//! The merge engine (`core::mirror`) only sees a [`PeerRead`]: per item a side digest and a neutral digest, coverage
//! per item kind, and the Grok side's applied log. A [`Transport`] produces it and fetches item content on demand,
//! checked against the digests the read advertised.
//!
//! - [`state`]: the Grok side's state file (`botport-mirror-state/1`) and the reader for a directory of them.
//! - [`store`]: where files live (a local or synced folder; the Drive API backend builds on the same trait).
//! - [`local`]: the read-only local transport.
//! - [`drive`]: the Drive transport (folder layout, change packs, `HEAD`); [`api`]: its Drive API store; [`pack`]: the
//!   change pack and `HEAD` formats.
use crate::core::item::Content;
use crate::core::mirror::{Applied, ItemId, Kind, SideRead, View};
use crate::core::util::Res;
use std::collections::BTreeMap;

pub mod api;
pub mod drive;
pub mod local;
pub mod pack;
pub mod state;
pub mod store;

/// The Grok side as one read saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerRead {
    /// Which snapshot this is (the state file's sequence number and digest), for logs and `mirror status`.
    pub snapshot: String,
    /// When the Grok side wrote it (its own clock, as recorded in the file).
    pub written: String,
    /// Items and coverage.
    pub side: SideRead,
    /// The applied log.
    pub applied: Vec<Applied>,
    /// The newest change pack generation the Grok side says it applied.
    pub applied_gen: Option<u64>,
    /// Whether the read was authenticated with the pair key.
    pub authenticated: bool,
    /// Kinds the Grok side could not read at all, with the reason.
    pub unread: BTreeMap<Kind, String>,
    /// Kinds the Grok side reads but never writes, with the reason.
    pub by_hand: BTreeMap<Kind, String>,
}

/// Bytes moved and time spent on the network, for the metrics row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoStats {
    /// bytes read
    pub read_bytes: u64,
    /// bytes written
    pub written_bytes: u64,
    /// milliseconds spent waiting on the network (0 for local files)
    pub net_ms: u64,
}

impl IoStats {
    /// Add another call's figures.
    pub fn add(&mut self, o: IoStats) {
        self.read_bytes += o.read_bytes;
        self.written_bytes += o.written_bytes;
        self.net_ms += o.net_ms;
    }
}

/// Where a send went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// A short description of the destination (no content).
    pub to: String,
    /// Bytes written.
    pub bytes: u64,
}

/// A transport to the Grok side. Reads never write; sends are refused by a read-only transport.
pub trait Transport {
    /// Stable id (`local`, `drive`, `hybrid`), recorded in the state and the metrics.
    fn id(&self) -> &'static str;
    /// The Grok side's newest snapshot, or `None` when this transport has nothing usable (hybrid then falls back).
    fn read_peer(&mut self) -> Res<Option<PeerRead>>;
    /// The content of one item, checked against the view the read advertised (a mismatch is an error: the side
    /// changed mid-run, and the item is skipped this run).
    fn fetch(&mut self, id: &ItemId, view: &View) -> Res<Content>;
    /// The Grok side's activity digest, if there is one.
    fn read_peer_digest(&mut self) -> Res<Option<Vec<u8>>>;
    /// Publish the Hermes activity digest.
    fn send_digest(&mut self, text: &[u8]) -> Res<Sent>;
    /// Figures for the metrics row since the transport was opened.
    fn io(&self) -> IoStats;
}
