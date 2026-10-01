//! A wallpaper with cavawall turned off: no surface, no cava and no GL, so
//! nothing to draw, capture or hold in GPU memory. What is left waits on the
//! wallpaper, the control socket and signals, and runs in full again once a
//! wallpaper that wants bars is on screen

use super::*;

struct Dormant {
    config_path: PathBuf,
    config_dir: PathBuf,
    /// The wallpaper that is off; None when the source cannot say which
    key: Option<String>,
    watch: Option<scheme::Watch>,
    pinned_output: Option<String>,
}

impl Dormant {
    /// The wallpaper changed, or `refresh` says it may have: stay off while
    /// the new one is off too, else run in full
    fn recheck(&mut self) {
        let key = curve::current_wallpaper().and_then(|w| curve::content_key(&w));
        if key == self.key {
            return;
        }
        let off = key.as_deref().is_some_and(|k| {
            WallpaperConfig::load(&self.config_dir, k).ok().flatten().is_some_and(|w| w.is_disabled())
        });
        if off {
            say!("off on this wallpaper too ({})", key.as_deref().unwrap_or_default());
            self.key = key;
            return;
        }
        Self::wake(Pin::Keep);
    }

    /// Run in full again: same pid, same environment. Returns only if the
    /// exec failed, and stays off then
    fn wake(pin: Pin) {
        let err = exec_self(pin, &[]);
        say!("cannot start drawing again ({err}); staying off");
    }

    fn stop(why: &str) -> ! {
        say!("stopping: {why}");
        cavawall::log::exited(why);
        cavawall::notify::send(NotifyEvent::Stop, &format!("stopped: {why}"));
        control::unbind();
        exit(0);
    }

    /// The full instance's protocol, answered from here. Orders that replace
    /// or end the process do not return
    fn serve_control(&mut self, stream: &std::os::unix::net::UnixStream) {
        use control::{Request, Response};
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(control::SERVE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(control::SERVE_TIMEOUT));
        let Some(req) = control::read_request(stream) else {
            control::write_response(stream, &Response::err("unparseable request"));
            return;
        };
        // Answer before acting: an exec never comes back to write one
        control::write_response(stream, &match req {
            Request::Status => Response::ok(Some(serde_json::json!({
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"),
                "exe": std::env::current_exe().ok().map(|p| p.display().to_string()),
                "config": self.config_path.display().to_string(),
                "disabled": true,
                "pinned_output": self.pinned_output,
                "curve_key": self.key,
            }))),
            _ => Response::ok(None),
        });
        match req {
            Request::Status => {}
            Request::Move { output } => Self::wake(output.as_deref().map_or(Pin::Clear, Pin::Set)),
            Request::Refresh => self.recheck(),
            Request::Stop => Self::stop("asked to stop"),
            Request::Reload => Self::wake(Pin::Keep),
        }
    }
}

/// Wait, off, until the wallpaper or an order says otherwise. Never returns
pub(crate) fn run(config_path: PathBuf, config_dir: PathBuf, key: Option<String>, pinned_output: Option<String>) -> ! {
    say!("off on this wallpaper ({}); no bars and no cava until another one is on screen", key.as_deref().unwrap_or("unknown"));
    // Held only so this ends with the compositor, as the full instance does
    let conn = Connection::connect_to_env()
        .unwrap_or_else(|e| fatal!("cannot reach a Wayland compositor ({e}); is WAYLAND_DISPLAY set?"));
    let queue = conn.new_event_queue::<Dormant>();
    let mut event_loop: EventLoop<Dormant> =
        EventLoop::try_new().unwrap_or_else(|e| fatal!("cannot start the event loop: {e}"));
    let handle = event_loop.handle();
    let mut state = Dormant { config_path, config_dir, key, watch: scheme::Watch::new(false, false, true), pinned_output };
    match control::bind() {
        Ok(listener) => {
            let _ = handle.insert_source(Generic::new(listener, Interest::READ, CalloopMode::Level), |_, listener, state| {
                while let Ok((stream, _)) = listener.accept() {
                    state.serve_control(&stream);
                }
                Ok(PostAction::Continue)
            });
        }
        Err(e) => say!("no control socket: {e}"),
    }
    if let Some(fd) = state.watch.as_ref().and_then(scheme::Watch::event_fd) {
        let _ = handle.insert_source(Generic::new(fd, Interest::READ, CalloopMode::Level), |_, _, state| {
            if state.watch.as_mut().map(scheme::Watch::take).is_some_and(|c| c.wallpaper) {
                state.recheck();
            }
            Ok(PostAction::Continue)
        });
    }
    // SAFETY: eventfd returns a fresh descriptor or -1, checked below
    let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake >= 0 {
        WAKE.store(wake, Ordering::Relaxed);
        // SAFETY: just created, and owned by nothing else
        let wake = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(wake) };
        let _ = handle.insert_source(Generic::new(wake, Interest::READ, CalloopMode::Level), |_, _, _| {
            Dormant::stop("SIGTERM or SIGINT")
        });
    }
    if let Err(e) = WaylandSource::new(conn, queue).insert(handle) {
        fatal!("cannot watch the compositor connection: {e}");
    }
    // SAFETY: malloc_trim only walks glibc's own free lists
    unsafe { libc::malloc_trim(0) };
    let ticked = event_loop.run(None, &mut state, |_| {
        if EXITING.load(Ordering::Relaxed) {
            Dormant::stop("SIGTERM or SIGINT");
        }
    });
    if let Err(e) = ticked {
        fatal!("the event loop stopped: {e}; the compositor connection most likely closed");
    }
    exit(0)
}
