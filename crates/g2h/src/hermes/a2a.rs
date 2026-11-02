//! `A2aBackend`: A2A v1.0 JSON-RPC over plain HTTP to a loopback URL.
//! `SendMessage` with `contextId` = conv id; `CancelTask` on deadline/interrupt.

use super::{ms, Health, PeerBackend, PeerError, Reply};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_BODY: u64 = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct A2aOptions {
    /// `http://127.0.0.1:<port>/` (loopback only).
    pub url: String,
    /// Bearer token; never logged or printed.
    pub bearer_token: Option<String>,
}

impl A2aOptions {
    pub fn new(url: &str) -> A2aOptions {
        A2aOptions {
            url: url.into(),
            bearer_token: None,
        }
    }
}

impl std::fmt::Debug for A2aOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aOptions")
            .field("url", &self.url)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

pub struct A2aBackend {
    opts: A2aOptions,
    next_id: AtomicU64,
    /// conv_id -> last task id
    tasks: Mutex<HashMap<String, String>>,
}

/// (host:port, path) of a loopback http URL.
fn split_url(url: &str) -> Result<(String, String), PeerError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| PeerError::Unavailable("a2a url must be http://".into()))?;
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = auth.rsplit_once(':').map(|h| h.0).unwrap_or(auth);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if !loopback {
        return Err(PeerError::Unavailable("a2a url must be loopback".into()));
    }
    let auth = if auth.contains(':') && !auth.ends_with(']') {
        auth.to_string()
    } else {
        format!("{auth}:80")
    };
    Ok((auth, path.to_string()))
}

fn reply_text(result: &Value) -> String {
    let r = result
        .get("task")
        .or_else(|| result.get("message"))
        .unwrap_or(result);
    let parts_text = |parts: &Value| -> Vec<String> {
        parts
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p["text"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out: Vec<String> = r["artifacts"]
        .as_array()
        .map(|a| a.iter().flat_map(|art| parts_text(&art["parts"])).collect())
        .unwrap_or_default();
    if out.is_empty() {
        out = parts_text(&r["parts"]);
    }
    if out.is_empty() {
        out = parts_text(&r["status"]["message"]["parts"]);
    }
    out.join("")
}

impl A2aBackend {
    pub fn new(opts: A2aOptions) -> A2aBackend {
        A2aBackend {
            opts,
            next_id: AtomicU64::new(1),
            tasks: Mutex::new(HashMap::new()),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, PeerError> {
        let (addr, path) = split_url(&self.opts.url)?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        let auth = match &self.opts.bearer_token {
            Some(t) => format!("Authorization: Bearer {t}\r\n"),
            None => String::new(),
        };
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n",
            body.len()
        );
        let mut tcp = tokio::net::TcpStream::connect(&addr)
            .await
            .map_err(|e| PeerError::Unavailable(format!("a2a connect: {}", e.kind())))?;
        tcp.write_all(head.as_bytes())
            .await
            .map_err(|_| PeerError::Disconnected("a2a write".into()))?;
        tcp.write_all(body.as_bytes())
            .await
            .map_err(|_| PeerError::Disconnected("a2a write".into()))?;
        let mut buf = Vec::new();
        tcp.take(MAX_BODY)
            .read_to_end(&mut buf)
            .await
            .map_err(|_| PeerError::Disconnected("a2a read".into()))?;
        let pos = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| PeerError::Protocol("a2a: bad http response".into()))?;
        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
        let status = head.split_whitespace().nth(1).unwrap_or("");
        if status != "200" {
            return Err(PeerError::Rpc {
                code: status.parse().unwrap_or(0),
                message: "a2a http error".into(),
            });
        }
        let mut payload = buf[pos + 4..].to_vec();
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            payload = dechunk(&payload)
                .ok_or_else(|| PeerError::Protocol("a2a: bad chunked body".into()))?;
        }
        let v: Value = serde_json::from_slice(&payload)
            .map_err(|_| PeerError::Protocol("a2a: invalid json".into()))?;
        if let Some(e) = v.get("error") {
            return Err(PeerError::Rpc {
                code: e["code"].as_i64().unwrap_or(0),
                message: e["message"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect(),
            });
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let i = b.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&b[..i]).ok()?.split(';').next()?.trim(),
            16,
        )
        .ok()?;
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(i + 2..i + 2 + size)?);
        b = b.get(i + 4 + size..)?;
    }
}

impl PeerBackend for A2aBackend {
    async fn ask(
        &self,
        conv_id: &str,
        framed_text: &str,
        deadline: Duration,
    ) -> Result<Reply, PeerError> {
        let t0 = Instant::now();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params = json!({"message": {
            "messageId": format!("g2h-{conv_id}-{id}"),
            "role": "ROLE_USER",
            "contextId": conv_id,
            "parts": [{"text": framed_text}],
        }});
        let result = match tokio::time::timeout(deadline, self.call("SendMessage", params)).await {
            Ok(r) => r?,
            Err(_) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), self.interrupt(conv_id)).await;
                return Err(PeerError::Deadline);
            }
        };
        let task = result.get("task").unwrap_or(&result);
        if let Some(tid) = task["id"].as_str() {
            self.tasks
                .lock()
                .unwrap()
                .insert(conv_id.into(), tid.into());
        }
        Ok(Reply {
            text: reply_text(&result),
            usage_tokens: None,
            first_delta_ms: None,
            total_ms: ms(t0.elapsed()),
            rotated: false,
            oob_stripped: 0,
            context_tokens: None,
        })
    }

    async fn interrupt(&self, conv_id: &str) -> Result<(), PeerError> {
        let Some(tid) = self.tasks.lock().unwrap().get(conv_id).cloned() else {
            return Ok(());
        };
        self.call("CancelTask", json!({"id": tid}))
            .await
            .map(|_| ())
    }

    async fn health(&self) -> Health {
        match split_url(&self.opts.url) {
            Ok((addr, _)) => match tokio::net::TcpStream::connect(&addr).await {
                Ok(_) => Health {
                    ok: true,
                    detail: "a2a endpoint reachable".into(),
                },
                Err(e) => Health {
                    ok: false,
                    detail: format!("a2a connect: {}", e.kind()),
                },
            },
            Err(e) => Health {
                ok: false,
                detail: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_must_be_loopback() {
        assert_eq!(
            split_url("http://127.0.0.1:9900/").unwrap(),
            ("127.0.0.1:9900".into(), "/".into())
        );
        assert_eq!(
            split_url("http://localhost").unwrap(),
            ("localhost:80".into(), "/".into())
        );
        assert!(split_url("http://10.1.2.3:9900/").is_err());
        assert!(split_url("https://127.0.0.1:9900/").is_err());
    }

    #[test]
    fn extracts_text_from_task_or_message() {
        assert_eq!(
            reply_text(
                &json!({"task": {"artifacts": [{"parts": [{"text": "a"}, {"text": "b"}]}]}})
            ),
            "ab"
        );
        assert_eq!(
            reply_text(&json!({"message": {"parts": [{"text": "m"}]}})),
            "m"
        );
        assert_eq!(
            reply_text(&json!({"status": {"message": {"parts": [{"text": "s"}]}}})),
            "s"
        );
    }

    #[test]
    fn dechunks() {
        assert_eq!(
            dechunk(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap(),
            b"abcde"
        );
    }

    #[test]
    fn debug_redacts_token() {
        let mut o = A2aOptions::new("http://127.0.0.1:1/");
        o.bearer_token = Some("sekrit".into());
        assert!(!format!("{o:?}").contains("sekrit"));
    }
}
