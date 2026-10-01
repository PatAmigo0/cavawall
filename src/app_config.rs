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
    /// Desktop notifications for errors, crashes and, if asked, starts and
    /// stops. Absent means errors and crashes, sent the automatic way
    pub notify: Option<NotifyConfig>,
    pub wallpaper: Option<WallpaperSource>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FullscreenPolicy {
    Move,
    Pause,
    #[default]
    Ignore,
}

/// `[notify]`: whether, how and for what cavawall raises notifications
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct NotifyConfig {
    pub enabled: Option<bool>,
    pub via: Option<NotifyVia>,
    /// For `via = "command"`: the program and its leading arguments; the
    /// message is appended as the last one
    pub command: Option<Vec<String>>,
    pub events: Option<Vec<NotifyEvent>>,
}

/// How a notification is delivered
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NotifyVia {
    /// notify-send when it is installed, else Hyprland's own
    #[default]
    Auto,
    /// The desktop notification daemon, through notify-send: Caelestia,
    /// mako, dunst, swaync and the rest
    Dbus,
    /// hyprctl notify
    Hyprland,
    Command,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NotifyEvent {
    Start,
    Stop,
    Error,
    Crash,
}

/// Coordinates are authored to four places; f32 widened to f64 is not.
pub fn round_floats(value: &mut toml::Value) {
    match value {
        toml::Value::Float(f) => *f = (*f * 1e4).round() / 1e4,
        toml::Value::Array(a) => a.iter_mut().for_each(round_floats),
        toml::Value::Table(t) => t.iter_mut().for_each(|(_, v)| round_floats(v)),
        _ => {}
    }
}

/// Per-wallpaper bar overrides; each field optional so one can be set alone.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct BarOverride {
    pub amount: Option<u32>,
    pub gap: Option<f32>,
    pub max_height: Option<f32>,
    pub opacity: Option<f32>,
    pub matte: Option<f32>,
    pub left: Option<f32>,
    pub span: Option<f32>,
    pub baseline: Option<f32>,
    pub grow: Option<Grow>,
    pub radius: Option<f32>,
    pub mirror: Option<bool>,
    pub blocks: Option<u32>,
    pub gradient: Option<GradientAxis>,
    pub reveal: Option<f32>,
    pub reveal_pulse: Option<bool>,
    /// The recipe cavawall-tune baked the reveal image from, kept so the
    /// next edit starts from it. The renderer reads only the image
    pub reveal_source: Option<String>,
    pub reveal_filter: Option<String>,
}

impl BarOverride {
    #[must_use]
    pub fn apply(&self, base: &BarConfig) -> BarConfig {
        BarConfig {
            amount: self.amount.unwrap_or(base.amount),
            gap: self.gap.unwrap_or(base.gap),
            max_height: self.max_height.or(base.max_height),
            opacity: self.opacity.or(base.opacity),
            matte: self.matte.or(base.matte),
            left: self.left.or(base.left),
            span: self.span.or(base.span),
            baseline: self.baseline.or(base.baseline),
            grow: self.grow.or(base.grow),
            radius: self.radius.or(base.radius),
            mirror: self.mirror.or(base.mirror),
            blocks: self.blocks.or(base.blocks),
            gradient: self.gradient.or(base.gradient),
            reveal: self.reveal.or(base.reveal),
            reveal_pulse: self.reveal_pulse.or(base.reveal_pulse),
            reveal_dir: base.reveal_dir.clone(),
        }
    }
}

/// Which way a row of bars grows from its baseline
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Grow {
    #[default]
    Up,
    /// Hanging from the baseline, for a row along the top
    Down,
}

/// What the gradient runs along: each bar from base to tip, or the whole row
/// from its first bar to its last
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GradientAxis {
    #[default]
    Height,
    Row,
}

/// Per-wallpaper overrides, one file each under `wallpapers/`.
///
/// Named by the wallpaper's content hash, so it survives a rename or a move.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct WallpaperConfig {
    /// Human label; the filename is a hash and says nothing on its own
    pub name: Option<String>,
    /// Which figure this wallpaper gets, overriding `general.mode`
    pub mode: Option<Mode>,
    /// Frames per second for this wallpaper, overriding `general.framerate`.
    /// Startup-only, being cava's own rate: a wallpaper switch re-execs
    /// whenever either side has a file, so it applies on the switch
    pub framerate: Option<u32>,
    /// This wallpaper's own gradient, replacing `[colors]`. Stops with a
    /// `role` follow the live palette exactly as there
    pub colors: Option<Palette>,
    pub circle: Option<CircleConfig>,
    pub bars: Option<BarOverride>,
    pub curve: Option<CurveConfig>,
}

