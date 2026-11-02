//! Hermes -> Grok wake webhook: credentials file and one outbound POST.
//!
//! `credentials` (dotenv, mode 0600) holds `webhook_url`, `sender_key` and
//! `key_mode` (`header:Authorization-Bearer` sends `Authorization: Bearer <key>`;
//! `header:<Name>` sends `<Name>: <key>`). The URL path/query and the key are
//! secret: only [`host_only`] is ever logged or shown. `http://` is posted
//! directly; `https://` goes through `curl` with its config on stdin, so neither
//! the URL nor the key appears in a process listing.

use crate::paths::check_perms;
use crate::secrets::{parse_dotenv, Fingerprints};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const HINT: &str = "run g2h recv";

#[derive(Clone)]
pub struct WebhookCreds {
    pub url: String,
    key: String,
    header: String,
    bearer: bool,
}

impl std::fmt::Debug for WebhookCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebhookCreds({}, key <redacted>)", host_only(&self.url))
    }
}

impl WebhookCreds {
    fn auth_value(&self) -> String {
        if self.bearer {
            format!("Bearer {}", self.key)
        } else {
            self.key.clone()
        }
    }
    pub fn header_name(&self) -> &str {
        &self.header
    }
}

/// Load the credentials file. `Ok(None)`: missing or incomplete.
/// Refuses group/world-accessible files without reading them.
pub fn load_creds(path: &Path) -> Result<Option<WebhookCreds>, String> {
    if !path.exists() {
        return Ok(None);
    }
    check_perms(path).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let kv = parse_dotenv(&text);
    let get = |k: &str| {
        kv.iter()
            .rev()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let (Some(url), Some(key)) = (get("webhook_url"), get("sender_key")) else {
        return Ok(None);
    };
    let mode = get("key_mode").unwrap_or_else(|| "header:Authorization-Bearer".into());
    let Some(h) = mode.strip_prefix("header:") else {
        return Err(format!(
            "unsupported key_mode {mode:?} (use header:<Name> or header:Authorization-Bearer)"
        ));
    };
    let (header, bearer) = match h.strip_suffix("-Bearer") {
        Some(name) => (name.to_string(), true),
        None => (h.to_string(), false),
    };
    if header.is_empty()
        || !header
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("key_mode header name must be [A-Za-z0-9-]".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("webhook_url must be http:// or https://".into());
    }
    if url.starts_with("http://") && !is_loopback(&url) {
        return Err("webhook_url: plain http:// only to a loopback host (use https://)".into());
    }
    Ok(Some(WebhookCreds {
        url,
        key,
        header,
        bearer,
    }))
}

/// "set" / "missing" / "bad: <why>" — never the values.
pub fn creds_state(path: &Path) -> String {
    match load_creds(path) {
        Ok(Some(_)) => "set".into(),
        Ok(None) => "missing".into(),
        Err(e) => format!("bad: {e}"),
    }
}

/// Fingerprints of every credentials value, so they are redacted anywhere.
pub fn creds_fingerprints(path: &Path) -> Option<Fingerprints> {
    check_perms(path).ok()?;
    let mut fp = Fingerprints::default();
    fp.add_env_file(path).ok()?;
    Some(fp)
}

/// Host of an `http(s)://` URL is `127.0.0.0/8`, `::1` or `localhost`.
fn is_loopback(url: &str) -> bool {
    let host = host_only(url);
    let host = host.split_once("://").map_or("", |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `scheme://host[:port]` only (no userinfo, path or query).
pub fn host_only(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("?", url));
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = auth.rsplit('@').next().unwrap_or("");
    format!("{scheme}://{host}")
}

/// Wake body: `{"conv_id":..,"count":N,"hint":"run g2h recv"}`, < 500 bytes.
pub fn body(conv_id: &str, count: usize) -> String {
    let conv: String = conv_id.chars().take(200).collect();
    serde_json::json!({"conv_id": conv, "count": count, "hint": HINT}).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    Ok(u16),
    /// 4xx: stop waking.
    Client(u16),
    /// 5xx, timeout, connect error: retry with backoff.
    Retryable(String),
}

fn classify(code: u16) -> PostOutcome {
    match code {
        200..=399 => PostOutcome::Ok(code),
        400..=499 => PostOutcome::Client(code),
        _ => PostOutcome::Retryable(format!("HTTP {code}")),
    }
}

pub async fn post(creds: &WebhookCreds, body: &str, timeout: Duration) -> PostOutcome {
    let r = tokio::time::timeout(timeout, async {
        if creds.url.starts_with("https://") {
            post_curl(creds, body, timeout).await
        } else {
            post_http(creds, body).await
        }
    })
    .await;
    match r {
        Ok(Ok(code)) => classify(code),
        Ok(Err(e)) => PostOutcome::Retryable(e),
        Err(_) => PostOutcome::Retryable("timeout".into()),
    }
}

async fn post_http(c: &WebhookCreds, body: &str) -> Result<u16, String> {
    let rest = &c.url["http://".len()..];
    let (auth, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    let hostport = auth.rsplit('@').next().unwrap_or(auth);
    let addr = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let mut s = tokio::net::TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("connect: {}", e.kind()))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\n{}: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        c.header,
        c.auth_value(),
        body.len()
    );
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write: {}", e.kind()))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 8192 {
        let n = s
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {}", e.kind()))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let line = String::from_utf8_lossy(&buf);
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| "bad HTTP status line".into())
}

