extern crate khronos_egl as egl;

use gl::types::{GLsizei, GLsizeiptr};
use smithay_client_toolkit::reexports::calloop::{
    generic::Generic, EventLoop, Interest, Mode as CalloopMode, PostAction,
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::ProvidesRegistryState;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    output::{OutputHandler, OutputInfo, OutputState},
    registry::RegistryState,
};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, registry_handlers,
};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy;
use wayland_client::{
    globals::{registry_queue_init, GlobalList},
    protocol::{wl_output, wl_surface},
    Connection, EventQueue, QueueHandle,
};
use wayland_egl::WlEglSurface;

use core::ffi;
use egl::API as egl;
use std::sync::atomic::{AtomicBool, Ordering};

// Set from the SIGTERM/SIGINT handler, which also writes WAKE so the event
// loop, asleep with no timeout, wakes to act on it
static EXITING: AtomicBool = AtomicBool::new(false);
/// An eventfd the loop watches; -1 until startup creates it
static WAKE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Below this, a bar counts as silence. One threshold for both halves of one
/// decision, park and unpark: two would park at one level and wake at another
const SILENCE_THRESHOLD: f32 = 0.005;
/// The same threshold in cava's raw 16-bit units, for comparing without
/// unpacking to f32 first
const SILENCE_RAW: u16 = (SILENCE_THRESHOLD * 65530.0) as u16;

/// `2.0 * (n / 65530.0) - 1.0` folded into one multiply-add per bar
/// One raw cava sample to NDC. The vertex fetch normalises a u16 by 65535, so
/// this divisor has to be the same one or damage stops covering the bars
const BAR_NDC_SCALE: f32 = 2.0 / 65535.0;

/// Damage rectangles emitted per frame
///
/// Bars are bucketed into this many columns, one rect per bucket rather than
/// one per bar. Rect COUNT costs a compositor as well as area: measured with
/// an SHM probe, 76 rects covering a quarter of a band cost more than one rect
/// of the same area
const DAMAGE_BUCKETS: usize = 8;
/// Masks a bucket index into range. Cheaper than the bounds check it replaces,
/// and unlike the check it cannot branch or panic inside the per-frame loop
const DAMAGE_MASK: usize = DAMAGE_BUCKETS - 1;
const _: () = assert!(DAMAGE_BUCKETS.is_power_of_two(), "DAMAGE_MASK needs a power of two");

/// `eglSwapBuffersWithDamageKHR`, resolved once
///
/// Plain `eglSwapBuffers` declares the WHOLE surface damaged. Declaring what
/// moved is what the protocol asks of a client
///
/// Measured on a dmabuf surface, same binary A/B'd with CAVAWALL_NO_DAMAGE:
/// neutral, not a win. A dmabuf already IS a texture, so there is no upload to
/// shrink; an SHM client uploads each damaged region and its cost does track
/// damaged area. Check the protocol before trusting any damage measurement:
///     WAYLAND_DEBUG=1 <client> 2>&1 | grep -oE 'wl_shm|zwp_linux_dmabuf_v1'
///
/// Worth keeping: free on both sides here, and the difference between workable
/// and hopeless wherever the surface does go through SHM or over a wire - a
/// VM, software rendering, waypipe
///
/// No GPU work either way. Every bar is still drawn; this changes only what
/// the compositor is told to re-read
type SwapDamageFn = unsafe extern "system" fn(
    egl::EGLDisplay,
    egl::EGLSurface,
    *const egl::Int,
    egl::Int,
) -> egl::Boolean;

/// The extension, or None when the driver lacks it and plain swaps are used
///
/// CAVAWALL_NO_DAMAGE=1 forces full-surface swaps, so both paths can be
/// compared from one binary
fn load_swap_with_damage(display: egl::Display) -> Option<SwapDamageFn> {
    if env::var_os("CAVAWALL_NO_DAMAGE").is_some_and(|v| v != "0") {
        return None;
    }
    let exts = egl.query_string(Some(display), egl::EXTENSIONS).ok()?;
    let exts = exts.to_str().ok()?;
    // KHR and EXT differ only in name; either is fine
    let name = if exts.contains("EGL_KHR_swap_buffers_with_damage") {
        "eglSwapBuffersWithDamageKHR"
    } else if exts.contains("EGL_EXT_swap_buffers_with_damage") {
        "eglSwapBuffersWithDamageEXT"
    } else {
        return None;
    };
    let f = egl.get_proc_address(name)?;
    // SAFETY: eglGetProcAddress returned a pointer for this exact name, whose
    // signature is fixed by the extension spec
    Some(unsafe { std::mem::transmute::<extern "system" fn(), SwapDamageFn>(f) })
}

/// Frames of continuous silence before parking. Measured, not guessed: with
/// monstercat=1.5 and noise_reduction=60 a tone cut from full volume decays
/// below the threshold in 8 frames (0.18s). 23 frames is 0.51s - roughly 3x
/// the real decay, the remainder being hysteresis so that a gap between tracks
/// does not park and unpark repeatedly
const SILENT_GRACE_FRAMES: u32 = 23;

/// Is this raw cava frame silence?
///
/// The one place that question is answered, per frame as it arrives, in
/// cava's own units: deciding on raw bytes keeps f32 unpacking off the path
fn is_silent(frame: &[u8]) -> bool {
    frame
        .as_chunks::<2>()
        .0
        .iter()
        .all(|s| u16::from_le_bytes(*s) <= SILENCE_RAW)
}

/// Connector-name prefixes that mean "the machine's own panel". Everything
/// else - HDMI, DP, DVI, a dock - counts as external and is preferred
///
/// A policy, not a per-machine list: nothing to keep in sync, and it survives
/// the panel's connector being renamed. eDP-1 and eDP-2 have been observed to
/// swap across reboots with no hardware change
const BUILTIN_CONNECTOR_PREFIXES: [&str; 3] = ["eDP", "LVDS", "DSI"];

fn is_builtin_connector(name: &str) -> bool {
    BUILTIN_CONNECTOR_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// CAVAWALL_DEBUG, resolved once. Some of the call sites below sit in the
/// per-frame path, and env::var allocates a String and takes the process-wide
/// environment lock on every call - not something to do 45 times a second
/// just to decide not to print
fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env::var_os("CAVAWALL_DEBUG").is_some_and(|v| v != "0"))
}

