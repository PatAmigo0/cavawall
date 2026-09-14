//! Reading Caelestia's live state directly, instead of having an external
//! script rewrite this program's config file.
//!
//! What this replaces: a python script that mapped the scheme onto eight hex
//! values, rewrote the `[colors]` block of config.toml in place, and restarted
//! the process so it would re-read it. That worked, but the config file it
//! rewrote is a stowed, version-controlled file, so every wallpaper change
//! produced a diff in a file whose remaining content is hand-written prose -
//! and the restart had to be gated on whether a fullscreen watcher had
//! deliberately stopped the visualiser, because relaunching it on top of a game
//! was exactly the thing that watcher existed to prevent.
//!
//! Reading the scheme here removes both problems at once: the config file is
//! never written, and colours are re-uploaded to the GPU in place, so there is
//! no restart to get wrong.
//!
//! Nothing here is required. Every function returns an Option and every failure
//! path - no Caelestia, no scheme yet, a half-written file, a renamed key -
//! leaves the static configuration in force.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const SCHEME_FILE: &str = "scheme.json";
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

/// Directory holding scheme.json. Watched rather than the file itself: the
/// writer may replace the inode rather than truncate it, and a watch on the old
/// inode would then go quiet forever while looking perfectly healthy.
///
/// Resolved once: every call otherwise re-read the environment, which takes a
/// process-wide lock and allocates, for a path that cannot change.
pub fn scheme_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| state_dir().join("caelestia"))
}

/// Directory holding shell.json, Caelestia's own settings file.
pub fn shell_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| config_dir().join("caelestia"))
}

/// Role name -> bare `rrggbb`, as Caelestia writes it (no leading `#`).
pub fn colours() -> Option<HashMap<String, String>> {
    let raw = std::fs::read_to_string(scheme_dir().join(SCHEME_FILE)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let serde_json::Value::Object(mut root) = parsed else {
        return None;
    };
    let serde_json::Value::Object(obj) = root.remove("colours")? else {
        return None;
    };
    // Moved out of the parsed document, not copied: the keys and hex values
    // are already owned, so cloning each duplicated the whole palette per
    // wallpaper change. Non-string values are still skipped, not rejected.
    Some(
        obj.into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(hex) => Some((k, hex)),
                _ => None,
            })
            .collect(),
    )
}

/// `services.visualiserBars` from Caelestia's shell.json.
///
/// Clamped rather than trusted: the value drives an index buffer and cava's bar
/// count, and a zero would divide by zero in the bar-width maths. The ceiling is
/// well above anything the settings UI offers and only exists so a corrupt file
/// cannot ask for a gigabyte of indices.
pub fn bar_count() -> Option<u32> {
    /// Absent `services` is not a failure - it just means nothing to follow.
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
    // Naming only the field of interest lets serde walk past the rest without
    // building a `Value` tree. Caelestia rewrites shell.json on every settings
    // change and this runs on each one.
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

/// A non-blocking inotify watch on the scheme directory.
///
/// Non-blocking on purpose: this is polled from the render loop's timeout
/// callback, which must never stall waiting on a file that may not change for
/// hours. The cost of a poll is one `read` returning EAGAIN.
pub struct Watch {
    fd: RawFd,
    file: &'static str,
    /// Owned rather than a local in `take_event`, which runs per watch per
    /// frame: a zero-initialised 4 KiB local is a 4 KiB memset each time.
    buf: Box<EventBuf>,
}

/// Backing store for the inotify queue, with its alignment pinned.
///
/// Events are read out of it through a `&inotify_event`, which needs 4-byte
/// alignment; a bare `[u8; N]` guarantees none, and an under-aligned reference
/// is UB even where the load would work. The kernel pads each record, so
/// pinning the base covers the whole walk.
#[repr(C, align(8))]
struct EventBuf([u8; 4096]);

impl Watch {
    /// None when there is nothing to watch - the directory does not exist (no
    /// Caelestia), or inotify is unavailable. The caller carries on with
    /// whatever the config file said.
    pub fn new(dir: &Path, file: &'static str) -> Option<Self> {
        if !dir.is_dir() {
            return None;
        }
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return None;
        }
        let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
        // IN_MOVED_TO as well as IN_CLOSE_WRITE: a write-then-rename produces
        // only the former, and which one a writer uses is not ours to decide.
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO;
        if unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) } < 0 {
            unsafe { libc::close(fd) };
            return None;
        }
        Some(Self { fd, file, buf: Box::new(EventBuf([0; 4096])) })
    }

    /// Drain every queued event and report whether any of them touched the file
    /// this watch cares about.
    ///
    /// Draining fully in one call is the point: a write-then-rename delivers two
    /// events for one logical change, and reacting to each in turn would
    /// re-upload the same palette twice.
    pub fn take_event(&mut self) -> bool {
        const HDR: usize = std::mem::size_of::<libc::inotify_event>();
        let buf = &mut self.buf.0;
        let mut hit = false;
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                // EAGAIN: queue empty, which is the normal case every frame.
                return hit;
            }
            let n = n as usize;
            let mut off = 0usize;
            while off + HDR <= n {
                // SAFETY: the kernel just wrote `n` bytes, of which
                // `off .. off + HDR` is a whole header; `EventBuf` pins the
                // base alignment and inotify pads each record, so `off` stays
                // a multiple of it.
                let ev = unsafe { &*buf.as_ptr().add(off).cast::<libc::inotify_event>() };
                let start = off + HDR;
                // Clamped to what was read, not to the buffer: a truncated
                // record must not expose bytes from an earlier one.
                let end = (start + ev.len as usize).min(n);
                if start < end {
                    // The name is NUL-padded out to the record length.
                    let name = buf[start..end].split(|b| *b == 0).next().unwrap_or(&[]);
                    if name == self.file.as_bytes() {
                        hit = true;
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
