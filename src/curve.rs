//! Turning authored control points into one GPU sample per bar
//!
//! Runs on configure, never per frame: the result is a static buffer the
//! vertex shader indexes by `gl_InstanceID`, so a curve costs what a straight
//! row costs - one float per bar per frame, and no path maths in `draw()`

use crate::math::fma;

/// One bar's place on the path, as uploaded
///
/// `std140`-friendly by construction: four floats, no padding to reason about
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct Sample {
    /// Base of the bar, NDC
    pub pos: [f32; 2],
    /// Unit normal the bar grows along, NDC. Also gives the tangent, which is
    /// the perpendicular, so the shader needs no second vector
    pub normal: [f32; 2],
}

/// Control point in normalised output space: x and y in 0..1 from the TOP-LEFT,
/// which is what an image editor reports, plus a bar scale at that point
#[derive(Clone, Copy, Debug)]
pub struct Control {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    /// Degrees, clockwise from straight up, overriding the tangent-derived
    /// normal here. `None` follows the path
    pub angle: Option<f32>,
}

impl Control {
    /// NDC has y running the other way and both axes spanning -1..1
    fn to_ndc(self) -> [f32; 2] {
        [self.x * 2.0 - 1.0, 1.0 - self.y * 2.0]
    }
}

/// Catmull-Rom through `p1`..`p2`, with `p0`/`p3` as the neighbouring tangent
/// controls. Chosen over Bezier because it passes THROUGH its control points:
/// a point clicked on a ridge is on the ridge, with no handles to tune
///
/// Evaluated by Horner's method
///
/// Horner is 3 multiplies and 3 adds per axis against 6 and 5 for the expanded
/// polynomial, needs no `t2`/`t3`, and rounds once per step instead of twice.
/// Each step is also exactly the shape FMA wants
fn catmull_rom(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], t: f32) -> [f32; 2] {
    let mut out = [0.0; 2];
    for i in 0..2 {
        let a = -p0[i] + 3.0 * p1[i] - 3.0 * p2[i] + p3[i];
        let b = 2.0 * p0[i] - 5.0 * p1[i] + 4.0 * p2[i] - p3[i];
        let c = -p0[i] + p2[i];
        let d = 2.0 * p1[i];
        out[i] = 0.5 * fma(fma(fma(a, t, b), t, c), t, d);
    }
    out
}

/// Dense polyline along the spline, with the per-point scale carried through
///
/// Endpoints are duplicated, not wrapped: an open curve, so wrapping would
/// bend its ends toward each other across the screen
fn densify(controls: &[Control], per_segment: usize) -> Vec<([f32; 2], f32, Option<f32>)> {
    let n = controls.len();
    let mut out = Vec::with_capacity((n - 1) * per_segment + 1);
    for i in 0..n - 1 {
        let p0 = controls[i.saturating_sub(1)].to_ndc();
        let p1 = controls[i].to_ndc();
        let p2 = controls[i + 1].to_ndc();
        let p3 = controls[(i + 2).min(n - 1)].to_ndc();
        let (s1, s2) = (controls[i].scale, controls[i + 1].scale);
        let (a1, a2) = (controls[i].angle, controls[i + 1].angle);
        for step in 0..per_segment {
            let t = step as f32 / per_segment as f32;
            // An override on either end wins over the tangent for this whole
            // segment, blending toward the other end. A segment with neither
            // carries None and is left to the path
            let angle = match (a1, a2) {
                (None, None) => None,
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (Some(a), Some(b)) => Some(a + (b - a) * t),
            };
            out.push((catmull_rom(p0, p1, p2, p3, t), s1 + (s2 - s1) * t, angle));
        }
    }
    let last = controls[n - 1];
    out.push((last.to_ndc(), last.scale, last.angle));
    out
}

/// A densified path plus its cumulative arc length, measured in the y-up
/// PIXEL frame rather than in NDC
///
/// NDC is square and the screen is not, so measuring there spaces bars evenly
/// in NDC and unevenly on screen. Built once so `build` can weigh a path
/// before sampling it
struct Arc {
    dense: Vec<([f32; 2], f32, Option<f32>)>,
    acc: Vec<f32>,
    total: f32,
}

