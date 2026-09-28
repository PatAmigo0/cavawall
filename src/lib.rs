//! The parts of cavawall that are not the renderer
//!
//! Split out so `cavawall-tune` can share the config types and the path maths
//! instead of duplicating them - two copies of `resample` would drift, and the
//! whole point of the authoring tool is that it produces exactly what the
//! renderer consumes
pub mod app_config;
pub mod control;
pub mod curve;
pub mod log;
pub mod notify;
pub mod math;
pub mod qoi;
pub mod scheme;
pub mod wallpaper;

/// Where the `cavawall <command>` helpers live: lib/cavawall beside the bin
/// dir, keeping them off PATH, then beside the running binary, then PATH
pub fn helper(name: &str) -> std::path::PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(std::path::Path::to_path_buf));
    dir.iter()
        .flat_map(|d| [d.join("../lib/cavawall").join(name), d.join(name)])
        .find(|p| p.exists())
        .unwrap_or_else(|| name.into())
}

/// The daemon, seen from a helper: bin/ beside the lib/cavawall it lives in,
/// then beside it (target/release), then PATH. The directory is resolved but
/// not the name, so the path stays the one scripts match on
pub fn daemon() -> Option<std::path::PathBuf> {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(std::path::Path::to_path_buf));
    let near = dir.into_iter().flat_map(|d| [d.join("../../bin/cavawall"), d.join("cavawall")]);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path: Vec<_> = std::env::split_paths(&path).map(|d| d.join("cavawall")).collect();
    near.chain(on_path)
        .find(|p| p.is_file())
        .and_then(|p| Some(p.parent()?.canonicalize().ok()?.join("cavawall")))
}