impl WallpaperConfig {
    /// Prepended on write; `toml` cannot emit comments itself.
    pub const HEADER: &'static str = "\
# cavawall settings for one wallpaper, keyed by its content hash so the file
# survives a rename or a move. Written by `cavawall tune`; hand edits are fine.
#
#   name      label for you; nothing reads it
#   mode      bars | circle | curve, overriding general.mode in config.toml
#   framerate frames per second, overriding general.framerate
#   colors    this wallpaper's own gradient, base to tip, replacing [colors]:
#             [\"#rrggbb\", { hex = \"#rrggbb\", alpha = 0.5, role = \"mauve\" }]
#   bars      amount, gap, max_height, opacity, matte, left, span, baseline,
#             grow, radius, blocks, mirror, gradient, reveal, reveal_pulse -
#             each falls back to [bars] in config.toml
#   circle    bars, diameter, inner_radius, inner_alpha, outer_alpha, and
#             anchor with margin_x/margin_y, or position = [x, y]
#   curve     bars, height, width, fit, [[curve.occluder]] shapes, and one
#             [[curve.path]] per stretch. A path's own bars is exactly its
#             count; the paths without one share curve.bars by length. A path
#             names what hides it in cut_by and can carry its own colors
#
# Delete this file to go back to config.toml's defaults for this wallpaper.

";

    #[must_use]
    pub fn path(dir: &std::path::Path, key: &str) -> std::path::PathBuf {
        dir.join("wallpapers").join(format!("{key}.toml"))
    }

