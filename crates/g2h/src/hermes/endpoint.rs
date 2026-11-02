//! Profile home resolution and gateway endpoint discovery over the control socket.

use super::PeerError;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LINE: usize = 512 * 1024;

/// `hermes_home` from config, else `$HERMES_HOME`, else `~/.hermes`; with a
/// profile name, `<home>/profiles/<name>`.
pub fn resolve_profile_home(hermes_home: Option<&Path>, profile: Option<&str>) -> Option<PathBuf> {
    let base = match hermes_home {
        Some(p) => p.to_path_buf(),
        None => match std::env::var_os("HERMES_HOME").filter(|v| !v.is_empty()) {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".hermes"),
        },
    };
    Some(match profile.filter(|p| !p.is_empty()) {
        Some(name) => base.join("profiles").join(name),
        None => base,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub profile_id: String,
    pub instance_id: String,
    pub authority_epoch: i64,
    /// `http://127.0.0.1:<port>`
    pub api_origin: String,
    control_socket: PathBuf,
}

impl Endpoint {
    pub fn ws_url(&self) -> String {
        format!("{}/api/ws", self.api_origin.replacen("http:", "ws:", 1))
    }
}

fn unavailable(m: impl Into<String>) -> PeerError {
    PeerError::Unavailable(m.into())
}

fn control_socket(home: &Path) -> Result<PathBuf, PeerError> {
    let direct = home.join("gateway.sock");
    if direct.symlink_metadata().is_ok() {
        return Ok(direct);
    }
    let pointer = home.join("gateway.sock.path");
    let meta = pointer
        .symlink_metadata()
        .map_err(|_| unavailable("no gateway control socket"))?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(unavailable("unsafe gateway.sock.path"));
    }
    let raw = std::fs::read_to_string(&pointer)
        .map_err(|_| unavailable("unreadable gateway.sock.path"))?;
    let target = PathBuf::from(raw.trim());
    if !target.is_absolute() || target.file_name().and_then(|n| n.to_str()) != Some("control.sock")
    {
        return Err(unavailable("invalid gateway.sock.path"));
    }
    Ok(target)
}

async fn control_query(sock: &Path, request: &Value) -> Result<Value, PeerError> {
    let fut = async {
        let mut s = UnixStream::connect(sock)
            .await
            .map_err(|e| unavailable(format!("control socket: {}", e.kind())))?;
        s.write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|_| unavailable("control write"))?;
        let mut line = Vec::new();
        let mut r = BufReader::new(s.take(MAX_LINE as u64));
        r.read_until(b'\n', &mut line)
            .await
            .map_err(|_| unavailable("control read"))?;
        let v: Value =
            serde_json::from_slice(&line).map_err(|_| unavailable("invalid control response"))?;
        if v["ok"] != json!(true)
            || v["protocol"] != json!(1)
            || v["id"] != json!(1)
            || !v["result"].is_object()
        {
            return Err(unavailable("gateway refused control request"));
        }
        Ok(v["result"].clone())
    };
    tokio::time::timeout(CONTROL_TIMEOUT, fut)
        .await
        .map_err(|_| unavailable("control timeout"))?
}

fn loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("http://") else {
        return false;
    };
    let Some((host, port)) = rest.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false);
    loopback && port.parse::<u16>().map(|p| p > 0).unwrap_or(false)
}

/// Identify the ready owner of `profile_home`. Read-only; never starts anything.
pub async fn discover(profile_home: &Path) -> Result<Endpoint, PeerError> {
    let home =
        std::fs::canonicalize(profile_home).map_err(|_| unavailable("profile home missing"))?;
    let sock = control_socket(&home)?;
    let r = control_query(&sock, &json!({"protocol": 1, "verb": "identify", "id": 1})).await?;
    if r["runtime_protocol"] != json!(1) {
        return Err(unavailable("incompatible runtime_protocol"));
    }
    let state = r["state"].as_str().unwrap_or("");
    if state != "ready" {
        return Err(unavailable(format!("gateway state {state:?}")));
    }
    let caps_ok = r["capabilities"]
        .as_array()
        .is_some_and(|c| c.iter().any(|x| x == "session-authority-v1"));
    if !caps_ok {
        return Err(unavailable("session authority unavailable"));
    }
    let wanted = home.to_string_lossy();
    let matches: Vec<&Value> = r["served_profiles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|p| {
                    p["home"].as_str().is_some_and(|h| {
                        std::fs::canonicalize(h)
                            .map(|c| c.to_string_lossy() == wanted)
                            .unwrap_or(h == wanted)
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let profile_id = match matches.as_slice() {
        [p] => p["profile_id"].as_str().filter(|s| !s.is_empty()),
        _ => None,
    }
    .ok_or_else(|| unavailable("profile mismatch"))?;
    let instance_id = r["instance_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| unavailable("bad identity"))?;
    let epoch = r["authority_epoch"]
        .as_i64()
        .filter(|e| *e > 0)
        .ok_or_else(|| unavailable("bad identity"))?;
    let origin = r["api_origin"]
        .as_str()
        .filter(|o| loopback_origin(o))
        .ok_or_else(|| unavailable("invalid api_origin"))?;
    Ok(Endpoint {
        profile_id: profile_id.into(),
        instance_id: instance_id.into(),
        authority_epoch: epoch,
        api_origin: origin.into(),
        control_socket: sock,
    })
}

/// Mint a fresh single-use WS ticket for `ep`.
pub(crate) async fn mint_ticket(ep: &Endpoint) -> Result<String, PeerError> {
    let req = json!({"protocol": 1, "id": 1, "verb": "session-ticket", "params": {
        "profile_id": ep.profile_id, "instance_id": ep.instance_id, "purpose": "interactive"}});
    let g = control_query(&ep.control_socket, &req).await?;
    if g["profile_id"] != json!(ep.profile_id)
        || g["instance_id"] != json!(ep.instance_id)
        || g["runtime_protocol"] != json!(1)
    {
        return Err(unavailable("gateway identity changed"));
    }
    g["ticket"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .ok_or_else(|| unavailable("no ticket"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_home_resolution() {
        let h = resolve_profile_home(Some(Path::new("/x/.hermes")), Some("bob")).unwrap();
        assert_eq!(h, PathBuf::from("/x/.hermes/profiles/bob"));
        assert_eq!(
            resolve_profile_home(Some(Path::new("/x")), None).unwrap(),
            PathBuf::from("/x")
        );
    }

    #[test]
    fn origin_must_be_loopback_with_port() {
        assert!(loopback_origin("http://127.0.0.1:8123"));
        assert!(loopback_origin("http://[::1]:8123"));
        assert!(!loopback_origin("http://10.0.0.2:8123"));
        assert!(!loopback_origin("http://127.0.0.1"));
        assert!(!loopback_origin("https://127.0.0.1:1"));
    }
}
