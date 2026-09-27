//! Damage tracking: which pixels actually changed between frames.

use super::*;

/// The affine map from OUTPUT NDC into a surface that covers only
/// `(left, top, w, h)` of that output, as `(scale, offset, occ_map)`.
///
/// Margins count from the top and NDC counts from the bottom, so the vertical
/// half is the one that is easy to get wrong. Separated out from configure()
/// to be testable: a wrong sign here puts the bars off screen with nothing to
/// look at
pub(crate) fn surface_map(out: (u32, u32), rect: (u32, u32, u32, u32)) -> ([f32; 2], [f32; 2], [f32; 4]) {
    let (ow, oh) = (out.0 as f32, out.1 as f32);
    let (l, t, sw, sh) = (rect.0 as f32, rect.1 as f32, rect.2 as f32, rect.3 as f32);
    let bottom = oh - t - sh;
    (
        [ow / sw, oh / sh],
        [(ow - sw - 2.0 * l) / sw, (oh - sh - 2.0 * bottom) / sh],
        [l / ow, bottom / oh, sw / ow, sh / oh],
    )
}

/// gl_FragCoord to reveal-image coordinates, as `[a, b, c, d]` for
/// `uv = frag * (a, b) + (c, d)`: the surface's place on the output, the
/// output's y running down where the fragment's runs up, and the image
/// cropped onto the output by cover, as the wallpaper itself is
pub(crate) fn reveal_map(
    image: (u32, u32),
    output: (u32, u32),
    (left, top): (u32, u32),
    surface_height: u32,
) -> [f32; 4] {
    let fit = curve::Fit::cover(image, output);
    let (ow, oh) = (output.0.max(1) as f32, output.1.max(1) as f32);
    let (left, bottom) = (left as f32, (top + surface_height) as f32);
    [
        1.0 / (ow * fit.sx),
        -1.0 / (oh * fit.sy),
        (left / ow - fit.ox) / fit.sx,
        (bottom / oh - fit.oy) / fit.sy,
    ]
}

/// Where a row of bars sits, as fractions of the output, defaults filled in
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BarPlacement {
    pub left: f32,
    pub span: f32,
    /// Where the bars stand, from the top
    pub baseline: f32,
    /// How tall a full-volume bar is
    pub reach: f32,
    pub down: bool,
}

impl BarPlacement {
    /// Unset means the full width along the bottom, growing up. Clamped here
    /// so a typo cannot push the row off the output, where the compositor
    /// clips it and it silently half-disappears
    pub(crate) fn from_config(b: &BarConfig) -> Self {
        let left = b.left.unwrap_or(0.0).clamp(0.0, 0.99);
        // Floored before use as a bound: 1.0 - 0.99 is just under 0.01 in
        // f32, and a clamp whose minimum passes its maximum panics
        let room = (1.0 - left).max(0.01);
        Self {
            left,
            span: b.span.unwrap_or(1.0).clamp(0.01, room),
            baseline: b.baseline.unwrap_or(1.0).clamp(0.0, 1.0),
            reach: b.max_height.unwrap_or(1.0).clamp(0.01, 1.0),
            down: b.grow == Some(Grow::Down),
        }
    }
}

/// The row's surface on a `w` x `h` output, in logical pixels
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Band {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    /// The default row: full width, standing on the bottom edge. Anchored to
    /// that edge rather than placed by margins, so it follows a resize alone
    pub bottom_row: bool,
}

pub(crate) fn bar_band(p: &BarPlacement, w: u32, h: u32) -> Band {
    let (wf, hf) = (w as f32, h as f32);
    let width = ((p.span * wf).round() as u32).clamp(1, w.max(1));
    let left = ((p.left * wf).round() as u32).min(w.saturating_sub(width));
    let height = ((p.reach * hf).ceil() as u32).clamp(1, h.max(1));
    let base = (p.baseline * hf).round();
    let top = if p.down { base } else { base - height as f32 };
    let top = (top.max(0.0) as u32).min(h.saturating_sub(height));
    let bottom_row = !p.down && left == 0 && width == w && top + height == h;
    Band { left, top, width, height, bottom_row }
}

/// Everything about damage that depends only on the surface size and the bar
/// layout, and therefore belongs on a configure rather than in a frame
///
/// A bar's x columns cannot move between frames, and neither can which bucket
/// it falls in. Computing them per bar per frame was an integer division, two
/// int-to-float conversions and five multiplies per bar, 45 times a second, for
/// answers that were identical every time
pub(crate) struct DamageMap {
    /// Bar index -> bucket, as a table rather than `i * BUCKETS / bars`.
    bucket_of: Box<[u8]>,
    /// Each bucket's pixel x-range, already widened and clamped
    bucket_x: [(i32, i32); DAMAGE_BUCKETS],
    /// Half the surface height, the one NDC-to-pixel factor still needed
    half_h: f32,
    height: i32,
    /// Bars hang from the top edge, so a height counts down from there
    down: bool,
}

