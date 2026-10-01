//! Query and control a running cavawall over its control socket.

use cavawall::app_config::{self, WallpaperConfig};
use cavawall::control::{request, Request, Response};
use cavawall::curve;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::Shell;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};
use std::process::exit;

#[derive(Parser)]
#[command(name = "cavawallctl", about = "Query and control a running cavawall")]
struct Cli {
    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// What the running instance is doing
    Status,
    /// Move to an output, or omit the name for automatic placement
    Move { output: Option<String> },
    /// Clear the surface and exit
    Stop,
    /// Re-read config; the instance runs again in place, keeping its pid
    Reload,
    /// Look again at the wallpaper and palette, for sources cavawall cannot watch
    Refresh,
    /// Start one if none is running
    Start,
    /// Stop whatever is running and start again, picking up a new binary
    Restart,
    /// SIGKILL the instance named by the lock, for one that stopped answering
    Kill,
    /// Open the editor for the current wallpaper
    Tune,
    /// Turn cavawall off while this wallpaper is on screen: no bars, no cava
    Disable {
        /// An image file, rather than the wallpaper on screen
        image: Option<PathBuf>,
    },
    /// Turn it back on for this wallpaper
    Enable {
        /// An image file, rather than the wallpaper on screen
        image: Option<PathBuf>,
    },
    /// Read the log: what happened, and why the last instance stopped
    Log {
        #[command(subcommand)]
        what: Option<LogWhat>,
    },
    /// Start at login as a systemd user service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Print a shell completion script
    Completions { shell: Shell },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Write the unit for this binary, enable it and hand the instance over
    Install,
    /// Disable the unit and delete the one install wrote
    Remove,
}

#[derive(Subcommand)]
enum LogWhat {
    /// The newest lines (the default)
    Show {
        /// How many lines
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
    },
    /// The newest lines, then every new one as it is written
    Watch {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// Where the log is kept
    Path,
    /// Why the last instance stopped
    LastExit,
}

/// `log watch`: the tail, then new lines as they land. A shrunk file means
/// the log rotated at startup, and the new one is read from its start
fn follow(lines: usize) -> ! {
    use std::io::{Read, Seek, SeekFrom, Write};
    if let Ok(text) = cavawall::log::tail(lines) {
        print!("{text}");
    }
    let path = cavawall::log::path();
    let mut pos = std::fs::metadata(&path).map_or(0, |m| m.len());
    let mut buf = Vec::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
        if len < pos {
            pos = 0;
        }
        if len == pos {
            continue;
        }
        if let Ok(mut f) = std::fs::File::open(&path)
            && f.seek(SeekFrom::Start(pos)).is_ok()
        {
            buf.clear();
            if f.read_to_end(&mut buf).is_ok() {
                pos += buf.len() as u64;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buf);
                let _ = out.flush();
            }
        }
    }
}

/// The name this was run as: `cavawall <command>` hands over to this binary
/// with CAVAWALL_AS set, so help and completions speak of `cavawall`
fn own_name() -> &'static str {
    match std::env::var("CAVAWALL_AS").as_deref() {
        Ok("cavawall") => "cavawall",
        _ => "cavawallctl",
    }
}

/// The lock names the running instance even when its socket is wedged, which
/// is the only case this is for. The exe is checked before signalling, so a
/// recycled pid belonging to something else is left alone.
fn locked_pid() -> Option<i32> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(|| std::path::PathBuf::from("/tmp"), std::path::PathBuf::from);
    let pid: i32 = std::fs::read_to_string(dir.join("cavawall.lock")).ok()?.trim().parse().ok()?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    exe.file_name()?.to_str()?.starts_with("cavawall").then_some(pid)
}

const UNIT: &str = "cavawall.service";

fn systemctl(args: &[&str]) -> std::io::Result<bool> {
    Proc::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
}

/// A session that activates graphical-session.target - uwsm, a display
/// manager's systemd session - is the only kind that has the Wayland
/// environment in systemd's; anything else starts cavawall directly
fn session_is_systemd() -> bool {
    systemctl(&["is-active", "graphical-session.target"]).unwrap_or(false)
}

/// Installed as a user or system unit in a session that can run it, so
/// systemd should own the process
fn has_unit() -> bool {
    session_is_systemd() && systemctl(&["cat", UNIT]).unwrap_or(false)
}

/// Through the service when there is one, so systemd tracks and restarts it;
/// otherwise the daemon itself, detached so it outlives the shell
fn launch() -> std::io::Result<()> {
    if has_unit() {
        return if systemctl(&["restart", UNIT])? {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("systemctl --user restart {UNIT} failed")))
        };
    }
    let bin = cavawall::daemon().ok_or_else(|| std::io::Error::other("no cavawall binary found"))?;
    let mut cmd = Proc::new(bin);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches only the child
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}

