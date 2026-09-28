//! Which monitors are showing a fullscreen window, from Hyprland's own IPC
//!
//! A covered monitor cannot show the bars, and drawing under a game costs the
//! compositor GPU time nobody sees (~13pp of Hyprland's at 30fps, measured).
//! Only VISIBLE fullscreen windows count: one on a monitor's active workspace,
//! or on a special workspace open over it. A fullscreen window left on a
//! hidden workspace covers nothing.
//!
//! Events arrive on `.socket2.sock` and the answer is re-derived from
//! `.socket.sock` on each burst, never from an event's payload. No
//! subprocesses: one connect, one batched read

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Events that can change whether a fullscreen window is visible. Not
/// `activewindow`: Hyprland re-sends it on every title change, about once a
/// second from a browser tab, and focus alone cannot change the answer
const WATCHED: [&str; 9] = [
    "fullscreen",
    "openwindow",
    "closewindow",
    "movewindow",
    "workspace",
    "focusedmon",
    "activespecial",
    "monitoradded",
    "monitorremoved",
];

/// `CAVAWALL_HYPR_RUNTIME` first: a test instance runs with a private
/// `XDG_RUNTIME_DIR` and still has to find the compositor's sockets
fn socket_dir() -> Option<PathBuf> {
    let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let run = std::env::var_os("CAVAWALL_HYPR_RUNTIME").or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))?;
    Some(PathBuf::from(run).join("hypr").join(sig))
}

/// The event stream, non-blocking, or None away from Hyprland
pub fn events() -> Option<UnixStream> {
    let s = UnixStream::connect(socket_dir()?.join(".socket2.sock")).ok()?;
    s.set_nonblocking(true).ok()?;
    Some(s)
}

/// Drain what the event socket holds: Some(true) when anything in it can
/// change coverage, None once Hyprland has closed it. A closed socket stays
/// readable forever, so the caller must drop it rather than wait for more.
/// Lines are only ever matched, never trusted
pub fn relevant(events: &mut UnixStream, partial: &mut Vec<u8>) -> Option<bool> {
    let mut buf = [0u8; 4096];
    let mut hit = false;
    loop {
        match events.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => partial.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return None,
        }
    }
    while let Some(nl) = partial.iter().position(|&b| b == b'\n') {
        let line = &partial[..nl];
        let name = line.split(|&b| b == b'>').next().unwrap_or(line);
        hit |= WATCHED.iter().any(|w| w.as_bytes() == name);
        partial.drain(..=nl);
    }
    Some(hit)
}

/// The one question asked: every monitor and every window, in one reply
const QUERY: &[u8] = b"[[BATCH]]j/monitors;j/clients";

/// Monitor names showing a fullscreen window right now, waiting for the
/// answer. Only at startup, before anything is drawn; the running instance
/// asks through `query` and the event loop instead. None when Hyprland could
/// not be read - which is not "nothing is covered" and must not be treated
/// as it
pub fn covered() -> Option<BTreeSet<String>> {
    let mut s = UnixStream::connect(socket_dir()?.join(".socket.sock")).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    s.write_all(QUERY).ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    parse_covered(&raw)
}

/// Ask without waiting: the request is written - a few dozen bytes, which a
/// fresh socket always takes whole - and the reply is left for the event
/// loop to read as it arrives. Hyprland closes the socket after answering,
/// and that end is when the reply is complete
pub fn query() -> Option<UnixStream> {
    let mut s = UnixStream::connect(socket_dir()?.join(".socket.sock")).ok()?;
    s.write_all(QUERY).ok()?;
    s.set_nonblocking(true).ok()?;
    Some(s)
}

/// Read what has arrived of a reply: Some(true) once it is complete
pub fn read_reply(s: &mut UnixStream, raw: &mut Vec<u8>) -> Option<bool> {
    let mut buf = [0u8; 16384];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return Some(true),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Some(false),
            Err(_) => return None,
        }
    }
}

/// The covered monitors from a complete reply
pub fn parse_covered(raw: &[u8]) -> Option<BTreeSet<String>> {
    let mut docs = serde_json::Deserializer::from_slice(raw).into_iter::<serde_json::Value>();
    let monitors = docs.next()?.ok()?;
    let clients = docs.next()?.ok()?;

    let id = |v: &serde_json::Value, key: &str| v.get(key).and_then(|w| w.get("id")).and_then(serde_json::Value::as_i64);
    let fullscreen_ws: Vec<i64> = clients
        .as_array()?
        .iter()
        .filter(|c| c.get("fullscreen").and_then(serde_json::Value::as_i64).unwrap_or(0) != 0)
        .filter_map(|c| id(c, "workspace"))
        .collect();
    let mut out = BTreeSet::new();
    for m in monitors.as_array()? {
        let Some(name) = m.get("name").and_then(serde_json::Value::as_str) else { continue };
        // A special workspace id of 0 means none is open on this monitor
        let visible = [id(m, "activeWorkspace"), id(m, "specialWorkspace").filter(|&i| i != 0)];
        if visible.iter().flatten().any(|w| fullscreen_ws.contains(w)) {
            out.insert(name.to_owned());
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::relevant;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn matches_only_watched_events_and_reports_a_closed_socket() {
        let (mut tx, mut rx) = UnixStream::pair().expect("pair");
        rx.set_nonblocking(true).expect("nonblocking");
        let mut partial = Vec::new();
        tx.write_all(b"activewindow>>kitty,title\nwindowtitle>>0x1\n").expect("write");
        assert_eq!(relevant(&mut rx, &mut partial), Some(false), "title noise is not coverage");
        tx.write_all(b"fullscreen>>1\nworkspa").expect("write");
        assert_eq!(relevant(&mut rx, &mut partial), Some(true));
        assert_eq!(partial, b"workspa", "a split line waits for its end");
        drop(tx);
        assert_eq!(relevant(&mut rx, &mut partial), None, "EOF is closed, not quiet");
    }
}
