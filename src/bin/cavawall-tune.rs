//! Edit the current wallpaper's settings and write them to its own file.
//!
//! A separate binary on purpose. cavawall's argv must stay exactly `[binary]` -
//! the launcher and cavawall-theme identify the process by an exact match, so
//! a `--edit-curve` flag would have broken both. This ships and installs alongside it and touches none of that
//!
//! The UI is a page served to the browser rather than a window: clicking points
//! on an image is what a browser is already good at, and cavawall has no input
//! region - its layer surface is deliberately click-through

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

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
/// What is being edited, cloned per request. `/switch` replaces it whole, so a
/// request never pairs the new wallpaper with the old key
#[derive(Clone)]
struct Site {
    wallpaper: PathBuf,
    key: String,
    /// The only Host values accepted: this port on the loopback names. Anything
    /// else is a page elsewhere reaching in through DNS rebinding
    hosts: [String; 2],
}

fn main() {
    // The same palette and wallpaper sources the visualiser uses
    let config = load_config();
    let scheme_cfg = config.as_ref().and_then(|c| c.scheme.as_ref());
    scheme::configure_colours(scheme_cfg.and_then(|s| s.source.as_deref()), scheme_cfg.and_then(|s| s.path.as_deref()));
    cavawall::wallpaper::configure(config.as_ref().and_then(|c| c.wallpaper.as_ref()));
    let Some(wallpaper) = curve::current_wallpaper().filter(|p| p.is_file()) else {
        eprintln!("cavawall-tune: cannot tell which wallpaper is on screen; set [wallpaper] source in config.toml");
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

    let site = Arc::new(RwLock::new(Site {
        wallpaper,
        key,
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
    }));
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

fn handle(stream: TcpStream, server: &RwLock<Site>) {
    let site = &server.read().unwrap_or_else(PoisonError::into_inner).clone();
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
        // Settings that live in config.toml, shared by every wallpaper
        ("POST", "/global") => {
            let body = match edit_global(&req.body) {
                Ok(applied) => serde_json::json!({ "ok": true, "applied": applied }),
                Err(e) => serde_json::json!({ "ok": false, "error": e }),
            };
            reply(&mut out, "200 OK", "application/json", body.to_string().as_bytes());
        }
        ("GET", "/sources") => {
            let body = serde_json::json!(audio_sources()).to_string();
            reply(&mut out, "200 OK", "application/json", body.as_bytes());
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
        // What is on screen now, so the page can offer to follow a change
        ("GET", "/current") => {
            let body = match on_screen() {
                Some((path, key)) => serde_json::json!({
                    "key": key,
                    "name": path.file_name().map(|n| n.to_string_lossy().into_owned()),
                }),
                None => serde_json::json!({ "key": null }),
            };
            reply(&mut out, "200 OK", "application/json", body.to_string().as_bytes());
        }
        // Edit the wallpaper on screen from now on; the page reloads after
        ("POST", "/switch") => {
            let body = match on_screen() {
                Some((wallpaper, key)) => {
                    println!("cavawall-tune: now editing {}", wallpaper.display());
                    {
                        let mut s = server.write().unwrap_or_else(PoisonError::into_inner);
                        s.wallpaper = wallpaper;
                        s.key.clone_from(&key);
                    }
                    serde_json::json!({ "ok": true, "key": key })
                }
                None => serde_json::json!({ "ok": false, "error": "cannot tell which wallpaper is on screen" }),
            };
            reply(&mut out, "200 OK", "application/json", body.to_string().as_bytes());
        }
        _ => reply(&mut out, "404 Not Found", "text/plain", b"no"),
    }
}

/// The wallpaper on screen and its content key. The key hashes the whole
/// image and the page asks every few seconds, so it is kept until the path,
/// size or mtime changes
fn on_screen() -> Option<(PathBuf, String)> {
    static LAST: Mutex<Option<(PathBuf, u64, SystemTime, String)>> = Mutex::new(None);
    let path = curve::current_wallpaper().filter(|p| p.is_file())?;
    let meta = std::fs::metadata(&path).ok()?;
    let (len, mtime) = (meta.len(), meta.modified().ok()?);
    let lock = || LAST.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((p, l, m, key)) = lock().as_ref()
        && *p == path
        && *l == len
        && *m == mtime
    {
        return Some((path, key.clone()));
    }
    // Hashed unlocked, so a slow image never holds up another request
    let key = curve::content_key(&path)?;
    *lock() = Some((path.clone(), len, mtime, key.clone()));
    Some((path, key))
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
const fn sniff(bytes: &[u8]) -> Option<&'static str> {
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
        "config": std::fs::read_to_string(config_dir().join("config.toml"))
            .ok()
            .and_then(|s| toml::from_str::<toml::Value>(&s).ok()),
        "scheme": scheme::colours(),
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

/// The tables the page may edit, so a request cannot write anything else
const GLOBAL_TABLES: [&str; 7] = ["general", "smoothing", "colors", "scheme", "bars", "notify", "wallpaper"];

fn json_to_value(v: &serde_json::Value) -> Option<toml_edit::Value> {
    use serde_json::Value as J;
    Some(match v {
        J::Bool(b) => (*b).into(),
        J::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64()?.into(),
        },
        J::String(s) => s.as_str().into(),
        J::Array(a) => toml_edit::Value::Array(a.iter().map(json_to_value).collect::<Option<_>>()?),
        J::Object(o) => {
            // A colour stop reads role, hex, alpha, as the example config
            // writes it; anything else follows in name order
            let rank = |k: &str| ["role", "hex", "alpha"].iter().position(|r| *r == k).unwrap_or(3);
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by_key(|k| (rank(k), k.as_str()));
            let mut t = toml_edit::InlineTable::new();
            for k in keys {
                if !o[k].is_null() {
                    t.insert(k, json_to_value(&o[k])?);
                }
            }
            toml_edit::Value::InlineTable(t)
        }
        J::Null => return None,
    })
}

/// Apply `{"set": [[table, key, value|null], ...], "reload": bool}` to
/// config.toml in place: untouched lines, comments included, stay as they
/// are. The result must parse as cavawall's own config before it is written
fn edit_global(body: &[u8]) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Edit {
        set: Vec<(String, String, serde_json::Value)>,
        #[serde(default)]
        reload: bool,
    }
    let edit: Edit = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let path = config_dir().join("config.toml");
    // Through the link to the real file: a stowed config.toml is a symlink,
    // and renaming over it would replace the link with a copy
    let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let text = std::fs::read_to_string(&real).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    for (table, key, value) in &edit.set {
        if !GLOBAL_TABLES.contains(&table.as_str()) {
            return Err(format!("[{table}] is not editable here"));
        }
        if value.is_null() {
            if let Some(t) = doc.get_mut(table).and_then(|t| t.as_table_like_mut()) {
                t.remove(key);
            }
            continue;
        }
        let v = json_to_value(value).ok_or_else(|| format!("{table}.{key}: unsupported value"))?;
        if doc.get(table).is_none() {
            doc.insert(table, toml_edit::table());
        }
        let t = doc[table.as_str()].as_table_like_mut().ok_or_else(|| format!("[{table}] is not a table"))?;
        match t.get_mut(key).and_then(|i| i.as_value_mut()) {
            // Keeps the key's own spacing and trailing comment
            Some(old) => {
                let decor = old.decor().clone();
                *old = v;
                *old.decor_mut() = decor;
            }
            None => {
                t.insert(key, toml_edit::Item::Value(v));
            }
        }
    }
    // A table the page emptied goes too, unless something is written in it
    for (table, _, value) in &edit.set {
        let empty = value.is_null()
            && doc.get(table).and_then(|t| t.as_table()).is_some_and(|t| {
                t.is_empty() && t.decor().prefix().and_then(|p| p.as_str()).is_none_or(|p| !p.contains('#'))
            });
        if empty {
            doc.remove(table);
        }
    }
    let new = doc.to_string();
    toml::from_str::<Config>(&new).map_err(|e| format!("the result would not load: {e}"))?;
    let dir = real.parent().ok_or("config.toml has no directory")?;
    let tmp = dir.join(".config.toml.tune");
    std::fs::write(&tmp, &new)
        .and_then(|()| std::fs::rename(&tmp, &real))
        .map_err(|e| format!("{}: {e}", real.display()))?;
    println!("cavawall-tune: wrote {}", real.display());
    if !edit.reload {
        return Ok("written".to_owned());
    }
    Ok(match control::request(&Request::Reload) {
        Ok(r) if r.ok => "reloaded".to_owned(),
        Ok(r) => format!("not reloaded: {}", r.error.unwrap_or_default()),
        Err(_) => "written; cavawall is not running".to_owned(),
    })
}

/// Capture sources the sound server offers, for the page's source list.
/// Empty when there is no pactl or no server
fn audio_sources() -> Vec<String> {
    let Ok(out) = Command::new("pactl").args(["list", "short", "sources"]).stderr(Stdio::null()).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split('\t').nth(1).map(str::to_owned))
        .collect()
}

/// Store this wallpaper's settings, replacing its file, then apply them
///
/// A running instance re-execs in place, which keeps its pid and its
/// environment - a `cavawall move` pin included. Only when none is
/// running is one started, through `cavawall start`, detached
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
            let how = if wedged { "restart" } else { "start" };
            match detached(Command::new(cavawall::helper("cavawallctl")).arg(how)) {
                Ok(_) if wedged => "restarted".to_owned(),
                Ok(_) => "started".to_owned(),
                Err(e) => format!("not started: {e}"),
            }
        }
    };
    Ok((WallpaperConfig::path(&dir, key), applied))
}

