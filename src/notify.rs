//! Desktop notifications for the events worth interrupting someone for
//!
//! Sent by a detached child - notify-send, hyprctl or the user's command -
//! reaped on a short-lived thread, so the loop never waits on one. Only rare
//! events come here: a start, a stop, an error, a crash

use crate::app_config::{NotifyConfig, NotifyEvent, NotifyVia};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

struct Settings {
    enabled: bool,
    via: NotifyVia,
    command: Vec<String>,
    events: Vec<NotifyEvent>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Errors and crashes, sent the automatic way: what applies before the
/// config is read, and when it has no `[notify]`
const DEFAULT_EVENTS: [NotifyEvent; 2] = [NotifyEvent::Error, NotifyEvent::Crash];

/// Adopt the config's `[notify]`. Once, after the config is read; anything
/// sent before that uses the defaults
pub fn configure(cfg: Option<&NotifyConfig>) {
    let _ = SETTINGS.set(Settings {
        enabled: cfg.and_then(|c| c.enabled).unwrap_or(true),
        via: cfg.and_then(|c| c.via).unwrap_or_default(),
        command: cfg.and_then(|c| c.command.clone()).unwrap_or_default(),
        events: cfg.and_then(|c| c.events.clone()).unwrap_or_else(|| DEFAULT_EVENTS.to_vec()),
    });
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// Raise `body` for `event`, if the config wants it
pub fn send(event: NotifyEvent, body: &str) {
    let (enabled, via, command, wanted) = match SETTINGS.get() {
        Some(s) => (s.enabled, s.via, s.command.as_slice(), s.events.contains(&event)),
        None => (true, NotifyVia::Auto, &[][..], DEFAULT_EVENTS.contains(&event)),
    };
    if !enabled || !wanted {
        return;
    }
    let urgent = matches!(event, NotifyEvent::Error | NotifyEvent::Crash);
    let via = match via {
        NotifyVia::Auto if on_path("notify-send") => NotifyVia::Dbus,
        NotifyVia::Auto if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() => NotifyVia::Hyprland,
        NotifyVia::Auto => return,
        v => v,
    };
    let mut cmd = match via {
        NotifyVia::Dbus => {
            let mut c = Command::new("notify-send");
            c.args(["-a", "cavawall", "-u", if urgent { "critical" } else { "normal" }, "cavawall", body]);
            c
        }
        NotifyVia::Hyprland => {
            // Icons: 3 error, 1 info
            let mut c = Command::new("hyprctl");
            c.args(["notify", if urgent { "3" } else { "1" }, "6000", "0", &format!("cavawall: {body}")]);
            c
        }
        NotifyVia::Command => {
            let Some((program, args)) = command.split_first() else { return };
            let mut c = Command::new(program);
            c.args(args).arg(body);
            c
        }
        NotifyVia::Auto => return,
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe; the child leaves our session so a
    // stop or a crash here never takes it down mid-send
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