impl DamageMap {
    pub(crate) fn new(
        bar_count: u32,
        bar_width: f32,
        bar_stride: f32,
        (width, height): (u32, u32),
        down: bool,
    ) -> Self {
        const _: () = assert!(DAMAGE_BUCKETS <= u8::MAX as usize, "bucket must fit a u8");
        let bars = bar_count as usize;
        let (w, iw) = (width as f32, width as i32);
        let mut bucket_of = vec![0u8; bars].into_boxed_slice();
        let mut bucket_x = [(i32::MAX, i32::MIN); DAMAGE_BUCKETS];
        for (i, slot) in bucket_of.iter_mut().enumerate() {
            let b = (i * DAMAGE_BUCKETS / bars.max(1)) & DAMAGE_MASK;
            *slot = b as u8;
            // Widened a pixel each way here, once, rather than per frame: the
            // NDC-to-pixel conversion rounds, and a rect one pixel short leaves
            // a stale line of the old bar on screen
            let x0 = (bar_stride * i as f32 * 0.5 * w).floor() as i32 - 1;
            let x1 = ((bar_stride * i as f32 + bar_width) * 0.5 * w).ceil() as i32 + 1;
            bucket_x[b].0 = bucket_x[b].0.min(x0.clamp(0, iw));
            bucket_x[b].1 = bucket_x[b].1.max(x1.clamp(0, iw));
        }
        Self { bucket_of, bucket_x, half_h: height as f32 * 0.5, height: height as i32, down }
    }
}

/// Rectangles covering everything that moved, in EGL surface coordinates
///
/// Free rather than a method so it can be tested: it is pure arithmetic over
/// two height arrays, and an under-reported rect leaves a stale strip of the
/// old bar on screen that no test touching GL would catch either
///
/// The per-bar loop is a table lookup and four min/max with no arithmetic at
/// all; heights stay in NDC until the eight buckets are converted at the end,
/// so the conversion runs eight times instead of once per bar
///
/// EGL wants surface coordinates with the origin bottom-left, which is the
/// direction bar heights already run, so nothing has to be flipped
///
/// Sound because draw() repaints the WHOLE buffer every frame: the pixels
/// outside these rects are bit-identical to what the compositor already holds,
/// so telling it to keep them is true. An app rendering incrementally into an
/// aged back buffer would need EGL_BUFFER_AGE_EXT here; this one does not
pub(crate) fn damage_rects(frame: &[u8], prev: &[u8], map: &DamageMap, out: &mut [egl::Int]) -> usize {
    // Raw samples, not NDC: the comparison and the running extremes are all
    // integer, and only the eight survivors are ever converted
    let mut lo = [u16::MAX; DAMAGE_BUCKETS];
    let mut hi = [u16::MIN; DAMAGE_BUCKETS];
    let (new_s, old_s) = (frame.as_chunks::<2>().0, prev.as_chunks::<2>().0);
    for ((n, o), &b) in new_s.iter().zip(old_s).zip(map.bucket_of.iter()) {
        let (new, old) = (u16::from_le_bytes(*n), u16::from_le_bytes(*o));
        if new == old {
            continue;
        }
        // Masked, not indexed raw: the index is a u8 and the compiler cannot
        // prove it is under DAMAGE_BUCKETS, so a plain index puts a compare,
        // a branch and a panic path in this loop once per bar per frame
        let b = b as usize & DAMAGE_MASK;
        lo[b] = lo[b].min(new).min(old);
        hi[b] = hi[b].max(new).max(old);
    }

    let mut n = 0;
    for (b, (&lr, &hr)) in lo.iter().zip(hi.iter()).enumerate() {
        if lr > hr {
            continue; // nothing in this bucket moved
        }
        let (l, h) = (
            fma(f32::from(lr), BAR_NDC_SCALE, -1.0),
            fma(f32::from(hr), BAR_NDC_SCALE, -1.0),
        );
        let (x0, x1) = map.bucket_x[b];
        let y0 = (((l + 1.0) * map.half_h).floor() as i32 - 1).clamp(0, map.height);
        let y1 = (((h + 1.0) * map.half_h).ceil() as i32 + 1).clamp(0, map.height);
        let (y0, y1) = if map.down { (map.height - y1, map.height - y0) } else { (y0, y1) };
        if x1 > x0 && y1 > y0 {
            out[n..n + 4].copy_from_slice(&[x0, y0, x1 - x0, y1 - y0]);
            n += 4;
        }
    }
    n
}