extern "C" fn on_terminate(_sig: libc::c_int) {
    // Only async-signal-safe work here: a flag and one write(2)
    EXITING.store(true, Ordering::SeqCst);
    let fd = WAKE.load(Ordering::Relaxed);
    if fd >= 0 {
        let one = 1u64;
        // SAFETY: write is async-signal-safe; eight bytes is an eventfd's unit
        unsafe { libc::write(fd, (&raw const one).cast(), 8) };
    }
}
use std::ffi::{CStr, CString};
use std::io::Write;
use std::process::exit;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::os::unix::ffi::OsStrExt;
use std::{env, fs, ptr};
use std::{
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

mod hypr;
mod render;
mod startup;
mod toplevel;
mod wayland;

use render::{bar_band, damage_rects, reveal_map, surface_map, BarPlacement, DamageMap};

use cavawall::{app_config, control, curve, fatal, say};
use app_config::WallpaperConfig;
use cavawall::math::fma;
use app_config::*;
use std::collections::HashSet;
use cavawall::scheme;
pub mod cli_help;
use cli_help::*;
use std::collections::HashMap;

const VERTEX_SHADER_SRC: &str = include_str!("shaders/vertex_shader.glsl");

const FRAGMENT_SHADER_SRC: &str = include_str!("shaders/fragment_shader.glsl");
const CIRCLE_VERTEX_SHADER_SRC: &str = include_str!("shaders/circle_vertex_shader.glsl");
const CIRCLE_FRAGMENT_SHADER_SRC: &str = include_str!("shaders/circle_fragment_shader.glsl");
const CURVE_VERTEX_SHADER_SRC: &str = include_str!("shaders/curve_vertex_shader.glsl");
const CURVE_FRAGMENT_SHADER_SRC: &str = include_str!("shaders/curve_fragment_shader.glsl");
const MASK_VERTEX_SHADER_SRC: &str = include_str!("shaders/mask_vertex_shader.glsl");
const MASK_FRAGMENT_SHADER_SRC: &str = include_str!("shaders/mask_fragment_shader.glsl");

/// Bar width and stride in NDC, both fixed until a re-exec
///
/// Handed to the vertex shader as uniforms, so a bar's geometry
/// be one instance of a unit quad rather than four vertices in a buffer
fn bar_geometry(bar_count: u32, gap: f32) -> (f32, f32) {
    let bars = bar_count as f32;
    // NDC is 2.0 wide, shared by `bars` bars and `bars - 1` gaps of `gap` bars
    let bar_width = 2.0 / (bars + (bars - 1.0) * gap);
    // Left edge to the next left edge
    (bar_width, bar_width * (1.0 + gap))
}

/// `[circle]` with every default filled in, so the GL setup and the placement
/// path both read plain values rather than `Option`s
#[derive(Clone, Copy)]
struct CircleGeom {
    /// Surface edge in logical pixels, before it is clamped to the output
    diameter: u32,
    /// Fraction of the radius the bars start at. The hole in the middle
    inner_radius: f32,
    inner_alpha: f32,
    outer_alpha: f32,
    anchor: CircleAnchor,
    /// Distance from the anchored edges, logical pixels
    margin: (u32, u32),
    /// Centre as fractions of the output; overrides anchor and margin
    position: Option<(f32, f32)>,
}

impl CircleGeom {
    /// Top and left margins that put a `d`-wide circle where `anchor` says, on
    /// an output `w` x `h`. Both are what `set_margin` wants alongside an
    /// `Anchor::TOP | Anchor::LEFT` surface
    ///
    /// Centring is computed here rather than left to the compositor, so every
    /// anchor takes the same path
    fn margins_for(&self, d: u32, w: u32, h: u32) -> (i32, i32) {
        let (mx, my) = (self.margin.0 as i32, self.margin.1 as i32);
        let (free_w, free_h) = (w.saturating_sub(d) as i32, h.saturating_sub(d) as i32);
        if let Some((x, y)) = self.position {
            let half = d as f32 * 0.5;
            let left = (x * w as f32 - half).round() as i32;
            let top = (y * h as f32 - half).round() as i32;
            return (top.clamp(0, free_h), left.clamp(0, free_w));
        }
        let centre_x = free_w / 2;
        let centre_y = free_h / 2;
        let (left, top) = match self.anchor {
            CircleAnchor::Center => (centre_x, centre_y),
            CircleAnchor::Top => (centre_x, my),
            CircleAnchor::Bottom => (centre_x, free_h - my),
            CircleAnchor::Left => (mx, centre_y),
            CircleAnchor::Right => (free_w - mx, centre_y),
            CircleAnchor::TopLeft => (mx, my),
            CircleAnchor::TopRight => (free_w - mx, my),
            CircleAnchor::BottomLeft => (mx, free_h - my),
            CircleAnchor::BottomRight => (free_w - mx, free_h - my),
        };
        // A margin larger than the free space would push the surface off the
        // output, where the compositor clips it and the circle silently
        // half-disappears
        (top.clamp(0, free_h), left.clamp(0, free_w))
    }
}

impl CircleGeom {
    fn from_config(c: Option<&CircleConfig>) -> Self {
        // Clamped here rather than trusted: inner_radius at 1.0 leaves no span
        // for a bar to grow into, and a negative one puts the base outside the
        // surface where it is silently clipped
        Self {
            diameter: c.and_then(|c| c.diameter).unwrap_or(520).max(16),
            inner_radius: c.and_then(|c| c.inner_radius).unwrap_or(0.35).clamp(0.0, 0.95),
            inner_alpha: c.and_then(|c| c.inner_alpha).unwrap_or(1.0).clamp(0.0, 1.0),
            outer_alpha: c.and_then(|c| c.outer_alpha).unwrap_or(1.0).clamp(0.0, 1.0),
            anchor: c.and_then(|c| c.anchor).unwrap_or_default(),
            margin: (
                c.and_then(|c| c.margin_x).unwrap_or(0),
                c.and_then(|c| c.margin_y).unwrap_or(0),
            ),
            position: c
                .and_then(|c| c.position)
                .map(|[x, y]| (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))),
        }
    }
}

/// One unit quad in triangle-strip order: top-left, top-right, bottom-left,
/// bottom-right. Each instance is its own strip, so bars cannot run into one
/// another and there is nothing to index
const UNIT_QUAD: [f32; 8] = [0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0];

/// Compile and link the one program this renderer has
///
/// # Panics
///
/// On a compile or link failure, with the driver's log. COMPILE_STATUS is
/// checked per stage: a link log does not name the offending line
fn build_program(mode: Mode, defines: &str) -> u32 {
    // Two programs, one picked at startup. Mode cannot change without a
    // re-exec, so nothing ever calls UseProgram again and the "GL state is set
    // once" invariant holds for both
    let (vert_src, frag_src) = match mode {
        Mode::Bars => (VERTEX_SHADER_SRC, FRAGMENT_SHADER_SRC),
        Mode::Circle => (CIRCLE_VERTEX_SHADER_SRC, CIRCLE_FRAGMENT_SHADER_SRC),
        // Curve's fragment stage is the circle's plus the occluder test
        Mode::Curve => (CURVE_VERTEX_SHADER_SRC, CURVE_FRAGMENT_SHADER_SRC),
    };
    link_program(&with_defines(vert_src, defines), &with_defines(frag_src, defines))
}

/// `src` with `defines` after its `#version` line, which must stay first.
/// Features that are off are then absent from the shader, not skipped in it
fn with_defines(src: &str, defines: &str) -> String {
    let (version, rest) = src.split_once('\n').unwrap_or((src, ""));
    format!("{version}\n{defines}{rest}")
}

/// Compile and link one pair of stages.
fn link_program(vert_src: &str, frag_src: &str) -> u32 {
    let vert = compile_shader(gl::VERTEX_SHADER, vert_src, "vertex");
    let frag = compile_shader(gl::FRAGMENT_SHADER, frag_src, "fragment");
    // SAFETY: main() has a current EGL context and loaded GL symbols by here
    unsafe {
        let program = gl::CreateProgram();
        gl::AttachShader(program, vert);
        gl::AttachShader(program, frag);
        gl::LinkProgram(program);
        let mut status: gl::types::GLint = 0;
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut status);
        if status != gl::TRUE as gl::types::GLint {
            fatal!("the GPU driver would not link the shaders; please report this with the log:\n{}", program_log(program));
        }
        // The linked program holds everything it needs; left attached, as they
        // were, both stages stay alive for the life of the process
        gl::DetachShader(program, vert);
        gl::DetachShader(program, frag);
        gl::DeleteShader(vert);
        gl::DeleteShader(frag);
        program
    }
}

/// Compile one stage straight from a `&str`: glShaderSource takes an explicit
/// length, so the `CString` only added a NUL the driver was told to ignore
fn compile_shader(kind: gl::types::GLenum, src: &str, what: &str) -> u32 {
    // SAFETY: as build_program; `src` outlives the ShaderSource call
    unsafe {
        let shader = gl::CreateShader(kind);
        gl::ShaderSource(
            shader,
            1,
            &src.as_ptr().cast::<gl::types::GLchar>(),
            &(src.len() as gl::types::GLint),
        );
        gl::CompileShader(shader);
        let mut status: gl::types::GLint = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut status);
        if status != gl::TRUE as gl::types::GLint {
            let mut len: gl::types::GLint = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
            fatal!(
                "the GPU driver would not compile the {what} shader; please report this with the log:\n{}",
                read_log(len, |n, written, buf| gl::GetShaderInfoLog(shader, n, written, buf))
            );
        }
        shader
    }
}

fn program_log(program: u32) -> String {
    // SAFETY: live program name, current context
    unsafe {
        let mut len: gl::types::GLint = 0;
        gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut len);
        read_log(len, |n, written, buf| gl::GetProgramInfoLog(program, n, written, buf))
    }
}

