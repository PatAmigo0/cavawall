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
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use cavawall::app_config::{self, Config, WallpaperConfig};
use cavawall::control::{self, Request};
use cavawall::{curve, scheme};

const PAGE: &str = include_str!("../assets/tune.html");

/// A request is a few hundred bytes of headers and a few KB of JSON - or a
/// baked reveal image, a QOI of at most a wallpaper's size. Past this it is
/// not the page talking
const MAX_BODY: usize = 64 << 20;

/// How long one connection may sit silent. Browsers open speculative
/// connections and leave them idle; one of those must not hold a thread forever
const IDLE: Duration = Duration::from_secs(10);

/// What every connection needs, shared read-only between their threads
struct Site {
    wallpaper: PathBuf,
    key: String,
    /// The only Host values accepted: this port on the loopback names. Anything
    /// else is a page elsewhere reaching in through DNS rebinding
    hosts: [String; 2],
}

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
    let port = listener.local_addr().map_or(0, |a: SocketAddr| a.port());
    let url = format!("http://127.0.0.1:{port}/");
    println!("cavawall-tune: editing {}", wallpaper.display());
    println!("cavawall-tune: key {key}");
    println!("cavawall-tune: open {url}");
    // Best effort, and detached: a browser started here must not be ours to
    // take down with a ctrl-c. A headless run still prints the URL above
    let _ = detached(Command::new("xdg-open").arg(&url));
    println!("cavawall-tune: ctrl-c when finished");

    let site = Arc::new(Site {
        wallpaper,
        key,
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
    });
    for stream in listener.incoming().flatten() {
        let site = Arc::clone(&site);
        // One thread per connection: a stalled one then stalls nothing else
        std::thread::spawn(move || handle(stream, &site));
    }
}

/// Run `cmd` in a session of its own, with no stdio. Anything started from
/// here would otherwise sit in the terminal's process group, and a ctrl-c or a
/// closed terminal would take it down along with this tool
fn detached(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches nothing of the parent's
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
}

/// One parsed request: method, path, and the headers this needs
struct Req {
    method: String,
    path: String,
    host: Option<String>,
    origin: Option<String>,
    body: Vec<u8>,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Req> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let (mut len, mut host, mut origin) = (0usize, None, None);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 || h.trim().is_empty() {
            break;
        }
        let Some((name, value)) = h.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = value.parse().ok()?,
            "host" => host = Some(value.to_owned()),
            "origin" => origin = Some(value.to_owned()),
            _ => {}
        }
    }
    if len > MAX_BODY {
        return None;
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(Req { method, path, host, origin, body })
}

