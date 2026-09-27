//! The shell's live state, read directly. config.toml is never written and
//! colours reach the GPU in place, so there is no restart
//!
//! Every function returns an Option. No scheme source, no scheme yet, a
//! half-written file or a renamed key all leave the static config in force

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const SHELL_FILE: &str = "shell.json";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

/// The live palette's file: Caelestia's scheme.json unless `[scheme]` names
/// another source. Watched through its directory, not the file itself: a
/// writer that replaces the inode leaves a watch on the file silently dead
static PALETTE: OnceLock<PathBuf> = OnceLock::new();

/// Adopt `[scheme] source` and `path`. Once, before anything reads colours
pub fn configure_colours(source: Option<&str>, path: Option<&str>) {
    let file = match (source.unwrap_or("caelestia"), path) {
        (_, Some(p)) => crate::wallpaper::expand(p),
        ("pywal", None) => std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".cache"))
            .join("wal/colors.json"),
        ("caelestia", None) => state_dir().join("caelestia/scheme.json"),
        (other, None) => {
            eprintln!("cavawall: [scheme] source {other:?} needs a path; using Caelestia's palette");
            state_dir().join("caelestia/scheme.json")
        }
    };
    let _ = PALETTE.set(file);
}

fn palette() -> &'static Path {
    PALETTE.get_or_init(|| state_dir().join("caelestia/scheme.json"))
}

/// Directory holding shell.json, the shell's own settings file
pub fn shell_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| config_dir().join("caelestia"))
}

/// Role name -> bare `rrggbb`, from any palette JSON: every colour-valued
/// string is a role named by its own key, however deep it sits - Caelestia's
/// `colours.primary`, pywal's `colors.color4` and `special.background`, or a
/// matugen template's layout. The first of two same-named keys wins
#[must_use]
pub fn colours() -> Option<HashMap<String, String>> {
    parse_palette(&std::fs::read_to_string(palette()).ok()?)
}

/// `colours` over a document already read
#[must_use]
pub fn parse_palette(raw: &str) -> Option<HashMap<String, String>> {
    fn walk(v: serde_json::Value, key: Option<String>, out: &mut HashMap<String, String>) {
        match v {
            serde_json::Value::Object(map) => map.into_iter().for_each(|(k, v)| walk(v, Some(k), out)),
            serde_json::Value::String(s) => {
                let hex = s.trim_start_matches('#');
                if let Some(k) = key.filter(|_| hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit())) {
                    out.entry(k).or_insert_with(|| hex.to_owned());
                }
            }
            _ => {}
        }
    }
    let mut out = HashMap::new();
    walk(serde_json::from_str(raw).ok()?, None, &mut out);
    (!out.is_empty()).then_some(out)
}

/// `services.visualiserBars` from the shell's shell.json
///
/// Clamped: the value sizes an index buffer, and zero divides by zero in the
/// bar-width maths
#[must_use]
pub fn bar_count() -> Option<u32> {
    /// Absent `services` means nothing to follow, not a failure
    #[derive(serde::Deserialize)]
    struct Shell {
        #[serde(default)]
        services: Services,
    }

    #[derive(serde::Deserialize, Default)]
    struct Services {
        #[serde(rename = "visualiserBars")]
        visualiser_bars: Option<u64>,
    }

    let raw = std::fs::read_to_string(shell_dir().join(SHELL_FILE)).ok()?;
    // Naming one field lets serde walk past the rest without building a
    // `Value` tree. The shell rewrites shell.json on every settings change
    let n = serde_json::from_str::<Shell>(&raw)
        .ok()?
        .services
        .visualiser_bars?;
    if (1..=512).contains(&n) {
        Some(n as u32)
    } else {
        None
    }
}

/// A non-blocking inotify watch over both files cavawall follows
///
/// One inotify instance carries every watch descriptor, so a frame costs one
/// `read` returning EAGAIN. Non-blocking: this is polled from the render loop
pub struct Watch {
    fd: RawFd,
    /// -1 when that file is not being followed
    scheme_wd: i32,
    shell_wd: i32,
    wallpaper_wd: i32,
    /// The one file in the wallpaper directory that matters; None when any
    /// change there does (swww's cache)
    wallpaper_name: Option<String>,
    /// Owned, not a local in `take`: that runs once per frame and a
    /// zero-initialised 4 KiB local is a 4 KiB memset each time
    buf: Box<EventBuf>,
}

/// Which of the watched files changed since the last drain
///
/// All answered at once: draining is destructive, so a second question asked
/// separately always comes back false
#[derive(Default, Clone, Copy)]
pub struct Changed {
    pub scheme: bool,
    pub shell: bool,
    pub wallpaper: bool,
}

/// Backing store for the inotify queue, with its alignment pinned
///
/// Events are read out of it through a `&inotify_event`, which needs 4-byte
/// alignment; a bare `[u8; N]` guarantees none, and an under-aligned reference
/// is UB even where the load would work. The kernel pads each record, so
/// pinning the base covers the whole walk
#[repr(C, align(8))]
struct EventBuf([u8; 4096]);