/// Drain an info log. Lossy and truncated to what was actually written: this
/// only runs on the way to a panic, so a short write must not turn a compile
/// error into an unrelated UTF-8 one
unsafe fn read_log(
    len: gl::types::GLint,
    get: impl Fn(GLsizei, *mut GLsizei, *mut gl::types::GLchar),
) -> String {
    let mut buf = vec![0u8; len.max(0) as usize];
    let mut written: GLsizei = 0;
    get(len, &mut written, buf.as_mut_ptr().cast());
    buf.truncate(written.max(0) as usize);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Log a resolved palette as hex, which is how it is written in the config and
/// in the scheme - printing the f32 quads it becomes would be unreadable
fn debug_palette(what: &str, rgba: &[[f32; 4]]) {
    if !debug_enabled() {
        return;
    }
    let stops: Vec<String> = rgba
        .iter()
        .map(|c| {
            format!(
                "#{:02x}{:02x}{:02x}@{:.2}",
                (c[0] * 255.0).round() as u8,
                (c[1] * 255.0).round() as u8,
                (c[2] * 255.0).round() as u8,
                c[3]
            )
        })
        .collect();
    say!("{what} palette: {}", stops.join(" "));
}






fn main() {
    startup::run();
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    width: u32,
    height: u32,
    layer_shell: LayerShell,
    layer_surface: LayerSurface,
    surface: WlSurface,
    /// cava's stdout, non-blocking. The pipe belongs to its event source for
    /// the life of the process; this is only ever read, in `on_cava`
    cava_fd: std::os::fd::RawFd,
    /// Frames land here straight from the pipe, and the newest is copied out.
    /// A multiple of the frame size, so a full read ends on a frame boundary
    cava_scratch: Box<[u8]>,
    /// Bytes of a frame already read, when a read ended mid-frame
    cava_partial: usize,
    /// A frame has arrived since the last draw
    fresh: bool,
    wl_egl_surface: WlEglSurface,
    egl_surface: egl::Surface,
    egl_config: egl::Config,
    egl_context: egl::Context,
    egl_display: egl::Display,
    /// The per-instance height buffer draw() streams into. The program, vertex
    /// array and static quad need no handle: bound once at startup, never
    /// rebound. The SSBO is the other exception - the palette is re-uploaded
    height_vbo: u32,
    bar_count: u32,
    /// Kept so the palette can be re-uploaded in place
    gradient_colors_ssbo: u32,
    /// The configured stops, already in gradient order
    color_stops: Vec<ConfigColor>,
    /// None when neither the palette nor the bar count follows the shell
    watch: Option<scheme::Watch>,
    /// The cava child, kept so a re-exec can kill and reap it
    cava_pid: u32,
    /// One raw cava frame, reused. It is also the whole per-frame vertex
    /// payload: everything else about a bar is a uniform or gl_InstanceID
    cava_buffer: Box<[u8]>,
    /// The frame on screen, for the damage comparison and to skip a frame
    /// that would draw the same pixels
    prev_frame: Box<[u8]>,
    /// Bar geometry in NDC, kept so the damage map can be rebuilt on a resize
    bar_width: f32,
    bar_stride: f32,
    /// Bar-to-pixel mapping, rebuilt only when the surface size changes
    damage_map: DamageMap,
    /// Byte size of `heights`, for the per-frame upload. Constant, so it is not
    /// re-derived from the slice every frame
    frame_bytes: GLsizeiptr,
    /// None when the driver has no swap-with-damage extension; then every frame
    /// declares the whole surface, exactly as before
    swap_damage: Option<SwapDamageFn>,
    /// Next frame must declare everything: the previous heights describe a
    /// different surface, or every pixel changed for a reason bar heights do
    /// not capture - a new palette, a resize, coming back from parked
    force_full_damage: bool,
    /// Where the row goes in bars mode
    bars_at: BarPlacement,
    /// Startup-only, like the bar count: the two modes are different programs
    /// with different uniforms and different surface geometry
    mode: Mode,
    /// Which config file this instance read, for `status`
    config_path: PathBuf,
    /// Where `bar_count` came from, for `status`: curve, circle, wallpaper,
    /// shell or config
    bars_from: &'static str,
    /// The count came from the shell's setting, so a change there re-execs.
    /// False whenever something more specific set it
    bars_follow_shell: bool,
    /// A frame callback has been requested and not yet received
    frame_pending: bool,
    /// A frame is owed: a callback arrived or a configure changed the surface.
    /// Drawn from `tick()`, never inside Wayland dispatch - a swap there reads
    /// the socket and refills the queue, so dispatch never returns while
    /// audio plays and every other source starves
    redraw: bool,
    circle: CircleGeom,
    /// Curve mode only. Kept so the bars can be rebuilt in configure: normals
    /// are perpendicular in PIXEL space, which depends on the output's aspect
    /// ratio, and that is not known until a surface is configured
    curve_paths: Box<[curve::PathSpec]>,
    path_ssbo: u32,
    width_ssbo: u32,
    /// Every bar, built when an output is chosen and uploaded on the next
    /// configure. They depend on the OUTPUT's shape - normals are
    /// perpendicular in pixel space, and the crop follows the output's aspect
    /// - so a configure that only resizes the surface does not change them
    curve_bars: Box<[curve::Bar]>,
    /// Occluders as authored, in IMAGE coordinates; index i is mask bit 1 << i.
    /// Where they land depends on how the wallpaper crops onto the output, so
    /// they are rasterised only once one is known
    occluders: Box<[curve::Occluder]>,
    /// The bits every bar tests: only those occluders can cut the surface
    common_mask: u16,
    /// Rasterises the occluders; None when there are none
    mask: Option<MaskPass>,
    /// How high the occluders every bar tests reach, in output coordinates.
    /// Kept so the bounding box can stop where nothing is visible below
    curve_horizon: Box<[f32]>,
    curve_fit: FitMode,
    /// The wallpaper the running curve was resolved against, and every key the
    /// config has one for. Together they answer the only question a wallpaper
    /// change asks: would this still draw the same thing?
    curve_key: Option<String>,
    curve_keys: HashSet<String>,
    /// The bar VAO and program. Bound at startup; kept so the mask pass can
    /// hand the pipeline back after rasterising the occluders
    vao: u32,
    program: u32,
    /// The wallpaper's pixel size, when its format could be read
    curve_image: Option<(u32, u32)>,
    /// Where the curve surface sits on the output, in output pixels:
    /// (left, top, width, height). None means the whole output
    curve_box: Option<(u32, u32, u32, u32)>,
    /// Output size, which stops being `width`/`height` the moment the surface
    /// is smaller than the output it is on
    curve_output: (u32, u32),
    /// Output width over height. Normals are unit length in pixels, and NDC
    /// stretches x by this
    aspect_location: gl::types::GLint,
    /// Sizes in pixels for the rounded tips; -1, so ignored, when rounding
    /// is compiled out
    surface_px_location: gl::types::GLint,
    output_px_location: gl::types::GLint,
    /// Where the surface's top left sits on the output, logical pixels
    surface_origin: (u32, u32),
    /// The reveal image's size when bars show one, for the crop that maps a
    /// fragment to its texel
    reveal_size: Option<(u32, u32)>,
    reveal_map_location: gl::types::GLint,
    /// The per-bar heights, persistently mapped; None on a context too old
    /// for it, where each frame respecifies the buffer instead
    ring: Option<HeightRing>,
    /// Kept because the matte tone follows the palette, which a live scheme
    /// can change under us
    matte_color_location: gl::types::GLint,
    path_scale_location: gl::types::GLint,
    path_offset_location: gl::types::GLint,
    silent_frames: u32,
    /// Only read to restore the clear colour if a re-exec fails; it is set once
    /// at startup now rather than per frame
    background_color: [f32; 4],
    /// Explicit output pin: CAVAWALL_OUTPUT, else the config's
    /// preferred_output. None means choose automatically - an external
    /// monitor if one is connected, the built-in panel otherwise
    pinned_output: Option<String>,
    /// `general.on_fullscreen`, and the monitors a visible fullscreen window
    /// covers right now. Empty and untouched when the policy is ignore
    on_fullscreen: FullscreenPolicy,
    covered: std::collections::BTreeSet<String>,
    /// A line of Hyprland events split across reads
    hypr_partial: Vec<u8>,
    /// cava suspended with SIGSTOP while nothing can be shown
    cava_stopped: bool,
    /// Windows from the foreign-toplevel protocol, where Hyprland's IPC is
    /// not there to ask
    toplevels: toplevel::Toplevels,
    /// Name of the output we currently have a mapped surface on. None means
    /// nothing is drawn: either no output has been chosen yet, or the one we
    /// were on went away
    placed_on: Option<String>,
    /// Logical size that surface was built for, so an unrelated output
    /// property change does not tear it down and rebuild it for nothing
    placed_size: Option<(i32, i32)>,
    /// False until the startup roundtrip has enumerated every output. Outputs
    /// are announced one at a time, so acting on the first one to arrive meant
    /// placing on the laptop panel and then moving to the external monitor a
    /// moment later - a visible flash of bars on the wrong screen at login
    startup_settled: bool,
    compositor: CompositorState,
    /// Parked: silent, not committing, waiting for audio on the idle tick
    idle: bool,
    /// Kept because event_loop.run's callback hands back only &mut AppState,
    /// and `tick()` draws, which requests frame callbacks
    qh: QueueHandle<AppState>,
    /// Same reason, for the roundtrip in clear_and_exit and reexec
    conn: Connection,
}

impl AppState {
    /// Paint one fully transparent frame and commit it before exiting, so the
    /// compositor is left with a clean surface rather than our last set of
    /// bars. Without this a hard kill leaves that frame visible on the
    /// background until something else forces a repaint
    fn clear_and_exit(&mut self, why: &str) -> ! {
        say!("stopping: {why}");
        cavawall::log::exited(why);
        cavawall::notify::send(NotifyEvent::Stop, &format!("stopped: {why}"));
        self.clear_surface();
        control::unbind();
        // SAFETY: a plain signal to our own child
        unsafe { libc::kill(self.cava_pid as libc::pid_t, libc::SIGKILL) };
        std::process::exit(0);
    }

    /// Present one transparent frame and wait for it to reach the compositor.
    /// Hyprland does not reliably repaint under a layer surface that just
    /// vanishes, so leaving without this burns the last bars onto the wallpaper
    ///
    /// Nothing to clear when unplaced: the surface was closed or never mapped
    fn clear_surface(&mut self) {
        if self.placed_on.is_none() {
            return;
        }
        unsafe {
            gl::ClearColor(0.0, 0.0, 0.0, 0.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
        let _ = egl.swap_buffers(self.egl_display, self.egl_surface);
        // Round-trip so the commit reaches the compositor before our objects
        // are destroyed
        let _ = self.conn.roundtrip();
    }

    /// Act on the external watches: a new palette, a new bar count or a new
    /// wallpaper. An event source, so this runs only when a file changed
    ///
    /// Unplaced is fine: a context is always current, on the last surface if
    /// not a live one, and clear_surface skips the clear when nothing is shown.
    /// It has to drain regardless - the source is level-triggered
    pub fn poll_external(&mut self) {
        let changed = self.watch.as_mut().map(scheme::Watch::take).unwrap_or_default();
        self.act_on(changed);
    }

    /// What poll_external does with a set of changes; `cavawall refresh`
    /// comes here directly
    fn act_on(&mut self, changed: scheme::Changed) {
        // Ordered so the common case (nothing changed) never reaches
        // debug_enabled(). The watch is otherwise unobservable from outside
        if (changed.scheme || changed.shell || changed.wallpaper) && debug_enabled() {
            say!(
                "watch fired scheme={} shell={} wallpaper={}",
                changed.scheme, changed.shell, changed.wallpaper
            );
        }
        // Bars first: a changed count re-execs, which re-reads the scheme on the
        // way up anyway, so resolving colours before that would be thrown away
        //
        // Only when the running count came from the shell. A curve, a circle or
        // a wallpaper that names its own count wins over it, and comparing
        // against the shell anyway would re-exec on every settings change
        if changed.shell && self.bars_follow_shell {
            // Every settings change rewrites the whole of shell.json, so most
            // wake-ups here are about something else. Compare before acting:
            // an unrelated toggle must not restart the visualiser
            if scheme::bar_count().is_some_and(|n| n != self.bar_count) {
                self.reexec();
            }
        }
        // Same reasoning as the bar count, and the same answer: a curve is
        // resolved against ONE wallpaper, and everything downstream of that
        // choice - which shader program, how many bars, how big a surface -
        // is fixed at startup. Rebuilding all of it in place would be a second
        // implementation of startup; re-exec is the one that already works
        if changed.wallpaper {
            let key = curve::current_wallpaper().and_then(|w| curve::content_key(&w));
            // Nothing to do when neither the old wallpaper nor the new one has
            // a curve: both draw plain bars, and restarting to keep drawing
            // the same thing is churn the user would see as a flicker
            let drawn = self.curve_key.as_ref().is_some_and(|k| self.curve_keys.contains(k));
            let wanted = key.as_ref().is_some_and(|k| self.curve_keys.contains(k));
            if key != self.curve_key && (drawn || wanted) {
                if debug_enabled() {
                    say!(
                        "wallpaper changed {:?} -> {key:?}, restarting",
                        self.curve_key
                    );
                }
                self.reexec();
            }
            self.curve_key = key;
        }
        if changed.scheme {
            self.reload_colors();
        }
    }

    /// Answer one client. Orders that replace or end the process do not return
    fn serve_control(&mut self, stream: &std::os::unix::net::UnixStream) {
        use control::{Request, Response};
        // This runs on the render thread: a client that connects and never
        // writes would otherwise stop drawing and SIGTERM handling with it.
        // Accepted sockets do not inherit the listener's O_NONBLOCK
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(control::SERVE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(control::SERVE_TIMEOUT));
        let Some(req) = control::read_request(stream) else {
            control::write_response(stream, &Response::err("unparseable request"));
            return;
        };
        match req {
            Request::Status => {
                let data = serde_json::json!({
                    "pid": std::process::id(),
                    "version": env!("CARGO_PKG_VERSION"),
                    "exe": std::env::current_exe().ok().map(|p| p.display().to_string()),
                    "config": self.config_path.display().to_string(),
                    "mode": self.mode,
                    "pinned_output": self.pinned_output,
                    "placed_on": self.placed_on,
                    "covered": self.covered,
                    "outputs": self.output_state.outputs().filter_map(|o| self.output_state.info(&o)?.name).collect::<Vec<_>>(),
                    "on_fullscreen": self.on_fullscreen,
                    "output_size": self.placed_size,
                    "bars": self.bar_count,
                    "bars_from": self.bars_from,
                    "parked": self.idle,
                    "curve_key": self.curve_key,
                });
                control::write_response(stream, &Response::ok(Some(data)));
            }
            // Answer before acting: re-exec never comes back to write one
            Request::Move { output } => {
                match &output {
                    Some(name) => env::set_var("CAVAWALL_OUTPUT", name),
                    None => env::remove_var("CAVAWALL_OUTPUT"),
                }
                control::write_response(stream, &Response::ok(None));
                self.reexec();
            }
            Request::Refresh => {
                control::write_response(stream, &Response::ok(None));
                self.act_on(scheme::Changed { scheme: true, shell: false, wallpaper: true });
            }
            Request::Stop => {
                control::write_response(stream, &Response::ok(None));
                self.clear_and_exit("asked to stop");
            }
            // Bars, mode and curve are all sampled at startup, so re-reading
            // config means running again - the same file, never PATH
            Request::Reload => {
                control::write_response(stream, &Response::ok(None));
                self.reexec();
            }
        }
    }

    /// Start over, because the bar count changed and cannot be changed in place
    ///
    /// It reaches the GPU as an index buffer sized once at startup, and reaches
    /// cava as a config written to that child's stdin at exec time. Neither is
    /// reachable from here, so a fresh process is the only honest way to apply
    /// a new count - which is why colours update live and this does not
    ///
    /// exec, rather than spawning cavawall-launch as everything else does. exec
    /// keeps the PID, so every guard built on "is one running" stays true right
    /// through the swap: the launcher's flock and 5s kill-wait. There is never a moment with zero or
    /// two instances, so the stacking race that lock exists for cannot start
    /// here. The environment carries over too, so a CAVAWALL_OUTPUT pin set by
    /// `cavawall move` survives the restart
    fn reexec(&mut self) {
        // On exec our Wayland connection closes exactly as it would on a kill
        self.clear_surface();

        // Kill and reap cava before replacing our image, because exec keeps the
        // PID and therefore keeps the children: the outgoing cava stays OUR
        // child, the incoming image has no handle on it and never waits for it,
        // and nothing else will ever collect it. It dies on its own the moment
        // its stdout pipe closes, so what is left is a zombie - one per
        // bar-count change, all parented to a process that will not reap them.
        // Measured: eight bar-count changes left seven <defunct> cava
        //
        // SIGKILL rather than SIGTERM: cava owns no surface, no files and no
        // cleanup worth waiting on, and we want it gone before the exec rather
        // than at some point after it
        unsafe { libc::kill(self.cava_pid as libc::pid_t, libc::SIGKILL) };
        // Reaps that child and any zombie an earlier re-exec left behind, since
        // those are still ours for the same reason. Terminates on ECHILD
        loop {
            if unsafe { libc::waitpid(-1, ptr::null_mut(), 0) } <= 0 {
                break;
            }
        }

        // argv[0] before current_exe(): cavawall-launch execs us by absolute
        // path, and after a `cargo install` over a running instance
        // /proc/self/exe reads back as "<path> (deleted)", which will not exec.
        // Both are checked for existence so neither can hand over a dead path
        let program: Option<PathBuf> = env::args_os()
            .next()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.exists())
            .or_else(|| env::current_exe().ok().filter(|p| p.exists()));
        let args: Vec<CString> = env::args_os()
            .filter_map(|a| CString::new(a.as_os_str().as_bytes()).ok())
            .collect();

        // The next image is this instance carrying on, not a new start
        env::set_var("CAVAWALL_REEXEC", "1");
        if let Some(program) = program {
            if let Ok(prog) = CString::new(program.as_os_str().as_bytes()) {
                let mut argv: Vec<*const libc::c_char> =
                    args.iter().map(|a| a.as_ptr()).collect();
                argv.push(ptr::null());
                unsafe { libc::execv(prog.as_ptr(), argv.as_ptr()) };
            }
        }
        // Only reachable if the exec failed. Carrying on at the old bar count
        // beats dying over a settings change: the surface just cleared gets
        // repainted by the next draw, so the visible cost is one blank frame
        //
        // The clear colour has to go back: it is set once at startup now, so
        // the transparent one above would otherwise stand for the session
        unsafe {
            gl::ClearColor(
                self.background_color[0],
                self.background_color[1],
                self.background_color[2],
                self.background_color[3],
            );
        }
        say!(
            "re-exec failed, keeping {} bars: {}",
            self.bar_count,
            std::io::Error::last_os_error()
        );
    }

    /// Re-resolve the palette against the current scheme and re-upload it
    ///
    /// Cheap enough to do inline: one small buffer upload, no pipeline rebuild,
    /// no surface reconfigure. Nothing reachable from here can change the stop
    /// COUNT - a role the scheme lacks falls back to that stop's own hex rather
    /// than dropping it - so no geometry is invalidated and the next frame
    /// simply draws in the new colours
    ///
    /// A scheme that will not read or parse leaves the current palette alone
    /// instead of falling back to the static one. The file is written while we
    /// may be reading it, and a momentary flash of the fallback palette every
    /// time the wallpaper changes would be worse than a frame of staleness
    fn reload_colors(&mut self) {
        let Some(live) = scheme::colours() else {
            return;
        };
        let rgba = resolve_stops(&self.color_stops, Some(&live));
        debug_palette("reloaded", &rgba);
        // Every bar is repainted in new colours, which bar heights do not
        // describe: without this the next frame would declare only the bars
        // that moved and leave the rest in the old palette on screen
        self.force_full_damage = true;
        let buf = gradient_buffer(&rgba);
        let mean = palette_mean(&rgba);
        unsafe {
            // The matte tone is the palette's mean, so a new palette means a
            // new tone; leaving it would flatten toward the old scheme
            gl::Uniform3f(self.matte_color_location, mean[0], mean[1], mean[2]);
            gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, self.gradient_colors_ssbo);
            gl::BufferData(
                gl::SHADER_STORAGE_BUFFER,
                buf.len() as GLsizeiptr,
                buf.as_ptr() as *const ffi::c_void,
                gl::STATIC_DRAW,
            );
            gl::BindBufferBase(gl::SHADER_STORAGE_BUFFER, 0, self.gradient_colors_ssbo);
            gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, 0);
        }
    }

    /// Runs after every event-loop dispatch. The loop sleeps with no timeout:
    /// cava's frames, frame callbacks, the watches, the control socket and
    /// signals all arrive as events, so an idle instance wakes for nothing
    ///
    /// Frames are drawn here or from `on_cava`, never inside Wayland dispatch.
    /// A swap reads the Wayland socket and queues the next callback, so a draw
    /// inside dispatch kept it busy for as long as audio played, and no other
    /// source got a turn
    pub fn tick(&mut self) {
        if EXITING.load(Ordering::Relaxed) {
            self.clear_and_exit("SIGTERM or SIGINT");
        }
        self.maybe_draw();
    }

    /// Draw when both halves are in: the compositor wants a frame, and cava
    /// has produced one. Whichever arrives second triggers it
    fn maybe_draw(&mut self) {
        if self.redraw && self.fresh && !self.frame_pending && !self.idle && self.placed_on.is_some() {
            self.draw();
        }
    }

    /// cava's pipe is readable. Take every whole frame waiting, count silence
    /// frame by frame, keep only the newest: a backlog after a stall is
    /// dropped rather than fast-forwarded through
    ///
    /// Parking and unparking are both decided here, per frame, against the
    /// one threshold in `is_silent`
    pub fn on_cava(&mut self) {
        let frame = self.cava_buffer.len();
        let mut got = false;
        let mut loud = false;
        loop {
            let at = self.cava_partial;
            let room = self.cava_scratch.len() - at;
            // SAFETY: the fd is the open read end of cava's pipe for the life
            // of the process, and the range is inside the scratch buffer
            let n = unsafe {
                libc::read(self.cava_fd, self.cava_scratch.as_mut_ptr().add(at).cast(), room)
            };
            if n == 0 {
                self.cava_gone();
            }
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break; // EAGAIN: drained
            }
            let end = at + n as usize;
            let whole = end - end % frame;
            for f in self.cava_scratch[..whole].chunks_exact(frame) {
                if is_silent(f) {
                    self.silent_frames = self.silent_frames.saturating_add(1);
                } else {
                    self.silent_frames = 0;
                    loud = true;
                }
            }
            if whole > 0 {
                self.cava_buffer.copy_from_slice(&self.cava_scratch[whole - frame..whole]);
                got = true;
            }
            self.cava_scratch.copy_within(whole..end, 0);
            self.cava_partial = end - whole;
            // Short means the pipe is empty; no second read to be told so
            if (n as usize) < room {
                break;
            }
        }
        if !got {
            return;
        }
        self.fresh = true;
        if self.idle {
            if !loud {
                return;
            }
            self.idle = false;
            // Parked frames were never presented, so prev_frame describes one
            // older than what is on screen
            self.force_full_damage = true;
            self.redraw = true;
        } else if self.silent_frames > SILENT_GRACE_FRAMES {
            // PARK: no draw and no commit. Hyprland damages a layer by its
            // geometry on any commit, buffer attached or not, so even a
            // bufferless one recomposites the band. The grace lets the bars
            // finish falling first: with monstercat=1.5 and noise_reduction=60
            // a tone cut from full volume decays below the threshold in 8
            // frames (0.18s); the rest of the 23 (0.51s) is hysteresis, so a
            // gap between tracks does not park and unpark repeatedly
            if debug_enabled() {
                say!("parking (silent_frames={})", self.silent_frames);
            }
            self.idle = true;
            return;
        }
        self.maybe_draw();
    }

    /// Leave the way SIGTERM does. `panic = "abort"` skips all cleanup, so
    /// panicking instead leaves the last frame burnt onto the wallpaper
    fn cava_gone(&mut self) -> ! {
        self.clear_and_exit("cava exited; check the audio source, or run cava by hand to see why");
    }

    /// Rank a connected output; lower wins, None means "not eligible at all".
    ///
    /// A pin excludes everything else outright rather than merely preferring
    /// the pinned output - when `cavawall move eDP-1` names a monitor, falling
    /// back to another would defeat the point
    fn output_rank(&self, name: &str) -> Option<u8> {
        if self.on_fullscreen == FullscreenPolicy::Move && self.covered.contains(name) {
            return None;
        }
        match &self.pinned_output {
            Some(pin) => (pin == name).then_some(0),
            None => Some(u8::from(is_builtin_connector(name))),
        }
    }

    /// The output we should be on, out of everything currently connected
    ///
    /// Ties break on name purely so the choice is stable: two externals must
    /// not swap between calls and rebuild the surface each time
    ///
    /// Hands back the name it resolved, so the caller does not clone it again
    fn choose_output(&self) -> Option<(wl_output::WlOutput, OutputInfo, String)> {
        self.output_state
            .outputs()
            .filter_map(|o| {
                let info = self.output_state.info(&o)?;
                // No logical size yet means the compositor has not finished
                // describing it; set_size would have nothing to work from
                info.logical_size?;
                let name = info.name.clone()?;
                let rank = self.output_rank(&name)?;
                Some((rank, name, o, info))
            })
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(_, name, o, info)| (o, info, name))
    }

    /// Re-run the whole output policy against what is connected right now, and
    /// move if the answer changed. Every OutputHandler callback funnels here,
    /// so the three cannot drift apart
    fn retarget(&mut self, qh: &QueueHandle<Self>) {
        // Drop a placement whose output is gone before choosing a new one.
        // Checked against the live output list rather than trusting which
        // output the callback named, so this holds no matter what order
        // OutputState applies the removal in
        //
        // Borrowed, not cloned: `placed_on` and `output_state` are separate
        // fields, so both shared borrows coexist
        let still_connected = self.placed_on.as_deref().is_none_or(|current| {
            self.output_state
                .outputs()
                .filter_map(|o| self.output_state.info(&o))
                .any(|i| i.name.as_deref() == Some(current))
        });
        if !still_connected {
            self.placed_on = None;
            self.placed_size = None;
        }

        let chosen = self
            .choose_output()
            .filter(|(_, _, name)| !(self.on_fullscreen == FullscreenPolicy::Pause && self.covered.contains(name)));
        let Some((output, info, name)) = chosen else {
            if self.placed_on.is_some() {
                say!("nothing to show on: {}", if self.covered.is_empty() { "no usable output" } else { "covered by fullscreen" });
            }
            self.hide();
            return;
        };
        if self.placed_on.as_deref() == Some(name.as_str()) && self.placed_size == info.logical_size
        {
            return; // already there, same size - nothing worth rebuilding for
        }
        self.place_on(qh, &output, &info, name);
    }

    /// The path's bounding box in output pixels, or None to keep the whole
    /// output
    ///
    /// Bars are angled, so the box is the hull of every bar at full volume,
    /// not just of the path. A box that saves little is not worth the mapping:
    /// below a fifth saved this returns None and the surface stays whole
    fn curve_bbox(&self) -> Option<(u32, u32, u32, u32)> {
        if self.curve_bars.is_empty() {
            return None;
        }
        let (ow, oh) = (self.width as f32, self.height as f32);
        let (x0, y0, x1, y1) = curve::bounds(&self.curve_bars, ow / oh.max(1.0), self.bars_at.mirror);
        // NDC -> pixels, y flipped: NDC counts up, a margin counts down
        let pad = 2.0;
        let left = (((x0 + 1.0) * 0.5 * ow) - pad).floor().clamp(0.0, ow);
        let right = (((x1 + 1.0) * 0.5 * ow) + pad).ceil().clamp(0.0, ow);
        let top = ((1.0 - (y1 + 1.0) * 0.5) * oh - pad).floor().clamp(0.0, oh);
        let bottom = ((1.0 - (y0 + 1.0) * 0.5) * oh + pad).ceil().clamp(0.0, oh);
        // The occluder cuts the box too: every fragment below the ridge is
        // discarded, so the surface never has to reach down there. On a
        // ridgeline that sits high in the frame this is the difference
        // between a band and a strip
        let bottom = match self.horizon_floor(left / ow, right / ow) {
            Some(h) => bottom.min(((1.0 - h) * oh + pad).ceil().clamp(0.0, oh)),
            None => bottom,
        };
        let (w, h) = ((right - left).max(1.0), (bottom - top).max(1.0));
        if w * h > ow * oh * 0.8 {
            return None;
        }
        Some((left as u32, top as u32, w as u32, h as u32))
    }

    /// How this wallpaper's coordinates land on an output of this size
    ///
    /// Without a readable image size there is nothing to crop against, so the
    /// image is taken as already output-shaped
    fn fit_for(&self, out: (u32, u32)) -> curve::Fit {
        match (self.curve_fit, self.curve_image) {
            (FitMode::Cover, Some(image)) => curve::Fit::cover(image, out),
            _ => curve::Fit::STRETCH,
        }
    }

    /// The lowest any silhouette drops across `x0..x1`, as a height above the
    /// bottom of the output, or None when none of them do
    ///
    /// Below this nothing is drawn at any x in the range, so it is a floor
    /// for the surface and not just for one column
    fn horizon_floor(&self, x0: f32, x1: f32) -> Option<f32> {
        let n = self.curve_horizon.len();
        if n < 2 {
            return None;
        }
        let last = n - 1;
        let lo = (x0.clamp(0.0, 1.0) * last as f32).floor() as usize;
        let hi = (x1.clamp(0.0, 1.0) * last as f32).ceil() as usize;
        self.curve_horizon[lo.min(last)..=hi.min(last)]
            .iter()
            .copied()
            .reduce(f32::min)
    }

    /// Nothing can be shown: unmap the surface and suspend cava, so a covered
    /// or outputless instance costs nothing until it can draw again
    fn hide(&mut self) {
        if self.placed_on.take().is_some() {
            self.surface.attach(None, 0, 0);
            self.surface.commit();
        }
        self.placed_size = None;
        if !self.cava_stopped {
            // SAFETY: a plain signal to our own child
            unsafe { libc::kill(self.cava_pid as libc::pid_t, libc::SIGSTOP) };
            self.cava_stopped = true;
        }
    }

    /// A window's fullscreen state or outputs changed, from foreign-toplevel
    pub fn recheck_toplevels(&mut self, qh: &QueueHandle<Self>) {
        let now = self.toplevels.covered(|o| self.output_state.info(o)?.name);
        if now != self.covered {
            self.covered = now;
            self.retarget(qh);
        }
    }

    /// Hyprland said something that may cover or uncover a monitor
    pub fn on_hypr(&mut self, events: &mut std::os::unix::net::UnixStream) {
        if !hypr::relevant(events, &mut self.hypr_partial) {
            return;
        }
        // Unreadable is not "nothing covered": keep the last answer
        if let Some(now) = hypr::covered() {
            if now != self.covered {
                self.covered = now;
                let qh = self.qh.clone();
                self.retarget(&qh);
            }
        }
    }

    /// Build a fresh layer surface on `output` and start drawing to it
    ///
    /// Moving is always a rebuild: a layer surface belongs to the output it was
    /// created for and there is no request to move one across
    fn place_on(
        &mut self,
        qh: &QueueHandle<Self>,
        output: &wl_output::WlOutput,
        info: &OutputInfo,
        name: String,
    ) {
        let Some(logical_size) = info.logical_size else {
            return;
        };
        if debug_enabled() {
            say!("placing on {name} ({}x{})", logical_size.0, logical_size.1);
        }
        if self.cava_stopped {
            // SAFETY: a plain signal to our own child
            unsafe { libc::kill(self.cava_pid as libc::pid_t, libc::SIGCONT) };
            self.cava_stopped = false;
        }
        self.surface = self.compositor.create_surface(qh);
        let fresh = self.layer_shell.create_layer_surface(
            qh,
            self.surface.clone(),
            Layer::Bottom,
            Some("cavawall"),
            Some(output),
        );
        // Kept alive until EGL has moved off it: dropping a LayerSurface
        // destroys its wl_surface too, and the EGL window still points there
        let old_layer = std::mem::replace(&mut self.layer_surface, fresh);
        self.width = logical_size.0 as u32;
        self.height = logical_size.1 as u32;
        // same empty input region as at startup - the surface is
        // recreated here, so it would otherwise regain the default one
        let input_region = Region::new(&self.compositor).ok();
        if let Some(r) = &input_region {
            self.layer_surface.set_input_region(Some(r.wl_region()));
        }
        // Only ask for the band the bars can actually reach, anchored to the
        // bottom they grow from
        //
        // Hyprland damages a layer by its GEOMETRY, not by the buffer damage
        // a client declares - verified by trying the latter first:
        // eglSwapBuffersWithDamageKHR sent .damage_buffer(0, 377, 1920, 703)
        // 331 times and the damage overlay still showed the whole output.
        // Shrinking the surface moved it immediately. So surface size is the
        // only lever a client has here
        //
        // max_height caps how far up a full-volume bar goes, as a fraction of
        // the screen, so anything above it is cleared-transparent every frame
        // and recomposited for nothing. With the default 0.65 that is the top
        // 35% of the output
        //
        // The bar NDC is rescaled to match (see draw) so the bars look
        // identical - inside a surface that IS the band, they use its full
        // height rather than max_height of it
        self.layer_surface.set_exclusive_zone(-1); // see note at startup
        let requested = match self.mode {
            Mode::Bars => {
                let band = bar_band(&self.bars_at, self.width, self.height);
                self.surface_origin = (band.left, band.top);
                self.layer_surface.set_size(band.width, band.height);
                if band.bottom_row {
                    self.layer_surface.set_anchor(Anchor::BOTTOM);
                } else {
                    self.layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
                    self.layer_surface.set_margin(band.top as i32, 0, 0, band.left as i32);
                }
                (band.width, band.height)
            }
            // A square surface keeps NDC square and the circle round with no
            // aspect uniform. Anchored to a corner and positioned
            // with margins rather than left to centre itself, so offset_x/y
            // have somewhere to apply
            //
            // This claims diameter^2 where the bar band claims the full width
            // times max_height - on a 1920x1080 output a 520px circle is 270k
            // pixels against 1.3M, so the same damage argument that shrank the
            // band favours this even more strongly.
            // The path is authored against the OUTPUT, so the surface can be
            // anything as long as the shader maps output NDC into it - which
            // PathScale/PathOffset do. A ridgeline across the upper third of a
            // 1920x1080 output claims about 300k pixels where the whole output
            // is 2.1M, and Hyprland recomposites by geometry
            Mode::Curve => {
                self.curve_output = (self.width, self.height);
                // Both of these depend on the output's shape, because the
                // crop does: a move to a differently proportioned monitor
                // re-lands the whole curve rather than leaving it beside the
                // ridge it was drawn on
                let fit = self.fit_for(self.curve_output);
                let aspect = self.width as f32 / self.height.max(1) as f32;
                self.curve_bars = curve::build(&self.curve_paths, self.bar_count, aspect, fit).into();
                self.curve_horizon =
                    curve::occluder_floor(&self.occluders, self.common_mask, fit).into_boxed_slice();
                self.curve_box = self.curve_bbox();
                match self.curve_box {
                    Some((left, top, w, h)) => {
                        self.surface_origin = (left, top);
                        self.layer_surface.set_size(w, h);
                        self.layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
                        self.layer_surface.set_margin(top as i32, 0, 0, left as i32);
                        (w, h)
                    }
                    None => {
                        self.surface_origin = (0, 0);
                        self.layer_surface.set_size(self.width, self.height);
                        self.layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
                        (self.width, self.height)
                    }
                }
            }
            Mode::Circle => {
                let d = self.circle.diameter.min(self.width).min(self.height).max(1);
                let (top, left) = self.circle.margins_for(d, self.width, self.height);
                self.surface_origin = (left.max(0) as u32, top.max(0) as u32);
                self.layer_surface.set_size(d, d);
                self.layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
                self.layer_surface.set_margin(top, 0, 0, left);
                (d, d)
            }
        };
        self.surface.commit();
        drop(input_region);
        self.rebind_egl(requested);
        // Role object first, then its wl_surface; SCTK's drop does both
        drop(old_layer);
        // A fresh surface carries no frame callback and nothing parked. The
        // configure this commit provokes is what starts the loop, and it only
        // draws because placed_on is set here first
        self.idle = false;
        self.silent_frames = 0;
        self.frame_pending = false;
        self.redraw = false;
        self.placed_on = Some(name);
        self.placed_size = info.logical_size;
    }

    /// Point EGL at `self.surface`, which has just been replaced
    ///
    /// The context is unbound first: NVIDIA leaves a surface destroyed while
    /// current in a state where the next one fails eglSwapBuffers with
    /// EGL_BAD_SURFACE. The swap interval belongs to the surface, so a new one
    /// is back at the driver's default of 1 - on NVIDIA that is FIFO, which
    /// costs a second commit per frame and a vsync wait inside every swap
    fn rebind_egl(&mut self, (w, h): (u32, u32)) {
        let (w, h) = (w.max(1) as i32, h.max(1) as i32);
        egl.make_current(self.egl_display, None, None, None).ok();
        egl.destroy_surface(self.egl_display, self.egl_surface).ok();
        self.wl_egl_surface =
            WlEglSurface::new(self.surface.id(), w, h).unwrap_or_else(|e| fatal!("cannot create the EGL window for the new surface ({e})"));
        // SAFETY: the window was just created for a live wl_surface and
        // outlives the EGL surface, which is destroyed before it is replaced
        self.egl_surface = unsafe {
            egl.create_window_surface(
                self.egl_display,
                self.egl_config,
                self.wl_egl_surface.ptr() as egl::NativeWindowType,
                None,
            )
        }
        .unwrap_or_else(|e| fatal!("cannot create the EGL surface for the new output ({e})"));
        egl.make_current(
            self.egl_display,
            Some(self.egl_surface),
            Some(self.egl_surface),
            Some(self.egl_context),
        )
        .unwrap_or_else(|e| fatal!("cannot make the new surface current ({e})"));
        if egl.swap_interval(self.egl_display, 0).is_err() && debug_enabled() {
            say!("swap interval unchanged, frames pace on vsync too");
        }
    }

}

