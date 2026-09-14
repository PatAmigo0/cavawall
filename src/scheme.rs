//! Reading Caelestia's live state directly, instead of having an external
//! script rewrite this program's config file.
//!
//! What this replaces: a python script that mapped the scheme onto eight hex
//! values, rewrote the `[colors]` block of config.toml in place, and restarted
//! the process so it would re-read it. That worked, but the config file it
//! rewrote is a stowed, version-controlled file, so every wallpaper change
//! produced a diff in a file whose remaining content is hand-written prose --
//! and the restart had to be gated on whether a fullscreen watcher had
//! deliberately stopped the visualiser, because relaunching it on top of a game
//! was exactly the thing that watcher existed to prevent.
//!
//! Reading the scheme here removes both problems at once: the config file is
//! never written, and colours are re-uploaded to the GPU in place, so there is
//! no restart to get wrong.
//!
//! Nothing here is required. Every function returns an Option and every failure
//! path -- no Caelestia, no scheme yet, a half-written file, a renamed key --
//! leaves the static configuration in force.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

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
pub fn scheme_dir() -> PathBuf {
    state_dir().join("caelestia")
}

/// Directory holding shell.json, Caelestia's own settings file.
pub fn shell_dir() -> PathBuf {
    config_dir().join("caelestia")
}

/// Role name -> bare `rrggbb`, as Caelestia writes it (no leading `#`).
pub fn colours() -> Option<HashMap<String, String>> {
    let raw = std::fs::read_to_string(scheme_dir().join(SCHEME_FILE)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let obj = parsed.get("colours")?.as_object()?;
    Some(
        obj.iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
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
    let raw = std::fs::read_to_string(shell_dir().join(SHELL_FILE)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let n = parsed.get("services")?.get("visualiserBars")?.as_u64()?;
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
}

impl Watch {
    /// None when there is nothing to watch -- the directory does not exist (no
    /// Caelestia), or inotify is unavailable. The caller carries on with
    /// whatever the config file said.
    pub fn new(dir: PathBuf, file: &'static str) -> Option<Self> {
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
        Some(Self { fd, file })
    }

    /// Drain every queued event and report whether any of them touched the file
    /// this watch cares about.
    ///
    /// Draining fully in one call is the point: a write-then-rename delivers two
    /// events for one logical change, and reacting to each in turn would
    /// re-upload the same palette twice.
    pub fn take_event(&self) -> bool {
        let mut hit = false;
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                // EAGAIN: queue empty, which is the normal case every frame.
                return hit;
            }
            let mut off = 0usize;
            let hdr = std::mem::size_of::<libc::inotify_event>();
            while off + hdr <= n as usize {
                let ev = unsafe { &*(buf.as_ptr().add(off) as *const libc::inotify_event) };
                let len = ev.len as usize;
                if len > 0 {
                    let start = off + hdr;
                    let raw = &buf[start..(start + len).min(buf.len())];
                    let name = raw.split(|b| *b == 0).next().unwrap_or(&[]);
                    if name == self.file.as_bytes() {
                        hit = true;
                    }
                }
                off += hdr + len;
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}
