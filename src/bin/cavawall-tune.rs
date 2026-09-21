//! Edit the current wallpaper's settings and write them to its own file.
//!
//! A separate binary on purpose. cavawall's argv must stay exactly `[binary]` -
//! the launcher, cavawall-theme and fullscreen-watch all identify the process
//! by an exact match, so a `--edit-curve` flag would have broken all three at
//! once. This ships and installs alongside it and touches none of that
//!
//! The UI is a page served to the browser rather than a window: clicking points
//! on an image is what a browser is already good at, and cavawall has no input
//! region - its layer surface is deliberately click-through

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

use cavawall::app_config::WallpaperConfig;
use cavawall::curve;

const PAGE: &str = include_str!("../assets/tune.html");

fn main() {
    let Some(wallpaper) = curve::current_wallpaper().filter(|p| p.is_file()) else {
        eprintln!("cavawall-tune: no current wallpaper in the shell's state");
        std::process::exit(1);
    };
    let Some(key) = curve::content_key(&wallpaper) else {
        eprintln!("cavawall-tune: cannot read {}", wallpaper.display());
        std::process::exit(1);
    };

    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cavawall-tune: cannot listen: {e}");
            std::process::exit(1);
        }
    };
    // Port 0 means the kernel picks one, so two runs never collide and nothing
    // has to guess whether a fixed port is free
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let url = format!("http://127.0.0.1:{port}/");
    println!("cavawall-tune: editing {}", wallpaper.display());
    println!("cavawall-tune: key {key}");
    println!("cavawall-tune: open {url}");
    // Best effort. A headless run still prints the URL above
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    println!("cavawall-tune: ctrl-c when finished");
    for stream in listener.incoming().flatten() {
        if handle(stream, &wallpaper, &key).is_break() {
            break;
        }
    }
}

/// Serve one request. Breaking ends the process, which is what Save does
fn handle(mut s: TcpStream, wallpaper: &PathBuf, key: &str) -> std::ops::ControlFlow<()> {
    let mut reader = BufReader::new(match s.try_clone() {
        Ok(c) => c,
        Err(_) => return std::ops::ControlFlow::Continue(()),
    });
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return std::ops::ControlFlow::Continue(());
    }
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    // Headers, only for Content-Length: the body cannot be read without it
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }

    match (method, path) {
        ("GET", "/") => {
            let body = PAGE.replace("__KEY__", key);
            reply(&mut s, "200 OK", "text/html; charset=utf-8", body.as_bytes());
        }
        ("GET", "/wallpaper") => match std::fs::read(wallpaper) {
            Ok(bytes) => {
                let mime = match wallpaper.extension().and_then(|e| e.to_str()) {
                    Some("png") => "image/png",
                    Some("webp") => "image/webp",
                    Some("gif") => "image/gif",
                    _ => "image/jpeg",
                };
                reply(&mut s, "200 OK", mime, &bytes);
            }
            Err(_) => reply(&mut s, "404 Not Found", "text/plain", b"no wallpaper"),
        },
        ("GET", "/existing") => {
            // Hand back whatever block is already in the config so an edit
            // starts from the current curve rather than a blank image
            let body = WallpaperConfig::load(&config_dir(), key)
                .as_ref()
                .and_then(|w| serde_json::to_string(w).ok())
                .unwrap_or_else(|| "{}".to_string());
            reply(&mut s, "200 OK", "application/json; charset=utf-8", body.as_bytes());
        }
        ("POST", "/save") => {
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).is_err() {
                reply(&mut s, "400 Bad Request", "text/plain", b"short body");
                return std::ops::ControlFlow::Continue(());
            }
            let incoming = String::from_utf8_lossy(&body).to_string();
            match write_config(key, &incoming) {
                Ok(p) => {
                    println!("cavawall-tune: wrote {} bytes to {}", incoming.len(), p.display());
                    // The path is sampled into an SSBO at startup and never
                    // re-read, so a saved curve does nothing until cavawall
                    // restarts. Through the launcher, never the binary: it
                    // holds the flock and reaps the stale instance, and
                    // starting the binary directly stacks a second layer
                    match std::process::Command::new("cavawall-launch").spawn() {
                        Ok(_) => println!("cavawall-tune: restarted cavawall"),
                        Err(e) => eprintln!("cavawall-tune: run cavawall-launch yourself: {e}"),
                    }
                    reply(&mut s, "200 OK", "text/plain", b"saved");
                    // Deliberately NOT breaking. Quitting after one save made
                    // the connection close under the client, which made fetch
                    // reject on a save that had succeeded - and the page then
                    // could not tell that apart from a real failure. Staying
                    // up also makes save/look/adjust/save the normal loop
                }
                Err(e) => {
                    eprintln!("cavawall-tune: save failed: {e}");
                    reply(&mut s, "500 Server Error", "text/plain", e.as_bytes());
                }
            }
        }
        _ => reply(&mut s, "404 Not Found", "text/plain", b"no"),
    }
    std::ops::ControlFlow::Continue(())
}

fn reply(s: &mut TcpStream, status: &str, mime: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("cavawall")
}

/// Store this wallpaper's settings, replacing its file.
fn write_config(key: &str, json: &str) -> Result<PathBuf, String> {
    let incoming: WallpaperConfig = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let dir = config_dir();
    incoming.save(&dir, key).map_err(|e| e.to_string())?;
    Ok(WallpaperConfig::path(&dir, key))
}
