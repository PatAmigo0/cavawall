//! What a wallpaper's curve costs the GPU, without starting the renderer
//!
//! Usage: cargo run --example draws -- ~/.config/cavawall/wallpapers/<key>.toml [bars]

use cavawall::app_config::{OccluderShape, WallpaperConfig};
use cavawall::curve;

fn ctrl(pts: &[Vec<f32>]) -> Box<[curve::Control]> {
    pts.iter()
        .filter(|p| p.len() >= 2)
        .map(|p| curve::Control {
            x: p[0],
            y: p[1],
            scale: p.get(2).copied().unwrap_or(1.0).max(0.0),
            angle: p.get(3).copied(),
        })
        .collect()
}

fn main() {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: draws <wallpaper.toml> [bars]");
        std::process::exit(2);
    };
    let text = match std::fs::read_to_string(&arg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("draws: {arg}: {e}");
            std::process::exit(1);
        }
    };
    let wp: WallpaperConfig = match toml::from_str(&text) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("draws: {arg}: {e}");
            std::process::exit(1);
        }
    };
    let Some(c) = wp.curve else {
        println!("no curve block: circle or bars mode, one draw per frame");
        return;
    };
    // The paths' own counts, plus what the others share
    let (own, shares) = c.counts();
    let bars: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| own + if shares { c.bars.unwrap_or(23) } else { 0 });

    let occlusion = c.occlusion();
    let occluders: Vec<curve::Occluder> = occlusion
        .shapes
        .iter()
        .map(|(pts, shape)| curve::Occluder { points: ctrl(pts), closed: *shape == OccluderShape::Closed })
        .collect();
    let specs: Vec<curve::PathSpec> = c
        .paths()
        .iter()
        .zip(occlusion.masks.iter().copied())
        .map(|(p, mask)| curve::PathSpec::from_config(p, &c, mask, 0))
        .collect();
    let built = curve::build(&specs, bars, 1920.0 / 1200.0, curve::Fit::STRETCH);
    let tris = curve::occluder_triangles(&occluders, curve::Fit::STRETCH);

    println!("bars            : {}", built.len());
    println!("occluders       : {}", occluders.len());
    for (i, o) in occluders.iter().enumerate() {
        let kind = if o.closed { "closed" } else { "skyline" };
        println!("  bit {i:<2} {kind:<8} {} points", o.points.len());
    }
    for (i, (s, mask)) in specs.iter().zip(&occlusion.masks).enumerate() {
        println!("  path {i:<2} {} points, cut by {mask:#06b}", s.controls.len());
    }
    println!("mask triangles  : {} (rasterised once per configure)", tris.len() / 3);
    println!("GL draws/frame  : 1");
}
