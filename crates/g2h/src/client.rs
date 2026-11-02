//! Socket client: one request line, one response line.

use crate::ipc::{self, Request, Response};
use std::path::Path;
use std::time::Duration;

/// Async (tokio) call.
pub async fn call(socket: &Path, req: &Request) -> Result<Response, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let s = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let (r, mut w) = s.into_split();
    w.write_all(ipc::encode(req).map_err(|e| e.to_string())?.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(r)
        .read_line(&mut line)
        .await
        .map_err(|e| e.to_string())?;
    if line.is_empty() {
        return Err("daemon closed the connection".into());
    }
    ipc::decode(&line).map_err(|e| e.to_string())
}

/// Blocking call with a read timeout (for the CLI).
pub fn call_blocking(
    socket: &Path,
    req: &Request,
    read_timeout: Duration,
) -> Result<Response, ClientError> {
    use std::io::{BufRead, BufReader, Write};
    let mut s =
        std::os::unix::net::UnixStream::connect(socket).map_err(ClientError::Unreachable)?;
    s.set_read_timeout(Some(read_timeout))
        .map_err(|e| ClientError::Io(e.to_string()))?;
    let line = ipc::encode(req).map_err(|e| ClientError::Io(e.to_string()))?;
    s.write_all(line.as_bytes())
        .map_err(|e| ClientError::Io(e.to_string()))?;
    let mut out = String::new();
    BufReader::new(&s)
        .read_line(&mut out)
        .map_err(|e| ClientError::Io(e.to_string()))?;
    if out.is_empty() {
        return Err(ClientError::Io("daemon closed the connection".into()));
    }
    ipc::decode(&out).map_err(|e| ClientError::Io(e.to_string()))
}

#[derive(Debug)]
pub enum ClientError {
    /// No daemon on the socket.
    Unreachable(std::io::Error),
    Io(String),
}