/// Asks the running instance to exit and waits up to 5s for it to go
fn stop_running() -> bool {
    request(&Request::Stop).is_err()
        || (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            request(&Request::Status).is_err()
        })
}

fn user_unit_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("systemd/user")
        .join(UNIT)
}

/// The packaged unit already names /usr/bin/cavawall; anything else gets a
/// user copy naming the binary this helper belongs to
fn service(action: &ServiceAction, name: &str) -> Result<&'static str, String> {
    let path = user_unit_path();
    // A link means something else manages the unit - a dotfiles repo, say
    if path.is_symlink() {
        return Err(format!("{} is a symlink, managed elsewhere; remove it first", path.display()));
    }
    match action {
        ServiceAction::Install => {
            if !session_is_systemd() {
                return Err("this session does not activate graphical-session.target, so a user \
                            service would never start; use the compositor's autostart instead \
                            (Hyprland: exec-once = cavawall)"
                    .to_owned());
            }
            let bin = cavawall::daemon().ok_or("no cavawall binary found")?;
            let packaged = std::path::Path::new("/usr/lib/systemd/user").join(UNIT);
            if !(bin == std::path::Path::new("/usr/bin/cavawall") && packaged.exists()) {
                let unit = include_str!("../../packaging/cavawall.service")
                    .replace("ExecStart=/usr/bin/cavawall", &format!("ExecStart={}", bin.display()));
                std::fs::create_dir_all(path.parent().unwrap_or(&path))
                    .and_then(|()| std::fs::write(&path, unit))
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                systemctl(&["daemon-reload"]).map_err(|e| e.to_string())?;
            }
            if !systemctl(&["enable", UNIT]).map_err(|e| e.to_string())? {
                return Err(format!("systemctl --user enable {UNIT} failed"));
            }
            // An instance started some other way would make the service's
            // own stand down as a duplicate
            if !stop_running() {
                return Err(format!("the running instance did not stop; try `{name} kill`"));
            }
            if !systemctl(&["start", UNIT]).map_err(|e| e.to_string())? {
                return Err(format!("installed, but it did not start: see `{name} log show`"));
            }
            Ok("installed and started; it now starts with the session")
        }
        ServiceAction::Remove => {
            systemctl(&["disable", "--now", UNIT]).map_err(|e| e.to_string())?;
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    systemctl(&["daemon-reload"]).map_err(|e| e.to_string())?;
                    Ok("removed")
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("disabled"),
                Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
            }
        }
    }
}

/// Turn cavawall off or back on for a wallpaper: the image named, else the
/// one on screen. The running instance says which config and wallpaper it
/// has; with none running, config.toml says where the wallpaper comes from
fn set_disabled(image: Option<&Path>, off: bool) -> Result<String, String> {
    let status = request(&Request::Status).ok().and_then(|r| r.data);
    let field = |k: &str| status.as_ref().and_then(|d| d.get(k)?.as_str().map(str::to_owned));
    let config = field("config").map_or_else(|| app_config::config_dir().join("config.toml"), PathBuf::from);
    let dir = config.parent().map(Path::to_path_buf).unwrap_or_default();
    let running = field("curve_key");
    let (key, what) = if let Some(p) = image {
        let key = curve::content_key(p).ok_or_else(|| format!("cannot read {}", p.display()))?;
        (key, p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()))
    } else if let Some(key) = running.clone() {
        (key, "this wallpaper".to_owned())
    } else {
        let cfg = app_config::load_config(&config).ok();
        cavawall::wallpaper::configure(cfg.as_ref().and_then(|c| c.wallpaper.as_ref()));
        let wp = curve::current_wallpaper().ok_or("cannot tell which wallpaper is on screen; name an image")?;
        (curve::content_key(&wp).ok_or_else(|| format!("cannot read {}", wp.display()))?, "this wallpaper".to_owned())
    };
    if WallpaperConfig::load(&dir, &key)?.is_some_and(|w| w.is_disabled()) == off {
        return Ok(format!("already {} on {what}", if off { "off" } else { "drawing" }));
    }
    WallpaperConfig::set_disabled(&dir, &key, off)?;
    let done = format!("{} on {what}", if off { "off" } else { "drawing again" });
    // Only the instance on that wallpaper has anything to change
    if running.as_deref() != Some(key.as_str()) {
        return Ok(done);
    }
    Ok(match request(&Request::Reload) {
        Ok(r) if r.ok => done,
        _ => format!("{done} from its next start: the running instance did not reload"),
    })
}