fn arc(controls: &[Control], aspect: f32) -> Arc {
    let dense = densify(controls, 16);
    let mut acc = Vec::with_capacity(dense.len());
    let mut total = 0.0f32;
    acc.push(0.0f32);
    for w in dense.windows(2) {
        let (a, b) = (w[0].0, w[1].0);
        total += (((b[0] - a[0]) * aspect).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        acc.push(total);
    }
    Arc { dense, acc, total }
}

/// `count` samples spaced evenly BY ARC LENGTH, not by parameter
///
/// Even in t bunches bars wherever control points sit close together, which on
/// a hand-drawn ridge is where the detail is
///
/// `aspect` is the output's width / height. A normal perpendicular in NDC is
/// NOT perpendicular on screen, so normals are taken in a y-up PIXEL frame -
/// the frame the editor's preview draws in
///
/// Returns each bar's base, its unit normal, and the scale interpolated there
#[must_use]
pub fn resample(
    controls: &[Control],
    count: u32,
    flip: bool,
    upright: bool,
    aspect: f32,
) -> Vec<(Sample, f32)> {
    let count = count.max(1) as usize;
    if controls.len() < 2 {
        let p = controls.first().map_or([0.0, 0.0], |c| c.to_ndc());
        let s = controls.first().map_or(1.0, |c| c.scale);
        return vec![(Sample { pos: p, normal: [0.0, 1.0] }, s); count];
    }
    sample_arc(&arc(controls, aspect), count, flip, upright, aspect)
}

fn sample_arc(a: &Arc, count: usize, flip: bool, upright: bool, aspect: f32) -> Vec<(Sample, f32)> {
    let (dense, acc, total) = (&a.dense, &a.acc, a.total);
    let mut out = Vec::with_capacity(count);
    let mut cursor = 0usize;
    for i in 0..count {
        // Centre of the i-th slot, so the first and last bars sit inside the
        // curve rather than exactly on its ends
        let target = total * (i as f32 + 0.5) / count as f32;
        while cursor + 2 < dense.len() && acc[cursor + 1] < target {
            cursor += 1;
        }
        let seg = (acc[cursor + 1] - acc[cursor]).max(f32::EPSILON);
        let t = ((target - acc[cursor]) / seg).clamp(0.0, 1.0);
        let (a, sa, aa) = dense[cursor];
        let (b, sb, _) = dense[cursor + 1];
        let pos = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];

        // Tangent from the segment, normal perpendicular to it. Degenerate
        // segments fall back to straight up rather than producing NaN.
        // Into the pixel frame before taking the perpendicular
        let (dx, dy) = ((b[0] - a[0]) * aspect, b[1] - a[1]);
        let len = (dx * dx + dy * dy).sqrt();
        // Direction first, THEN flip, applied uniformly - an angle override
        // included
        let mut normal = if let Some(deg) = aa {
            // Degrees clockwise from up, so 0 is [0,1] and 90 is [1,0].
            let r = deg.to_radians();
            [r.sin(), r.cos()]
        } else if upright || len <= f32::EPSILON {
            // Straight rectangles rising from the path rather than leaning
            // with it
            [0.0, 1.0]
        } else {
            [-dy / len, dx / len]
        };
        if flip {
            normal = [-normal[0], -normal[1]];
        }
        out.push((Sample { pos, normal }, sa + (sb - sa) * t));
    }
    out
}

/// One stretch of path, resolved from config into what the sampler needs
#[derive(Clone, Debug, Default)]
pub struct PathSpec {
    pub controls: Box<[Control]>,
    /// Exactly this many bars, rather than a share of the total by length
    pub bars: Option<u32>,
    /// Reach of a full-volume bar and its width, both NDC, before the
    /// per-point scale
    pub reach: f32,
    pub width: f32,
    pub flip: bool,
    pub upright: bool,
}

/// One bar, ready for the GPU: base, the normal's angle, and the reach and
/// width already multiplied by the per-point scale
///
/// The scale is folded in here because the shader has no per-path anything,
/// just one bar after another
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bar {
    pub pos: [f32; 2],
    pub angle: f32,
    pub reach: f32,
    pub width: f32,
}