fn handle(stream: TcpStream, site: &Site) {
    let _ = stream.set_read_timeout(Some(IDLE));
    let _ = stream.set_write_timeout(Some(IDLE));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut out = stream;
    let Some(req) = read_request(&mut BufReader::new(read_half)) else { return };

    // Only this page may talk to this server. The Host check stops DNS
    // rebinding; the Origin check stops another site POSTing a config
    let local = |v: &Option<String>, strip: &str| {
        v.as_deref().is_some_and(|v| {
            let v = v.strip_prefix(strip).unwrap_or(v);
            site.hosts.iter().any(|h| h == v)
        })
    };
    if !local(&req.host, "") || (req.origin.is_some() && !local(&req.origin, "http://")) {
        reply(&mut out, "403 Forbidden", "text/plain", b"not from this page");
        return;
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => {
            let body = PAGE.replace("__KEY__", &site.key);
            reply(&mut out, "200 OK", "text/html; charset=utf-8", body.as_bytes());
        }
        ("GET", "/wallpaper") => match std::fs::read(&site.wallpaper) {
            Ok(bytes) => reply(&mut out, "200 OK", mime(&site.wallpaper), &bytes),
            Err(_) => reply(&mut out, "404 Not Found", "text/plain", b"no wallpaper"),
        },
        ("GET", "/existing") => {
            // Whatever this wallpaper already has, so an edit starts from it
            let body = WallpaperConfig::load(&config_dir(), &site.key)
                .and_then(|w| serde_json::to_string(&w).ok())
                .unwrap_or_else(|| "{}".to_owned());
            reply(&mut out, "200 OK", "application/json", body.as_bytes());
        }
        ("GET", "/context") => {
            let body = context(site).to_string();
            reply(&mut out, "200 OK", "application/json", body.as_bytes());
        }
        // The x-ray picture the user keeps for this wallpaper, if any
        ("GET", "/reveal-source") => match reveal_source(&site.wallpaper) {
            Some(p) => match std::fs::read(&p) {
                Ok(bytes) => reply(&mut out, "200 OK", mime(&p), &bytes),
                Err(_) => reply(&mut out, "404 Not Found", "text/plain", b"unreadable"),
            },
            None => reply(&mut out, "404 Not Found", "text/plain", b"none"),
        },
        // A picture chosen in the page, kept in the x-ray folder under the
        // wallpaper's name so it can be filtered again later
        ("POST", "/reveal-source") => {
            let body = match keep_reveal_source(&site.wallpaper, &req.body) {
                Ok(p) => serde_json::json!({ "ok": true, "path": p.display().to_string() }),
                Err(e) => serde_json::json!({ "ok": false, "error": e }),
            };
            reply(&mut out, "200 OK", "application/json", body.to_string().as_bytes());
        }
        // The baked reveal: a QOI the page encoded, or an empty body to drop it
        ("POST", "/reveal") => {
            let path = config_dir().join("wallpapers").join(format!("{}.reveal.qoi", site.key));
            let result = if req.body.is_empty() {
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                    _ => Ok(()),
                }
            } else if cavawall::qoi::decode(&req.body).is_none() {
                Err("not a readable QOI image".to_owned())
            } else {
                path.parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&path, &req.body))
                    .map_err(|e| e.to_string())
            };
            let body = match result {
                Ok(()) => serde_json::json!({ "ok": true }),
                Err(e) => serde_json::json!({ "ok": false, "error": e }),
            };
            reply(&mut out, "200 OK", "application/json", body.to_string().as_bytes());
        }
        ("POST", "/save") => {
            let body = match save(&site.key, &req.body) {
                Ok((path, applied)) => {
                    println!("cavawall-tune: wrote {} ({applied})", path.display());
                    serde_json::json!({ "ok": true, "applied": applied })
                }
                Err(e) => {
                    eprintln!("cavawall-tune: save failed: {e}");
                    serde_json::json!({ "ok": false, "error": e })
                }
            };
            reply(&mut out, "200 OK", "application/json", body.to_string().as_bytes());
        }
        _ => reply(&mut out, "404 Not Found", "text/plain", b"no"),
    }
}

const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "gif", "avif"];

/// Where x-ray pictures live: `bars.reveal_dir` in config.toml, else
/// ~/Pictures/cavawall-xray. Never the wallpaper folder - the shell's picker
/// scans that one and would offer every x-ray picture as a wallpaper
fn reveal_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let set = load_config().and_then(|c| c.bars.reveal_dir).filter(|d| !d.is_empty());
    match set.as_deref() {
        Some(d) => d.strip_prefix("~/").map_or_else(|| PathBuf::from(d), |rest| home.join(rest)),
        None => home.join("Pictures/cavawall-xray"),
    }
}

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// `<wallpaper stem>.<image extension>` in the x-ray folder
fn reveal_source(wallpaper: &Path) -> Option<PathBuf> {
    let stem = wallpaper.file_stem()?;
    std::fs::read_dir(reveal_dir())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_stem() == Some(stem) && is_image(p))
}

/// The extension a picture's own bytes call for, so a file named wrongly, or
/// something that is no picture at all, is caught here
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [0xff, 0xd8, 0xff, ..] => Some("jpg"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        [_, _, _, _, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f' | b's', ..] => Some("avif"),
        _ => None,
    }
}