fn curl_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], "")
    )
}

async fn post_curl(c: &WebhookCreds, body: &str, timeout: Duration) -> Result<u16, String> {
    use std::process::Stdio;
    let cfg = format!(
        "url = {}\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\nheader = {}\ndata-raw = {}\nmax-time = {}\nsilent\noutput = \"/dev/null\"\nwrite-out = \"%{{http_code}}\"\n",
        curl_quote(&c.url),
        curl_quote(&format!("{}: {}", c.header, c.auth_value())),
        curl_quote(body),
        timeout.as_secs().max(1)
    );
    let mut child = tokio::process::Command::new("curl")
        .args(["-q", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("curl: {}", e.kind()))?;
    let mut stdin = child.stdin.take().ok_or("curl: no stdin")?;
    stdin
        .write_all(cfg.as_bytes())
        .await
        .map_err(|e| format!("curl stdin: {}", e.kind()))?;
    drop(stdin);
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("curl: {}", e.kind()))?;
    let code: u16 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if code == 0 {
        return Err(format!("curl failed ({})", out.status.code().unwrap_or(-1)));
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn host_only_strips_secret_parts() {
        assert_eq!(
            host_only("https://u:p@hooks.example.com:8443/a/b?token=x#f"),
            "https://hooks.example.com:8443"
        );
        assert_eq!(host_only("http://127.0.0.1:9/x"), "http://127.0.0.1:9");
    }

    #[test]
    fn body_shape_and_size() {
        let b = body(&"c".repeat(1000), 5);
        assert!(b.len() < 500);
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(
            (v["count"].as_u64(), v["hint"].as_str()),
            (Some(5), Some(HINT))
        );
    }

    #[test]
    fn creds_modes_and_perms() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("credentials");
        assert_eq!(creds_state(&p), "missing");
        std::fs::write(
            &p,
            "webhook_url=https://h.example/x\nsender_key=k123\nkey_mode=header:X-Key\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(creds_state(&p).starts_with("bad"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = load_creds(&p).unwrap().unwrap();
        assert_eq!(
            (c.header_name(), c.auth_value().as_str()),
            ("X-Key", "k123")
        );
        assert!(!format!("{c:?}").contains("k123") && !format!("{c:?}").contains("/x"));
        std::fs::write(&p, "webhook_url=https://h.example/x\nsender_key=k123\n").unwrap();
        let c = load_creds(&p).unwrap().unwrap();
        assert_eq!(
            (c.header_name(), c.auth_value().as_str()),
            ("Authorization", "Bearer k123")
        );
        std::fs::write(&p, "webhook_url=https://h.example/x\n").unwrap();
        assert_eq!(creds_state(&p), "missing");
    }

    #[test]
    fn plain_http_only_to_loopback() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("credentials");
        for (url, ok) in [
            ("http://hooks.example.com/x", false),
            ("http://127.0.0.1.example.com/x", false),
            ("http://10.0.0.5:8644/x", false),
            ("http://127.0.0.1:8644/x", true),
            ("http://localhost:8644/x", true),
            ("http://[::1]:8644/x", true),
            ("https://hooks.example.com/x", true),
        ] {
            std::fs::write(&p, format!("webhook_url={url}\nsender_key=k\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(load_creds(&p).is_ok(), ok, "{url}");
        }
    }
}
