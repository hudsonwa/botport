//! Warm client to one Hermes bot (one profile), used by g2hd to deliver a peer
//! message and collect the bot's reply.
//!
//! # Protocol subset (Hermes gateway, `hermes_cli/gateway_client.py`)
//!
//! 1. **Endpoint discovery** (re-done on every connect, never cached): the
//!    profile home's control socket is `<home>/gateway.sock`, or the absolute
//!    path named in `<home>/gateway.sock.path` (must end in `control.sock`; the
//!    pointer file must not be group/world accessible). One line
//!    `{"protocol":1,"verb":"identify","id":1}` returns the owner's identity:
//!    `state` must be `ready`, `runtime_protocol` 1, `capabilities` must hold
//!    `session-authority-v1`, `served_profiles` must list this home (canonical
//!    path) and give its `profile_id`; `instance_id`, `authority_epoch` and a
//!    loopback `api_origin` (`http://127.0.0.1:<port>`) are taken from it.
//! 2. **Ticket**: on the same socket, `{"protocol":1,"id":1,"verb":"session-ticket",
//!    "params":{"profile_id","instance_id","purpose":"interactive"}}` returns a
//!    single-use `ticket` whose identity must match the endpoint.
//! 3. **WebSocket**: `ws://<api_origin host:port>/api/ws` with subprotocols
//!    `hermes-gateway-v1` and `hermes-gateway-ticket.<ticket>`; the server must
//!    echo `hermes-gateway-v1`.
//! 4. **JSON-RPC 2.0** over the socket: `session.create` (`title`, `source`
//!    = `oneshot`, `request_id` = stable hash of the conv, `toolsets` if set) → `{session_id, stored_session_id}`; `session.resume`
//!    (`session_id` = stored id) → a new runtime `session_id`; `prompt.submit`
//!    (`session_id`, `input_id`, `text`, `finite`, `unattended`); `session.interrupt`
//!    (`session_id`).
//! 5. **Events**: frames `{"method":"event","params":{"type","session_id","payload"}}`,
//!    filtered by runtime `session_id`: `message.delta` (`payload.text`) and
//!    `message.complete` (`payload.text`, `payload.status`, `payload.usage.total`).
//!
//! Not used: the multiplexer path (a profile served by another home's gateway),
//! Windows named pipes, approvals/clarify requests, starting a gateway. This
//! client **never starts or spawns** a gateway: an absent owner is `Unavailable`
//! after a bounded wait.
//!
//! # Toolset restriction
//! `peer_toolsets` is always sent as `session.create` `toolsets` (a Hermes
//! CREATE_FIELDS key, validated against the profile's toolset names). Hermes sets
//! the session's platform toolsets to exactly that list, so the default `[]` gives
//! peer turns no tools at all; [`Health::detail`] reports which applies.
//!
//! # Bounded context
//! CREATE_FIELDS has no compression threshold or context limit, so the gateway
//! backend rotates a conv's session itself (`max_turns` / `max_tokens`, see
//! [`GatewayOptions`]): a new create generation plus a framed recap.
//!
//! # A2A
//! [`A2aBackend`] speaks A2A v1.0 JSON-RPC (`SendMessage`, `CancelTask`) to a
//! loopback URL; `contextId` = conv id.

mod a2a;
mod endpoint;
mod gateway;

pub use a2a::{A2aBackend, A2aOptions};
pub use endpoint::{discover, resolve_profile_home, Endpoint};
pub use gateway::{GatewayOptions, GatewayWsBackend};

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    /// Tokens this turn (gateway: the growth of the session's running `usage.total`,
    /// else of `input + output`); what the daily token cap charges.
    pub usage_tokens: Option<u64>,
    /// Current context occupancy (`usage.context_used`), when the peer reports it.
    pub context_tokens: Option<u64>,
    /// Milliseconds from submit to the first `message.delta` (None: no delta).
    pub first_delta_ms: Option<u64>,
    pub total_ms: u64,
    /// This turn started a new peer session (bounded-context rotation).
    pub rotated: bool,
    /// Bytes of Hermes out-of-band (steer) text removed from `text`.
    pub oob_stripped: usize,
}

const OOB_OPEN: &str = "[OUT-OF-BAND USER MESSAGE";
const OOB_CLOSE: &str = "[/OUT-OF-BAND USER MESSAGE]";

/// Remove Hermes' injected out-of-band user-message blocks (mid-turn steer
/// rows) from assistant text. An unclosed block runs to the end. Returns the
/// remaining text (trimmed when anything was removed) and the bytes removed.
pub fn strip_oob(text: &str) -> (String, usize) {
    if !text.contains(OOB_OPEN) {
        return (text.to_string(), 0);
    }
    let (mut out, mut removed, mut rest) = (String::new(), 0, text);
    while let Some(i) = rest.find(OOB_OPEN) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail
            .find(OOB_CLOSE)
            .map_or(tail.len(), |j| j + OOB_CLOSE.len());
        removed += end;
        rest = &tail[end..];
    }
    out.push_str(rest);
    (out.trim().to_string(), removed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerError {
    /// No ready gateway within the bounded wait (nothing was started).
    Unavailable(String),
    /// Per-turn deadline hit; the turn was interrupted.
    Deadline,
    /// Connection lost mid-turn; the next ask reconnects and resumes.
    Disconnected(String),
    /// The gateway refused a request.
    Rpc { code: i64, message: String },
    /// Unexpected frame or reply shape.
    Protocol(String),
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::Unavailable(m) => write!(f, "peer unavailable: {m}"),
            PeerError::Deadline => write!(f, "peer turn deadline exceeded (interrupted)"),
            PeerError::Disconnected(m) => write!(f, "peer connection lost: {m}"),
            PeerError::Rpc { code, message } => write!(f, "peer refused ({code}): {message}"),
            PeerError::Protocol(m) => write!(f, "peer protocol error: {m}"),
        }
    }
}

impl std::error::Error for PeerError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
}

#[allow(async_fn_in_trait)]
pub trait PeerBackend {
    /// One turn on the conv's dedicated peer session.
    async fn ask(
        &self,
        conv_id: &str,
        framed_text: &str,
        deadline: Duration,
    ) -> Result<Reply, PeerError>;
    /// Interrupt a running turn of this conv (no-op if none).
    async fn interrupt(&self, conv_id: &str) -> Result<(), PeerError>;
    async fn health(&self) -> Health;
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}