/// Store a chosen picture as this wallpaper's x-ray source, replacing any
/// earlier one of another extension so exactly one is found
fn keep_reveal_source(wallpaper: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let ext = sniff(bytes).ok_or("not a PNG, JPEG, WebP, GIF or AVIF picture")?;
    let stem = wallpaper.file_stem().ok_or("the wallpaper has no name")?;
    let dir = reveal_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    while let Some(old) = reveal_source(wallpaper) {
        std::fs::remove_file(&old).map_err(|e| format!("{}: {e}", old.display()))?;
    }
    let path = dir.join(stem).with_extension(ext);
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("cavawall-tune: x-ray picture {}", path.display());
    Ok(path)
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("avif") => "image/avif",
        _ => "image/jpeg",
    }
}

fn reply(s: &mut TcpStream, status: &str, mime: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
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

/// Everything the page shows besides the wallpaper's own file: what an unset
/// field falls back to, the gradient the bars are drawn in, the image's size
/// and what the running instance is doing
fn load_config() -> Option<Config> {
    std::fs::read_to_string(config_dir().join("config.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
}

fn context(site: &Site) -> serde_json::Value {
    let config = load_config();
    let gradient = config.as_ref().map(|c| {
        let live = c.scheme.as_ref().and_then(|s| s.colors).unwrap_or(false);
        let stops = app_config::ordered_stops(&c.colors);
        let rgba = app_config::resolve_stops(&stops, if live { scheme::colours() } else { None }.as_ref());
        rgba.iter()
            .map(|c| {
                let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                serde_json::json!([byte(c[0]), byte(c[1]), byte(c[2]), c[3]])
            })
            .collect::<Vec<_>>()
    });
    let status = control::request(&Request::Status).ok().and_then(|r| r.data);
    let baked = config_dir().join("wallpapers").join(format!("{}.reveal.qoi", site.key));
    serde_json::json!({
        "key": site.key,
        "wallpaper": site.wallpaper.display().to_string(),
        "image": curve::image_size(&site.wallpaper),
        "reveal_source": reveal_source(&site.wallpaper).map(|p| p.display().to_string()),
        "reveal_baked": baked.is_file(),
        "reveal_dir": reveal_dir().display().to_string(),
        "defaults": config.as_ref().map(|c| serde_json::json!({
            "mode": c.general.mode,
            "bars": c.bars,
            "circle": c.circle,
            "follow_shell_bars": c.scheme.as_ref().and_then(|s| s.bars).unwrap_or(false),
        })),
        "shell_bars": scheme::bar_count(),
        "gradient": gradient,
        "status": status,
    })
}

/// Store this wallpaper's settings, replacing its file, then apply them
///
/// A running instance re-execs in place, which keeps its pid and its
/// environment - fullscreen-watch's output pin included. Only when none is
/// running is one started, through the launcher, detached
fn save(key: &str, json: &[u8]) -> Result<(PathBuf, String), String> {
    let incoming: WallpaperConfig = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let dir = config_dir();
    incoming.save(&dir, key).map_err(|e| e.to_string())?;
    let applied = match control::request(&Request::Reload) {
        Ok(r) if r.ok => "reloaded".to_owned(),
        Ok(r) => format!("not reloaded: {}", r.error.unwrap_or_default()),
        Err(e) => {
            // Silent rather than absent: replace it, since it cannot reload
            let wedged = matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut);
            match detached(&mut Command::new(launcher())) {
                Ok(_) if wedged => "restarted".to_owned(),
                Ok(_) => "started".to_owned(),
                Err(e) => format!("not started: {e}"),
            }
        }
    };
    Ok((WallpaperConfig::path(&dir, key), applied))
}

/// The launcher next to this binary's usual home, else whatever PATH finds
fn launcher() -> PathBuf {
    let local = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/bin/cavawall-launch"))
        .filter(|p| p.exists());
    local.unwrap_or_else(|| PathBuf::from("cavawall-launch"))
}