/// Split `count` bars between paths: whatever `fixed` asks for, and the rest
/// in proportion to length
///
/// Largest remainder, so the total is exactly `count`: the instance count,
/// the SSBO and cava's channel count have to agree. A path too short to earn a
/// bar gets none rather than taking one from a long path
///
/// Fixed counts asking for more than exists are scaled back in proportion -
/// the total is what cava produces
#[must_use]
pub fn allocate(lengths: &[f32], fixed: &[Option<u32>], count: u32) -> Vec<u32> {
    let n = lengths.len();
    if n == 0 {
        return Vec::new();
    }
    if fixed.iter().any(Option::is_some) {
        let asked: u32 = fixed.iter().flatten().sum();
        let mut out = vec![0u32; n];
        if asked >= count {
            // Everything goes to the paths that named a number, in their
            // proportion; the rest get nothing, which is what asking for more
            // than exists means
            let weights: Vec<f32> = fixed.iter().map(|f| f.unwrap_or(0) as f32).collect();
            return allocate(&weights, &vec![None; n], count);
        }
        for (slot, f) in out.iter_mut().zip(fixed) {
            *slot = f.unwrap_or(0);
        }
        // The rest share what is left, by length
        let rest: Vec<f32> =
            lengths.iter().zip(fixed).map(|(l, f)| if f.is_some() { 0.0 } else { *l }).collect();
        if rest.iter().any(|l| *l > 0.0) {
            for (slot, share) in
                out.iter_mut().zip(allocate(&rest, &vec![None; n], count - asked))
            {
                if *slot == 0 {
                    *slot = share;
                }
            }
        }
        return out;
    }
    let total: f32 = lengths.iter().sum();
    // NaN included: a path whose length is not a positive number cannot be
    // weighted by it
    if !total.is_finite() || total <= 0.0 {
        // Degenerate paths: spread evenly and let the remainder fall to the
        // front, which at least keeps the total right
        let each = count / n as u32;
        let mut out = vec![each; n];
        for slot in out.iter_mut().take((count % n as u32) as usize) {
            *slot += 1;
        }
        return out;
    }
    let ideal: Vec<f32> = lengths.iter().map(|l| count as f32 * l / total).collect();
    let mut out: Vec<u32> = ideal.iter().map(|v| *v as u32).collect();
    let assigned: u32 = out.iter().sum();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        (ideal[b] - ideal[b].floor())
            .total_cmp(&(ideal[a] - ideal[a].floor()))
            .then(a.cmp(&b))
    });
    for &i in order.iter().cycle().take(count.saturating_sub(assigned) as usize) {
        out[i] += 1;
    }
    out
}

/// Every bar of every path, in one buffer
///
/// One draw call: the shader indexes per instance and has no idea paths
/// exist, so a second curve costs only its own bars
#[must_use]
pub fn build(paths: &[PathSpec], count: u32, aspect: f32, fit: Fit) -> Vec<Bar> {
    let usable: Vec<&PathSpec> = paths.iter().filter(|p| p.controls.len() >= 2).collect();
    if usable.is_empty() {
        return Vec::new();
    }
    // Densified once and kept: measuring a path and sampling it are the same
    // walk, and build runs on every configure
    let mapped: Vec<Box<[Control]>> = usable
        .iter()
        .map(|p| {
            p.controls
                .iter()
                .map(|c| {
                    let m = fit.map([c.x, c.y]);
                    Control { x: m[0], y: m[1], ..*c }
                })
                .collect()
        })
        .collect();
    let arcs: Vec<Arc> = mapped.iter().map(|c| arc(c, aspect)).collect();
    let counts = allocate(
        &arcs.iter().map(|a| a.total).collect::<Vec<_>>(),
        &usable.iter().map(|p| p.bars).collect::<Vec<_>>(),
        count,
    );
    let mut out = Vec::with_capacity(count as usize);
    for ((spec, a), n) in usable.iter().zip(&arcs).zip(counts) {
        for (s, scale) in sample_arc(a, n as usize, spec.flip, spec.upright, aspect) {
            out.push(Bar {
                pos: s.pos,
                angle: s.normal[1].atan2(s.normal[0]),
                reach: spec.reach * scale,
                width: spec.width * scale,
            });
        }
    }
    out
}

