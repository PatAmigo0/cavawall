//! Control socket: a running instance answers status queries and takes orders.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

/// Beside the instance lock, one per session
#[must_use]
pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join("cavawall.sock")
}

/// What a client asks for
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Everything the instance knows about itself
    Status,
    /// Re-exec pinned to `output`, or to automatic when it is None
    Move { output: Option<String> },
    /// Clear the surface and exit
    Stop,
    /// Re-read config and palette without restarting
    Reload,
}

/// What it gets back. `data` carries a Status payload, nothing otherwise
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    #[must_use]
    pub fn ok(data: Option<serde_json::Value>) -> Self {
        Self { ok: true, error: None, data }
    }
    #[must_use]
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), data: None }
    }
}

/// Bind the socket, replacing a stale one.
///
/// Unlinking first is safe only while the instance lock is held.
pub fn bind() -> std::io::Result<UnixListener> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Best effort; a socket left behind is replaced on the next bind anyway
pub fn unbind() {
    let _ = std::fs::remove_file(socket_path());
}

/// One line in, one line out.
pub fn read_request(stream: &UnixStream) -> Option<Request> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

pub fn write_response(mut stream: &UnixStream, response: &Response) {
    if let Ok(mut body) = serde_json::to_vec(response) {
        body.push(b'\n');
        let _ = stream.write_all(&body);
    }
}

/// Client side: one round trip, or an error when nothing is listening
///
/// # Errors
/// When the socket is absent or unreadable, which is how a caller learns
/// there is no running instance.
pub fn request(req: &Request) -> std::io::Result<Response> {
    let stream = UnixStream::connect(socket_path())?;
    let mut body = serde_json::to_vec(req)?;
    body.push(b'\n');
    (&stream).write_all(&body)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(line.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
