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

// Set from the SIGTERM/SIGINT handler; the draw loop notices and shuts down
// cleanly instead of being killed mid-frame
static EXITING: AtomicBool = AtomicBool::new(false);

/// Below this, a bar counts as silence. Shared by draw() and poll_resume():
/// they are the two halves of one decision (park / unpark) and drifting apart
/// would mean parking at one threshold and waking at another
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
/// The one place that question is answered: draw() parks on it and
/// poll_resume() unparks on it, in the same units. Deciding on raw bytes keeps
/// f32 unpacking off the silent path
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
    // Only async-signal-safe work here: flip a flag, nothing else
    EXITING.store(true, Ordering::SeqCst);
}
use std::ffi::{CStr, CString};
use std::io::Write;
use std::process::{exit, ChildStdout};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::os::unix::ffi::OsStrExt;
use std::{env, fs, ptr};
use std::{
    io::{BufReader, Read},
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

mod render;
mod startup;
mod wayland;

use render::{damage_rects, surface_map, DamageMap};

use cavawall::{app_config, control, curve};
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
fn build_program(mode: Mode) -> u32 {
    // Two programs, one picked at startup. Mode cannot change without a
    // re-exec, so nothing ever calls UseProgram again and the "GL state is set
    // once" invariant holds for both
    let (vert_src, frag_src) = match mode {
        Mode::Bars => (VERTEX_SHADER_SRC, FRAGMENT_SHADER_SRC),
        Mode::Circle => (CIRCLE_VERTEX_SHADER_SRC, CIRCLE_FRAGMENT_SHADER_SRC),
        // Curve reuses the circle's fragment stage: "gradient along the bar
        // with an alpha ramp from base to tip" is the same job, and both feed
        // it the same vRadial
        Mode::Curve => (CURVE_VERTEX_SHADER_SRC, CURVE_FRAGMENT_SHADER_SRC),
    };
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
            panic!("shader program failed to link:\n{}", program_log(program));
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
            panic!(
                "{what} shader failed to compile:\n{}",
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
    eprintln!("cavawall: {what} palette: {}", stops.join(" "));
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
    cava_reader: BufReader<ChildStdout>,
    wl_egl_surface: WlEglSurface,
    egl_surface: egl::Surface,
    egl_config: egl::Config,
    egl_context: egl::Context,
    egl_display: egl::Display,
    /// The per-instance height buffer draw() streams into. The program, vertex
    /// array and static quad need no handle: bound once at startup, never
    /// rebound. The SSBO is the other exception - the palette is re-uploaded
    height_vbo: u32,
    gradient_scale_location: i32,
    /// Stops in the SSBO, which is 2 even when one colour is configured
    gradient_stops: u32,
    bar_count: u32,
    /// Kept so the palette can be re-uploaded in place
    gradient_colors_ssbo: u32,
    /// The configured stops, already in gradient order
    color_stops: Vec<ConfigColor>,
    /// None when neither the palette nor the bar count follows the shell
    watch: Option<scheme::Watch>,
    /// The cava child, kept so a re-exec can kill and reap it
    cava_pid: u32,
    /// One NDC height per bar, reused. The entire per-frame vertex payload:
    /// everything else about a bar's geometry is a uniform or gl_InstanceID

    /// One raw cava frame, reused. Both were `vec![..]` locals in draw(), so an
    /// idle machine still did three allocations and three frees per frame
    cava_buffer: Box<[u8]>,
    /// Last frame's heights, so damage can be the span each bar actually moved
    /// through rather than the whole band
    /// The frame behind the one on screen, for the damage comparison
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
    max_height: f32,
    /// Startup-only, like the bar count: the two modes are different programs
    /// with different uniforms and different surface geometry
    mode: Mode,
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
    /// The silhouette as authored, in IMAGE coordinates. Sampled into a
    /// horizon only once an output is known, since where it lands depends on
    /// how the wallpaper is cropped onto that output
    curve_occlude: Box<[curve::Control]>,
    /// The sampled silhouette, in output coordinates. Kept so the bounding box
    /// can be cut down to what is visible above it. Empty when there is none
    curve_horizon: Box<[f32]>,
    curve_fit: FitMode,
    /// The wallpaper the running curve was resolved against, and every key the
    /// config has one for. Together they answer the only question a wallpaper
    /// change asks: would this still draw the same thing?
    curve_key: Option<String>,
    curve_keys: HashSet<String>,
    /// Rewritten on every configure, since the crop it is sampled through
    /// depends on the output
    occ_ssbo: u32,
    /// The wallpaper's pixel size, when its format could be read
    curve_image: Option<(u32, u32)>,
    /// Where the curve surface sits on the output, in output pixels:
    /// (left, top, width, height). None means the whole output
    curve_box: Option<(u32, u32, u32, u32)>,
    /// Output size, which stops being `width`/`height` the moment the surface
    /// is smaller than the output it is on
    curve_output: (u32, u32),
    resolution_location: gl::types::GLint,
    /// The context is 4.5 or newer, so a buffer can be named in the call
    dsa: bool,
    /// Kept because the matte tone follows the palette, which a live scheme
    /// can change under us
    matte_color_location: gl::types::GLint,
    path_scale_location: gl::types::GLint,
    path_offset_location: gl::types::GLint,
    occ_map_location: gl::types::GLint,
    silent_frames: u32,
    /// Only read to restore the clear colour if a re-exec fails; it is set once
    /// at startup now rather than per frame
    background_color: [f32; 4],
    /// Explicit output pin: CAVAWALL_OUTPUT, else the config's
    /// preferred_output. None means choose automatically - an external
    /// monitor if one is connected, the built-in panel otherwise
    pinned_output: Option<String>,
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
    /// Kept so the idle tick can request a frame callback - event_loop.run's
    /// callback hands back only &mut AppState, not the QueueHandle
    qh: QueueHandle<AppState>,
    /// Same reason, for clear_and_exit: SIGTERM must be honoured while parked,
    /// and the parked path has no Connection handed to it
    conn: Connection,
}

impl AppState {
    /// Paint one fully transparent frame and commit it before exiting, so the
    /// compositor is left with a clean surface rather than our last set of
    /// bars. Without this a hard kill leaves that frame visible on the
    /// background until something else forces a repaint
    fn clear_and_exit(&mut self) -> ! {
        unsafe {
            gl::ClearColor(0.0, 0.0, 0.0, 0.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
        let _ = egl.swap_buffers(self.egl_display, self.egl_surface);
        self.surface.commit();
        // Round-trip so the commit actually reaches the compositor before the
        // process goes away and its objects are destroyed
        let _ = self.conn.roundtrip();
        control::unbind();
        std::process::exit(0);
    }

    /// Act on the external watches: a new palette, or a new bar count
    ///
    /// Called from BOTH draw() and poll_resume(), because between them they are
    /// the only two states this program has and NEITHER covers the other
    ///
    /// poll_resume alone is not enough. It runs from calloop's timeout
    /// callback, which gets a turn only when the Wayland source runs out of
    /// work, and while audio plays it never does: frame callbacks arrive
    /// faster than draw() consumes cava frames at 45fps. Measured: zero
    /// invocations in six seconds of playback, against ~270 expected
    ///
    /// Registering the inotify fd with calloop as an event source does NOT
    /// help. calloop polls once at the top of dispatch_events and then
    /// dispatches ready sources; WaylandSource::process_events loops on
    /// dispatch_pending until the queue drains, and every draw() in that loop
    /// calls eglSwapBuffers, which reads the socket and refills it. No second
    /// poll happens, so a source is starved exactly as the timeout callback
    /// is. Measured: draw=29, poll_resume=0 over the first second of playback
    ///
    /// Guarded on placement because both paths touch GL - one re-uploads the
    /// SSBO, the other clears the surface before exec'ing - and unplaced means
    /// there is no EGL surface to be current on. A change arriving while no
    /// output is usable waits for the next one; nothing is on screen anyway
    fn poll_external(&mut self) {
        if self.placed_on.is_none() {
            return;
        }
        let changed = self.watch.as_mut().map(scheme::Watch::take).unwrap_or_default();
        // Ordered so the common case (nothing changed) never reaches
        // debug_enabled(). The watch is otherwise unobservable from outside
        if (changed.scheme || changed.shell) && debug_enabled() {
            eprintln!("cavawall: watch fired scheme={} shell={}", changed.scheme, changed.shell);
        }
        // Bars first: a changed count re-execs, which re-reads the scheme on the
        // way up anyway, so resolving colours before that would be thrown away
        if changed.shell {
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
                    eprintln!(
                        "cavawall: wallpaper changed {:?} -> {key:?}, restarting",
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
        let Some(req) = control::read_request(stream) else {
            control::write_response(stream, &Response::err("unparseable request"));
            return;
        };
        match req {
            Request::Status => {
                let data = serde_json::json!({
                    "pid": std::process::id(),
                    "pinned_output": self.pinned_output,
                    "placed_on": self.placed_on,
                    "bars": self.bar_count,
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
            Request::Stop => {
                control::write_response(stream, &Response::ok(None));
                self.clear_and_exit();
            }
            Request::Reload => {
                self.reload_colors();
                control::write_response(stream, &Response::ok(None));
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
    /// through the swap: the launcher's flock and 5s kill-wait, and
    /// fullscreen-watch's instance count. There is never a moment with zero or
    /// two instances, so the stacking race that lock exists for cannot start
    /// here. The environment carries over too, so a CAVAWALL_OUTPUT that
    /// fullscreen-watch set to move us to another monitor survives the restart
    fn reexec(&mut self) {
        // The same transparent frame clear_and_exit paints, for the same reason:
        // on exec our Wayland connection closes exactly as it would on a kill,
        // and the compositor does not reliably repaint under a layer surface
        // that just disappears. Without this the old bars stay burnt onto the
        // wallpaper until something else damages that strip
        unsafe {
            gl::ClearColor(0.0, 0.0, 0.0, 0.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
        let _ = egl.swap_buffers(self.egl_display, self.egl_surface);
        self.surface.commit();
        let _ = self.conn.roundtrip();

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
        eprintln!(
            "cavawall: re-exec failed, keeping {} bars: {}",
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

    /// Called on every event-loop timeout, whether or not the compositor sent
    /// anything. While parked this is the only thing running: it drains whatever
    /// cava has produced and unparks the moment a sample crosses the threshold
    ///
    /// Reads are gated on poll() rather than made non-blocking, so the blocking
    /// read_exact in draw() keeps working unchanged. cava writes a whole 24-byte
    /// frame at a time at the configured framerate, so a readable fd means a
    /// frame is there; a partial read would complete within one frame period
    /// anyway
    pub fn poll_resume(&mut self) {
        // SIGTERM is caught by a handler that only sets EXITING, and the
        // checks that act on it live in draw(), which does not run while
        // parked. Without this a parked instance ignores SIGTERM entirely
        if EXITING.load(Ordering::SeqCst) {
            self.clear_and_exit();
        }
        // Parked AND unplaced: the output went away while the audio was
        // silent. Committing here would map a surface belonging to nothing.
        // retarget() restarts the loop when an output comes back
        if self.placed_on.is_none() {
            return;
        }

        self.poll_external();

        if !self.idle {
            // Running: the compositor's frame callbacks drive draw(), and this
            // tick has nothing to do. NOT a second drive for
            // draw() - rendering ahead of the callback is what the callback
            // exists to throttle, and cava's pipe is drained by draw() anyway
            return;
        }
        let fd = self.cava_reader.get_ref().as_raw_fd();
        loop {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 0) } <= 0 || pfd.revents & libc::POLLIN == 0 {
                return; // nothing waiting; stay parked
            }
            if self.cava_reader.read_exact(&mut self.cava_buffer).is_err() {
                return;
            }
            if !is_silent(&self.cava_buffer) {
                // Unpark: one commit restarts the frame-callback loop, and
                // rendering is driven by the compositor again from here
                self.idle = false;
                self.silent_frames = 0;
                // Parked frames were never presented, so prev_heights describes
                // a frame older than whatever is on screen
                self.force_full_damage = true;
                self.surface.frame(&self.qh, self.surface.clone());
                self.surface.commit();
                return;
            }
        }
    }

    /// Rank a connected output; lower wins, None means "not eligible at all".
    ///
    /// A pin excludes everything else outright rather than merely preferring
    /// the pinned output - when fullscreen-watch says "eDP-1", falling back
    /// to the monitor it just ruled out would defeat the point
    fn output_rank(&self, name: &str) -> Option<u8> {
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

        let Some((output, info, name)) = self.choose_output() else {
            if self.placed_on.take().is_some() || self.placed_size.take().is_some() {
                eprintln!("cavawall: no usable output, idling until one appears");
            }
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
        let (x0, y0, x1, y1) = curve::bounds(&self.curve_bars);
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

    /// The lowest the silhouette drops across `x0..x1`, as a height above the
    /// bottom of the output, or None when there is no silhouette
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
            eprintln!("cavawall: placing on {name} ({}x{})", logical_size.0, logical_size.1);
        }
        let old_surface = self.surface.clone();
        self.surface = self.compositor.create_surface(qh);
        self.layer_surface = self.layer_shell.create_layer_surface(
            qh,
            self.surface.clone(),
            Layer::Bottom,
            Some("cavawall"),
            Some(output),
        );
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
        match self.mode {
            Mode::Bars => {
                let band =
                    ((self.height as f32 * self.max_height).ceil() as u32).clamp(1, self.height);
                self.layer_surface.set_size(self.width, band);
                self.layer_surface.set_anchor(Anchor::BOTTOM);
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
                self.curve_bars =
                    curve::build(&self.curve_paths, self.bar_count, aspect, fit).into();
                self.curve_horizon =
                    curve::horizon(&self.curve_occlude, fit).into_boxed_slice();
                self.curve_box = self.curve_bbox();
                match self.curve_box {
                    Some((left, top, w, h)) => {
                        self.layer_surface.set_size(w, h);
                        self.layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
                        self.layer_surface.set_margin(top as i32, 0, 0, left as i32);
                    }
                    None => {
                        self.layer_surface.set_size(self.width, self.height);
                        self.layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
                    }
                }
            }
            Mode::Circle => {
                let d = self.circle.diameter.min(self.width).min(self.height).max(1);
                let (top, left) = self.circle.margins_for(d, self.width, self.height);
                self.layer_surface.set_size(d, d);
                self.layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
                self.layer_surface.set_margin(top, 0, 0, left);
            }
        }
        self.surface.commit();
        drop(input_region);
        old_surface.destroy();
        // A fresh surface carries no frame callback and nothing parked. The
        // configure this commit provokes is what restarts the loop, and it
        // only draws because placed_on is set here first
        self.idle = false;
        self.silent_frames = 0;
        self.placed_on = Some(name);
        self.placed_size = info.logical_size;
    }

}

/// Upload every bar: one vec4 each for base and unit normal, one vec2 each
/// for width and reach
///
/// Rewritten whole, since it changes only when the
/// output's shape does, which is a monitor change, not a frame
///
/// # Safety
/// A GL context must be current and both buffers must already exist
unsafe fn upload_bars(bars: &[curve::Bar], path_ssbo: u32, width_ssbo: u32) {
    let packed: Vec<[f32; 4]> =
        bars.iter().map(|b| [b.pos[0], b.pos[1], b.normal[0], b.normal[1]]).collect();
    let widths: Vec<[f32; 2]> = bars.iter().map(|b| [b.width, b.reach]).collect();
    // SAFETY: the caller guarantees a current context and live buffers; both
    // are only ever rewritten here, each bound to its own binding point
    unsafe {
        let pairs: [(u32, usize, *const ffi::c_void, u32); 2] = [
            (path_ssbo, size_of_val(packed.as_slice()), packed.as_ptr().cast(), 1),
            (width_ssbo, size_of_val(widths.as_slice()), widths.as_ptr().cast(), 3),
        ];
        for (buf, bytes, ptr, binding) in pairs {
            gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, buf);
            gl::BufferData(gl::SHADER_STORAGE_BUFFER, bytes as GLsizeiptr, ptr, gl::STATIC_DRAW);
            gl::BindBufferBase(gl::SHADER_STORAGE_BUFFER, binding, buf);
        }
        gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, 0);
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
            // Whole surface. Not the same as passing zero rects, which means
            // "nothing changed" and would present a frame nobody redraws
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
            Some(f) => unsafe {
                f(
                    self.egl_display.as_ptr(),
                    self.egl_surface.as_ptr(),
                    rects.as_ptr(),
                    (len / 4) as egl::Int,
                );
            },
            None => {
                egl.swap_buffers(self.egl_display, self.egl_surface).unwrap();
            }
        }
    }

    pub fn draw(&mut self, _conn: &Connection, qh: &QueueHandle<Self>) {
        // Before any GL work, and before the blocking read below: while audio is
        // playing this is the ONLY path that runs, so it is the only chance a
        // scheme or bar-count change gets to be noticed. Costs one non-blocking
        // read per watch per frame, returning EAGAIN in the overwhelming
        // majority of them
        self.poll_external();
        if let Err(e) = self.cava_reader.read_exact(&mut self.cava_buffer) {
            // A signal interrupts the blocking read, which is exactly how we
            // find out it is time to go
            if EXITING.load(Ordering::SeqCst) {
                self.clear_and_exit();
            }
            if e.kind() == std::io::ErrorKind::Interrupted {
                return;
            }
            // cava is gone and the pipe read back EOF. Leave the way SIGTERM
            // does: `panic = "abort"` skips all cleanup, so panicking here
            // leaves the last frame of bars burnt onto the wallpaper
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                eprintln!("cavawall: cava exited, shutting down");
                self.clear_and_exit();
            }
            panic!("cava read failed: {e}");
        }
        if EXITING.load(Ordering::SeqCst) {
            self.clear_and_exit();
        }

        // Drop stale frames and render the newest
        //
        // cava writes at the configured framerate regardless of whether we are
        // keeping up. Reading exactly one frame per draw means a stall leaves a
        // backlog in the pipe, and on recovery every queued frame is rendered in
        // turn - the visualiser freezes, then fast-forwards through the audio
        // it missed. Skipping to the newest frame keeps it in step with what is
        // actually playing
        //
        // The BufReader's own buffer has to be checked as well as the fd: bytes
        // already pulled out of the pipe are invisible to poll(), so polling
        // alone would report "nothing waiting" while a backlog sat in memory
        let frame_len = self.cava_buffer.len();
        let fd = self.cava_reader.get_ref().as_raw_fd();
        let mut skipped = 0u32;
        while skipped < 512 {
            let ready = if self.cava_reader.buffer().len() >= frame_len {
                true
            } else {
                let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                (unsafe { libc::poll(&mut pfd, 1, 0) }) > 0 && pfd.revents & libc::POLLIN != 0
            };
            if !ready {
                break;
            }
            if self.cava_reader.read_exact(&mut self.cava_buffer).is_err() {
                break;
            }
            skipped += 1;
        }

        // Skip GPU work while the audio is silent. cava emits frames at the
        // configured framerate whether or not anything is playing, so without
        // this the full-screen surface is recomposited 60x/sec forever just to
        // draw bars that are all zero - measurably pinning an integrated GPU
        //
        // Commit with no new buffer instead of drawing: that still schedules
        // Grace before parking, so the bars finish falling to zero rather than
        // freezing part-way down
        //
        // Measured, not guessed: with monstercat=1.5 and noise_reduction=60, a
        // tone cut from full volume decays below the threshold in 8 frames -
        // 0.18s. The original 90 (2.0s) was 11x that. 23 frames is 0.51s, still
        // ~3x the real decay
        //
        // The remainder is hysteresis rather than decay: a quiet passage or a
        // gap between tracks would otherwise park and unpark repeatedly. That
        // costs almost nothing - parking sets a flag, unparking is one commit
        // - and is invisible, since the bars are already at zero whenever it
        // happens
        if is_silent(&self.cava_buffer) {
            self.silent_frames = self.silent_frames.saturating_add(1);
        } else {
            self.silent_frames = 0;
        }
        if self.silent_frames > SILENT_GRACE_FRAMES {
            if debug_enabled() {
                eprintln!("cavawall: parking (silent_frames={})", self.silent_frames);
            }
            // PARK. No draw, and critically no commit either
            //
            // A bufferless commit is free for us but not for the compositor:
            // Hyprland damages a layer by its GEOMETRY on any commit, buffer
            // attached or not, so every one recomposited the whole band. The
            // original comment here claimed it "produces no damage" - false,
            // and visible in the damage overlay as a flash on an idle workspace
            // with no audio playing
            //
            // Committing was only ever there to keep frame callbacks coming, so
            // that audio returning would be noticed. That job moves to
            // poll_resume(), driven by the timeout event_loop.run already has,
            // which owes nothing to the compositor. So while silent this draws
            // nothing, commits nothing, and damages nothing
            self.idle = true;
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
            if self.dsa {
                gl::NamedBufferData(
                    self.height_vbo,
                    self.frame_bytes,
                    self.cava_buffer.as_ptr().cast(),
                    gl::DYNAMIC_DRAW,
                );
            } else {
                gl::BindBuffer(gl::ARRAY_BUFFER, self.height_vbo);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    self.frame_bytes,
                    self.cava_buffer.as_ptr().cast(),
                    gl::DYNAMIC_DRAW,
                );
            }
            gl::Clear(gl::COLOR_BUFFER_BIT);
            // Four vertices, once per bar. No index buffer: each instance is
            // its own strip, so there are no shared vertices to index
            gl::DrawArraysInstanced(gl::TRIANGLE_STRIP, 0, 4, self.bar_count as GLsizei);
        }
        // Ask for the next callback BEFORE the swap, never after
        //
        // "The frame request will take effect on the next wl_surface.commit"
        // (wayland.xml) - it is double-buffered state like a buffer or a
        // damage region, so it needs a commit AFTER it to be applied.
        // eglSwapBuffers is that commit: Mesa attaches the new buffer, adds
        // damage, and commits, all inside the call
        //
        // Requesting it afterwards instead left the request sitting in pending
        // state with nothing left to apply it, so the loop ran on the callback
        // committed by the PREVIOUS draw - self-sustaining only once two
        // draws had happened, and dead the moment one draw did not swap (the
        // silence park returns before this point) or the surface holding the
        // in-flight callback was destroyed (new_output does exactly that).
        // Replacing the request with a bare commit() after the swap is worse
        // still: no request is created at all, so the very first draw is the
        // last one, and the trailing bufferless commit re-damages the layer by
        // its geometry for nothing (see the park note above).
        //
        // frame-then-swap is what weston-simple-egl does, and it keeps exactly
        // one callback in flight with no dependence on a second commit
        self.surface.frame(qh, self.surface.clone());
        self.present();
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
