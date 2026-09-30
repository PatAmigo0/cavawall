//! Which image is on screen, from whichever tool put it there
//!
//! cavawall keeps settings per wallpaper, so it has to know the current one.
//! Each source is read on demand and, where the tool writes a file, watched
//! by the same inotify instance as the palette: no polling. A tool that
//! writes nothing is read through `command`, with `cavawall refresh` from the
//! script that changed it

use crate::app_config::WallpaperSource;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

enum Source {
    /// A file holding the path, one line: Caelestia's path.txt, or any
    File(PathBuf),
    /// `swww query`, watched through swww's cache directory
    Swww,
    /// waypaper's config.ini, `wallpaper = <path>`
    Waypaper(PathBuf),
    Command(Vec<String>),
}

static SOURCE: OnceLock<Source> = OnceLock::new();

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(|| home().join(fallback), PathBuf::from)
}

/// `~/` expanded against home; anything else as written
pub fn expand(p: &str) -> PathBuf {
    p.strip_prefix("~/").map_or_else(|| PathBuf::from(p), |rest| home().join(rest))
}

/// Adopt the config's `[wallpaper]`. Once, before anything asks; an unknown
/// source says so and falls back to Caelestia's file
pub fn configure(cfg: Option<&WallpaperSource>) {
    let caelestia = || Source::File(xdg("XDG_STATE_HOME", ".local/state").join("caelestia/wallpaper/path.txt"));
    let src = match cfg.and_then(|c| c.source.as_deref()).unwrap_or("caelestia") {
        "caelestia" => caelestia(),
        "swww" => Source::Swww,
        "waypaper" => Source::Waypaper(
            cfg.and_then(|c| c.path.as_deref())
                .map_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("waypaper/config.ini"), expand),
        ),
        "file" => {
            if let Some(p) = cfg.and_then(|c| c.path.as_deref()) {
                Source::File(expand(p))
            } else {
                eprintln!("cavawall: [wallpaper] source = \"file\" needs a path; using Caelestia's");
                caelestia()
            }
        }
        "command" => {
            if let Some(c) = cfg.and_then(|c| c.command.clone()).filter(|c| !c.is_empty()) {
                Source::Command(c)
            } else {
                eprintln!("cavawall: [wallpaper] source = \"command\" needs a command; using Caelestia's");
                caelestia()
            }
        }
        other => {
            eprintln!("cavawall: unknown [wallpaper] source {other:?}; using Caelestia's");
            caelestia()
        }
    };
    let _ = SOURCE.set(src);
}

fn source() -> &'static Source {
    SOURCE.get_or_init(|| {
        Source::File(xdg("XDG_STATE_HOME", ".local/state").join("caelestia/wallpaper/path.txt"))
    })
}

fn first_line(text: &str) -> Option<PathBuf> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(expand(line))
}

fn run(program: &str, args: &[String]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The current wallpaper, if the source can say
#[must_use]
pub fn current() -> Option<PathBuf> {
    match source() {
        Source::File(p) => first_line(&std::fs::read_to_string(p).ok()?),
        // "eDP-1: 1920x1080, scale: 1, currently displaying: image: /path"
        Source::Swww => parse_swww(&run("swww", &["query".into()])?),
        Source::Waypaper(ini) => parse_waypaper(&std::fs::read_to_string(ini).ok()?),
        Source::Command(c) => first_line(&run(&c[0], &c[1..])?),
    }
}

/// What to watch for changes: a directory and, when only one file in it
/// matters, that file's name. None for a command, which is refreshed by hand
#[must_use]
pub fn watch_target() -> Option<(PathBuf, Option<String>)> {
    let split = |p: &Path| Some((p.parent()?.to_path_buf(), Some(p.file_name()?.to_string_lossy().into_owned())));
    match source() {
        Source::File(p) | Source::Waypaper(p) => split(p),
        Source::Swww => Some((xdg("XDG_CACHE_HOME", ".cache").join("swww"), None)),
        Source::Command(_) => None,
    }
}

/// The first image in `swww query`'s output:
/// "eDP-1: 1920x1080, scale: 1, currently displaying: image: /path"
#[must_use]
pub fn parse_swww(out: &str) -> Option<PathBuf> {
    out.lines().find_map(|l| l.split_once("image: ").map(|(_, p)| expand(p.trim())))
}

/// waypaper's `wallpaper = <path>`; with several monitors listed, the first
#[must_use]
pub fn parse_waypaper(ini: &str) -> Option<PathBuf> {
    ini.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "wallpaper").then(|| expand(v.trim().split(',').next().unwrap_or("").trim()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swww_names_the_image() {
        let out = "eDP-1: 1920x1080, scale: 1, currently displaying: image: /w/a b.png\nDP-1: 2560x1440, scale: 1, currently displaying: color: 000000\n";
        assert_eq!(parse_swww(out), Some(PathBuf::from("/w/a b.png")));
        assert_eq!(parse_swww("DP-1: currently displaying: color: 000000"), None);
    }

    #[test]
    fn waypaper_reads_its_wallpaper_key() {
        let ini = "[Settings]\nfolder = ~/Pictures\nwallpaper = /w/one.jpg,/w/two.jpg\nbackend = swww\n";
        assert_eq!(parse_waypaper(ini), Some(PathBuf::from("/w/one.jpg")));
    }
}