/// Upload every bar: one vec4 of base and unit normal, one vec4 of width,
/// reach and occluder mask
///
/// Rewritten whole, since it changes only when the output's shape does, which
/// is a monitor change, not a frame
///
/// # Safety
/// A GL context must be current and both buffers must already exist
unsafe fn upload_bars(bars: &[curve::Bar], path_ssbo: u32, width_ssbo: u32) {
    let packed: Vec<[f32; 4]> =
        bars.iter().map(|b| [b.pos[0], b.pos[1], b.normal[0], b.normal[1]]).collect();
    let geom: Vec<[f32; 4]> =
        bars.iter().map(|b| [b.width, b.reach, f32::from(b.mask), 0.0]).collect();
    // SAFETY: the caller guarantees a current context and live buffers; both
    // are only ever rewritten here, each bound to its own binding point
    unsafe {
        let pairs: [(u32, usize, *const ffi::c_void, u32); 2] = [
            (path_ssbo, size_of_val(packed.as_slice()), packed.as_ptr().cast(), 1),
            (width_ssbo, size_of_val(geom.as_slice()), geom.as_ptr().cast(), 3),
        ];
        for (buf, bytes, ptr, binding) in pairs {
            gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, buf);
            gl::BufferData(gl::SHADER_STORAGE_BUFFER, bytes as GLsizeiptr, ptr, gl::STATIC_DRAW);
            gl::BindBufferBase(gl::SHADER_STORAGE_BUFFER, binding, buf);
        }
        gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, 0);
    }
}

