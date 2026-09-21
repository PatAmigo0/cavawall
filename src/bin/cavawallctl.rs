//! Query and control a running cavawall over its control socket.

use cavawall::control::{request, Request, Response};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use std::os::unix::process::CommandExt;
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
    /// Start one if none is running
    Start,
    /// Stop whatever is running and start again, picking up a new binary
    Restart,
    /// SIGKILL the instance named by the lock, for one that stopped answering
    Kill,
    /// Print a shell completion script
    Completions { shell: Shell },
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

/// Detached, so it outlives the shell that asked for it.
fn spawn_launcher() -> std::io::Result<()> {
    let launcher = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".local/bin/cavawall-launch");
    let mut cmd = Proc::new(if launcher.exists() { launcher.into() } else { std::ffi::OsString::from("cavawall-launch") });
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}

/// Errors to stderr, data to stdout, non-zero when the answer is no
fn main() {
    let cli = Cli::parse();
    match &cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(*shell, &mut Cli::command(), "cavawallctl", &mut std::io::stdout());
            return;
        }
        Command::Kill => {
            match locked_pid() {
                Some(pid) => {
                    // SAFETY: a plain signal to a pid this process just resolved
                    if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                        if !cli.json {
                            println!("killed {pid}");
                        }
                    } else {
                        eprintln!("cavawallctl: cannot kill {pid}: {}", std::io::Error::last_os_error());
                        exit(1);
                    }
                }
                None => {
                    eprintln!("cavawallctl: no locked instance to kill");
                    exit(1);
                }
            }
            return;
        }
        Command::Restart => {
            // Reload re-execs the same image, so a rebuilt binary needs the
            // process replaced rather than refreshed
            if request(&Request::Stop).is_ok() {
                let gone = (0..50).any(|_| {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    request(&Request::Status).is_err()
                });
                if !gone {
                    eprintln!("cavawallctl: the running instance did not stop");
                    exit(1);
                }
            }
            match spawn_launcher() {
                Ok(()) => {
                    if !cli.json {
                        println!("restarted");
                    }
                }
                Err(e) => {
                    eprintln!("cavawallctl: cannot start: {e}");
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
            match spawn_launcher() {
                Ok(()) => {
                    if !cli.json {
                        println!("started");
                    }
                }
                Err(e) => {
                    eprintln!("cavawallctl: cannot start: {e}");
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
        Command::Start | Command::Restart | Command::Kill | Command::Completions { .. } => unreachable!("handled above"),
    };

    let response = match request(&req) {
        Ok(r) => r,
        Err(e) => {
            if cli.json {
                println!(r#"{{"ok":false,"error":"not running"}}"#);
            } else {
                eprintln!("cavawallctl: not running ({e})");
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
        eprintln!("cavawallctl: {error}");
    } else if let Some(data) = &response.data {
        for (k, v) in data.as_object().into_iter().flatten() {
            println!("{k:<15} {}", v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        }
    }
    if !response.ok {
        exit(1);
    }
}