/// NDC bounding box of every bar at full volume: `(min_x, min_y, max_x,
/// max_y)`.
///
/// The hull of both ends of every bar, not of the path: a leaning bar reaches
/// outside the path's own box. Lets the surface shrink to the curve
#[must_use]
pub fn bounds(bars: &[Bar]) -> (f32, f32, f32, f32) {
    let (mut x0, mut y0) = (f32::MAX, f32::MAX);
    let (mut x1, mut y1) = (f32::MIN, f32::MIN);
    for bar in bars {
        let n = [bar.angle.cos(), bar.angle.sin()];
        let t = [-n[1], n[0]];
        let half = bar.width * 0.5;
        let tip = [n[0] * bar.reach, n[1] * bar.reach];
        for base in [[0.0, 0.0], tip] {
            for side in [-half, half] {
                let x = bar.pos[0] + base[0] + t[0] * side;
                let y = bar.pos[1] + base[1] + t[1] * side;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 > x1 { (-1.0, -1.0, 1.0, 1.0) } else { (x0, y0, x1, y1) }
}

/// Resolution of the sampled horizon
///
/// 2048 is about one output pixel per bucket at 1920, fine enough to hold a
/// steep stretch of ridge and to agree with the editor's preview to the pixel.
/// The buffer is 8KB and the fragment does one lookup
pub const HORIZON_BUCKETS: usize = 2048;

/// A polyline in normalised coordinates, resampled into a height-above-bottom
/// per x bucket
///
/// Points need not be sorted or span the full width: the ends extend flat, so
/// a silhouette drawn across the middle still occludes correctly at the edges
#[must_use]
pub fn horizon(points: &[Control], fit: Fit) -> Vec<f32> {
    if points.len() < 2 {
        return Vec::new();
    }
    let mut pts: Vec<(f32, f32)> = points
        .iter()
        .map(|c| {
            let m = fit.map([c.x, c.y]);
            Control { x: m[0], y: m[1], ..*c }
        })
        .collect::<Vec<_>>()
        .iter()
        // Stored counting UP from the bottom, which is the direction
        // gl_FragCoord.y runs, so the shader flips neither
        .map(|c| (c.x.clamp(0.0, 1.0), 1.0 - c.y.clamp(0.0, 1.0)))
        .collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));

    // One walk, not a search per bucket: both sequences are sorted by x, so
    // the segment for bucket i+1 is at or after the segment for bucket i.
    // Searching from the start each time is O(buckets * points) - 268k
    // comparisons for a 131-point silhouette - against O(buckets + points)
    let mut out = Vec::with_capacity(HORIZON_BUCKETS);
    let last = pts.len() - 1;
    let mut k = 0usize;
    for i in 0..HORIZON_BUCKETS {
        let x = i as f32 / (HORIZON_BUCKETS - 1) as f32;
        while k < last && pts[k].0 < x {
            k += 1;
        }
        out.push(if pts[k].0 < x {
            // Past the last point: the horizon holds flat
            pts[last].1
        } else if k == 0 {
            // Before the first: flat the other way
            pts[0].1
        } else {
            let (a, b) = (pts[k - 1], pts[k]);
            let span = (b.0 - a.0).max(f32::EPSILON);
            fma(b.1 - a.1, (x - a.0) / span, a.1)
        });
    }
    out
}

/// FNV-1a over a file's bytes, hex
///
/// Content, not path: this wallpaper collection gets renamed and moved between
/// folders, and a filename key breaks on every one of those while a content key
/// survives them. Not cryptographic and does not need to be - it distinguishes
/// a few hundred images
///
/// Hand-rolled rather than `DefaultHasher`, whose output std explicitly does
/// not promise to be stable across releases: a toolchain bump would silently
/// invalidate every key in the config
#[must_use]
pub fn content_key(path: &std::path::Path) -> Option<String> {
    Some(describe(path)?.0)
}

/// A wallpaper's key and its pixel size, from one read of the file
///
/// Both answers come out of the same bytes, and the file is a few megabytes:
/// asking for them separately reads it twice
#[must_use]
pub fn describe(path: &std::path::Path) -> Option<(String, Option<(u32, u32)>)> {
    let bytes = std::fs::read(path).ok()?;
    Some((format!("{:016x}", fnv1a(&bytes)), size_of_image(&bytes)))
}

/// A wallpaper's pixel dimensions, read from the file's header
///
/// Header only: the size is needed to work out how the image is cropped onto
/// the output, and decoding a 3-megapixel JPEG to learn two integers would be
/// absurd. PNG and JPEG cover what wallpaper daemons are fed; anything else
/// returns None and the caller treats the image as already output-shaped,
/// which is what every curve authored before this assumed
#[must_use]
pub fn image_size(path: &std::path::Path) -> Option<(u32, u32)> {
    size_of_image(&std::fs::read(path).ok()?)
}

/// As `image_size`, for bytes already in hand
#[must_use]
pub fn size_of_image(b: &[u8]) -> Option<(u32, u32)> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.len() >= 24 {
        // IHDR is always the first chunk: width and height as big-endian u32
        let n = |at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        return Some((n(16), n(20)));
    }
    if !b.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    // JPEG: walk the marker segments to the frame header, which is the only
    // one carrying the dimensions. Markers are 0xFF followed by a type; the
    // padding between them is any number of further 0xFF bytes
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        if marker == 0xff {
            i += 1;
            continue;
        }
        // Standalone markers carry no length: RSTn, SOI, EOI, TEM
        if (0xd0..=0xd9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        // Any SOFn except the four that are not frame headers
        let sof = (0xc0..=0xcf).contains(&marker)
            && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if sof {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]);
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]);
            return Some((u32::from(w), u32::from(h)));
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// How a wallpaper's coordinates land on the output
///
/// A curve is drawn on the IMAGE, but the image is not what the output shows:
/// a wallpaper daemon covers the screen with it, scaling to the larger of the
/// two ratios and cropping the overflow. Same aspect, and the two agree and
/// this is the identity; different aspect, and a point that sits on a ridge in
/// the file sits somewhere else entirely on the screen
///
/// The map is a uniform scale plus a translation, so it moves bars without
/// skewing them: an angle on the image is the same angle on the output
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub sx: f32,
    pub sy: f32,
    pub ox: f32,
    pub oy: f32,
}

impl Default for Fit {
    fn default() -> Self {
        Self::STRETCH
    }
}

