//! Newline-delimited JSON request/response types for the CLI <-> daemon socket.

use crate::envelope::Envelope;
use crate::ops::{OpResult, PendingOp};
use serde::{Deserialize, Serialize};

/// Longest blocking `ask`/`wait`, in seconds (12 min).
pub const MAX_TIMEOUT_SECS: u64 = 720;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Grok -> Hermes: send and block for the complete reply.
    Ask {
        text: String,
        conv_id: Option<String>,
        /// Message id this answers (hop = its hop + 1); None starts a new chain.
        reply_to: Option<String>,
        timeout_secs: Option<u64>,
    },
    /// Grok -> Hermes, fire-and-forget; the reply lands in Grok's inbox.
    Send {
        text: String,
        conv_id: Option<String>,
        reply_to: Option<String>,
        /// One-way: the Hermes turn runs but its reply is only logged.
        #[serde(default)]
        no_reply: bool,
        /// Send as `kind: notice` (one-way like `no_reply`; nothing may reply to it).
        #[serde(default)]
        notice: bool,
    },
    /// Long-poll Grok's inbox.
    Wait {
        timeout_secs: Option<u64>,
    },
    /// Drain Grok's inbox now.
    Recv,
    /// Grok is done: clear "Grok active".
    Bye,
    Status,
    Doctor,
    /// Hermes -> Grok (the Hermes bot's tool); may trigger a wake.
    HermesSend {
        text: String,
        conv_id: Option<String>,
        reply_to: Option<String>,
        #[serde(default)]
        urgent: bool,
        /// Send as `kind: end` (no reply allowed).
        #[serde(default)]
        end: bool,
    },
    /// Clear a stopped-wake alert (e.g. after fixing credentials).
    WakeReset,
    /// Selftest: `live = false` logs the would-be POST; `live = true` fires one
    /// POST whose hint is `run g2h report <token>`. While `dry_run` is set a
    /// live test needs `consent` (the operator said yes to one real POST).
    WakeTest {
        live: bool,
        token: String,
        #[serde(default)]
        consent: bool,
    },
    /// Grok side answers a selftest wake.
    Report {
        token: String,
    },
    /// Block until `token` is reported; `Ok { id }` carries wake->report ms.
    AwaitReport {
        token: String,
        timeout_secs: Option<u64>,
    },
    /// Grok -> Hermes: request an owner-allowlisted op (`name` = op name).
    SendOp {
        name: String,
        args: serde_json::Value,
    },
    /// Owner (Hermes side) -> Grok: queue an `op` for the Grok peer. Grok's
    /// owner decides (its standing instruction); g2hd only validates and logs.
    OpToGrok {
        name: String,
        args: serde_json::Value,
    },
    /// Grok answers an [`Request::OpToGrok`]; lands in the owner inbox.
    OpResultFromGrok {
        request_id: String,
        status: crate::ops::OpStatus,
        detail: String,
    },
    /// Owner: drain the owner inbox (`op_result`s from Grok).
    RecvOwner,
    /// Owner: run a pending approve-tier op once. `code` is the one-time code
    /// g2hd sent out of band through `[ops_approver]`; nothing else approves.
    Approve {
        request_id: String,
        #[serde(default)]
        code: String,
    },
    /// Owner: drop a pending op (needs no code: it can only refuse).
    Deny {
        request_id: String,
    },
    /// Owner: list pending ops.
    Ops,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// `g2h status`. Credentials appear only as "set"/"missing".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub dry_run: bool,
    pub backend_ok: bool,
    pub backend_detail: String,
    pub messages_today: u64,
    pub messages_cap: u64,
    pub tokens_today: u64,
    pub tokens_cap: u64,
    pub wakes_today: u64,
    pub wakes_cap: u64,
    #[serde(default)]
    pub ops_auto: u64,
    #[serde(default)]
    pub ops_approved: u64,
    #[serde(default)]
    pub ops_refused: u64,
    pub grok_inbox: u64,
    pub turns_in_flight: u64,
    pub waiters: u64,
    /// Older daemons and clients call this `peer_active`; both names are read, the new one is written.
    #[serde(alias = "peer_active")]
    pub grok_active: bool,
    pub wake_pending: bool,
    pub webhook: String,
    pub alert: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok {
        id: Option<String>,
    },
    Reply {
        env: Envelope,
        first_delta_ms: Option<u64>,
        total_ms: u64,
    },
    Messages {
        envs: Vec<Envelope>,
    },
    Timeout,
    Status(StatusInfo),
    Doctor {
        checks: Vec<Check>,
    },
    Error {
        code: String,
        message: String,
    },
    Op {
        result: OpResult,
    },
    Pending {
        ops: Vec<PendingOp>,
    },
}

