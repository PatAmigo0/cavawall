//! A log file for the times nobody is watching stderr
//!
//! The session's instance is started by a launcher with no terminal, so a
//! message printed there is lost. Every line cavawall prints also lands in
//! `$XDG_STATE_HOME/cavawall/cavawall.log`, and the reason the last instance
//! stopped - a stop, a signal, a fatal error, a panic, a crash - in
//! `last-exit`. `cavawall log` reads both. Only rare events are written:
//! nothing here runs per frame

use std::fs::{self, OpenOptions};
use std::os::fd::IntoRawFd as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

/// Past this the log is moved to `cavawall.log.1` at startup, so it holds
/// the current run and the previous stretch, never growing without bound
const ROTATE_AT: u64 = 256 * 1024;

/// The open log, as a raw descriptor so the fatal-signal handler can write to
/// it with nothing but write(2)
static FD: AtomicI32 = AtomicI32::new(-1);

/// Set once a panic has recorded itself: `panic = "abort"` follows every
/// panic with SIGABRT, whose bare "aborted" must not replace the message
static PANICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("cavawall")
}

pub fn path() -> PathBuf {
    dir().join("cavawall.log")
}

pub fn last_exit_path() -> PathBuf {
    dir().join("last-exit")
}

/// Local wall-clock time, `2026-09-28 00:25:03`
fn now() -> String {
    // SAFETY: time and localtime_r only fill the structs they are handed
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// Open the log for this run and record how a crash would end it. Called
/// once, first thing; a log that will not open costs the log, never the run
pub fn init() {
    prepare_crash_record();
    let dir = dir();
    let _ = fs::create_dir_all(&dir);
    let path = path();
    if fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_AT) {
        let _ = fs::rename(&path, dir.join("cavawall.log.1"));
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        FD.store(f.into_raw_fd(), Ordering::Relaxed);
    }
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown".to_owned());
        let msg = format!("panic{place}: {what}");
        say(&msg);
        exited(&msg);
        PANICKED.store(true, Ordering::Relaxed);
    }));
    for sig in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT] {
        // SAFETY: the handler does only async-signal-safe work
        unsafe { libc::signal(sig, on_fatal_signal as *const () as libc::sighandler_t) };
    }
}

/// One line to stderr and to the log
pub fn say(msg: &str) {
    eprintln!("cavawall: {msg}");
    let fd = FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let line = format!("{} [{}] {msg}\n", now(), std::process::id());
        // SAFETY: fd is the log, open for the life of the process
        unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
    }
}

/// Why this instance stopped, for `cavawall log last-exit`. Overwrites
pub fn exited(reason: &str) {
    let line = format!("{} pid {}: {reason}\n", now(), std::process::id());
    let _ = fs::write(last_exit_path(), line);
}

/// Say it, record it as the reason, exit 1
pub fn fatal(msg: &str) -> ! {
    say(msg);
    exited(msg);
    std::process::exit(1);
}

/// A crash in native code, a driver's included. Only write(2) and open(2)
/// here, both async-signal-safe; then the default action, so the core dump
/// and the exit status are what they would have been
extern "C" fn on_fatal_signal(sig: libc::c_int) {
    let msg: &[u8] = match sig {
        libc::SIGSEGV => b"crashed: SIGSEGV (invalid memory access)\n",
        libc::SIGBUS => b"crashed: SIGBUS (bad memory access)\n",
        libc::SIGILL => b"crashed: SIGILL (illegal instruction)\n",
        libc::SIGFPE => b"crashed: SIGFPE (arithmetic fault)\n",
        _ => b"crashed: SIGABRT (aborted)\n",
    };
    // SAFETY: async-signal-safe calls only
    unsafe {
        let fd = FD.load(Ordering::Relaxed);
        if fd >= 0 {
            libc::write(fd, msg.as_ptr().cast(), msg.len());
        }
        libc::write(2, b"cavawall: ".as_ptr().cast(), 10);
        libc::write(2, msg.as_ptr().cast(), msg.len());
        let mut path = [0u8; 4096];
        let after_panic = sig == libc::SIGABRT && PANICKED.load(Ordering::Relaxed);
        if let Some(p) = LAST_EXIT.get().filter(|_| !after_panic) {
            let n = p.len().min(path.len() - 1);
            path[..n].copy_from_slice(&p[..n]);
            let f = libc::open(path.as_ptr().cast(), libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644);
            if f >= 0 {
                libc::write(f, msg.as_ptr().cast(), msg.len());
                libc::close(f);
            }
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// The last-exit path as bytes, resolved before any crash can need it: the
/// signal handler cannot allocate or read the environment
static LAST_EXIT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// Resolve what the crash handler needs, while allocating is still allowed
pub fn prepare_crash_record() {
    let _ = LAST_EXIT.set(last_exit_path().into_os_string().into_encoded_bytes());
}

/// The last `n` lines of the log
pub fn tail(n: usize) -> std::io::Result<String> {
    let text = fs::read_to_string(path())?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = String::new();
    for l in &lines[start..] {
        let _ = writeln!(out, "{l}");
    }
    Ok(out)
}

use std::fmt::Write as _;

/// `say!` with format arguments
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => { $crate::log::say(&format!($($arg)*)) };
}

/// `fatal!` with format arguments
#[macro_export]
macro_rules! fatal {
    ($($arg:tt)*) => { $crate::log::fatal(&format!($($arg)*)) };
}