/// Slots in the height ring. A slot is rewritten two frames after the GPU last
/// read it: only one frame callback is ever in flight, and a callback arrives
/// once the compositor has presented - so after the GPU finished - that frame
const RING_SLOTS: u32 = 3;

/// The per-bar heights in a persistently mapped, coherent buffer: a frame
/// costs a memcpy of cava's bytes, and no driver call allocates, orphans or
/// copies anything
struct HeightRing {
    /// Mapped for the life of the process; write-only, never read back
    ptr: std::ptr::NonNull<u8>,
    frame: usize,
    next: u32,
}

impl HeightRing {
    /// Give the bound ARRAY_BUFFER immutable storage for every slot and map
    /// it. None if the driver refuses the mapping
    ///
    /// # Safety
    /// A GL 4.4 context must be current, with the target buffer bound to
    /// ARRAY_BUFFER and no storage yet
    unsafe fn map(frame_bytes: GLsizeiptr) -> Option<Self> {
        let flags = gl::MAP_WRITE_BIT | gl::MAP_PERSISTENT_BIT | gl::MAP_COHERENT_BIT;
        let size = frame_bytes * RING_SLOTS as GLsizeiptr;
        // SAFETY: the caller guarantees the context and the binding
        let ptr = unsafe {
            gl::BufferStorage(gl::ARRAY_BUFFER, size, std::ptr::null(), flags);
            gl::MapBufferRange(gl::ARRAY_BUFFER, 0, size, flags)
        };
        let ptr = std::ptr::NonNull::new(ptr.cast::<u8>())?;
        Some(Self { ptr, frame: usize::try_from(frame_bytes).ok()?, next: 0 })
    }