/// Errors to stderr, data to stdout, non-zero when the answer is no
fn main() {
    let name = own_name();
    let command = || Cli::command().name(name).bin_name(name);
    let cli = Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit());
    match &cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(*shell, &mut command(), name, &mut std::io::stdout());
            return;
        }
        Command::Log { what } => {
            match what.as_ref().unwrap_or(&LogWhat::Show { lines: 40 }) {
                LogWhat::Show { lines } => match cavawall::log::tail(*lines) {
                    Ok(text) => print!("{text}"),
                    Err(e) => {
                        eprintln!("{name}: no log at {} ({e})", cavawall::log::path().display());
                        exit(1);
                    }
                },
                LogWhat::Watch { lines } => follow(*lines),
                LogWhat::Path => println!("{}", cavawall::log::path().display()),
                LogWhat::LastExit => match std::fs::read_to_string(cavawall::log::last_exit_path()) {
                    Ok(text) => print!("{text}"),
                    Err(_) => println!("no exit recorded yet"),
                },
            }
            return;
        }
        Command::Tune => {
            let tune = cavawall::helper("cavawall-tune");
            let err = Proc::new(&tune).exec();
            eprintln!("{name}: cannot run {}: {err}", tune.display());
            exit(127);
        }
        Command::Disable { image } | Command::Enable { image } => {
            let off = matches!(cli.command, Command::Disable { .. });
            match set_disabled(image.as_deref(), off) {
                Ok(msg) if cli.json => println!("{}", serde_json::json!({ "ok": true, "off": off, "said": msg })),
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("{name}: {e}");
                    exit(1);
                }
            }
            return;
        }
        Command::Kill => {
            if let Some(pid) = locked_pid() {
                // SAFETY: a plain signal to a pid this process just resolved
                if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                    if !cli.json {
                        println!("killed {pid}");
                    }
                } else {
                    eprintln!("{name}: cannot kill {pid}: {}", std::io::Error::last_os_error());
                    exit(1);
                }
            } else {
                eprintln!("{name}: no locked instance to kill");
                exit(1);
            }
            return;
        }
        Command::Service { action } => {
            match service(action, name) {
                Ok(msg) => {
                    if !cli.json {
                        println!("{msg}");
                    }
                }
                Err(e) => {
                    eprintln!("{name}: {e}");
                    exit(1);
                }
            }
            return;
        }
        Command::Restart => {
            // Reload re-execs the same image, so a rebuilt binary needs the
            // process replaced rather than refreshed
            if !stop_running() {
                eprintln!("{name}: the running instance did not stop");
                exit(1);
            }
            match launch() {
                Ok(()) => {
                    if !cli.json {
                        println!("restarted");
                    }
                }
                Err(e) => {
                    eprintln!("{name}: cannot start: {e}");
                    exit(1);
                }
            }
            return;
        }
        Command::Start => {
            if request(&Request::Status).is_ok() {
                if !cli.json {
                    println!("already running");
                }
                return;
            }
            match launch() {
                Ok(()) => {
                    if !cli.json {
                        println!("started");
                    }
                }
                Err(e) => {
                    eprintln!("{name}: cannot start: {e}");
                    exit(1);
                }
            }
            return;
        }
        _ => {}
    }

    let req = match &cli.command {
        Command::Status => Request::Status,
        Command::Move { output } => Request::Move { output: output.clone() },
        Command::Stop => Request::Stop,
        Command::Reload => Request::Reload,
        Command::Refresh => Request::Refresh,
        Command::Start
        | Command::Restart
        | Command::Kill
        | Command::Tune
        | Command::Disable { .. }
        | Command::Enable { .. }
        | Command::Log { .. }
        | Command::Service { .. }
        | Command::Completions { .. } => {
            unreachable!("handled above")
        }
    };

    let response = match request(&req) {
        Ok(r) => r,
        Err(e) => {
            if cli.json {
                println!(r#"{{"ok":false,"error":"not running"}}"#);
            } else {
                eprintln!("{name}: not running ({e})");
            }
            exit(1);
        }
    };

    report(&cli, &response);
}

fn report(cli: &Cli, response: &Response) {
    if cli.json {
        println!("{}", serde_json::to_string(response).unwrap_or_default());
    } else if let Some(error) = &response.error {
        eprintln!("{}: {error}", own_name());
    } else if let Some(data) = &response.data {
        for (k, v) in data.as_object().into_iter().flatten() {
            println!("{k:<15} {}", v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        }
    }
    if !response.ok {
        exit(1);
    }
}
