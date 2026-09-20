//! Turning authored control points into one GPU sample per bar.
//!
//! All of this runs once, at startup, for the same reason the bar count is
//! fixed there: the result is a static buffer the vertex shader indexes by
//! `gl_InstanceID`, so a curve costs exactly what a straight row costs - one
//! float per bar per frame, and no path maths in `draw()` at all.

/// One bar's place on the path, as uploaded.
///
/// `std140`-friendly by construction: four floats, no padding to reason about.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct Sample {
    /// Base of the bar, NDC.
    pub pos: [f32; 2],
    /// Unit normal the bar grows along, NDC. Also gives the tangent, which is
    /// the perpendicular, so the shader needs no second vector.
    pub normal: [f32; 2],
}

/// Control point in normalised output space: x and y in 0..1 from the TOP-LEFT,
/// which is what an image editor reports, plus a bar scale at that point.
#[derive(Clone, Copy, Debug)]
pub struct Control {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    /// Degrees, clockwise from straight up, overriding the tangent-derived
    /// normal at this point. `None` means follow the path, which is what you
    /// want almost everywhere - an override is for the handful of places where
    /// the curve is right but the lean is not.
    pub angle: Option<f32>,
}

impl Control {
    /// NDC has y running the other way and both axes spanning -1..1.
    fn to_ndc(self) -> [f32; 2] {
        [self.x * 2.0 - 1.0, 1.0 - self.y * 2.0]
    }
}

/// Catmull-Rom through `p1`..`p2`, with `p0`/`p3` as the neighbouring tangent
/// controls. Chosen over Bezier because it passes THROUGH its control points:
/// a point clicked on a ridge is on the ridge, with no handles to tune.
fn catmull_rom(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], t: f32) -> [f32; 2] {
    let (t2, t3) = (t * t, t * t * t);
    let mut out = [0.0; 2];
    for i in 0..2 {
        out[i] = 0.5
            * ((2.0 * p1[i])
                + (-p0[i] + p2[i]) * t
                + (2.0 * p0[i] - 5.0 * p1[i] + 4.0 * p2[i] - p3[i]) * t2
                + (-p0[i] + 3.0 * p1[i] - 3.0 * p2[i] + p3[i]) * t3);
    }
    out
}

/// Dense polyline along the spline, with the per-point scale carried through.
///
/// Endpoints are duplicated rather than wrapped: a ridge is an open curve, and
/// wrapping would bend its ends toward each other across the screen.
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
            // carries None and is left to the path.
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

