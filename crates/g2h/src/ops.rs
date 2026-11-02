//! Owner-approved operations. A peer may only REQUEST an op (an `op`
//! envelope carrying an [`OpRequest`]); the owner's `[ops]` allowlist in
//! `config.toml` decides. Tiers: `auto` runs, `approve` waits for
//! `g2h approve <request_id>` (single use, TTL), `never` refuses. The default
//! allowlist is empty, so every op is refused. Pure logic; the clock is injected.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

pub const MAX_OP_NAME: usize = 32;
pub const MAX_REQUEST_ID: usize = 64;
pub const MAX_ARGS_BYTES: usize = 16 * 1024;
/// Largest accepted op envelope body (args plus room for the other fields).
pub const MAX_BODY_BYTES: usize = MAX_ARGS_BYTES + 512;
pub const DEFAULT_APPROVAL_TTL_SECS: i64 = 86_400;
/// Remembered request ids for replay refusal.
pub const SEEN_KEEP: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Auto,
    Approve,
    Never,
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Tier::Auto => "auto",
            Tier::Approve => "approve",
            Tier::Never => "never",
        })
    }
}

/// Body of a `kind: op` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpRequest {
    pub op: String,
    pub args: serde_json::Value,
    pub request_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Done,
    Refused,
    PendingApproval,
    Failed,
}

/// Body of a `kind: op_result` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpResult {
    pub request_id: String,
    pub status: OpStatus,
    pub detail: String,
}

fn valid_op_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_OP_NAME
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_request_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REQUEST_ID
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl OpRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_op_name(&self.op) {
            return Err(format!(
                "bad op name (must be [a-z0-9-]{{1,{MAX_OP_NAME}}})"
            ));
        }
        if !valid_request_id(&self.request_id) {
            return Err(format!(
                "bad request_id (must be [A-Za-z0-9_-]{{1,{MAX_REQUEST_ID}}})"
            ));
        }
        if !self.args.is_object() {
            return Err("args must be a JSON object".into());
        }
        let n = serde_json::to_string(&self.args).map_or(usize::MAX, |s| s.len());
        if n > MAX_ARGS_BYTES {
            return Err(format!("args too large ({n} > {MAX_ARGS_BYTES} bytes)"));
        }
        Ok(())
    }
}

/// Parse and validate an op envelope body; the size is checked before parsing.
pub fn parse_op_body(body: &str) -> Result<OpRequest, String> {
    if body.len() > MAX_BODY_BYTES {
        return Err(format!(
            "op body too large ({} > {MAX_BODY_BYTES} bytes)",
            body.len()
        ));
    }
    let r: OpRequest = serde_json::from_str(body).map_err(|e| format!("malformed op: {e}"))?;
    r.validate()?;
    Ok(r)
}

/// Op name -> tier, from the `[ops]` section of `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist(BTreeMap<String, Tier>);

impl FromIterator<(String, Tier)> for Allowlist {
    fn from_iter<I: IntoIterator<Item = (String, Tier)>>(it: I) -> Self {
        Allowlist(it.into_iter().collect())
    }
}

impl From<&Config> for Allowlist {
    fn from(c: &Config) -> Self {
        Allowlist(c.ops.clone())
    }
}

impl Allowlist {
    pub fn tier(&self, op: &str) -> Option<Tier> {
        self.0.get(op).copied()
    }
}

pub trait UnixClock {
    fn now(&self) -> i64;
}

