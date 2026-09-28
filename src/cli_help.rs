//! `cavawall` is the daemon only as bare `cavawall`, or with `--config` for
//! tests: the launcher and the fullscreen watcher identify it by that exact
//! argv. Any other first word is a command, handed to cavawallctl, so one
//! name covers everything: `cavawall status`, `cavawall tune`, `cavawall help`

use std::os::unix::process::CommandExt;

/// Returns only when argv is the daemon's
pub fn dispatch() {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        None => {}
        Some(flag) if flag == "--config" => {}
        Some(first) => {
            let ctl = cavawall::helper("cavawallctl");
            let err = std::process::Command::new(&ctl)
                .arg(first)
                .args(args)
                .env("CAVAWALL_AS", "cavawall")
                .exec();
            eprintln!("cavawall: cannot run {}: {err}", ctl.display());
            std::process::exit(127);
        }
    }
}