    /// Copy one frame into the next slot; returns the base instance that
    /// reads it
    fn write(&mut self, frame: &[u8]) -> u32 {
        let slot = self.next;
        self.next = (slot + 1) % RING_SLOTS;
        debug_assert_eq!(frame.len(), self.frame);
        // SAFETY: the mapping is persistent and covers RING_SLOTS frames, so
        // slot * frame + frame is in bounds; coherent, so no flush is needed
        unsafe {
            std::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                self.ptr.as_ptr().add(slot as usize * self.frame),
                self.frame,
            );
        }
        // Heights are two bytes per bar, one per instance
        slot * (self.frame / 2) as u32
    }
}

/// Rasterises every occluder into one integer texture, a bit per occluder,
/// which the curve fragment stage reads to discard what is hidden
///
/// Once per configure rather than once per frame: the shapes only move when
/// the output does, so a frame costs one texel fetch per fragment and no draws
struct MaskPass {
    program: u32,
    scale_location: gl::types::GLint,
    offset_location: gl::types::GLint,
    vao: u32,
    vbo: u32,
    fbo: u32,
    /// R16UI, surface-sized, bound to texture unit 0 for the life of the process
    texture: u32,
}

impl MaskPass {
    /// # Safety
    /// A GL context must be current
    unsafe fn new(texture: u32) -> Self {
        let program = link_program(MASK_VERTEX_SHADER_SRC, MASK_FRAGMENT_SHADER_SRC);
        let (mut vao, mut vbo, mut fbo) = (0, 0, 0);
        // SAFETY: the caller guarantees a current context
        unsafe {
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);
            gl::GenFramebuffers(1, &mut fbo);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
            let stride = std::mem::size_of::<curve::MaskVertex>() as GLsizei;
            gl::EnableVertexAttribArray(0);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, stride, std::ptr::null());
            gl::EnableVertexAttribArray(1);
            // I, not the float form: the bit has to arrive as an integer
            gl::VertexAttribIPointer(1, 1, gl::UNSIGNED_INT, stride, std::ptr::without_provenance(8));
            Self {
                program,
                scale_location: gl::GetUniformLocation(program, c"PathScale".as_ptr()),
                offset_location: gl::GetUniformLocation(program, c"PathOffset".as_ptr()),
                vao,
                vbo,
                fbo,
                texture,
            }
        }
    }

    /// Size the mask to the surface and fill it. Leaves the bar program, VAO
    /// and the default framebuffer bound, as every frame expects
    ///
    /// # Safety
    /// A GL context must be current, and `bar_program`/`bar_vao` must be live
    unsafe fn rasterise(
        &self,
        tris: &[curve::MaskVertex],
        (w, h): (u32, u32),
        (scale, offset): ([f32; 2], [f32; 2]),
        (bar_program, bar_vao): (u32, u32),
    ) {
        let (w, h) = (w.max(1) as GLsizei, h.max(1) as GLsizei);
        // SAFETY: the caller guarantees a current context and live names; the
        // texture is attached to this FBO and nothing samples it until after
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.texture);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::R16UI as i32,
                w,
                h,
                0,
                gl::RED_INTEGER,
                gl::UNSIGNED_SHORT,
                std::ptr::null(),
            );
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.fbo);
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                self.texture,
                0,
            );
            if debug_enabled() {
                let status = gl::CheckFramebufferStatus(gl::FRAMEBUFFER);
                if status != gl::FRAMEBUFFER_COMPLETE {
                    say!("occluder mask framebuffer incomplete: {status:#x}");
                }
            }
            gl::Viewport(0, 0, w, h);
            gl::ClearBufferuiv(gl::COLOR, 0, [0u32; 4].as_ptr());
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                std::mem::size_of_val(tris) as GLsizeiptr,
                tris.as_ptr().cast(),
                gl::STATIC_DRAW,
            );
            gl::UseProgram(self.program);
            gl::Uniform2f(self.scale_location, scale[0], scale[1]);
            gl::Uniform2f(self.offset_location, offset[0], offset[1]);
            gl::BindVertexArray(self.vao);
            // XOR per bit is a parity fill; logic op replaces blending here
            gl::Enable(gl::COLOR_LOGIC_OP);
            gl::LogicOp(gl::XOR);
            gl::DrawArrays(gl::TRIANGLES, 0, tris.len() as GLsizei);
            gl::Disable(gl::COLOR_LOGIC_OP);
            if debug_enabled() {
                let err = gl::GetError();
                say!(
                    "occluder mask {w}x{h}, {} triangles, GL error {err:#x}",
                    tris.len() / 3
                );
            }
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::UseProgram(bar_program);
            gl::BindVertexArray(bar_vao);
        }
    }
}