    /// This wallpaper's settings: `Ok(None)` when it has no file
    ///
    /// # Errors
    /// The file exists but will not read or parse, with the file named
    pub fn load(dir: &std::path::Path, key: &str) -> Result<Option<Self>, String> {
        let path = Self::path(dir, key);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        toml::from_str(&text).map(Some).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// # Errors
    /// Serialisation or filesystem failure.
    pub fn save(&self, dir: &std::path::Path, key: &str) -> std::io::Result<()> {
        let path = Self::path(dir, key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut value = toml::Value::try_from(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        round_floats(&mut value);
        let body = toml::to_string(&value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut doc: toml_edit::DocumentMut =
            body.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        inline_palettes(doc.as_table_mut());
        if let Some(paths) = doc.get_mut("curve").and_then(|c| c.get_mut("path")).and_then(toml_edit::Item::as_array_of_tables_mut) {
            paths.iter_mut().for_each(inline_palettes);
        }
        std::fs::write(path, Self::HEADER.to_owned() + &doc.to_string())
    }
}

/// A palette as one line, `colors = [{ hex = .., alpha = .. }, ..]`, where
/// the writer would give every stop a `[[colors]]` block of its own
fn inline_palettes(table: &mut toml_edit::Table) {
    let Some(item) = table.get_mut("colors") else { return };
    let mut stops = match std::mem::take(item) {
        toml_edit::Item::ArrayOfTables(blocks) => blocks.into_array(),
        toml_edit::Item::Value(toml_edit::Value::Array(stops)) => stops,
        other => {
            *item = other;
            return;
        }
    };
    let rank = |k: &str| ["hex", "alpha", "role"].iter().position(|r| *r == k).unwrap_or(3);
    for stop in stops.iter_mut().filter_map(toml_edit::Value::as_inline_table_mut) {
        stop.sort_values_by(|a, _, b, _| rank(a.get()).cmp(&rank(b.get())));
        stop.fmt();
    }
    stops.fmt();
    *item = toml_edit::value(stops);
    if let Some(mut key) = table.key_mut("colors") {
        key.fmt();
    }
}

/// Gradient stops in order, base to tip: a wallpaper's own palette, or a
/// curve path's
///
/// Written as an array, or as a table keyed like `[colors]` and ordered the
/// same way, by the number its keys end in. Always written back as an array
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Palette(pub Vec<ConfigColor>);

impl Serialize for Palette {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Palette {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            List(Vec<ConfigColor>),
            Keyed(HashMap<String, ConfigColor>),
        }
        Ok(Self(match Written::deserialize(d)? {
            Written::List(stops) => stops,
            Written::Keyed(keyed) => into_ordered_stops(keyed),
        }))
    }
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
    /// Bars shared by length between the paths that set no count of their
    /// own; a path's own count is added on top, never taken from here. Unset,
    /// this wallpaper's `[bars] amount`, the shell's or config.toml's applies
    pub bars: Option<u32>,
    /// Silhouette to hide behind: `[[x, y], ...]` in the same coordinates as
    /// `points`. Anything BELOW it is discarded, so bars rise from behind a
    /// ridge rather than being placed to look as though they do
    ///
    /// The one-shape form, cutting every path; `occluder` is the general one
    pub occlude: Option<Vec<Vec<f32>>>,
    /// Named shapes bars hide behind. A path names the ones that cut it in
    /// `cut_by`; one that names none is cut by all of them
    pub occluder: Option<Vec<OccluderConfig>>,
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
    /// Exactly this many bars on this path, instead of a share of
    /// `CurveConfig::bars`. A short foreground ridge can want more bars than a
    /// long distant one
    pub bars: Option<u32>,
    /// This path's own gradient; absent uses the wallpaper's
    pub colors: Option<Palette>,
    pub height: Option<f32>,
    pub width: Option<f32>,
    pub flip: Option<bool>,
    pub upright: Option<bool>,
    /// This path's own silhouette; absent falls back to the curve's
    pub occlude: Option<Vec<Vec<f32>>>,
    /// False leaves this path unclipped, ignoring every silhouette
    pub clip: Option<bool>,
    /// The occluders that cut this path, by name. Absent is every one of
    /// them; empty is none. Several cut by their union
    pub cut_by: Option<Vec<String>>,
}

impl PathConfig {
    /// Two points or more: anything less draws nothing and takes no bars
    #[must_use]
    pub fn is_drawn(&self) -> bool {
        self.points.iter().filter(|p| p.len() >= 2).count() >= 2
    }
}

/// A shape bars hide behind, shared by every path that names it
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct OccluderConfig {
    /// What paths call it; also its label in the editor
    pub name: Option<String>,
    /// `[x, y]` in the same image coordinates as a path's points
    pub points: Vec<Vec<f32>>,
    pub shape: Option<OccluderShape>,
}

/// How an occluder's outline becomes an area
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OccluderShape {
    /// Closed straight down to the bottom edge: a ridge or a skyline, hiding
    /// everything beneath it
    #[default]
    Skyline,
    /// Closed on itself: a tree, a rock, anything with sky below it too. An
    /// outline that crosses itself is filled by parity, so it can have holes
    Closed,
}

/// Occluders, resolved: every shape and the set that cuts each path
#[derive(Debug, Default, PartialEq)]
pub struct Occlusion<'a> {
    /// In bit order: shape `i` is bit `1 << i` of a path's mask
    pub shapes: Vec<(&'a [Vec<f32>], OccluderShape)>,
    /// One per path of `CurveConfig::paths`, in order
    pub masks: Vec<u16>,
}

/// Shapes past this are dropped: the mask a bar tests is sixteen bits
pub const MAX_OCCLUDERS: usize = 16;

impl CurveConfig {
    /// The bars drawn paths ask for by name, and whether any drawn path
    /// names none and so shares the curve's count
    ///
    /// The total cava is told to produce is the first, plus the shared count
    /// when the second holds
    #[must_use]
    pub fn counts(&self) -> (u32, bool) {
        self.paths().iter().filter(|p| p.is_drawn()).fold((0, false), |(own, shares), p| match p.bars {
            Some(n) => (own.saturating_add(n), shares),
            None => (own, true),
        })
    }

    /// Some path has two points to draw between
    #[must_use]
    pub fn is_drawable(&self) -> bool {
        self.paths().iter().any(PathConfig::is_drawn)
    }

    /// The paths to draw, however they were written
    ///
    /// Borrowed where they exist, synthesised only for the single-path
    /// shorthand, which shares the curve's `bars` like any path with no count
    #[must_use]
    pub fn paths(&self) -> Cow<'_, [PathConfig]> {
        match &self.path {
            Some(paths) if !paths.is_empty() => Cow::Borrowed(paths),
            _ => Cow::Owned(vec![PathConfig {
                points: self.points.clone().unwrap_or_default(),
                bars: None,
                colors: None,
                height: self.height,
                width: self.width,
                flip: self.flip,
                upright: self.upright,
                // The shorthand is one path; its silhouette is the curve's
                occlude: None,
                clip: None,
                cut_by: None,
            }]),
        }
    }

    /// Every shape and which of them cut each path, one form for all three ways
    /// a config can say it: named `occluder`s, the one-shape `occlude`, and a
    /// path's own `occlude`
    ///
    /// A path's own `occlude` cuts only that path. `clip = false` is cut by
    /// nothing. Otherwise `cut_by` names the shapes, and without it every
    /// shared shape applies. Shapes with fewer than two points are dropped,
    /// and so is every name that matches none
    #[must_use]
    pub fn occlusion(&self) -> Occlusion<'_> {
        let drawn = |p: &[Vec<f32>]| p.iter().filter(|v| v.len() >= 2).count() >= 2;
        let mut shapes = Vec::new();
        let mut names: Vec<Option<&str>> = Vec::new();
        for o in self.occluder.iter().flatten().filter(|o| drawn(&o.points)) {
            shapes.push((o.points.as_slice(), o.shape.unwrap_or_default()));
            names.push(o.name.as_deref());
        }
        if let Some(pts) = self.occlude.as_deref().filter(|p| drawn(p)) {
            shapes.push((pts, OccluderShape::Skyline));
            names.push(None);
        }
        shapes.truncate(MAX_OCCLUDERS);
        let bit = |i: usize| if i < MAX_OCCLUDERS { 1u16 << i } else { 0 };
        let shared = (0..shapes.len()).fold(0u16, |m, i| m | bit(i));

        // The single-path shorthand has no silhouette of its own: every
        // shared shape cuts it
        let paths = match &self.path {
            Some(paths) if !paths.is_empty() => paths.as_slice(),
            _ => return Occlusion { shapes, masks: vec![shared] },
        };
        let mut masks = Vec::with_capacity(paths.len());
        for p in paths {
            let own = p.occlude.as_deref().filter(|o| drawn(o));
            let mask = if p.clip == Some(false) {
                0
            } else if let Some(cut_by) = &p.cut_by {
                cut_by
                    .iter()
                    .filter_map(|n| names.iter().position(|m| *m == Some(n.as_str())))
                    .fold(0, |m, i| m | bit(i))
            } else if let Some(own) = own {
                shapes.push((own, OccluderShape::Skyline));
                names.push(None);
                bit(shapes.len() - 1)
            } else {
                shared
            };
            masks.push(mask);
        }
        shapes.truncate(MAX_OCCLUDERS);
        Occlusion { shapes, masks }
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
    /// The centre as `[x, y]` fractions of the output, origin top left.
    /// Overrides `anchor` and the margins, for a circle placed by hand
    pub position: Option<[f32; 2]>,
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
    /// Where the live palette comes from: `caelestia` (the default), `pywal`,
    /// or `file` with `path`. Any JSON works: every colour-valued entry is a
    /// role named by its own key, however deep it sits
    pub source: Option<String>,
    /// The palette file, overriding the source's usual place. `~/` is home
    pub path: Option<String>,
}

/// `[wallpaper]`: how cavawall learns which image is on screen, so it can use
/// that wallpaper's own settings
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct WallpaperSource {
    /// `caelestia` (the default), `swww`, `waypaper`, `file` (a file holding
    /// the path, with `path`), or `command` (prints the path, with `command`)
    pub source: Option<String>,
    pub path: Option<String>,
    pub command: Option<Vec<String>>,
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
    /// What to do while a fullscreen window covers the monitor: `move` to a
    /// free one (pausing when none is left), `pause` in place, or `ignore`,
    /// the default. On Hyprland; off costs nothing, not even a socket
    pub on_fullscreen: Option<FullscreenPolicy>,
    /// `nvidia` or `mesa`: load only that EGL driver. Unset, the loader loads
    /// every installed one to find which fits - Mesa's pulls in LLVM, 7 MB of
    /// memory an NVIDIA machine never uses. Set it to the GPU the compositor
    /// renders on
    pub gl_driver: Option<String>,
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
    /// Seconds of silence before cava sleeps: it stops analysing and nearly
    /// stops writing, so this process parks with next to nothing to read.
    /// The cost is waking - measured 480ms from a tone to the first loud frame
    /// against 37ms awake - so only silences longer than this pay it
    pub sleep_timer: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
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
    /// Where the row sits, as fractions of the output. Unset, it runs the full
    /// width along the bottom. `left` and `span` are its horizontal extent;
    /// `baseline` is where the bars stand, from the top
    pub left: Option<f32>,
    pub span: Option<f32>,
    pub baseline: Option<f32>,
    pub grow: Option<Grow>,
    /// Rounds each bar's tip, as a fraction of its width: 0.5 is a full
    /// semicircle. Bars and curve only; a circle's bars are wedges. Zero or
    /// absent compiles the rounding out of the shader altogether
    pub radius: Option<f32>,
    /// Bars reach both ways from their line, mirrored. Bars and curve; twice
    /// the pixels of a plain row
    pub mirror: Option<bool>,
    /// Splits each bar into this many segments at full height, LED style.
    /// Zero or absent draws solid bars
    pub blocks: Option<u32>,
    /// `row` runs the gradient along the row instead of up each bar
    pub gradient: Option<GradientAxis>,
    /// How far bars show the wallpaper's reveal image instead of the
    /// gradient, 0 to 1: an x-ray through the bars. The image is
    /// `wallpapers/<key>.reveal.qoi`, written by cavawall-tune. Zero, absent,
    /// or no image compiles it out
    pub reveal: Option<f32>,
    /// The reveal follows each bar's loudness: quiet bars keep the gradient,
    /// loud ones show the picture
    pub reveal_pulse: Option<bool>,
    /// Where cavawall-tune keeps x-ray pictures, one per wallpaper named
    /// after it. `~/` is home. Absent is ~/Pictures/cavawall-xray
    pub reveal_dir: Option<String>,
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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum ConfigColor {
    Simple(String),
    Complex(HexColorConfig),
}

impl ConfigColor {
    #[must_use]
    pub fn hex(&self) -> &str {
        match self {
            Self::Simple(hex) => hex,
            Self::Complex(c) => &c.hex,
        }
    }

    /// Takes its colour from the live palette when one is followed
    #[must_use]
    pub const fn has_role(&self) -> bool {
        matches!(self, Self::Complex(HexColorConfig { role: Some(_), .. }))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleep_timer: Option<u32>,
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
/// whose iteration order is arbitrary and randomised per process
///
/// Ordered on the key's trailing number where it has one, so this handles both
/// naming styles in use - `c1..c8` and `gradient_color_1..8` - and
/// puts c10 after c9 rather than after c1, which a plain string sort would not.
/// A key with no trailing digits keeps a stable place at the end, sorted by
/// name: losing a stop silently is worse than giving it an arbitrary position
#[must_use]
pub fn ordered_stops(colors: &HashMap<String, ConfigColor>) -> Vec<ConfigColor> {
    // Sorts on borrowed keys, cloning only the colours that reach the result
    let mut stops: Vec<(&str, &ConfigColor)> = colors.iter().map(|(k, v)| (k.as_str(), v)).collect();
    // Unstable is free: map keys are unique, so there are no ties to preserve,
    // and it skips `sort_by`'s scratch allocation
    stops.sort_unstable_by(|a, b| stop_order(a.0).cmp(&stop_order(b.0)));
    stops.into_iter().map(|(_, v)| v.clone()).collect()
}

/// `ordered_stops` for a table already owned, moving the colours out
#[must_use]
pub fn into_ordered_stops(colors: HashMap<String, ConfigColor>) -> Vec<ConfigColor> {
    let mut stops: Vec<(String, ConfigColor)> = colors.into_iter().collect();
    stops.sort_unstable_by(|a, b| stop_order(&a.0).cmp(&stop_order(&b.0)));
    stops.into_iter().map(|(_, v)| v).collect()
}

/// Where a stop's key sorts: numbered keys by their number, then the rest by name
fn stop_order(k: &str) -> (bool, u64, &str) {
    let digits = k.trim_end_matches(|c: char| !c.is_ascii_digit());
    // Counted in bytes: the run is ASCII digits, so the count is also a
    // valid byte index
    let start = digits.len() - digits.bytes().rev().take_while(u8::is_ascii_digit).count();
    let n = digits[start..].parse().ok();
    (n.is_none(), n.unwrap_or(0), k)
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

/// Where one palette sits among the uploaded stops
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteSlot {
    /// Its first stop's index in the shader's `gradient_colors`
    pub first: u32,
    /// Stops as uploaded, a lone configured one counted twice
    pub stops: u32,
}

/// Where each palette lands in `gradient_buffer`: back to back, and with
/// more than one, each followed by its mean
#[must_use]
pub fn palette_slots(configured: &[usize]) -> Vec<PaletteSlot> {
    let mean = u32::from(configured.len() > 1);
    let mut first = 0;
    configured
        .iter()
        .map(|&n| {
            let slot = PaletteSlot { first, stops: uploaded_stops(n) as u32 };
            first += slot.stops + mean;
            slot
        })
        .collect()
}

/// Pack palettes into the std430 layout the shaders declare: an int count,
/// then in what would be padding before the vec4-aligned stops the two
/// numbers every fragment needs - count - 1 as a float, count - 2 as an int -
/// so no fragment converts or subtracts them itself. The header describes the
/// first palette, the only one a single-palette shader reads
///
/// Further palettes are curve paths' own. With them, every palette's stops are
/// followed by its mean, the tone its bars flatten toward under a matte finish
///
/// Shared by the initial upload and every live re-upload; when this layout and
/// the shader's `GradientColors` block disagree the result is silent garbage on
/// screen, so there is exactly one copy of it
#[must_use]
pub fn gradient_buffer<P: AsRef<[[f32; 4]]>>(palettes: &[P]) -> Vec<u8> {
    /// i32 count, f32 span, i32 last pair, one word of padding: the stops'
    /// vec4 alignment
    const HEADER: usize = 16;
    const STOP: usize = std::mem::size_of::<[f32; 4]>();
    let counts: Vec<usize> = palettes.iter().map(|p| p.as_ref().len()).collect();
    let slots = palette_slots(&counts);
    let means = usize::from(palettes.len() > 1);
    let stops = slots.first().map_or(2, |s| s.stops as usize);
    // Sized up front: one allocation for the whole buffer
    let total: usize = slots.iter().map(|s| s.stops as usize + means).sum();
    let mut buf = Vec::with_capacity(HEADER + total * STOP);
    buf.extend_from_slice(&(stops as i32).to_le_bytes());
    buf.extend_from_slice(&((stops - 1) as f32).to_le_bytes());
    buf.extend_from_slice(&(stops as i32 - 2).to_le_bytes());
    buf.extend_from_slice(&[0u8; HEADER - 12]);
    let mut put = |c: &[f32]| c.iter().for_each(|v| buf.extend_from_slice(&v.to_le_bytes()));
    // An empty palette still takes its two stops, transparent, so every slot
    // lands where palette_slots says
    let clear = [[0.0; 4]];
    for rgba in palettes {
        let rgba = Some(rgba.as_ref()).filter(|p| !p.is_empty()).unwrap_or(&clear);
        // A lone stop is written twice. It costs 16 bytes and lets the shader
        // index `size - 2` unconditionally, which is what makes its clamp
        // branchless
        for color in rgba.iter().chain(rgba.last().filter(|_| rgba.len() == 1)) {
            put(color);
        }
        if means == 1 {
            let m = palette_mean(rgba);
            put(&[m[0], m[1], m[2], 1.0]);
        }
    }
    debug_assert_eq!(buf.len(), HEADER + total * STOP);
    buf
}

/// Stops as the GPU sees them: a single configured stop is uploaded twice, so
/// the fragment shader always has a pair to mix between
#[must_use]
pub fn uploaded_stops(configured: usize) -> usize {
    configured.max(2)
}

/// `$XDG_CONFIG_HOME/cavawall`, else `~/.config/cavawall`: where every
/// cavawall binary looks unless told otherwise
#[must_use]
pub fn config_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("cavawall")
}

/// config.toml, parsed
///
/// # Errors
/// It will not read or parse, with the file and toml's line and column named
pub fn load_config(path: &std::path::Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::suboptimal_flops, clippy::manual_midpoint, clippy::decimal_bitwise_operands, reason = "tests compare exact values and keep their maths independent of the code they check")]
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
            .map(|i| (format!("c{i}"), stop(&format!("#{i:02x}0000"))))
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
                    stop(&format!("#{i:02x}0000")),
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
        let scheme = HashMap::from([("mauve".to_string(), "ff8000".to_string())]);
        let got = resolve_stops(&stops, Some(&scheme));
        assert_eq!(got[0][0], 1.0);
        assert!((got[0][1] - 0.501_960_8).abs() < 1e-6);
        assert_eq!(got[0][2], 0.0);
        assert_eq!(got[0][3], 0.5, "alpha must come from the config, not the scheme");
    }

    /// Every one of these must land on the static hex rather than on a hole: a
    /// machine with no scheme source, a stop with no role, a role the scheme does
    /// not carry, and a value that will not parse
    #[test]
    fn every_miss_falls_back_to_static_hex() {
        let empty: HashMap<String, String> = HashMap::new();
        let junk = HashMap::from([("mauve".to_string(), "not-a-colour".to_string())]);
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
        let buf = gradient_buffer(&[[[0.25, 0.5, 0.75, 1.0]]]);
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
        let buf = gradient_buffer(&[[[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]]]);
        assert_eq!(&buf[0..4], &2i32.to_le_bytes());
        assert_eq!(&buf[4..8], &1.0f32.to_le_bytes(), "stop_span, count - 1");
        assert_eq!(&buf[8..12], &0i32.to_le_bytes(), "last_pair, count - 2");
        assert_eq!(&buf[12..16], &[0u8; 4], "vec4 alignment padding");
        // A lone stop is doubled, so the shader still sees a pair
        let one = gradient_buffer(&[[[1.0, 0.0, 0.0, 1.0]]]);
        assert_eq!((&one[0..4], &one[4..8], &one[8..12]), (&2i32.to_le_bytes()[..], &1.0f32.to_le_bytes()[..], &0i32.to_le_bytes()[..]));
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
            r"
            [abc]
            bars = 27
            height = 0.10
            upright = true
            points = [[0.1, 0.2], [0.3, 0.4]]
            ",
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
            r"
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
            ",
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

    /// Every way a config can name a silhouette lands in one list of bits,
    /// and each path's mask says exactly which of them cut it
    #[test]
    fn occluders_resolve_to_bits_and_masks() {
        let c: HashMap<String, CurveConfig> = toml::from_str(
            r#"
            [k]
            occlude = [[0.0, 0.9], [1.0, 0.9]]
            [[k.occluder]]
            name = "ridge"
            points = [[0.0, 0.5], [1.0, 0.5]]
            [[k.occluder]]
            name = "tree"
            shape = "closed"
            points = [[0.2, 0.2], [0.3, 0.2], [0.25, 0.4]]
            [[k.occluder]]
            name = "stray"
            points = [[0.5, 0.5]]
            [[k.path]]
            points = [[0.0, 0.6], [1.0, 0.6]]
            [[k.path]]
            points = [[0.0, 0.6], [1.0, 0.6]]
            cut_by = ["tree", "nope"]
            [[k.path]]
            points = [[0.0, 0.6], [1.0, 0.6]]
            occlude = [[0.0, 0.3], [1.0, 0.3]]
            [[k.path]]
            points = [[0.0, 0.6], [1.0, 0.6]]
            clip = false
            [[k.path]]
            points = [[0.0, 0.6], [1.0, 0.6]]
            cut_by = []
            "#,
        )
        .expect("parses");
        let o = c["k"].occlusion();
        // ridge, tree, the shared one, then the third path's own; the one
        // point "stray" is no shape at all
        assert_eq!(o.shapes.len(), 4);
        assert_eq!(o.shapes[1].1, OccluderShape::Closed);
        assert_eq!(o.shapes[3].1, OccluderShape::Skyline);
        assert_eq!(o.masks, vec![0b0111, 0b0010, 0b1000, 0, 0]);

        // The shorthand is one path, cut by every shared shape
        let short: HashMap<String, CurveConfig> =
            toml::from_str("[k]\npoints = [[0.0, 0.5], [1.0, 0.5]]\nocclude = [[0.0, 0.2], [1.0, 0.2]]")
                .expect("parses");
        assert_eq!(short["k"].occlusion().masks, vec![1]);
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

    /// A palette reads as an array or as a `[colors]`-style table, in the
    /// table's numbered order, and is written back as the array
    #[test]
    fn a_palette_reads_both_ways_and_writes_an_array() {
        let w: WallpaperConfig = toml::from_str(
            r##"
            colors = ["#ff0000", { hex = "#00ff00", alpha = 0.5, role = "green" }]
            [[curve.path]]
            points = [[0.0, 0.5], [1.0, 0.5]]
            [curve.path.colors]
            c10 = "#0000aa"
            c2 = "#000022"
            "##,
        )
        .expect("parses");
        let own = w.colors.as_ref().expect("wallpaper palette");
        assert_eq!(own.0.len(), 2);
        assert_eq!(own.0[1].hex(), "#00ff00");
        assert!(own.0[1].has_role() && !own.0[0].has_role());
        let path = w.curve.as_ref().unwrap().paths()[0].colors.clone().expect("path palette");
        assert_eq!(path.0.iter().map(ConfigColor::hex).collect::<Vec<_>>(), ["#000022", "#0000aa"], "c2 before c10");
        let out = toml::to_string(&toml::Value::try_from(&w).unwrap()).unwrap();
        let back: WallpaperConfig = toml::from_str(&out).expect("what is written reads back");
        assert_eq!(back.colors, w.colors, "{out}");
        assert_eq!(back.curve.unwrap().paths()[0].colors.as_ref(), Some(&path), "{out}");
    }

    /// Several palettes: the header still describes the first, each palette
    /// lands where its slot says, and each is followed by its mean
    #[test]
    fn several_palettes_pack_back_to_back_with_their_means() {
        let base: &[[f32; 4]] = &[[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0], [0.0, 1.0, 0.0, 1.0]];
        let lone: &[[f32; 4]] = &[[0.5, 0.5, 0.5, 0.25]];
        let slots = palette_slots(&[3, 1]);
        assert_eq!(slots, [PaletteSlot { first: 0, stops: 3 }, PaletteSlot { first: 4, stops: 2 }]);
        let buf = gradient_buffer(&[base, lone]);
        let stop = |i: usize| -> [f32; 4] {
            let at = 16 + i * 16;
            std::array::from_fn(|k| f32::from_le_bytes(buf[at + k * 4..at + k * 4 + 4].try_into().unwrap()))
        };
        assert_eq!(&buf[0..4], &3i32.to_le_bytes(), "the header is the first palette's");
        assert_eq!(buf.len(), 16 + (3 + 1 + 2 + 1) * 16);
        assert_eq!(stop(3), [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 1.0], "the first palette's mean");
        assert_eq!(stop(4), lone[0]);
        assert_eq!(stop(5), lone[0], "a lone stop is still doubled");
        assert_eq!(stop(6), [0.5, 0.5, 0.5, 1.0]);
        // One palette is laid out exactly as it always was: no mean
        assert_eq!(gradient_buffer(&[base]).len(), 16 + 3 * 16);
        assert_eq!(palette_slots(&[3]), [PaletteSlot { first: 0, stops: 3 }]);
    }

    /// A path's own count is its count; only the paths without one share.
    /// Paths with nothing to draw neither ask nor share
    #[test]
    fn own_counts_add_and_the_rest_share() {
        let c: HashMap<String, CurveConfig> = toml::from_str(
            r"
            [all]
            bars = 99
            [[all.path]]
            bars = 3
            points = [[0.0, 0.5], [0.5, 0.5]]
            [[all.path]]
            bars = 10
            points = [[0.5, 0.5], [1.0, 0.5]]
            [mixed]
            [[mixed.path]]
            bars = 4
            points = [[0.0, 0.5], [0.5, 0.5]]
            [[mixed.path]]
            points = [[0.5, 0.5], [1.0, 0.5]]
            [[mixed.path]]
            bars = 50
            points = [[0.5, 0.5]]
            [short]
            bars = 27
            points = [[0.1, 0.2], [0.3, 0.4]]
            [empty]
            points = [[0.1, 0.2]]
            ",
        )
        .expect("parses");
        assert_eq!(c["all"].counts(), (13, false), "every path names its count: the curve's bars go unused");
        assert_eq!(c["mixed"].counts(), (4, true), "a one-point path asks for nothing");
        assert_eq!(c["short"].counts(), (0, true), "the shorthand shares the curve's bars");
        assert!(c["short"].is_drawable());
        assert!(!c["empty"].is_drawable());
        assert_eq!(c["empty"].counts(), (0, false));
    }

    #[test]
    fn a_broken_wallpaper_file_is_an_error_and_a_missing_one_is_not() {
        let dir = std::env::temp_dir().join(format!("cavawall-load-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("wallpapers")).unwrap();
        assert!(matches!(WallpaperConfig::load(&dir, "absent"), Ok(None)));
        std::fs::write(WallpaperConfig::path(&dir, "bad"), "mode = [").unwrap();
        let err = WallpaperConfig::load(&dir, "bad").expect_err("a typo is reported");
        assert!(err.contains("bad.toml"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A wallpaper's own rate survives a save, and an unset one is not written
    #[test]
    fn a_wallpaper_can_carry_its_own_framerate() {
        let w: WallpaperConfig = toml::from_str("mode = \"bars\"\nframerate = 30\n").unwrap();
        assert_eq!(w.framerate, Some(30));
        let out = toml::to_string(&toml::Value::try_from(&w).unwrap()).unwrap();
        assert!(out.contains("framerate = 30"), "{out}");
        let none = toml::to_string(&toml::Value::try_from(WallpaperConfig::default()).unwrap()).unwrap();
        assert!(!none.contains("framerate"), "{none}");
    }
}
