use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub general: GeneralConfig,
    pub bars: BarConfig,
    pub colors: HashMap<String, ConfigColor>,
    pub smoothing: SmoothingConfig,
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
