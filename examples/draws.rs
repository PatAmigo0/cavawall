//! What a wallpaper's curve costs in GL draws, without starting the renderer
//!
//! Usage: cargo run --example draws -- ~/.config/cavawall/wallpapers/<key>.toml

use cavawall::{app_config::WallpaperConfig, curve};

fn ctrl(pts: &[Vec<f32>]) -> Vec<curve::Control> {
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
    let bars: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(23);
    let wp: WallpaperConfig = toml::from_str(&std::fs::read_to_string(&arg).unwrap()).unwrap();
    let Some(c) = wp.curve else {
        println!("no curve block: circle or bars mode, one draw");
        return;
    };
    let specs: Vec<curve::PathSpec> = c
        .paths()
        .iter()
        .map(|p| curve::PathSpec {
            controls: ctrl(&p.points).into(),
            bars: p.bars,
            reach: p.height.or(c.height).unwrap_or(0.18) * 2.0,
            width: p.width.or(c.width).unwrap_or(0.006) * 2.0,
            flip: p.flip.unwrap_or(false),
            upright: p.upright.unwrap_or(false),
            occlude: p.occlude.as_deref().map(|o| ctrl(o).into()),
            clip: p.clip.unwrap_or(true),
        })
        .collect();
    let shared = c.occlude.as_deref().map(ctrl).unwrap_or_default();
    let built = curve::build(&specs, &shared, bars, 1920.0 / 1200.0, curve::Fit::STRETCH);

    println!("paths in config : {}", specs.len());
    println!("occluder fans   : {}", built.occluders.len());
    for (i, o) in built.occluders.iter().enumerate() {
        println!("   fan {i}: {} outline points", o.len());
    }
    println!("bar draws       : {}", built.draws.len());
    for (i, d) in built.draws.iter().enumerate() {
        println!("   draw {i}: bars {}..{}  occ={:?}", d.first, d.first + d.count, d.occ);
    }
    println!("TOTAL GL DRAWS  : {}", built.occluders.len() + built.draws.len());
}