impl Fit {
    /// The image is treated as already output-shaped. What every curve
    /// authored before `image_size` existed assumed, and what a daemon that
    /// stretches rather than crops actually does
    pub const STRETCH: Fit = Fit { sx: 1.0, sy: 1.0, ox: 0.0, oy: 0.0 };

    /// Scale to cover the output, centre, crop the overflow
    #[must_use]
    pub fn cover(image: (u32, u32), output: (u32, u32)) -> Fit {
        let (iw, ih) = (image.0 as f32, image.1 as f32);
        let (ow, oh) = (output.0 as f32, output.1 as f32);
        if iw <= 0.0 || ih <= 0.0 || ow <= 0.0 || oh <= 0.0 {
            return Fit::STRETCH;
        }
        let scale = (ow / iw).max(oh / ih);
        let (sx, sy) = (iw * scale / ow, ih * scale / oh);
        Fit { sx, sy, ox: (1.0 - sx) * 0.5, oy: (1.0 - sy) * 0.5 }
    }

    /// Image coordinates to output coordinates, both normalised, origin top
    /// left
    #[must_use]
    pub fn map(&self, p: [f32; 2]) -> [f32; 2] {
        [p[0] * self.sx + self.ox, p[1] * self.sy + self.oy]
    }
}

/// FNV-1a 64. Split out so a known-answer test can reach it: the prime is 11
/// hex digits and a twelfth typo'd zero still produces plausible-looking
/// hashes that simply never match the ones anything else computes
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The wallpaper the shell currently has set, if it says
#[must_use]
pub fn current_wallpaper() -> Option<std::path::PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME").map_or_else(
        || std::path::PathBuf::from(std::env::var_os("HOME")?).join(".local/state").into(),
        |s| Some(std::path::PathBuf::from(s)),
    )?;
    let txt = std::fs::read_to_string(state.join("caelestia/wallpaper/path.txt")).ok()?;
    let trimmed = txt.trim();
    (!trimmed.is_empty()).then(|| std::path::PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize) -> Vec<Control> {
        (0..n)
            .map(|i| Control { x: i as f32 / (n - 1) as f32, y: 0.5, scale: 1.0, angle: None })
            .collect()
    }

    /// A horizontal line must come back as bars pointing straight up, evenly
    /// spaced - i.e. curve mode with a flat path reproduces the bars mode it
    /// generalises
    #[test]
    fn a_flat_path_reproduces_a_row_of_bars() {
        let s = resample(&line(4), 8, false, false, 1.0);
        assert_eq!(s.len(), 8);
        for (sample, scale) in &s {
            assert!((sample.pos[1] - 0.0).abs() < 1e-3, "y drifted: {:?}", sample.pos);
            assert!((sample.normal[0]).abs() < 1e-3, "normal not vertical");
            assert!((sample.normal[1] - 1.0).abs() < 1e-3, "normal not up");
            assert!((scale - 1.0).abs() < 1e-6);
        }
        // Evenly spaced along x
        let gaps: Vec<f32> = s.windows(2).map(|w| w[1].0.pos[0] - w[0].0.pos[0]).collect();
        let first = gaps[0];
        for g in &gaps {
            assert!((g - first).abs() < 1e-3, "uneven spacing: {gaps:?}");
        }
    }

    /// Normals stay unit length whatever the path does; a non-unit normal
    /// scales the bar with the slope and the visualiser gets taller on hills
    #[test]
    fn normals_are_unit_length_on_a_slope() {
        let controls = vec![
            Control { x: 0.0, y: 0.9, scale: 1.0, angle: None },
            Control { x: 0.35, y: 0.3, scale: 1.0, angle: None },
            Control { x: 0.7, y: 0.6, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.35, scale: 1.0, angle: None },
        ];
        for (s, _) in resample(&controls, 32, false, false, 1.0) {
            let len = (s.normal[0].powi(2) + s.normal[1].powi(2)).sqrt();
            assert!((len - 1.0).abs() < 1e-3, "normal length {len}");
        }
    }

    /// flip mirrors the normal and nothing else
    #[test]
    fn flip_only_reverses_the_normal() {
        let c = line(3);
        for ((a, _), (b, _)) in resample(&c, 6, false, false, 1.0).iter().zip(resample(&c, 6, true, false, 1.0).iter()) {
            assert_eq!(a.pos, b.pos);
            assert!((a.normal[0] + b.normal[0]).abs() < 1e-6);
            assert!((a.normal[1] + b.normal[1]).abs() < 1e-6);
        }
    }

    /// Per-point scale interpolates along the path - this is what makes a
    /// distant stretch of ridge carry shorter bars
    #[test]
    fn scale_interpolates_between_control_points() {
        let controls = vec![
            Control { x: 0.0, y: 0.5, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.5, scale: 0.2, angle: None },
        ];
        let s = resample(&controls, 10, false, false, 1.0);
        assert!(s[0].1 > s[9].1, "scale should fall along the path");
        assert!(s[0].1 <= 1.0 && s[9].1 >= 0.2);
        // Monotone, not jumping about
        for w in s.windows(2) {
            assert!(w[1].1 <= w[0].1 + 1e-6, "scale not monotone");
        }
    }

    /// Known answers from the FNV reference vectors. Without these a typo in
    /// the prime is invisible: every key still looks like a hash, and the only
    /// symptom is that no curve ever matches its wallpaper
    #[test]
    fn fnv1a_matches_the_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// A per-point angle overrides the tangent, and only where it is set
    #[test]
    fn angle_override_beats_the_tangent() {
        let controls = vec![
            Control { x: 0.0, y: 0.9, scale: 1.0, angle: Some(90.0) },
            Control { x: 1.0, y: 0.2, scale: 1.0, angle: Some(90.0) },
        ];
        for (s, _) in resample(&controls, 8, false, false, 1.0) {
            // 90 degrees clockwise from up is straight right
            assert!((s.normal[0] - 1.0).abs() < 1e-3, "normal {:?}", s.normal);
            assert!(s.normal[1].abs() < 1e-3);
        }
        // Without it the same slope leans
        let plain = vec![
            Control { x: 0.0, y: 0.9, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.2, scale: 1.0, angle: None },
        ];
        assert!(resample(&plain, 8, false, false, 1.0).iter().all(|(s, _)| s.normal[0] < 0.95));
    }

    /// A bar standing straight up from the middle of the screen occupies the
    /// upper half and nothing else - that is the whole point of shrinking the
    /// surface to it
    #[test]
    fn bounds_cover_bar_and_width() {
        let bar = Bar {
            pos: [0.0, 0.0],
            angle: std::f32::consts::FRAC_PI_2,
            reach: 0.5,
            width: 0.2,
        };
        let (x0, y0, x1, y1) = bounds(&[bar]);
        assert!((x0 - -0.1).abs() < 1e-6 && (x1 - 0.1).abs() < 1e-6, "width straddles the base");
        assert!((y0 - 0.0).abs() < 1e-6 && (y1 - 0.5).abs() < 1e-6, "reach sets the top");
        assert_eq!(bounds(&[]), (-1.0, -1.0, 1.0, 1.0), "no bars claims everything");
    }

    /// The instance count, the SSBO and cava's channels all have to agree, so
    /// an allocation that is one off is a desync, not a rounding detail
    #[test]
    fn allocation_is_proportional_and_exact() {
        let free = |n| vec![None; n];
        assert_eq!(allocate(&[3.0, 1.0], &free(2), 40), vec![30, 10]);
        // 10 bars over thirds: 3.33 each, and the remainder goes to the
        // largest fraction first
        let split = allocate(&[1.0, 1.0, 1.0], &free(3), 10);
        assert_eq!(split.iter().sum::<u32>(), 10);
        assert_eq!(split, vec![4, 3, 3]);
        // A path too short to earn a bar gets none rather than stealing one
        assert_eq!(allocate(&[100.0, 0.001], &free(2), 8), vec![8, 0]);
        // Degenerate input still totals exactly
        assert_eq!(allocate(&[0.0, 0.0], &free(2), 5).iter().sum::<u32>(), 5);
        assert!(allocate(&[], &free(0), 5).is_empty());
        for n in [1u32, 7, 27, 64, 200] {
            assert_eq!(allocate(&[2.0, 5.0, 0.3], &free(3), n).iter().sum::<u32>(), n, "for {n}");
        }
    }

    /// A path that names a count gets it; the others share what is left, and
    /// the total still lands exactly on what cava produces
    #[test]
    fn a_fixed_count_is_taken_off_the_top() {
        assert_eq!(allocate(&[1.0, 1.0], &[Some(6), None], 20), vec![6, 14]);
        // Length still decides between the paths that did not name one
        assert_eq!(allocate(&[1.0, 3.0, 1.0], &[Some(10), None, None], 30), vec![10, 15, 5]);
        // Asking for more than exists shares out what exists, in proportion
        // to what was asked - nobody gets their number, and the total holds
        let over = allocate(&[1.0, 1.0], &[Some(30), Some(10)], 20);
        assert_eq!(over, vec![15, 5]);
        assert_eq!(over.iter().sum::<u32>(), 20);
        // Every bar named, exactly: no remainder to share
        assert_eq!(allocate(&[1.0, 1.0], &[Some(7), Some(13)], 20), vec![7, 13]);
    }

    /// Two paths share one buffer and one draw call, and each carries its own
    /// reach - which is the whole reason a bar holds reach rather than the
    /// shader holding a uniform
    #[test]
    fn build_splits_bars_and_keeps_per_path_reach() {
        let line = |x0: f32, x1: f32| {
            vec![
                Control { x: x0, y: 0.5, scale: 1.0, angle: None },
                Control { x: x1, y: 0.5, scale: 1.0, angle: None },
            ]
            .into_boxed_slice()
        };
        let paths = vec![
            PathSpec { controls: line(0.0, 0.6), reach: 0.4, width: 0.01, ..Default::default() },
            PathSpec { controls: line(0.7, 1.0), reach: 0.1, width: 0.02, ..Default::default() },
        ];
        let bars = build(&paths, 20, 16.0 / 9.0, Fit::STRETCH);
        assert_eq!(bars.len(), 20);
        // Twice the length, twice the bars
        let long = bars.iter().filter(|b| (b.reach - 0.4).abs() < 1e-6).count();
        assert_eq!(long, 13, "0.6 against 0.3 of width");
        assert!(bars.iter().filter(|b| (b.reach - 0.1).abs() < 1e-6).count() == 7);
        // A path with too few points to be a curve is skipped, not drawn as a
        // point: one path left means it takes every bar
        let broken = vec![
            paths[0].clone(),
            PathSpec { controls: Box::new([]), ..Default::default() },
        ];
        assert_eq!(build(&broken, 9, 1.0, Fit::STRETCH).len(), 9);
        assert!(build(&[], 9, 1.0, Fit::STRETCH).is_empty());
    }

    /// The same point in the file has to land on the same feature of the
    /// picture whatever screen it is shown on, and cover-cropping is how a
    /// wallpaper daemon puts it there
    #[test]
    fn a_cover_fit_crops_the_overflow_and_centres_what_is_left() {
        // Same shape: nothing to crop, so nothing moves
        let same = Fit::cover((2560, 1440), (1920, 1080));
        assert!((same.sx - 1.0).abs() < 1e-5 && (same.sy - 1.0).abs() < 1e-5);
        assert!((same.map([0.25, 0.75])[0] - 0.25).abs() < 1e-5);

        // A 16:9 image on a 16:10 screen: height fits, width overflows and is
        // cropped evenly, so the centre holds and the edges pull inward
        let f = Fit::cover((1920, 1080), (1920, 1200));
        assert!((f.sy - 1.0).abs() < 1e-5, "the limiting axis fits exactly");
        assert!(f.sx > 1.1, "the other overflows: {}", f.sx);
        assert!((f.map([0.5, 0.5])[0] - 0.5).abs() < 1e-5, "the centre never moves");
        assert!(f.map([0.0, 0.0])[0] < 0.0, "the left edge is cropped away");
        assert!(f.map([1.0, 0.0])[0] > 1.0);
        // Uniform in both axes, so an angle survives the map
        let d = Fit::cover((3000, 1000), (1000, 1000));
        assert!((d.sx / d.sy - 3.0).abs() < 1e-4 || (d.sy - 1.0).abs() < 1e-5);
        // Nonsense in, identity out, rather than a NaN that draws nothing
        assert_eq!(Fit::cover((0, 0), (1920, 1080)), Fit::STRETCH);
    }

    /// The single walk has to agree with the obvious search-from-the-start
    /// version at every bucket, for silhouettes of every shape: sorted or not,
    /// duplicated x, spanning the width or a sliver of it
    #[test]
    fn the_horizon_walk_matches_a_brute_force_search() {
        fn brute(points: &[Control]) -> Vec<f32> {
            let mut pts: Vec<(f32, f32)> = points
                .iter()
                .map(|c| (c.x.clamp(0.0, 1.0), 1.0 - c.y.clamp(0.0, 1.0)))
                .collect();
            pts.sort_by(|a, b| a.0.total_cmp(&b.0));
            (0..HORIZON_BUCKETS)
                .map(|i| {
                    let x = i as f32 / (HORIZON_BUCKETS - 1) as f32;
                    match pts.iter().position(|p| p.0 >= x) {
                        None => pts[pts.len() - 1].1,
                        Some(0) => pts[0].1,
                        Some(k) => {
                            let (a, b) = (pts[k - 1], pts[k]);
                            let span = (b.0 - a.0).max(f32::EPSILON);
                            a.1 + (b.1 - a.1) * ((x - a.0) / span)
                        }
                    }
                })
                .collect()
        }
        // A deterministic spread of shapes, including ones that break naive
        // indexing: reversed input, repeated x, a sliver, the full width
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / 16_777_216.0
        };
        for case in 0..40 {
            let n = 2 + case % 17;
            let (lo, hi) = match case % 4 {
                0 => (0.0, 1.0),
                1 => (0.4, 0.45),
                2 => (0.0, 0.3),
                _ => (0.7, 1.0),
            };
            let pts: Vec<Control> = (0..n)
                .map(|i| Control {
                    x: lo + (hi - lo) * (i as f32 / (n - 1) as f32),
                    y: rand(),
                    scale: 1.0,
                    angle: None,
                })
                .collect();
            let (walk, slow) = (horizon(&pts, Fit::STRETCH), brute(&pts));
            assert_eq!(walk.len(), slow.len(), "case {case}");
            for (i, (w, b)) in walk.iter().zip(&slow).enumerate() {
                assert!((w - b).abs() < 1e-5, "case {case} bucket {i}: {w} vs {b}");
            }
        }
    }

    /// A flat silhouette occludes at a constant height, and one drawn across
    /// only part of the width extends flat to both edges rather than dropping
    /// to zero and letting bars show through at the sides
    #[test]
    fn horizon_samples_and_extends_flat() {
        let pts = vec![
            Control { x: 0.3, y: 0.6, scale: 1.0, angle: None },
            Control { x: 0.7, y: 0.6, scale: 1.0, angle: None },
        ];
        let h = horizon(&pts, Fit::STRETCH);
        assert_eq!(h.len(), HORIZON_BUCKETS);
        // y 0.6 from the top is 0.4 from the bottom, everywhere
        for v in &h {
            assert!((v - 0.4).abs() < 1e-3, "got {v}");
        }
        assert!(horizon(&pts[..1], Fit::STRETCH).is_empty(), "one point cannot be a horizon");
        // A slope interpolates rather than stepping
        let slope = vec![
            Control { x: 0.0, y: 1.0, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.0, scale: 1.0, angle: None },
        ];
        let h = horizon(&slope, Fit::STRETCH);
        assert!(h[0] < 0.01 && h[HORIZON_BUCKETS - 1] > 0.99);
        assert!((h[HORIZON_BUCKETS / 2] - 0.5).abs() < 0.01);
    }

    /// A normal has to be perpendicular ON SCREEN, not in NDC. NDC is square
    /// and a 16:9 output is not, so ignoring aspect leans every bar on a slope
    /// by a visible amount - and the editor, which works in pixels, would draw
    /// something the renderer never produced
    #[test]
    fn normals_are_perpendicular_on_screen_not_in_ndc() {
        // Pixel slope of exactly 45 degrees on 16:9. to_ndc doubles a
        // normalised delta, so dx_ndc = 2*dx_norm and dy_ndc = 2*dy_norm; for
        // dx_ndc * aspect == dy_ndc the normalised dx must be dy * 9/16
        let dy_norm = 0.5; // 0.75 -> 0.25
        let dx_norm = dy_norm * 9.0 / 16.0;
        let controls = vec![
            Control { x: 0.5 - dx_norm / 2.0, y: 0.75, scale: 1.0, angle: None },
            Control { x: 0.5 + dx_norm / 2.0, y: 0.25, scale: 1.0, angle: None },
        ];
        let aspect = 16.0 / 9.0;
        let (s, _) = resample(&controls, 3, false, false, aspect)[1];
        // Perpendicular to a 45-degree pixel slope is 45 degrees the other way
        assert!(
            (s.normal[0].abs() - s.normal[1].abs()).abs() < 0.02,
            "not 45 degrees in pixel space: {:?}",
            s.normal
        );
        // The same path at aspect 1.0 gives a measurably different lean, which
        // is precisely the bug this guards
        let (flat, _) = resample(&controls, 3, false, false, 1.0)[1];
        assert!(
            (flat.normal[0] - s.normal[0]).abs() > 0.05,
            "aspect made no difference, so it is not being applied"
        );
    }

    /// flip must reach EVERY bar, including one carrying an angle override.
    /// It did not, and the result was a single bar pointing the opposite way
    /// from all its neighbours
    #[test]
    fn flip_applies_to_angle_overrides_too() {
        let controls = vec![
            Control { x: 0.0, y: 0.5, scale: 1.0, angle: Some(0.0) },
            Control { x: 1.0, y: 0.5, scale: 1.0, angle: Some(0.0) },
        ];
        for (s, _) in resample(&controls, 6, true, false, 1.0) {
            assert!((s.normal[1] + 1.0).abs() < 1e-3, "override ignored flip: {:?}", s.normal);
        }
        // And upright flips as well
        let plain = vec![
            Control { x: 0.0, y: 0.4, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.7, scale: 1.0, angle: None },
        ];
        for (s, _) in resample(&plain, 6, true, true, 1.0) {
            assert_eq!(s.normal, [0.0, -1.0]);
        }
    }

    /// Fewer than two controls is a config mistake, not a crash
    #[test]
    fn degenerate_input_yields_usable_samples() {
        assert_eq!(resample(&[], 4, false, false, 1.0).len(), 4);
        let one = vec![Control { x: 0.5, y: 0.5, scale: 1.0, angle: None }];
        let s = resample(&one, 3, false, false, 1.0);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].0.normal, [0.0, 1.0]);
    }
}