/// Encode one value as a single JSON line (with trailing `\n`).
pub fn encode<T: Serialize>(v: &T) -> serde_json::Result<String> {
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    Ok(s)
}

pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Kind;

    #[test]
    fn requests_roundtrip_one_line() {
        let reqs = vec![
            Request::Send {
                text: "multi\nline".into(),
                conv_id: None,
                reply_to: None,
                no_reply: true,
                notice: false,
            },
            Request::Ask {
                text: "q".into(),
                conv_id: Some("c".into()),
                reply_to: Some("r".into()),
                timeout_secs: Some(30),
            },
            Request::Wait {
                timeout_secs: Some(5),
            },
            Request::Recv,
            Request::Bye,
            Request::Status,
            Request::Doctor,
            Request::HermesSend {
                text: "h".into(),
                conv_id: None,
                reply_to: None,
                urgent: true,
                end: false,
            },
            Request::WakeReset,
            Request::WakeTest {
                live: false,
                token: "t".into(),
                consent: false,
            },
            Request::Report { token: "t".into() },
            Request::AwaitReport {
                token: "t".into(),
                timeout_secs: Some(3),
            },
        ];
        for r in reqs {
            let l = encode(&r).unwrap();
            assert_eq!(l.matches('\n').count(), 1);
            assert_eq!(decode::<Request>(&l).unwrap(), r);
        }
        assert_eq!(encode(&Request::Status).unwrap(), "{\"op\":\"status\"}\n");
        assert!(decode::<Request>(
            "{\"op\":\"hermes_send\",\"text\":\"x\",\"conv_id\":null,\"reply_to\":null}"
        )
        .is_ok());
    }

    #[test]
    fn responses_roundtrip() {
        let env = Envelope::new("c", "a", "b", Kind::Msg, "hi");
        let resps = vec![
            Response::Ok {
                id: Some("x".into()),
            },
            Response::Reply {
                env: env.clone(),
                first_delta_ms: Some(3),
                total_ms: 9,
            },
            Response::Messages { envs: vec![env] },
            Response::Timeout,
            Response::Status(StatusInfo {
                dry_run: true,
                backend_ok: false,
                backend_detail: "d".into(),
                messages_today: 1,
                messages_cap: 2,
                tokens_today: 3,
                tokens_cap: 4,
                wakes_today: 5,
                wakes_cap: 6,
                ops_auto: 2,
                ops_approved: 0,
                ops_refused: 1,
                grok_inbox: 7,
                turns_in_flight: 0,
                waiters: 0,
                grok_active: true,
                wake_pending: false,
                webhook: "missing".into(),
                alert: None,
            }),
            Response::Doctor {
                checks: vec![Check {
                    name: "perms".into(),
                    ok: false,
                    detail: "0644".into(),
                }],
            },
            Response::Error {
                code: "cap".into(),
                message: "daily cap".into(),
            },
        ];
        for r in resps {
            assert_eq!(decode::<Response>(&encode(&r).unwrap()).unwrap(), r);
        }
        assert!(decode::<Request>("{\"op\":\"nope\"}").is_err());
    }

    #[test]
    fn status_writes_grok_active_and_reads_the_old_peer_active_key() {
        let new = r#"{"type":"status","dry_run":false,"backend_ok":true,"backend_detail":"","messages_today":0,"messages_cap":0,"tokens_today":0,"tokens_cap":0,"wakes_today":0,"wakes_cap":0,"grok_inbox":0,"turns_in_flight":0,"waiters":0,"grok_active":true,"wake_pending":false,"webhook":"","alert":null}"#;
        let r = decode::<Response>(new).unwrap();
        let line = encode(&r).unwrap();
        assert!(
            line.contains("\"grok_active\":true") && !line.contains("peer_active"),
            "{line}"
        );
        let old = decode::<Response>(&new.replace("grok_active", "peer_active")).unwrap();
        assert_eq!(old, r);
    }
}