/// `count` samples spaced evenly BY ARC LENGTH, not by parameter.
///
/// Even in t would bunch bars up wherever control points sit close together,
/// which on a hand-clicked ridge is exactly where the detail is - the bars
/// would crowd the interesting part and thin out across the flat stretches.
///
/// `aspect` is the output's width / height. NDC is square and the screen is
/// not, so a normal perpendicular in NDC is NOT perpendicular on screen - it
/// leans by the aspect ratio. Normals are taken in a y-up PIXEL frame instead,
/// which is also the frame the editor's preview draws in.
///
/// Returns each bar's base, its unit normal, and the scale interpolated there.
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
    let dense = densify(controls, 16);

    // Cumulative arc length, so a target length maps to a position by search.
    let mut acc = Vec::with_capacity(dense.len());
    let mut total = 0.0f32;
    acc.push(0.0f32);
    for w in dense.windows(2) {
        let (a, b) = (w[0].0, w[1].0);
        total += ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        acc.push(total);
    }

    let mut out = Vec::with_capacity(count);
    let mut cursor = 0usize;
    for i in 0..count {
        // Centre of the i-th slot, so the first and last bars sit inside the
        // curve rather than exactly on its ends.
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
        // Into the pixel frame before taking the perpendicular.
        let (dx, dy) = ((b[0] - a[0]) * aspect, b[1] - a[1]);
        let len = (dx * dx + dy * dy).sqrt();
        // Direction first, THEN flip - applied uniformly, including to an
        // angle override. Returning early from the override branch left one
        // bar pointing the other way from every neighbour, which reads as a
        // glitch rather than as a setting.
        let mut normal = if let Some(deg) = aa {
            // Degrees clockwise from up, so 0 is [0,1] and 90 is [1,0].
            let r = deg.to_radians();
            [r.sin(), r.cos()]
        } else if upright || len <= f32::EPSILON {
            // Straight rectangles rising from the path rather than leaning
            // with it.
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

/// NDC bounding box of every bar at full volume: `(min_x, min_y, max_x,
/// max_y)`.
///
/// A bar reaches `reach * scale` along its normal and is `width * scale`
/// across, so the box is the hull of both ends of every bar. This is what
/// lets the surface shrink to the path instead of claiming the whole output.
#[must_use]
pub fn bounds(samples: &[(Sample, f32)], reach: f32, width: f32) -> (f32, f32, f32, f32) {
    let (mut x0, mut y0) = (f32::MAX, f32::MAX);
    let (mut x1, mut y1) = (f32::MIN, f32::MIN);
    for (s, scale) in samples {
        let n = s.normal;
        let t = [-n[1], n[0]];
        let half = width * scale * 0.5;
        let tip = [n[0] * reach * scale, n[1] * reach * scale];
        for base in [[0.0, 0.0], tip] {
            for side in [-half, half] {
                let x = s.pos[0] + base[0] + t[0] * side;
                let y = s.pos[1] + base[1] + t[1] * side;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 > x1 { (-1.0, -1.0, 1.0, 1.0) } else { (x0, y0, x1, y1) }
}

/// Resolution of the sampled horizon. 512 buckets across an output is about
/// four pixels each at 1920, which is finer than a hand-drawn silhouette is.
pub const HORIZON_BUCKETS: usize = 512;

/// A polyline in normalised coordinates, resampled into a height-above-bottom
/// per x bucket.
///
/// Points need not be sorted or span the full width: the ends extend flat, so
/// a silhouette drawn across the middle still occludes correctly at the edges.
#[must_use]
pub fn horizon(points: &[Control]) -> Vec<f32> {
    if points.len() < 2 {
        return Vec::new();
    }
    let mut pts: Vec<(f32, f32)> = points
        .iter()
        // Stored counting UP from the bottom, which is the direction
        // gl_FragCoord.y runs, so the shader flips neither.
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

/// FNV-1a over a file's bytes, hex.
///
/// Content, not path: this wallpaper collection gets renamed and moved between
/// folders, and a filename key breaks on every one of those while a content key
/// survives them. Not cryptographic and does not need to be - it distinguishes
/// a few hundred images.
///
/// Hand-rolled rather than `DefaultHasher`, whose output std explicitly does
/// not promise to be stable across releases: a toolchain bump would silently
/// invalidate every key in the config.
#[must_use]
pub fn content_key(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:016x}", fnv1a(&bytes)))
}

/// FNV-1a 64. Split out so a known-answer test can reach it: the prime is 11
/// hex digits and a twelfth typo'd zero still produces plausible-looking
/// hashes that simply never match the ones anything else computes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The wallpaper Caelestia currently has set, if it says.
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
    /// generalises.
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
        // Evenly spaced along x.
        let gaps: Vec<f32> = s.windows(2).map(|w| w[1].0.pos[0] - w[0].0.pos[0]).collect();
        let first = gaps[0];
        for g in &gaps {
            assert!((g - first).abs() < 1e-3, "uneven spacing: {gaps:?}");
        }
    }

    /// Normals stay unit length whatever the path does; a non-unit normal
    /// scales the bar with the slope and the visualiser gets taller on hills.
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

    /// flip mirrors the normal and nothing else.
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
    /// distant stretch of ridge carry shorter bars.
    #[test]
    fn scale_interpolates_between_control_points() {
        let controls = vec![
            Control { x: 0.0, y: 0.5, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.5, scale: 0.2, angle: None },
        ];
        let s = resample(&controls, 10, false, false, 1.0);
        assert!(s[0].1 > s[9].1, "scale should fall along the path");
        assert!(s[0].1 <= 1.0 && s[9].1 >= 0.2);
        // Monotone, not jumping about.
        for w in s.windows(2) {
            assert!(w[1].1 <= w[0].1 + 1e-6, "scale not monotone");
        }
    }

    /// Known answers from the FNV reference vectors. Without these a typo in
    /// the prime is invisible: every key still looks like a hash, and the only
    /// symptom is that no curve ever matches its wallpaper.
    #[test]
    fn fnv1a_matches_the_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// A per-point angle overrides the tangent, and only where it is set.
    #[test]
    fn angle_override_beats_the_tangent() {
        let controls = vec![
            Control { x: 0.0, y: 0.9, scale: 1.0, angle: Some(90.0) },
            Control { x: 1.0, y: 0.2, scale: 1.0, angle: Some(90.0) },
        ];
        for (s, _) in resample(&controls, 8, false, false, 1.0) {
            // 90 degrees clockwise from up is straight right.
            assert!((s.normal[0] - 1.0).abs() < 1e-3, "normal {:?}", s.normal);
            assert!(s.normal[1].abs() < 1e-3);
        }
        // Without it the same slope leans.
        let plain = vec![
            Control { x: 0.0, y: 0.9, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.2, scale: 1.0, angle: None },
        ];
        assert!(resample(&plain, 8, false, false, 1.0).iter().all(|(s, _)| s.normal[0] < 0.95));
    }

    /// A bar standing straight up from the middle of the screen occupies the
    /// upper half and nothing else - that is the whole point of shrinking the
    /// surface to it.
    #[test]
    fn bounds_cover_bar_and_width() {
        let samples = vec![(Sample { pos: [0.0, 0.0], normal: [0.0, 1.0] }, 1.0)];
        let (x0, y0, x1, y1) = bounds(&samples, 0.5, 0.2);
        assert!((x0 - -0.1).abs() < 1e-6 && (x1 - 0.1).abs() < 1e-6, "width straddles the base");
        assert!((y0 - 0.0).abs() < 1e-6 && (y1 - 0.5).abs() < 1e-6, "reach sets the top");
        // The scale multiplies both, and an empty path claims everything
        // rather than collapsing to a point.
        let (_, _, _, y1) = bounds(&[(samples[0].0, 2.0)], 0.5, 0.2);
        assert!((y1 - 1.0).abs() < 1e-6);
        assert_eq!(bounds(&[], 0.5, 0.2), (-1.0, -1.0, 1.0, 1.0));
    }

    /// A flat silhouette occludes at a constant height, and one drawn across
    /// only part of the width extends flat to both edges rather than dropping
    /// to zero and letting bars show through at the sides.
    #[test]
    fn horizon_samples_and_extends_flat() {
        let pts = vec![
            Control { x: 0.3, y: 0.6, scale: 1.0, angle: None },
            Control { x: 0.7, y: 0.6, scale: 1.0, angle: None },
        ];
        let h = horizon(&pts);
        assert_eq!(h.len(), HORIZON_BUCKETS);
        // y 0.6 from the top is 0.4 from the bottom, everywhere.
        for v in &h {
            assert!((v - 0.4).abs() < 1e-3, "got {v}");
        }
        assert!(horizon(&pts[..1]).is_empty(), "one point cannot be a horizon");
        // A slope interpolates rather than stepping.
        let slope = vec![
            Control { x: 0.0, y: 1.0, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.0, scale: 1.0, angle: None },
        ];
        let h = horizon(&slope);
        assert!(h[0] < 0.01 && h[HORIZON_BUCKETS - 1] > 0.99);
        assert!((h[HORIZON_BUCKETS / 2] - 0.5).abs() < 0.01);
    }

    /// A normal has to be perpendicular ON SCREEN, not in NDC. NDC is square
    /// and a 16:9 output is not, so ignoring aspect leans every bar on a slope
    /// by a visible amount - and the editor, which works in pixels, would draw
    /// something the renderer never produced.
    #[test]
    fn normals_are_perpendicular_on_screen_not_in_ndc() {
        // Pixel slope of exactly 45 degrees on 16:9. to_ndc doubles a
        // normalised delta, so dx_ndc = 2*dx_norm and dy_ndc = 2*dy_norm; for
        // dx_ndc * aspect == dy_ndc the normalised dx must be dy * 9/16.
        let dy_norm = 0.5; // 0.75 -> 0.25
        let dx_norm = dy_norm * 9.0 / 16.0;
        let controls = vec![
            Control { x: 0.5 - dx_norm / 2.0, y: 0.75, scale: 1.0, angle: None },
            Control { x: 0.5 + dx_norm / 2.0, y: 0.25, scale: 1.0, angle: None },
        ];
        let aspect = 16.0 / 9.0;
        let (s, _) = resample(&controls, 3, false, false, aspect)[1];
        // Perpendicular to a 45-degree pixel slope is 45 degrees the other way.
        assert!(
            (s.normal[0].abs() - s.normal[1].abs()).abs() < 0.02,
            "not 45 degrees in pixel space: {:?}",
            s.normal
        );
        // The same path at aspect 1.0 gives a measurably different lean, which
        // is precisely the bug this guards.
        let (flat, _) = resample(&controls, 3, false, false, 1.0)[1];
        assert!(
            (flat.normal[0] - s.normal[0]).abs() > 0.05,
            "aspect made no difference, so it is not being applied"
        );
    }

    /// flip must reach EVERY bar, including one carrying an angle override.
    /// It did not, and the result was a single bar pointing the opposite way
    /// from all its neighbours.
    #[test]
    fn flip_applies_to_angle_overrides_too() {
        let controls = vec![
            Control { x: 0.0, y: 0.5, scale: 1.0, angle: Some(0.0) },
            Control { x: 1.0, y: 0.5, scale: 1.0, angle: Some(0.0) },
        ];
        for (s, _) in resample(&controls, 6, true, false, 1.0) {
            assert!((s.normal[1] + 1.0).abs() < 1e-3, "override ignored flip: {:?}", s.normal);
        }
        // And upright flips as well.
        let plain = vec![
            Control { x: 0.0, y: 0.4, scale: 1.0, angle: None },
            Control { x: 1.0, y: 0.7, scale: 1.0, angle: None },
        ];
        for (s, _) in resample(&plain, 6, true, true, 1.0) {
            assert_eq!(s.normal, [0.0, -1.0]);
        }
    }

    /// Fewer than two controls is a config mistake, not a crash.
    #[test]
    fn degenerate_input_yields_usable_samples() {
        assert_eq!(resample(&[], 4, false, false, 1.0).len(), 4);
        let one = vec![Control { x: 0.5, y: 0.5, scale: 1.0, angle: None }];
        let s = resample(&one, 3, false, false, 1.0);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].0.normal, [0.0, 1.0]);
    }
}
