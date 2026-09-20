use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub general: GeneralConfig,
    pub bars: BarConfig,
    pub colors: HashMap<String, ConfigColor>,
    pub smoothing: SmoothingConfig,
    /// Absent means "follow nothing".
    pub scheme: Option<SchemeConfig>,
    /// Only read when `general.mode` is `Circle`; absent means every default
    pub circle: Option<CircleConfig>,
    /// Only read when `general.mode` is `Curve`. Keyed by wallpaper content
    /// hash; no entry for the current wallpaper means fall back to bars rather
    /// than draw a path authored for a different image
    pub curves: Option<HashMap<String, CurveConfig>>,
}

/// Which shape the bars are arranged into
///
/// Startup-only, like the bar count: each mode is its own GL program with its
/// own uniforms and asks for different surface geometry. Changing it re-execs
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Left-to-right along the bottom of the output
    #[default]
    Bars,
    /// Radiating from the centre of a square surface
    Circle,
    /// Along an authored path, each bar on the path's normal
    Curve,
}

/// One authored path, tied to the wallpaper it was drawn against
///
/// Keyed by the wallpaper's CONTENT hash, not its filename: a hash survives a
/// rename or a move of the collection
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CurveConfig {
    /// Control points in normalised image coordinates, x and y both 0..1,
    /// origin top left. An optional third component scales the bars there,
    /// which is how a distant stretch of ridge carries shorter, thinner ones
    ///
    /// The single-path shorthand; several disconnected stretches write a
    /// `[[curves.<key>.path]]` block each
    pub points: Option<Vec<Vec<f32>>>,
    /// How far a full-volume bar reaches along the normal, as a fraction of
    /// the output height
    pub height: Option<f32>,
    /// Bar width as a fraction of the output width, before the per-point
    /// scale is applied
    pub width: Option<f32>,
    /// Flip which side of the path the bars grow toward
    pub flip: Option<bool>,
    /// Keep bars vertical instead of turning them onto the path's normal.
    /// Straight rectangles rising from the ridge rather than leaning with it
    pub upright: Option<bool>,
    /// Several disconnected paths, each with its own shape and its own bar
    /// geometry - two ridges at different distances want different reaches.
    /// Present, it replaces the fields above; absent, they are the one path
    pub path: Option<Vec<PathConfig>>,
    /// Bar count for this mode only, shared across every path and split
    /// between them by length. A ridge wants a different density from a bottom
    /// row, and `[bars] amount` is shared by all three modes
    pub bars: Option<u32>,
    /// Silhouette to hide behind: `[[x, y], ...]` in the same coordinates as
    /// `points`. Anything BELOW it is discarded, so bars rise from behind a
    /// ridge rather than being placed to look as though they do
    ///
    /// One per curve: it is the wallpaper's skyline, and every path on that
    /// wallpaper hides behind the same one
    pub occlude: Option<Vec<Vec<f32>>>,
    /// How the wallpaper covers the output. Points are authored on the IMAGE
    /// and have to be cropped onto the screen the same way the image itself
    /// is, or a curve lands beside the ridge it was drawn on
    pub fit: Option<FitMode>,
}

/// How the wallpaper is laid onto the output, which decides where a point
/// drawn on the image ends up on the screen
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    /// Scaled to cover the output and centre-cropped, which is what a
    /// wallpaper daemon does by default
    #[default]
    Cover,
    /// The image is treated as already output-shaped. Correct for a daemon
    /// told to stretch, and what every curve authored before this assumed
    Stretch,
}

/// One stretch of path. Everything about a bar's geometry lives here, so two
/// paths on the same wallpaper can differ in reach, width and lean
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct PathConfig {
    /// As `CurveConfig::points`.
    pub points: Vec<Vec<f32>>,
    /// Exactly this many bars on this path, instead of its share by length.
    /// A short foreground ridge can want more bars than a long distant one
    pub bars: Option<u32>,
    pub height: Option<f32>,
    pub width: Option<f32>,
    pub flip: Option<bool>,
    pub upright: Option<bool>,
}

impl CurveConfig {
    /// How many bars this curve draws in total
    ///
    /// The curve's own `bars` is the authority: it is what cava is told to
    /// produce. Without one, paths that each name a count add up to it
    #[must_use]
    pub fn total_bars(&self) -> Option<u32> {
        if let Some(n) = self.bars {
            return Some(n);
        }
        let paths = self.path.as_ref()?;
        paths
            .iter()
            .map(|p| p.bars)
            .try_fold(0u32, |acc, n| Some(acc + n?))
            .filter(|n| *n > 0)
    }