pub struct SystemClock;
impl UnixClock for SystemClock {
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// Who sent the request and at which hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin<'a> {
    pub peer: &'a str,
    pub hop: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run,
    /// Pending; the id to approve.
    NeedsApproval(String),
    Refuse(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOp {
    pub req: OpRequest,
    pub origin: String,
    pub tier: Tier,
    pub conv_id: String,
    /// Unix seconds; the approval is gone at or after this.
    pub expires_at: i64,
}

/// Persisted op state: pending approvals and recently seen request ids.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpBook {
    #[serde(default)]
    pub pending: BTreeMap<String, PendingOp>,
    #[serde(default)]
    pub seen: VecDeque<String>,
    /// Ops sent TO Grok awaiting its `op_result`: request_id -> op name.
    #[serde(default)]
    pub to_grok: BTreeMap<String, String>,
}

/// Open ops sent to Grok that are remembered (oldest dropped first).
pub const TO_GROK_KEEP: usize = 1_000;

pub struct OpGuard<C: UnixClock> {
    pub allowlist: Allowlist,
    pub peers: Vec<String>,
    pub hop_limit: u32,
    pub approval_ttl_secs: i64,
    clock: C,
}

impl<C: UnixClock> OpGuard<C> {
    pub fn new(
        allowlist: Allowlist,
        peers: Vec<String>,
        hop_limit: u32,
        approval_ttl_secs: i64,
        clock: C,
    ) -> Self {
        OpGuard {
            allowlist,
            peers,
            hop_limit,
            approval_ttl_secs,
            clock,
        }
    }

    pub fn check(&self, book: &mut OpBook, req: &OpRequest, origin: Origin) -> Decision {
        self.check_in(book, req, origin, "")
    }

    /// [`check`](Self::check) recording `conv_id` on a pending approval.
    pub fn check_in(
        &self,
        book: &mut OpBook,
        req: &OpRequest,
        origin: Origin,
        conv_id: &str,
    ) -> Decision {
        if let Err(e) = req.validate() {
            return Decision::Refuse(e);
        }
        if !self.peers.iter().any(|p| p == origin.peer) {
            return Decision::Refuse(format!("peer {:?} is not in config peers", origin.peer));
        }
        if origin.hop >= self.hop_limit {
            return Decision::Refuse(format!(
                "hop {} reaches the limit {}",
                origin.hop, self.hop_limit
            ));
        }
        if book.seen.iter().any(|s| s == &req.request_id)
            || book.pending.contains_key(&req.request_id)
        {
            return Decision::Refuse("duplicate request_id (replay)".into());
        }
        book.seen.push_back(req.request_id.clone());
        while book.seen.len() > SEEN_KEEP {
            book.seen.pop_front();
        }
        match self.allowlist.tier(&req.op) {
            None => Decision::Refuse(format!("op {:?} is not in the owner's allowlist", req.op)),
            Some(Tier::Never) => Decision::Refuse(format!("op {:?} is tier never", req.op)),
            Some(Tier::Auto) => Decision::Run,
            Some(tier @ Tier::Approve) => {
                book.pending.insert(
                    req.request_id.clone(),
                    PendingOp {
                        req: req.clone(),
                        origin: origin.peer.into(),
                        tier,
                        conv_id: conv_id.into(),
                        expires_at: self.clock.now().saturating_add(self.approval_ttl_secs),
                    },
                );
                Decision::NeedsApproval(req.request_id.clone())
            }
        }
    }

    pub fn purge_expired(&self, book: &mut OpBook) -> usize {
        let now = self.clock.now();
        let before = book.pending.len();
        book.pending.retain(|_, p| p.expires_at > now);
        before - book.pending.len()
    }

    /// Pending approvals (expired ones dropped first).
    pub fn pending(&self, book: &mut OpBook) -> Vec<PendingOp> {
        self.purge_expired(book);
        book.pending.values().cloned().collect()
    }

    fn take(&self, book: &mut OpBook, id: &str) -> Result<PendingOp, String> {
        let p = book
            .pending
            .remove(id)
            .ok_or_else(|| format!("unknown or expired request_id {id:?}"))?;
        if p.expires_at <= self.clock.now() {
            return Err(format!("approval for {id:?} expired"));
        }
        Ok(p)
    }

    /// Single use: the pending entry is removed. Unknown or expired ids are errors.
    pub fn approve(&self, book: &mut OpBook, id: &str) -> Result<PendingOp, String> {
        self.take(book, id)
    }

    pub fn deny(&self, book: &mut OpBook, id: &str) -> Result<PendingOp, String> {
        self.take(book, id)
    }
}