impl AppState {
    /// Present the frame, telling the compositor only what changed
    fn present(&mut self) {
        let mut rects = [0 as egl::Int; DAMAGE_BUCKETS * 4];
        let len = match self.swap_damage {
            // DamageMap buckets a bar by its x column, which only means
            // anything when bars are a row. A circle's bar sweeps an arc whose
            // bounding box depends on its angle, and the surface is already
            // diameter^2 rather than a full-width band - so declare all of it
            // and keep the per-bar arithmetic out of the frame entirely
            Some(_) if !self.force_full_damage && self.mode == Mode::Bars => {
                damage_rects(&self.cava_buffer, &self.prev_frame, &self.damage_map, &mut rects)
            }
            // Whole surface. EGL reads zero rects the same way, so an empty
            // list can never say "nothing changed" - draw() skips that frame
            _ => {
                rects[..4].copy_from_slice(&[0, 0, self.width as i32, self.height as i32]);
                4
            }
        };
        self.force_full_damage = false;
        // Swapped, not copied: the next read fills a whole frame, so the
        // buffer that just became stale is exactly the one to read into.
        // Two pointers instead of a memcpy
        std::mem::swap(&mut self.cava_buffer, &mut self.prev_frame);

        match self.swap_damage {
            // SAFETY: display and surface are current and live, and `rects`
            // holds `len` valid ints, which is `len / 4` complete rectangles
            Some(f) => {
                let ok = unsafe {
                    f(
                        self.egl_display.as_ptr(),
                        self.egl_surface.as_ptr(),
                        rects.as_ptr(),
                        (len / 4) as egl::Int,
                    )
                };
                if ok == egl::FALSE {
                    fatal!("presenting a frame failed; the GL context was probably lost, restart with `cavawall start`");
                }
            }
            None => {
                // A lost context - a GPU reset, a resume - is the realistic cause
                if let Err(e) = egl.swap_buffers(self.egl_display, self.egl_surface) {
                    fatal!("presenting a frame failed ({e}); the GL context was probably lost, restart with `cavawall start`");
                }
            }
        }
    }