    /// The paths to draw, however they were written
    ///
    /// Borrowed where they exist, synthesised only for the single-path
    /// shorthand
    #[must_use]
    pub fn paths(&self) -> Cow<'_, [PathConfig]> {
        match &self.path {
            Some(paths) if !paths.is_empty() => Cow::Borrowed(paths),
            _ => Cow::Owned(vec![PathConfig {
                points: self.points.clone().unwrap_or_default(),
                bars: self.bars,
                height: self.height,
                width: self.width,
                flip: self.flip,
                upright: self.upright,
            }]),
        }
    }
}

/// Where a circle sits on the output
///
/// Anchor plus margin, not an offset from the centre: "top-right, 80px in"
/// survives a resolution change where a pixel offset does not
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CircleAnchor {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Geometry and shading for `Mode::Circle`.
///
/// Every field is optional: the section can be written a key at a time, and a
/// config carrying it loads on a build without the mode
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CircleConfig {
    /// Surface edge length in logical pixels. The surface is square, which is
    /// what keeps NDC square and the circle round without an aspect uniform
    pub diameter: Option<u32>,
    /// Where a bar starts, as a fraction of the radius. The hole in the middle
    pub inner_radius: Option<f32>,
    /// Alpha multiplier at the inner edge, blended to `outer_alpha` at the tip
    pub inner_alpha: Option<f32>,
    /// Alpha multiplier at a full-volume bar's tip
    pub outer_alpha: Option<f32>,
    /// Which point of the output the circle is pinned to
    pub anchor: Option<CircleAnchor>,
    /// Bar count for this mode only; see the note on `CurveConfig::bars`.
    pub bars: Option<u32>,
    /// Distance from the anchored edges, logical pixels. Ignored on an axis
    /// the anchor centres - "top" centres horizontally, so margin_x does
    /// nothing there
    pub margin_x: Option<u32>,
    pub margin_y: Option<u32>,
}

/// What to take from the external scheme source rather than from this file
///
/// Additive: a build without this section ignores it and the `role` key
/// inside a colour entry, and renders the static palette and configured bar
/// count. The file is stowed to machines that are not all rebuilt at once
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct SchemeConfig {
    /// Resolve each `[colors]` stop through its `role` against the live
    /// scheme, and re-resolve whenever the scheme changes. Falls back to the
    /// stop's own `hex` for any role the scheme does not carry
    pub colors: Option<bool>,
    /// Take the bar count from `services.visualiserBars` in the shell's
    /// shell.json instead of from `[bars] amount`.
    ///
    /// Startup-only, and not for want of trying: the count is written into the
    /// spawned cava's config at exec time and baked into the GPU index buffer,
    /// so following it live would mean respawning cava and rebuilding buffers.
    /// Colours have no such constraint, which is why only they update live
    pub bars: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GeneralConfig {
    pub framerate: u32,
    /// Absent means `Mode::Bars`, which is what every config predating this
    /// key expects
    pub mode: Option<Mode>,
    pub background_color: ConfigColor,
    pub autosens: Option<bool>,
    pub sensitivity: Option<f32>,
    pub preferred_output: Option<String>,
    /// "mono" or "stereo", passed through to cava's [output] section
    ///
    /// cava defaults to stereo, and in stereo mode it does not give each bar a
    /// distinct frequency band: it splits the bars in half, drawing the LEFT
    /// channel reversed across the left half and the RIGHT channel across the
    /// right half. With near-identical channels - most music - the two halves
    /// come out as mirror images, bass meeting in the middle. That reads as a
    /// symmetric visualiser, which is a look, but it is not what most people
    /// expect from a full-width wallpaper spectrum
    ///
    /// "mono" averages the channels and gives one left-to-right sweep across
    /// every bar
    pub channels: Option<String>,
    /// With channels = "mono": "average" (default), "left" or "right".
    pub mono_option: Option<String>,
    /// Forwarded to cava's [input] section as method=pulse, source=<this>.
    ///
    /// Left unset, cava's own default ("auto") always monitors whatever the
    /// current DEFAULT SINK is, via PipeWire's stream.capture.sink=true
    /// convention - which env vars like PULSE_SOURCE cannot override, since
    /// cava requests it directly rather than asking for a named source. That
    /// breaks completely, not just gets quiet, the moment the default sink's
    /// monitor does not work: confirmed on a Bluetooth A2DP sink, whose
    /// monitor produced zero bytes over two full seconds of `parec` while
    /// music played audibly through it. Point this at a source that stays
    /// constant regardless of the current output device - e.g. a
    /// processAllOutputs-style pre-mix sink's own monitor - to survive
    /// output switches (Bluetooth, speakers, headphones) without silently
    /// going dead
    pub audio_source: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BarConfig {
    pub amount: u32,
    pub gap: f32,
    pub max_height: Option<f32>,
    /// One multiplier over the final alpha, in every mode, on top of whatever
    /// alpha the colour stops already carry. Absent means 1.0
    pub opacity: Option<f32>,
    /// How far each bar is mixed toward one flat tone, 0 for the gradient as
    /// written and 1 for the palette flattened to its own mean. A matte
    /// finish: the ramp stops reading as something lit
    pub matte: Option<f32>,
}

/// The mean of a palette, which is the tone a matte finish flattens toward
///
/// Its own mean rather than a fixed grey, so flattening a warm palette gives a
/// warm flat and the setting cannot introduce a colour that is not already in
/// the scheme. Weighted by nothing: the stops are the palette, evenly
#[must_use]
pub fn palette_mean(rgba: &[[f32; 4]]) -> [f32; 3] {
    if rgba.is_empty() {
        return [0.0; 3];
    }
    let n = rgba.len() as f32;
    let mut mean = [0.0f32; 3];
    for c in rgba {
        for (m, v) in mean.iter_mut().zip(&c[..3]) {
            *m += v / n;
        }
    }
    mean
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SmoothingConfig {
    pub monstercat: Option<f32>,
    pub waves: Option<i32>,
    pub noise_reduction: Option<f32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ConfigColor {
    Simple(String),
    Complex(HexColorConfig),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HexColorConfig {
    pub hex: String,
    pub alpha: Option<f32>,
    /// Name of a scheme role (`yellow`, `peach`, `mauve`, ...) to
    /// take this stop's colour from when `[scheme] colors` is on. `hex` stays
    /// the fallback, so the palette in this file is still a complete, valid
    /// gradient on a machine with no scheme source at all
    pub role: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CavaConfig {
    pub general: CavaGeneralConfig,
    pub smoothing: CavaSmoothingConfig,
    pub output: HashMap<String, String>,
    // Omitted (not just empty) when unset, so cava keeps its own default
    // input method rather than this program quietly picking one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CavaGeneralConfig {
    pub framerate: u32,
    pub bars: u32,
    pub autosens: Option<bool>,
    pub sensitivity: Option<f32>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CavaSmoothingConfig {
    pub monstercat: Option<f32>,
    pub waves: Option<i32>,
    pub noise_reduction: Option<f32>,
}

/// Parse a colour from the config file, where a bad value is a startup error
/// rather than something to survive: this file does not change under us
///
/// # Panics
///
/// If `hex` is not six hex digits, optionally prefixed with `#`.
#[must_use]
pub fn color_from_hex(hex: &str, a: f32) -> [f32; 4] {
    match try_color_from_hex(hex, a) {
        Some(rgba) => rgba,
        None => panic!("invalid colour {hex:?}: expected #rrggbb"),
    }
}

/// Borrows rather than consumes: this runs per stop on every palette reload,
/// and the old signature cloned the hex `String` twice per call to read six
/// characters out of it
#[must_use]
pub fn array_from_config_color(color: &ConfigColor) -> [f32; 4] {
    match color {
        ConfigColor::Simple(hex) => color_from_hex(hex, 1.0),
        ConfigColor::Complex(color) => color_from_hex(&color.hex, color.alpha.unwrap_or(1.0)),
    }
}

/// Try to parse `#rrggbb`, or bare `rrggbb` as a scheme file writes it
///
/// Fallible where `color_from_hex` panics, because this one runs on live input:
/// the scheme is re-read while the visualiser is running, and a truncated or
/// half-written file must not take the process down mid-song
#[must_use]
pub fn try_color_from_hex(hex: &str, a: f32) -> Option<[f32; 4]> {
    // The slice pattern is the length check, and decoding nibbles directly
    // replaces three `from_str_radix` calls over re-sliced `&str`s
    let &[r1, r0, g1, g0, b1, b0] = hex.strip_prefix('#').unwrap_or(hex).as_bytes() else {
        return None;
    };
    let byte = |hi: u8, lo: u8| Some(f32::from((hex_nibble(hi)? << 4) | hex_nibble(lo)?) / 255.0);
    Some([byte(r1, r0)?, byte(g1, g0)?, byte(b1, b0)?, a])
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The `[colors]` stops in gradient order
///
/// The shader treats the SSBO as an ordered ramp - it mixes stop `i` into
/// `i + 1` down the surface - but the section deserialises into a HashMap,
/// whose iteration order is arbitrary AND randomised per process. Upstream fed
/// that straight to the GPU, so the gradient was shuffled on every launch. It
/// went unnoticed here because the palette in use was eight near-identical
/// greys, and every permutation of those looks the same; the orange gradient
/// this repo also ships would have made it obvious
///
/// Ordered on the key's trailing number where it has one, so this handles both
/// naming styles in use - `c1..c8` and `gradient_color_1..8` - and
/// puts c10 after c9 rather than after c1, which a plain string sort would not.
/// A key with no trailing digits keeps a stable place at the end, sorted by
/// name: losing a stop silently is worse than giving it an arbitrary position
#[must_use]
pub fn ordered_stops(colors: &HashMap<String, ConfigColor>) -> Vec<ConfigColor> {
    fn trailing_number(k: &str) -> Option<u64> {
        let digits = k.trim_end_matches(|c: char| !c.is_ascii_digit());
        // Counted in bytes: the run is ASCII digits, so the count is also a
        // valid byte index
        let start = digits.len() - digits.bytes().rev().take_while(u8::is_ascii_digit).count();
        digits[start..].parse().ok()
    }
    // Sorts on borrowed keys, cloning only the colours that reach the result
    let mut stops: Vec<(bool, u64, &str, &ConfigColor)> = colors
        .iter()
        .map(|(k, v)| {
            let n = trailing_number(k);
            (n.is_none(), n.unwrap_or(0), k.as_str(), v)
        })
        .collect();
    // Unstable is free: map keys are unique, so there are no ties to preserve,
    // and it skips `sort_by`'s scratch allocation
    stops.sort_unstable_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    stops.into_iter().map(|(_, _, _, v)| v.clone()).collect()
}

/// Resolve ordered stops to RGBA, routing each through the live scheme when one
/// is supplied and the stop names a role it carries
///
/// Every fallback lands on the stop's own `hex`: no scheme, no role on this
/// stop, a role the scheme lacks, or a value that will not parse. The static
/// palette is therefore always the floor, never a hole
#[must_use]
pub fn resolve_stops(
    stops: &[ConfigColor],
    scheme: Option<&HashMap<String, String>>,
) -> Vec<[f32; 4]> {
    stops
        .iter()
        .map(|stop| live_colour(stop, scheme).unwrap_or_else(|| array_from_config_color(stop)))
        .collect()
}

/// The scheme's colour for this stop, if a scheme, a role and a parsable value
/// are all present. `None` is every fallback path rolled into one
fn live_colour(stop: &ConfigColor, scheme: Option<&HashMap<String, String>>) -> Option<[f32; 4]> {
    let ConfigColor::Complex(c) = stop else {
        return None;
    };
    let live = scheme?.get(c.role.as_deref()?)?;
    try_color_from_hex(live, c.alpha.unwrap_or(1.0))
}

/// Pack stops into the std430 layout the fragment shader declares: an int
/// count, three words of padding to satisfy vec4 alignment, then the stops
///
/// Shared by the initial upload and every live re-upload; when this layout and
/// the shader's `GradientColors` block disagree the result is silent garbage on
/// screen, so there is exactly one copy of it
#[must_use]
pub fn gradient_buffer(rgba: &[[f32; 4]]) -> Vec<u8> {
    /// i32 count plus three words of padding to reach the stops' vec4 alignment
    const HEADER: usize = 16;
    let stops = uploaded_stops(rgba.len());
    // Sized up front: one allocation for the whole buffer
    let mut buf = Vec::with_capacity(HEADER + stops * std::mem::size_of::<[f32; 4]>());
    buf.extend_from_slice(&(stops as i32).to_le_bytes());
    buf.extend_from_slice(&[0u8; HEADER - 4]);
    // A lone stop is written twice. It costs 16 bytes and lets the shader index
    // `size - 2` unconditionally, which is what makes its clamp branchless
    for color in rgba.iter().chain(rgba.last().filter(|_| rgba.len() == 1)) {
        for v in color {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    debug_assert_eq!(buf.len(), HEADER + stops * std::mem::size_of::<[f32; 4]>());
    buf
}

/// Stops as the GPU sees them: a single configured stop is uploaded twice, so
/// the fragment shader always has a pair to mix between. `GradientScale` must
/// be derived from this, not from the configured count
#[must_use]
pub fn uploaded_stops(configured: usize) -> usize {
    configured.max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(hex: &str) -> ConfigColor {
        ConfigColor::Simple(hex.to_string())
    }

    fn roled(hex: &str, role: &str) -> ConfigColor {
        ConfigColor::Complex(HexColorConfig {
            hex: hex.to_string(),
            alpha: Some(0.5),
            role: Some(role.to_string()),
        })
    }

    /// The regression this ordering exists for: a HashMap hands back its keys in
    /// an arbitrary, per-process order, so the only way to see the bug is to
    /// check that the ORDER IS RECOVERED rather than that some iteration happens
    /// to come out right
    #[test]
    fn stops_sort_by_trailing_number_not_string() {
        let colors: HashMap<String, ConfigColor> = (1..=12)
            .map(|i| (format!("c{i}"), stop(&format!("#{:02x}0000", i))))
            .collect();
        let got: Vec<[f32; 4]> = resolve_stops(&ordered_stops(&colors), None);
        let reds: Vec<u8> = got.iter().map(|c| (c[0] * 255.0).round() as u8).collect();
        // A plain string sort would give 1, 10, 11, 12, 2, 3, ...
        assert_eq!(reds, (1..=12).collect::<Vec<u8>>());
    }

    #[test]
    fn upstream_naming_also_orders() {
        let colors: HashMap<String, ConfigColor> = (1..=8)
            .map(|i| {
                (
                    format!("gradient_color_{i}"),
                    stop(&format!("#{:02x}0000", i)),
                )
            })
            .collect();
        let reds: Vec<u8> = resolve_stops(&ordered_stops(&colors), None)
            .iter()
            .map(|c| (c[0] * 255.0).round() as u8)
            .collect();
        assert_eq!(reds, (1..=8).collect::<Vec<u8>>());
    }

    #[test]
    fn role_resolves_from_scheme_and_keeps_alpha() {
        let stops = vec![roled("#000000", "mauve")];
        let scheme: HashMap<String, String> =
            [("mauve".to_string(), "ff8000".to_string())].into_iter().collect();
        let got = resolve_stops(&stops, Some(&scheme));
        assert_eq!(got[0][0], 1.0);
        assert!((got[0][1] - 0.5019608).abs() < 1e-6);
        assert_eq!(got[0][2], 0.0);
        assert_eq!(got[0][3], 0.5, "alpha must come from the config, not the scheme");
    }

    /// Every one of these must land on the static hex rather than on a hole: a
    /// machine with no scheme source, a stop with no role, a role the scheme does
    /// not carry, and a value that will not parse
    #[test]
    fn every_miss_falls_back_to_static_hex() {
        let empty: HashMap<String, String> = HashMap::new();
        let junk: HashMap<String, String> =
            [("mauve".to_string(), "not-a-colour".to_string())].into_iter().collect();
        for scheme in [None, Some(&empty), Some(&junk)] {
            let got = resolve_stops(&[roled("#00ff00", "mauve")], scheme);
            assert_eq!(got[0], [0.0, 1.0, 0.0, 0.5]);
        }
        // No role at all
        assert_eq!(resolve_stops(&[stop("#0000ff")], Some(&junk))[0], [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn hex_accepts_both_spellings_and_rejects_junk() {
        assert_eq!(try_color_from_hex("#ffffff", 1.0), Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(try_color_from_hex("ffffff", 1.0), Some([1.0, 1.0, 1.0, 1.0]));
        for bad in ["", "#fff", "#gggggg", "#12345", "#1234567"] {
            assert!(try_color_from_hex(bad, 1.0).is_none(), "{bad} should not parse");
        }
    }

    /// The layout the fragment shader's std430 block declares. If these two ever
    /// disagree the result is silent garbage, so the count and the padding are
    /// pinned here rather than left to be re-derived by eye.
    /// A one-colour `[colors]` must still give the shader two stops to mix
    /// between, or its unconditional `size - 2` index goes negative
    #[test]
    fn a_lone_stop_is_uploaded_twice() {
        let buf = gradient_buffer(&[[0.25, 0.5, 0.75, 1.0]]);
        assert_eq!(&buf[0..4], &2i32.to_le_bytes(), "count reported to the shader");
        assert_eq!(buf.len(), 16 + 2 * 16);
        assert_eq!(&buf[16..32], &buf[32..48], "both stops identical");
        assert_eq!(uploaded_stops(1), 2);
        assert_eq!(uploaded_stops(8), 8);
    }

    /// Flattening toward the palette's own mean keeps a warm palette warm.
    /// A fixed grey would introduce a colour the scheme never had
    #[test]
    fn the_matte_tone_is_the_palette_itself() {
        let mean = palette_mean(&[[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 0.5]]);
        assert!((mean[0] - 0.5).abs() < 1e-6 && (mean[2] - 0.5).abs() < 1e-6);
        assert!(mean[1].abs() < 1e-6, "no green went in, none comes out");
        // Alpha is not a colour and takes no part in it
        assert_eq!(palette_mean(&[[0.2, 0.4, 0.6, 0.1]]), [0.2, 0.4, 0.6]);
        assert_eq!(palette_mean(&[]), [0.0; 3]);
    }

    #[test]
    fn gradient_buffer_matches_std430_layout() {
        let buf = gradient_buffer(&[[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]]);
        assert_eq!(&buf[0..4], &2i32.to_le_bytes());
        assert_eq!(&buf[4..16], &[0u8; 12], "vec4 alignment padding");
        assert_eq!(buf.len(), 16 + 2 * 16);
        assert_eq!(&buf[16..20], &1.0f32.to_le_bytes());
    }

    /// Both spellings have to parse, and mean what they say: the shorthand is
    /// what every existing config is written in, and dropping it would break
    /// every curve anyone has already authored
    #[test]
    fn a_curve_reads_as_one_path_or_many() {
        // Deserialised as the curve map itself: the surrounding Config wants
        // half a dozen unrelated sections that say nothing about a curve
        let short: HashMap<String, CurveConfig> = toml::from_str(
            r#"
            [abc]
            bars = 27
            height = 0.10
            upright = true
            points = [[0.1, 0.2], [0.3, 0.4]]
            "#,
        )
        .expect("shorthand parses");
        let curve = &short["abc"];
        let paths = curve.paths();
        assert_eq!(paths.len(), 1, "the top level is the one path");
        assert_eq!(paths[0].points.len(), 2);
        assert_eq!(paths[0].height, Some(0.10));
        assert_eq!(paths[0].upright, Some(true));
        assert_eq!(curve.bars, Some(27));

        let many: HashMap<String, CurveConfig> = toml::from_str(
            r#"
            [abc]
            bars = 40
            occlude = [[0.0, 0.5], [1.0, 0.5]]

            [[abc.path]]
            height = 0.20
            points = [[0.1, 0.2], [0.3, 0.4]]

            [[abc.path]]
            height = 0.05
            flip = true
            points = [[0.6, 0.3], [0.9, 0.3]]
            "#,
        )
        .expect("path blocks parse");
        let curve = &many["abc"];
        let paths = curve.paths();
        assert_eq!(paths.len(), 2);
        // Each path keeps its own geometry; the silhouette stays shared
        assert_eq!(paths[0].height, Some(0.20));
        assert_eq!(paths[1].height, Some(0.05));
        assert_eq!(paths[1].flip, Some(true));
        assert_eq!(paths[0].flip, None);
        assert_eq!(curve.occlude.as_ref().unwrap().len(), 2);
        assert!(curve.points.is_none(), "path blocks replace the shorthand");
    }

    /// The shipped config is what install.sh copies into ~/.config, so a
    /// version of it that does not parse is a broken install, not a stale
    /// comment. It shipped that way once: a circle block had been pasted into
    /// the middle of a sentence in the [scheme] comment, and the tail of that
    /// sentence became a second [colors] table header
    #[test]
    fn the_shipped_config_parses() {
        let text = include_str!("../config.toml");
        let cfg: Config = toml::from_str(text).expect("config.toml deserialises");
        assert!(!cfg.colors.is_empty(), "a palette is not optional");
        assert!(cfg.general.framerate > 0);
    }
}
