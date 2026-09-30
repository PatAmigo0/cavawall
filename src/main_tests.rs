//! Tests for the binary crate's own items.
#![allow(clippy::float_cmp, clippy::suboptimal_flops, clippy::manual_midpoint, reason = "tests compare exact values and keep their maths independent of the code they check")]

    use super::*;

    /// One raw sample to NDC, as the vertex shader does
    const BAR_NDC_SCALE: f32 = 2.0 * BAR_UNIT;

    /// A circle closes, so it has as many gaps as bars - one more than a row
    /// of the same count. Getting that wrong leaves a visible seam at bar 0 or
    /// overlaps it with the last bar, and no GL test would catch either
    #[test]
    fn circle_slots_tile_the_full_turn() {
        for bars in [1u32, 8, 76, 255] {
            for gap in [0.0f32, 0.1, 0.5] {
                let step = std::f32::consts::TAU / bars as f32;
                let bar = step / (1.0 + gap);
                // Bars plus gaps come back to exactly one turn
                let total = (bar + bar * gap) * bars as f32;
                assert!(
                    (total - std::f32::consts::TAU).abs() < 1e-4,
                    "bars={bars} gap={gap} covered {total}"
                );
                // And the gap really is that fraction of the bar, as on a row
                assert!((bar * gap - (step - bar)).abs() < 1e-5, "gap ratio wrong");
            }
        }
    }

    /// Defaults and clamps, because an out-of-range inner_radius is silently
    /// invisible rather than loud: 1.0 leaves no span for a bar to grow into
    #[test]
    fn circle_geom_clamps_its_config() {
        let wild = CircleConfig {
            diameter: Some(0),
            inner_radius: Some(2.5),
            inner_alpha: Some(-1.0),
            outer_alpha: Some(9.0),
            bars: None,
            anchor: None,
            margin_x: None,
            margin_y: None,
            position: None,
        };
        let g = CircleGeom::from_config(Some(&wild));
        assert_eq!(g.diameter, 16, "diameter floored");
        assert_eq!(g.inner_radius, 0.95, "inner_radius leaves a span");
        assert_eq!(g.inner_alpha, 0.0);
        assert_eq!(g.outer_alpha, 1.0);

        let d = CircleGeom::from_config(None);
        assert_eq!(d.anchor, CircleAnchor::Center, "no section means centred");
        assert_eq!(d.margin, (0, 0));
        assert!(d.inner_radius > 0.0 && d.inner_radius < 1.0);
    }

    /// Placement is the one part a screenshot checks badly: a circle 20px off
    /// still looks fine, and only looks wrong on the other machine.
    /// The box's own corners have to land on the surface's edges: that is what
    /// "the surface is the bounding box" means, and it is the whole contract
    /// the shader relies on. A wrong sign here puts the bars off screen, with
    /// nothing left to look at to find out why
    #[test]
    fn surface_map_puts_the_box_on_the_edges() {
        // Hyprland granted exactly this for a ridgeline across the upper third
        // of a 1920x1080 output
        let (out, rect) = ((1920u32, 1080u32), (1031u32, 201u32, 857u32, 244u32));
        let (scale, offset, occ) = surface_map(out, rect);
        let map = |p: [f32; 2]| [p[0] * scale[0] + offset[0], p[1] * scale[1] + offset[1]];
        let (x0, x1) = (2.0 * 1031.0 / 1920.0 - 1.0, 2.0 * (1031.0 + 857.0) / 1920.0 - 1.0);
        // NDC counts up, the margin counts down, so top and bottom swap
        let (y1, y0) = (1.0 - 2.0 * 201.0 / 1080.0, 1.0 - 2.0 * (201.0 + 244.0) / 1080.0);
        for (p, want) in [([x0, y0], [-1.0, -1.0]), ([x1, y1], [1.0, 1.0])] {
            let got = map(p);
            assert!((got[0] - want[0]).abs() < 1e-4, "x: {got:?} want {want:?}");
            assert!((got[1] - want[1]).abs() < 1e-4, "y: {got:?} want {want:?}");
        }
        // A fragment at the surface's bottom-left sits at the box's
        // bottom-left on the output, both counted from the bottom
        assert!((occ[0] - 1031.0 / 1920.0).abs() < 1e-4);
        assert!((occ[1] - (1080.0 - 201.0 - 244.0) / 1080.0).abs() < 1e-4);
        // A surface that is the whole output maps to itself
        assert_eq!(
            surface_map(out, (0, 0, 1920, 1080)),
            ([1.0, 1.0], [0.0, 0.0], [0.0, 0.0, 1.0, 1.0])
        );
    }

    #[test]
    fn circle_anchors_land_where_they_say() {
        let geom = |a: CircleAnchor, mx, my| CircleGeom {
            diameter: 300,
            inner_radius: 0.35,
            inner_alpha: 1.0,
            outer_alpha: 1.0,
            anchor: a,
            margin: (mx, my),
            position: None,
        };
        // 1920x1080 output, 300px circle: 1620 and 780 of free space
        let (w, h, d) = (1920, 1080, 300);
        assert_eq!(geom(CircleAnchor::Center, 0, 0).margins_for(d, w, h), (390, 810));
        assert_eq!(geom(CircleAnchor::TopLeft, 40, 60).margins_for(d, w, h), (60, 40));
        assert_eq!(geom(CircleAnchor::TopRight, 40, 60).margins_for(d, w, h), (60, 1580));
        assert_eq!(geom(CircleAnchor::BottomRight, 40, 60).margins_for(d, w, h), (720, 1580));
        // Anchors that centre one axis ignore that axis's margin
        assert_eq!(geom(CircleAnchor::Top, 999, 25).margins_for(d, w, h), (25, 810));
        assert_eq!(geom(CircleAnchor::Left, 25, 999).margins_for(d, w, h), (390, 25));
        // A margin bigger than the output cannot push it off-screen
        let far = geom(CircleAnchor::TopLeft, 9999, 9999).margins_for(d, w, h);
        assert_eq!(far, (780, 1620), "clamped to the free space");
    }

    /// upright keeps every bar vertical whatever the path does - the "plain
    /// rectangles standing on the ridge" look, as against leaning with it
    #[test]
    fn upright_ignores_the_slope() {
        let controls = vec![
            curve::Control { x: 0.0, y: 0.9, scale: 1.0, angle: None },
            curve::Control { x: 0.5, y: 0.2, scale: 1.0, angle: None },
            curve::Control { x: 1.0, y: 0.8, scale: 1.0, angle: None },
        ];
        for (s, _) in curve::resample(&controls, 16, false, true, 1.0) {
            assert_eq!(s.normal, [0.0, 1.0], "upright bar leaned");
        }
        // And the slope still moves them when upright is off
        let leaned = curve::resample(&controls, 16, false, false, 1.0);
        assert!(leaned.iter().any(|(s, _)| s.normal[0].abs() > 0.1), "nothing leaned");
    }

    /// The vertex shader's own placement, replicated: it is the only consumer
    /// of `bar_geometry`, and nothing else checks that the two agree
    fn bar_edges(bar_count: u32, gap: f32) -> Vec<(f32, f32)> {
        let (width, stride) = bar_geometry(bar_count, gap);
        (0..bar_count)
            .map(|i| {
                let left = stride * i as f32 - 1.0;
                (left, left + width)
            })
            .collect()
    }

    #[test]
    fn bars_tile_ndc_left_to_right() {
        let e = bar_edges(4, 0.0);
        assert!((e[0].0 - -1.0).abs() < 1e-6, "first bar starts at -1");
        assert!((e[3].1 - 1.0).abs() < 1e-6, "last bar ends at +1");
        for pair in e.windows(2) {
            assert!((pair[0].1 - pair[1].0).abs() < 1e-6, "no gap means touching");
        }
    }

    #[test]
    fn gap_is_a_fraction_of_bar_width() {
        let e = bar_edges(2, 0.5);
        let (bar, gap) = (e[0].1 - e[0].0, e[1].0 - e[0].1);
        assert!((gap - bar * 0.5).abs() < 1e-6, "bar {bar}, gap {gap}");
    }

    /// A single bar fills the surface rather than dividing by the zero gaps
    #[test]
    fn one_bar_spans_the_whole_surface() {
        let e = bar_edges(1, 0.1);
        assert!((e[0].0 - -1.0).abs() < 1e-6 && (e[0].1 - 1.0).abs() < 1e-6, "{e:?}");
    }

    /// The quad must be a strip in the order the shader assumes: corner.x
    /// selects the left or right edge, corner.y the bottom or the top. These
    /// are the same two triangles the index buffer used to spell out
    #[test]
    fn unit_quad_is_a_bottom_anchored_strip() {
        let corners = UNIT_QUAD.as_chunks::<2>().0;
        assert_eq!(
            corners,
            &[[0.0, 1.0], [1.0, 1.0], [0.0, 0.0], [1.0, 0.0]],
            "top-left, top-right, bottom-left, bottom-right"
        );
        for c in corners {
            assert!(c[0] == 0.0 || c[0] == 1.0, "corner.x is an edge selector");
            assert!(c[1] == 0.0 || c[1] == 1.0, "corner.y is a bottom/top selector");
        }
    }

    /// Damage must COVER every pixel that changed. Under-reporting leaves a
    /// stale strip of the old bar on screen, which is invisible to any test
    /// that only checks the rects look reasonable - so these check containment
    /// against the span each bar actually moved through
    /// Cases are written in NDC because that is what a bar's height means on
    /// screen. A frame carries cava's raw samples, so they go back into that
    /// domain here
    fn raw(hs: &[f32]) -> Vec<u8> {
        hs.iter()
            .flat_map(|ndc| (((ndc + 1.0) / BAR_NDC_SCALE) as u16).to_le_bytes())
            .collect()
    }

    fn rects_of(heights: &[f32], prev: &[f32], w: u32, h: u32) -> Vec<[i32; 4]> {
        let (bw, stride) = bar_geometry(heights.len() as u32, 0.0);
        let map = DamageMap::new(heights.len() as u32, bw, stride, (w, h), false, false);
        let mut out = [0i32; DAMAGE_BUCKETS * 4];
        let n = damage_rects(&raw(heights), &raw(prev), &map, &mut out);
        out[..n].as_chunks::<4>().0.to_vec()
    }

    /// The pixel row a height sits on, the same mapping the shader uses
    fn row(ndc: f32, h: u32) -> i32 {
        ((ndc + 1.0) * 0.5 * h as f32) as i32
    }

    #[test]
    fn nothing_moving_damages_nothing() {
        let hs = [0.0, -0.5, 0.25, 1.0];
        assert!(rects_of(&hs, &hs, 1920, 703).is_empty());
    }

    #[test]
    fn a_moved_bar_is_fully_covered() {
        let prev = [-1.0, -1.0, -1.0, -1.0];
        let new = [-1.0, 0.5, -1.0, -1.0]; // bar 1 rises from the floor
        let rects = rects_of(&new, &prev, 1920, 703);
        assert_eq!(rects.len(), 1, "one bucket moved, one rect");
        let [x, y, rw, rh] = rects[0];
        let (lo, hi) = (row(-1.0, 703), row(0.5, 703));
        assert!(y <= lo && y + rh >= hi, "covers the span the bar swept: {rects:?}");
        // And it is a slice of the width, not the whole band
        assert!(rw < 1920 / 2, "one bar of four should not span the surface: {rw}");
        assert!(x >= 0 && x + rw <= 1920 && y >= 0 && y + rh <= 703, "in bounds");
    }

    #[test]
    fn a_falling_bar_covers_the_same_span_as_a_rising_one() {
        let a = rects_of(&[-1.0, 0.5, -1.0, -1.0], &[-1.0, -1.0, -1.0, -1.0], 1920, 703);
        let b = rects_of(&[-1.0, -1.0, -1.0, -1.0], &[-1.0, 0.5, -1.0, -1.0], 1920, 703);
        assert_eq!(a, b, "direction must not change what is damaged");
    }

    /// The invariant that matters: EVERY bar that moved is fully inside some
    /// rect. Anything less leaves a stale strip of the old bar on screen. A bar
    /// that did NOT move needs no coverage, which is the whole point
    #[test]
    fn every_moved_bar_is_covered_and_the_rect_count_stays_capped() {
        let (w, h) = (1920u32, 703u32);
        let bars = 76usize;
        let (bw, stride) = bar_geometry(bars as u32, 0.0);
        let prev = vec![-1.0f32; bars];
        // i % 7 == 0 leaves that bar exactly where it was, so the input mixes
        // moved and unmoved bars the way a real frame does
        let new: Vec<f32> = (0..bars).map(|i| -1.0 + (i % 7) as f32 * 0.2).collect();
        let rects = rects_of(&new, &prev, w, h);
        assert!(!rects.is_empty() && rects.len() <= DAMAGE_BUCKETS, "{}", rects.len());

        let mut moved = 0;
        for i in 0..bars {
            if new[i] == prev[i] {
                continue;
            }
            moved += 1;
            let x0 = (stride * i as f32 * 0.5 * w as f32) as i32;
            let x1 = ((stride * i as f32 + bw) * 0.5 * w as f32).ceil() as i32;
            let (lo, hi) = (row(prev[i].min(new[i]), h), row(prev[i].max(new[i]), h));
            assert!(
                rects.iter().any(|r| {
                    r[0] <= x0 && r[0] + r[2] >= x1 && r[1] <= lo && r[1] + r[3] >= hi
                }),
                "bar {i} moved {:?}->{:?} (x {x0}..{x1}, y {lo}..{hi}) is not covered by {rects:?}",
                prev[i], new[i]
            );
        }
        assert!(moved > 60, "the fixture should move most bars, moved {moved}");
    }

    /// Nothing in the damage maths may assume this panel. The surface size
    /// comes from the compositor's configure and the band from
    /// `bars.max_height`, so the same frame has to work at any resolution -
    /// these are three real ones plus the max_height fractions they imply
    #[test]
    fn damage_is_resolution_independent() {
        let bars = 40usize;
        let (bw, stride) = bar_geometry(bars as u32, 0.1);
        let prev = vec![-1.0f32; bars];
        let new: Vec<f32> = (0..bars).map(|i| -1.0 + (i % 5) as f32 * 0.3).collect();

        for (w, full_h, frac) in [(1920u32, 1080u32, 0.65f32), (2560, 1440, 0.5), (1366, 768, 1.0)] {
            let h = (full_h as f32 * frac).ceil() as u32;
            let map = DamageMap::new(bars as u32, bw, stride, (w, h), false, false);
            let mut out = [0i32; DAMAGE_BUCKETS * 4];
            let n = damage_rects(&raw(&new), &raw(&prev), &map, &mut out);
            let rects: Vec<&[i32]> = out[..n].as_chunks::<4>().0.iter().map(|r| &r[..]).collect();
            assert!(!rects.is_empty(), "{w}x{h}: nothing damaged");
            for r in &rects {
                assert!(r[0] >= 0 && r[1] >= 0, "{w}x{h}: {r:?}");
                assert!(r[0] + r[2] <= w as i32, "{w}x{h}: past the right edge {r:?}");
                assert!(r[1] + r[3] <= h as i32, "{w}x{h}: past the top {r:?}");
            }
            // Every moved bar still covered, at this size too
            for i in 0..bars {
                if new[i] == prev[i] {
                    continue;
                }
                let x0 = (stride * i as f32 * 0.5 * w as f32) as i32;
                let x1 = ((stride * i as f32 + bw) * 0.5 * w as f32).ceil() as i32;
                let (lo, hi) = (row(prev[i].min(new[i]), h), row(prev[i].max(new[i]), h));
                assert!(
                    rects.iter().any(|r| r[0] <= x0 && r[0] + r[2] >= x1
                        && r[1] <= lo && r[1] + r[3] >= hi),
                    "{w}x{h}: bar {i} uncovered"
                );
            }
        }
    }

    /// Bars at the very top and bottom must not produce rects outside the
    /// surface, which the one-pixel widening could otherwise do
    #[test]
    fn rects_stay_inside_the_surface_at_the_extremes() {
        for (a, b) in [(-1.0f32, 1.0f32), (1.0, -1.0)] {
            let rects = rects_of(&[a, a], &[b, b], 1920, 703);
            for r in &rects {
                assert!(r[0] >= 0 && r[1] >= 0, "{r:?}");
                assert!(r[0] + r[2] <= 1920 && r[1] + r[3] <= 703, "{r:?}");
                assert!(r[2] > 0 && r[3] > 0, "degenerate rect {r:?}");
            }
        }
    }

    /// draw() and audio_returned() must not be able to disagree about what silence
    /// is: they are the park and unpark halves of one decision. This pins the
    /// shared predicate to the f32 threshold draw() used to apply on its own
    #[test]
    fn silence_boundary_matches_the_f32_threshold() {
        let frame = |n: u16| n.to_le_bytes();
        let loudest_silent = SILENCE_RAW;
        assert!(is_silent(&frame(loudest_silent)));
        assert!(!is_silent(&frame(loudest_silent + 1)));
        assert!(f32::from(loudest_silent) / 65530.0 < SILENCE_THRESHOLD);
        assert!(f32::from(loudest_silent + 1) / 65530.0 >= SILENCE_THRESHOLD);
    }

    /// One loud bar in an otherwise quiet frame keeps the visualiser awake
    #[test]
    fn a_single_loud_bar_is_not_silence() {
        let mut frame = [0u8; 8];
        frame[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(!is_silent(&frame));
        assert!(is_silent(&[0u8; 8]));
    }

    /// Full scale maps to the top of the surface and zero to the bottom, by
    /// the same divisor the vertex fetch uses when it normalises a u16
    ///
    /// The two have to agree exactly. The GPU places the bar and the CPU
    /// decides which pixels to declare damaged; a different divisor on either
    /// side leaves a strip of the old bar on screen
    #[test]
    fn bar_height_spans_ndc() {
        let ndc = |n: u16| fma(f32::from(n), BAR_NDC_SCALE, -1.0);
        assert!((ndc(0) - -1.0).abs() < 1e-6);
        assert!((ndc(u16::MAX) - 1.0).abs() < 1e-6);
        for n in [1u16, 327, 1000, 32768, 65000] {
            // What the vertex fetch does: value / 65535, then the shader's
            // own `height * 2.0 - 1.0`
            let fetched = f32::from(n) / f32::from(u16::MAX) * 2.0 - 1.0;
            assert!((ndc(n) - fetched).abs() < 1e-6, "{n}: {} vs {fetched}", ndc(n));
        }
    }

    fn placement(left: f32, span: f32, baseline: f32, reach: f32, down: bool) -> BarPlacement {
        BarPlacement { left, span, baseline, reach, down, mirror: false }
    }

    /// No placement set is the row cavawall always drew: full width, standing
    /// on the bottom edge, anchored there so it follows a resize by itself
    #[test]
    fn the_default_row_is_the_bottom_band() {
        let cfg: BarConfig = toml::from_str("amount = 8\ngap = 0.1\nmax_height = 0.65").expect("parses");
        let b = bar_band(&BarPlacement::from_config(&cfg), 1920, 1080);
        assert_eq!((b.left, b.top, b.width, b.height), (0, 1080 - 702, 1920, 702));
        assert!(b.bottom_row);
    }

    #[test]
    fn a_placed_row_lands_where_it_says_and_stays_on_the_output() {
        // A third of the width, centred, standing on the middle of the screen
        let b = bar_band(&placement(1.0 / 3.0, 1.0 / 3.0, 0.5, 0.2, false), 1920, 1080);
        assert_eq!((b.left, b.top, b.width, b.height), (640, 540 - 216, 640, 216));
        assert!(!b.bottom_row);
        // Hanging from the top edge
        let b = bar_band(&placement(0.0, 1.0, 0.0, 0.3, true), 1920, 1080);
        assert_eq!((b.top, b.height), (0, 324));
        assert!(!b.bottom_row, "a hanging row is placed, not bottom-anchored");
        // A baseline too high for the reach is pulled down until it fits
        let b = bar_band(&placement(0.0, 1.0, 0.1, 0.5, false), 1920, 1080);
        assert_eq!(b.top, 0);
        // And too low for a hanging row is pushed up
        let b = bar_band(&placement(0.9, 0.5, 0.9, 0.5, true), 1920, 1080);
        assert_eq!(b.top + b.height, 1080);
        assert!(b.left + b.width <= 1920);
    }

    /// Wild values are clamped rather than trusted: `left = 1` once meant a
    /// clamp whose minimum passed its maximum, which panics
    #[test]
    fn placement_clamps_what_it_is_given() {
        let cfg: BarConfig =
            toml::from_str("amount = 8\ngap = 0.1\nleft = 1.0\nspan = 5.0\nbaseline = -2.0").expect("parses");
        let p = BarPlacement::from_config(&cfg);
        assert!(p.left <= 0.99 && p.left + p.span <= 1.0 + 1e-6, "{p:?}");
        assert_eq!(p.baseline, 0.0);
        let b = bar_band(&p, 1920, 1080);
        assert!(b.width >= 1 && b.left + b.width <= 1920);
    }

    /// A circle placed by hand is centred on its point, and still kept whole
    #[test]
    fn a_positioned_circle_is_centred_on_its_point() {
        let mut g = CircleGeom::from_config(None);
        g.position = Some((0.25, 0.5));
        assert_eq!(g.margins_for(200, 1920, 1080), (440, 380));
        g.position = Some((1.0, 0.0));
        assert_eq!(g.margins_for(200, 1920, 1080), (0, 1720), "clamped onto the output");
    }

    /// Mirrored, the band is twice the reach and centred on its baseline
    #[test]
    fn a_mirrored_row_is_centred_on_its_line() {
        let p = BarPlacement { left: 0.0, span: 1.0, baseline: 0.5, reach: 0.2, down: false, mirror: true };
        let b = bar_band(&p, 1920, 1080);
        assert_eq!((b.top, b.height), (540 - 216, 432));
        assert!(!b.bottom_row);
    }

    /// Mirrored, a moved bucket reports its whole column: the bar reaches
    /// both ways from the middle
    #[test]
    fn a_mirrored_row_damages_whole_columns() {
        let (w, h) = (1000u32, 100u32);
        let (bw, stride) = bar_geometry(4, 0.0);
        let map = DamageMap::new(4, bw, stride, (w, h), false, true);
        let mut out = [0; DAMAGE_BUCKETS * 4];
        let n = damage_rects(&raw(&[0.0, -1.0, -1.0, -1.0]), &raw(&[-1.0; 4]), &map, &mut out);
        assert_eq!(n, 4);
        assert_eq!((out[1], out[3]), (0, h as i32));
    }

    /// A hanging row counts its damage down from the top edge
    #[test]
    fn a_hanging_row_damages_from_the_top() {
        let (w, h) = (1000u32, 100u32);
        let (bw, stride) = bar_geometry(4, 0.0);
        let map = DamageMap::new(4, bw, stride, (w, h), true, false);
        let mut out = [0; DAMAGE_BUCKETS * 4];
        // NDC heights: -1 is silence, 0 is half the band
        let n = damage_rects(&raw(&[0.0, -1.0, -1.0, -1.0]), &raw(&[-1.0; 4]), &map, &mut out);
        assert_eq!(n, 4, "one bar moved, one rect");
        let (y, height) = (out[1], out[3]);
        assert_eq!(y + height, h as i32, "reaches the top edge, where the bar hangs from");
        assert!((45..=50).contains(&y), "and down past its tip: y={y}");
    }

    /// The reveal map has to put a fragment on the texel under it: with the
    /// surface being the whole output and the image output-shaped, the four
    /// corners of the surface land on the image's corners, y flipped
    #[test]
    fn the_reveal_map_lands_fragments_on_their_texels() {
        let uv = |m: [f32; 4], x: f32, y: f32| [x * m[0] + m[2], y * m[1] + m[3]];
        let m = reveal_map((1920, 1080), (1920, 1080), (0, 0), 1080);
        assert_eq!(uv(m, 0.0, 0.0), [0.0, 1.0], "bottom left of the surface is the image's bottom left");
        assert_eq!(uv(m, 1920.0, 1080.0), [1.0, 0.0]);
        // A band along the bottom samples only the bottom of the image
        let m = reveal_map((1920, 1080), (1920, 1080), (0, 1080 - 108), 108);
        let top = uv(m, 0.0, 108.0);
        assert!((top[1] - 0.9).abs() < 1e-6, "the band's top edge is 90% down: {top:?}");
        // A 16:9 image on a 16:10 output is cropped at the sides, as the
        // wallpaper is: the output's left edge is inside the image
        let m = reveal_map((1920, 1080), (1920, 1200), (0, 0), 1200);
        let left = uv(m, 0.0, 600.0);
        assert!(left[0] > 0.0 && (left[1] - 0.5).abs() < 1e-5, "{left:?}");
    }