    /// Render the newest cava frame. Only ever called from `maybe_draw`
    pub fn draw(&mut self) {
        self.redraw = false;
        self.fresh = false;
        // The same heights draw the same pixels, so there is nothing to
        // present. No commit is the saving; with no callback coming, the next
        // tick reads the next frame
        if !self.force_full_damage && self.cava_buffer == self.prev_frame {
            self.redraw = true;
            return;
        }

        // One float per bar, and that is the whole per-frame vertex payload.
        // NDC: -1.0 bottom, +1.0 top. max_height is NOT applied here - the
        // surface is already sized to that fraction of the screen, so a
        // full-volume bar fills it exactly. Applying it twice made the bars
        // max_height^2 tall, visibly short
        unsafe {
            // Respecifying the store orphans it, so the driver hands back a
            // fresh region and never waits for the GPU to finish reading the
            // old one. Naming the buffer in the call leaves the frame with no
            // binding to make; without direct state access it takes one
            let base = match &mut self.ring {
                Some(ring) => ring.write(&self.cava_buffer),
                None => {
                    // Respecifying the store orphans it, so the driver never
                    // waits for the GPU to finish reading the old one
                    gl::BindBuffer(gl::ARRAY_BUFFER, self.height_vbo);
                    gl::BufferData(
                        gl::ARRAY_BUFFER,
                        self.frame_bytes,
                        self.cava_buffer.as_ptr().cast(),
                        gl::DYNAMIC_DRAW,
                    );
                    0
                }
            };
            gl::Clear(gl::COLOR_BUFFER_BIT);
            // Every bar of every mode in one draw: four vertices per instance,
            // each its own strip, so there is nothing to index. Occluders cut
            // curve bars in the fragment stage against a mask rasterised once
            // per configure, so no path needs a draw of its own. The base
            // instance picks the ring slot; gl_InstanceID still starts at 0
            gl::DrawArraysInstancedBaseInstance(
                gl::TRIANGLE_STRIP,
                0,
                4,
                self.bar_count as GLsizei,
                base,
            );
        }
        // Ask for the next callback BEFORE the swap, never after. The request
        // is double-buffered state and takes effect on the next commit, which
        // the swap provides; requested afterwards it waits for a commit that
        // may never come. frame-then-swap keeps exactly one callback in flight
        self.surface.frame(&self.qh, self.surface.clone());
        self.frame_pending = true;
        self.present();
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