impl Watch {
    /// None when nothing was asked for, or nothing could be watched - no
    /// scheme source, or inotify unavailable. The caller carries on with whatever
    /// the config file said
    ///
    /// Directories are watched rather than the files themselves, so a name
    /// check is still needed on each event; see `take`.
    #[must_use]
    pub fn new(scheme: bool, shell: bool, wallpaper: bool) -> Option<Self> {
        if !scheme && !shell && !wallpaper {
            return None;
        }
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return None;
        }
        let mut w = Self {
            fd,
            scheme_wd: -1,
            shell_wd: -1,
            wallpaper_wd: -1,
            wallpaper_name: None,
            buf: Box::new(EventBuf([0; 4096])),
        };
        if scheme {
            if let Some(dir) = palette().parent() {
                w.scheme_wd = w.add(dir);
            }
        }
        if shell {
            w.shell_wd = w.add(shell_dir());
        }
        if wallpaper {
            if let Some((dir, name)) = crate::wallpaper::watch_target() {
                w.wallpaper_wd = w.add(&dir);
                w.wallpaper_name = name;
            }
        }
        if w.scheme_wd < 0 && w.shell_wd < 0 && w.wallpaper_wd < 0 {
            return None; // Drop closes the fd
        }
        Some(w)
    }

    /// A second descriptor on the same inotify instance, for an event loop to
    /// poll while this one does the reading. Close-on-exec, like the first
    #[must_use]
    pub fn event_fd(&self) -> Option<std::os::fd::OwnedFd> {
        // SAFETY: fcntl on a descriptor this struct owns; the result is fresh
        let fd = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        // SAFETY: a new descriptor that nothing else owns
        (fd >= 0).then(|| unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
    }

    /// -1 if the directory is missing or the watch could not be added
    fn add(&self, dir: &Path) -> i32 {
        if !dir.is_dir() {
            return -1;
        }
        let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
            return -1;
        };
        // IN_MOVED_TO as well as IN_CLOSE_WRITE: a write-then-rename produces
        // only the former, and which one a writer uses is not ours to decide
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO;
        unsafe { libc::inotify_add_watch(self.fd, path.as_ptr(), mask) }
    }

    /// Drain every queued event and report which watched files they touched
    ///
    /// Draining fully in one call is the point: a write-then-rename delivers two
    /// events for one logical change, and reacting to each in turn would
    /// re-upload the same palette twice
    pub fn take(&mut self) -> Changed {
        const HDR: usize = std::mem::size_of::<libc::inotify_event>();
        let (scheme_wd, shell_wd, wallpaper_wd) =
            (self.scheme_wd, self.shell_wd, self.wallpaper_wd);
        let palette_name = palette().file_name().map(std::ffi::OsStr::as_bytes).unwrap_or_default();
        let wallpaper_name = self.wallpaper_name.as_deref().map(str::as_bytes);
        let buf = &mut self.buf.0;
        let mut hit = Changed::default();
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                // EAGAIN: queue empty, which is the normal case every frame
                return hit;
            }
            let n = n as usize;
            let mut off = 0usize;
            while off + HDR <= n {
                // SAFETY: the kernel just wrote `n` bytes, of which
                // `off .. off + HDR` is a whole header; `EventBuf` pins the
                // base alignment and inotify pads each record, so `off` stays
                // a multiple of it
                let ev = unsafe { &*buf.as_ptr().add(off).cast::<libc::inotify_event>() };
                let start = off + HDR;
                // Clamped to what was read, not to the buffer: a truncated
                // record must not expose bytes from an earlier one
                let end = (start + ev.len as usize).min(n);
                if start < end {
                    // The name is NUL-padded out to the record length. Matched
                    // against the descriptor as well, so a shell.json dropped
                    // into the scheme directory cannot pass for the real one
                    let name = buf[start..end].split(|b| *b == 0).next().unwrap_or(&[]);
                    if ev.wd == scheme_wd && name == palette_name {
                        hit.scheme = true;
                    }
                    if ev.wd == shell_wd && name == SHELL_FILE.as_bytes() {
                        hit.shell = true;
                    }
                    if ev.wd == wallpaper_wd && wallpaper_name.is_none_or(|w| name == w) {
                        hit.wallpaper = true;
                    }
                }
                off = start + ev.len as usize;
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::parse_palette;

    #[test]
    fn caelestia_roles_come_from_colours() {
        let p = parse_palette(r##"{"name":"x","mode":"dark","colours":{"primary":"ffb4a8","mauve":"#cba6f7"}}"##).expect("parses");
        assert_eq!(p["primary"], "ffb4a8");
        assert_eq!(p["mauve"], "cba6f7");
        assert!(!p.contains_key("mode") && !p.contains_key("name"), "non-colours are skipped");
    }

    #[test]
    fn pywal_colours_and_specials_are_roles() {
        let p = parse_palette(r##"{"wallpaper":"/a.jpg","special":{"background":"#0f1115","foreground":"#e6e1cf"},"colors":{"color0":"#0f1115","color4":"#5a8cd6"}}"##).expect("parses");
        assert_eq!(p["color4"], "5a8cd6");
        assert_eq!(p["background"], "0f1115");
        assert!(!p.contains_key("wallpaper"));
    }

    #[test]
    fn nothing_colour_valued_is_no_palette() {
        assert!(parse_palette(r#"{"a":"b"}"#).is_none());
        assert!(parse_palette("not json").is_none());
    }
}
