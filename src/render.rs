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
}

impl DamageMap {
    pub(crate) fn new(bar_count: u32, bar_width: f32, bar_stride: f32, width: u32, height: u32) -> Self {
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
        Self { bucket_of, bucket_x, half_h: height as f32 * 0.5, height: height as i32 }
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
        if x1 > x0 && y1 > y0 {
            out[n..n + 4].copy_from_slice(&[x0, y0, x1 - x0, y1 - y0]);
            n += 4;
        }
    }
    n
}

