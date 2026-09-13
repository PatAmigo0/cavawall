use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub general: GeneralConfig,
    pub bars: BarConfig,
    pub colors: HashMap<String, ConfigColor>,
    pub smoothing: SmoothingConfig,
    /// Absent in an upstream config, and absent means "follow nothing".
    pub scheme: Option<SchemeConfig>,
}

/// What to take from Caelestia's live state rather than from this file.
///
/// This section is deliberately additive: an older cavawall ignores an unknown
/// top-level section, and ignores the unknown `role` key inside a colour entry,
/// so a config carrying both still runs on one -- it just renders the static
/// palette and the configured bar count. That matters because this file is
/// stowed to every machine, and they do not all get rebuilt at once.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct SchemeConfig {
    /// Resolve each `[colors]` stop through its `role` against the live
    /// scheme, and re-resolve whenever the scheme changes. Falls back to the
    /// stop's own `hex` for any role the scheme does not carry.
    pub colors: Option<bool>,
    /// Take the bar count from `services.visualiserBars` in Caelestia's
    /// shell.json instead of from `[bars] amount`.
    ///
    /// Startup-only, and not for want of trying: the count is written into the
    /// spawned cava's config at exec time and baked into the GPU index buffer,
    /// so following it live would mean respawning cava and rebuilding buffers.
    /// Colours have no such constraint, which is why only they update live.
    pub bars: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GeneralConfig {
    pub framerate: u32,
    pub background_color: ConfigColor,
    pub autosens: Option<bool>,
    pub sensitivity: Option<f32>,
    pub preferred_output: Option<String>,
    /// "mono" or "stereo", passed through to cava's [output] section.
    ///
    /// cava defaults to stereo, and in stereo mode it does not give each bar a
    /// distinct frequency band: it splits the bars in half, drawing the LEFT
    /// channel reversed across the left half and the RIGHT channel across the
    /// right half. With near-identical channels -- most music -- the two halves
    /// come out as mirror images, bass meeting in the middle. That reads as a
    /// symmetric visualiser, which is a look, but it is not what most people
    /// expect from a full-width wallpaper spectrum.
    ///
    /// "mono" averages the channels and gives one left-to-right sweep across
    /// every bar.
    pub channels: Option<String>,
    /// With channels = "mono": "average" (default), "left" or "right".
    pub mono_option: Option<String>,
    /// Forwarded to cava's [input] section as method=pulse, source=<this>.
    ///
    /// Left unset, cava's own default ("auto") always monitors whatever the
    /// current DEFAULT SINK is, via PipeWire's stream.capture.sink=true
    /// convention -- which env vars like PULSE_SOURCE cannot override, since
    /// cava requests it directly rather than asking for a named source. That
    /// breaks completely, not just gets quiet, the moment the default sink's
    /// monitor does not work: confirmed on a Bluetooth A2DP sink, whose
    /// monitor produced zero bytes over two full seconds of `parec` while
    /// music played audibly through it. Point this at a source that stays
    /// constant regardless of the current output device -- e.g. a
    /// processAllOutputs-style pre-mix sink's own monitor -- to survive
    /// output switches (Bluetooth, speakers, headphones) without silently
    /// going dead.
    pub audio_source: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BarConfig {
    pub amount: u32,
    pub gap: f32,
    pub max_height: Option<f32>,
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
    /// Name of a Caelestia scheme role (`yellow`, `peach`, `mauve`, ...) to
    /// take this stop's colour from when `[scheme] colors` is on. `hex` stays
    /// the fallback, so the palette in this file is still a complete, valid
    /// gradient on a machine with no Caelestia at all.
    pub role: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CavaConfig {
    pub general: CavaGeneralConfig,
    pub smoothing: CavaSmoothingConfig,
    pub output: HashMap<String, String>,
    // Omitted (not just empty) when unset, so cava keeps its own default
    // input method rather than this program quietly picking one.
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

pub fn color_from_hex(hex: String, a: f32) -> [f32; 4] {
    let r = u8::from_str_radix(&hex[1..3], 16).unwrap() as f32 / 255f32;
    let g = u8::from_str_radix(&hex[3..5], 16).unwrap() as f32 / 255f32;
    let b = u8::from_str_radix(&hex[5..7], 16).unwrap() as f32 / 255f32;
    [r, g, b, a]
}

pub fn array_from_config_color(color: ConfigColor) -> [f32; 4] {
    match color {
        ConfigColor::Simple(hex) => color_from_hex(hex.to_string(), 1.0),
        ConfigColor::Complex(color) => {
            color_from_hex(color.hex.to_string(), color.alpha.unwrap_or(1.0))
        }
    }
}

/// Try to parse `#rrggbb`, or bare `rrggbb` as Caelestia writes it.
///
/// Fallible where `color_from_hex` panics, because this one runs on live input:
/// the scheme is re-read while the visualiser is running, and a truncated or
/// half-written file must not take the process down mid-song.
pub fn try_color_from_hex(hex: &str, a: f32) -> Option<[f32; 4]> {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|v| v as f32 / 255.0);
    Some([c(0)?, c(2)?, c(4)?, a])
}

/// The `[colors]` stops in gradient order.
///
/// The shader treats the SSBO as an ordered ramp -- it mixes stop `i` into
/// `i + 1` down the surface -- but the section deserialises into a HashMap,
/// whose iteration order is arbitrary AND randomised per process. Upstream fed
/// that straight to the GPU, so the gradient was shuffled on every launch. It
/// went unnoticed here because the palette in use was eight near-identical
/// greys, and every permutation of those looks the same; the orange gradient
/// this repo also ships would have made it obvious.
///
/// Ordered on the key's trailing number where it has one, so this handles both
/// naming styles in use -- `c1..c8` and upstream's `gradient_color_1..8` -- and
/// puts c10 after c9 rather than after c1, which a plain string sort would not.
/// A key with no trailing digits keeps a stable place at the end, sorted by
/// name: losing a stop silently is worse than giving it an arbitrary position.
pub fn ordered_stops(colors: &HashMap<String, ConfigColor>) -> Vec<ConfigColor> {
    fn trailing_number(k: &str) -> Option<u64> {
        let digits = k.trim_end_matches(|c: char| !c.is_ascii_digit());
        let start = digits.len() - digits.chars().rev().take_while(|c| c.is_ascii_digit()).count();
        digits[start..].parse().ok()
    }
    let mut stops: Vec<(bool, u64, String, ConfigColor)> = colors
        .iter()
        .map(|(k, v)| {
            let n = trailing_number(k);
            (n.is_none(), n.unwrap_or(0), k.clone(), v.clone())
        })
        .collect();
    stops.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    stops.into_iter().map(|(_, _, _, v)| v).collect()
}

/// Resolve ordered stops to RGBA, routing each through the live scheme when one
/// is supplied and the stop names a role it carries.
///
/// Every fallback lands on the stop's own `hex`: no scheme, no role on this
/// stop, a role the scheme lacks, or a value that will not parse. The static
/// palette is therefore always the floor, never a hole.
pub fn resolve_stops(
    stops: &[ConfigColor],
    scheme: Option<&HashMap<String, String>>,
) -> Vec<[f32; 4]> {
    stops
        .iter()
        .map(|stop| {
            if let (ConfigColor::Complex(c), Some(sc)) = (stop, scheme) {
                if let Some(role) = &c.role {
                    if let Some(live) = sc.get(role) {
                        if let Some(rgba) = try_color_from_hex(live, c.alpha.unwrap_or(1.0)) {
                            return rgba;
                        }
                    }
                }
            }
            array_from_config_color(stop.clone())
        })
        .collect()
}

/// Pack stops into the std430 layout the fragment shader declares: an int
/// count, three words of padding to satisfy vec4 alignment, then the stops.
///
/// Shared by the initial upload and every live re-upload; when this layout and
/// the shader's `GradientColors` block disagree the result is silent garbage on
/// screen, so there is exactly one copy of it.
pub fn gradient_buffer(rgba: &[[f32; 4]]) -> Vec<u8> {
    let mut buf: Vec<u8> = (rgba.len() as i32).to_le_bytes().to_vec();
    buf.extend([0, 0, 0, 0].repeat(3));
    for color in rgba {
        for v in color {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    buf
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
    /// to come out right.
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
    /// machine with no Caelestia, a stop with no role, a role the scheme does
    /// not carry, and a value that will not parse.
    #[test]
    fn every_miss_falls_back_to_static_hex() {
        let empty: HashMap<String, String> = HashMap::new();
        let junk: HashMap<String, String> =
            [("mauve".to_string(), "not-a-colour".to_string())].into_iter().collect();
        for scheme in [None, Some(&empty), Some(&junk)] {
            let got = resolve_stops(&vec![roled("#00ff00", "mauve")], scheme);
            assert_eq!(got[0], [0.0, 1.0, 0.0, 0.5]);
        }
        // No role at all.
        assert_eq!(resolve_stops(&vec![stop("#0000ff")], Some(&junk))[0], [0.0, 0.0, 1.0, 1.0]);
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
    #[test]
    fn gradient_buffer_matches_std430_layout() {
        let buf = gradient_buffer(&[[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]]);
        assert_eq!(&buf[0..4], &2i32.to_le_bytes());
        assert_eq!(&buf[4..16], &[0u8; 12], "vec4 alignment padding");
        assert_eq!(buf.len(), 16 + 2 * 16);
        assert_eq!(&buf[16..20], &1.0f32.to_le_bytes());
    }
}
