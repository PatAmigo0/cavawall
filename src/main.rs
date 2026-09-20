extern crate khronos_egl as egl;

use gl::types::{GLsizei, GLsizeiptr};
use smithay_client_toolkit::reexports::calloop::EventLoop;
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
    globals::registry_queue_init,
    protocol::{wl_output, wl_surface},
    Connection, QueueHandle,
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
    time::Duration,
};

use cavawall::{app_config, curve};
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

/// Config path when no `--config` was given. The inherited path is now built
/// only once the preferred one is ruled out, not unconditionally
fn default_config_path() -> PathBuf {
    let home = PathBuf::from(env::var_os("HOME").expect("Unable to get home directory"));
    let own = home.join(".config/cavawall/config.toml");
    if own.exists() {
        return own;
    }
    // The inherited path is still honoured, so a config left where it was
    // keeps working
    let inherited = home.join(".config/wallpaper-cava/config.toml");
    if inherited.exists() {
        eprintln!(
            "cavawall: using {}\n\
             cavawall: move it to ~/.config/cavawall/config.toml when convenient",
            inherited.display()
        );
        return inherited;
    }
    PathBuf::from("config.toml")
}

fn main() {
    let mut args = env::args_os().skip(1);
    let config_filename = match (args.next(), args.next(), args.next()) {
        (None, _, _) => default_config_path(),
        (Some(flag), Some(path), None) if flag == "--config" => PathBuf::from(path),
        _ => {
            print_help();
            exit(0);
        }
    };
    // Shut down cleanly on SIGTERM so the surface can be cleared first. A hard
    // kill leaves the last frame burnt into the background: the layer surface
    // goes away, but Hyprland does not reliably repaint underneath it, so a
    // frozen strip of bars stays on the wallpaper until something else forces a
    // redraw. Anything that stops this process (a session manager, a
    // fullscreen watcher) should therefore use SIGTERM, not SIGKILL
    unsafe {
        libc::signal(libc::SIGTERM, on_terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_terminate as *const () as libc::sighandler_t);
    }

    let config_str = fs::read_to_string(&config_filename)
        .unwrap_or_else(|e| panic!("unable to read {}: {e}", config_filename.display()));
    let config: Config = match toml::from_str(&config_str) {
        Ok(config) => config,
        Err(error) => panic!("Error parsing config: {}", error.message()),
    };
    // Effective bar count. Startup-only by construction: it is written into
    // the spawned cava's config below and baked into the index buffer further
    // down, so there is no honest way to follow it live. See SchemeConfig::bars
    let follow_bars = config.scheme.as_ref().and_then(|s| s.bars).unwrap_or(false);
    // Resolved before bar_count because each mode may override it: a ridge
    // wants a different density from a bottom row, and `[bars] amount` is
    // shared by all three
    let configured_mode = config.general.mode.unwrap_or_default();
    // Resolved here, not at the curve setup below, because the bar count
    // comes out of it. It has to be the curve for the CURRENT wallpaper:
    // HashMap order is undefined, so any other choice is arbitrary
    // One read of the wallpaper answers both questions it is asked: which
    // curve this is, and how the image crops onto the output
    let wallpaper = (configured_mode == Mode::Curve)
        .then(curve::current_wallpaper)
        .flatten()
        .and_then(|w| curve::describe(&w));
    let curve_key = wallpaper.as_ref().map(|(key, _)| key.clone());
    let curve_keys: HashSet<String> =
        config.curves.iter().flat_map(|m| m.keys().cloned()).collect();
    let active_curve = (configured_mode == Mode::Curve)
        .then(|| {
            let key = curve_key.clone()?;
            let found = config.curves.as_ref()?.get(&key);
            if found.is_none() && debug_enabled() {
                eprintln!("cavawall: no curve for wallpaper {key}, falling back to bars");
            }
            found
        })
        .flatten();
    let bar_count = match configured_mode {
        Mode::Circle => config.circle.as_ref().and_then(|c| c.bars),
        Mode::Curve => active_curve.and_then(CurveConfig::total_bars),
        Mode::Bars => None,
    }
    .unwrap_or(if follow_bars {
        scheme::bar_count().unwrap_or(config.bars.amount)
    } else {
        config.bars.amount
    });
    // Zero divides by zero in the bar-width maths. The ceiling is a sanity
    // bound: 4096 bars is already sub-pixel on any real monitor
    const MAX_BARS: u32 = 4096;
    assert!(
        (1..=MAX_BARS).contains(&bar_count),
        "bar count must be between 1 and {MAX_BARS}, got {bar_count}"
    );
    let mut cava_output_config: HashMap<String, String> = HashMap::from([
        ("method".into(), "raw".into()),
        ("raw_target".into(), "/dev/stdout".into()),
        ("bit_format".into(), "16bit".into()),
    ]);
    // Only forwarded when set, so leaving it out keeps cava's own default
    // rather than this program quietly picking one
    if let Some(ch) = &config.general.channels {
        cava_output_config.insert("channels".into(), ch.clone());
    }
    if let Some(mo) = &config.general.mono_option {
        cava_output_config.insert("mono_option".into(), mo.clone());
    }
    let cava_input_config = config.general.audio_source.as_ref().map(|src| {
        HashMap::from([
            ("method".into(), "pulse".into()),
            ("source".into(), src.clone()),
        ])
    });
    let cava_config = CavaConfig {
        general: CavaGeneralConfig {
            framerate: config.general.framerate,
            bars: bar_count,
            autosens: config.general.autosens,
            sensitivity: config.general.sensitivity,
        },
        smoothing: CavaSmoothingConfig {
            monstercat: config.smoothing.monstercat,
            waves: config.smoothing.waves,
            noise_reduction: config.smoothing.noise_reduction,
        },
        output: cava_output_config,
        input: cava_input_config,
    };
    let string_cava_config: String = toml::to_string(&cava_config).unwrap();
    // CAVAWALL_DEBUG=1 shows what cava is being told. cava is spawned
    // with its config on stdin, so there is no file to inspect afterwards and
    // no other way to check a setting actually got through
    if debug_enabled() {
        eprintln!("cavawall: cava config >>>\n{string_cava_config}<<<");
    }
    let mut cmd = Command::new("cava");
    cmd.arg("-p").arg("/dev/stdin");
    // The `Child` is taken apart rather than kept: exec keeps our PID and so
    // keeps this child, so what must survive is the raw pid. reexec() kills and
    // waits before replacing our image, and a cava that dies on its own takes
    // draw() out through clear_and_exit(), after which init collects it
    #[allow(clippy::zombie_processes)]
    let cava_process = cmd
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn cava process");
    // Captured before the field moves below leave `cava_process` partially moved
    // and unusable as a whole. Needed so a re-exec can reap this child: see
    // reexec(), where not having it leaked a zombie per bar-count change
    let cava_pid = cava_process.id();
    let mut cava_stdin = cava_process.stdin.unwrap();
    cava_stdin.write_all(string_cava_config.as_bytes()).unwrap();
    drop(cava_stdin);
    let cava_stdout = cava_process.stdout.unwrap();
    let cava_reader = BufReader::new(cava_stdout);
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    // WaylandSource is inserted further down, AFTER the output list has been
    // settled with an explicit roundtrip - see the note there
    let frame_duration = Duration::from_secs(1) / config.general.framerate;
    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let surface = compositor.create_surface(&qh);
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell not available");
    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface.clone(),
        Layer::Bottom,
        Some("cavawall"),
        None,
    );
    // Empty input region: a wallpaper must never accept pointer input
    //
    // The default region is the whole surface, which takes pointer focus
    // across the screen. This surface never calls set_cursor, and a Wayland
    // cursor keeps whatever shape the focused surface last asked for, so the
    // I-beam from a terminal survives onto an empty workspace
    //
    // Parking does not help: a parked surface stays mapped and keeps its
    // region. set_input_region has copy semantics, so the wl_region may drop
    // right after the commit
    let input_region = Region::new(&compositor).ok();
    if let Some(r) = &input_region {
        layer_surface.set_input_region(Some(r.wl_region()));
    }
    // -1, not the default 0. Zero means "reserve nothing, but stay inside the
    // area other layers have reserved", so any bar with an exclusive zone
    // shifts and shrinks the wallpaper: on a 1920-wide output with a bar, this
    // surface was placed at x=25 and ran 25px off the right edge. -1 means
    // "ignore exclusive zones", which is what a wallpaper wants - it belongs
    // to the output, not to whatever is left over
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_size(256, 256);
    layer_surface.set_anchor(Anchor::TOP);
    surface.commit();
    drop(input_region);
    egl.bind_api(egl::OPENGL_API).unwrap();
    let egl_display = unsafe {
        egl.get_display(conn.display().id().as_ptr() as *mut std::ffi::c_void)
            .unwrap()
    };
    egl.initialize(egl_display).unwrap();
    const ATTRIBUTES: [i32; 9] = [
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::NONE,
    ];

    let egl_config = egl
        .choose_first_config(egl_display, &ATTRIBUTES)
        .unwrap()
        .unwrap();
    // 4.3 is the floor: the shaders are `#version 430 core` and index SSBOs.
    // 4.5 adds direct state access, which draw() uses when it is there, so the
    // higher version is asked for first and 4.3 answers everything else
    const fn context_attributes(minor: i32) -> [i32; 7] {
        [
            egl::CONTEXT_MAJOR_VERSION,
            4,
            egl::CONTEXT_MINOR_VERSION,
            minor,
            egl::CONTEXT_OPENGL_PROFILE_MASK,
            egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ]
    }
    let egl_context = [6, 5, 4, 3]
        .into_iter()
        .find_map(|minor| {
            egl.create_context(egl_display, egl_config, None, &context_attributes(minor)).ok()
        })
        .expect("no OpenGL 4.3 core context: cavawall needs SSBOs");

    let wl_egl_surface = WlEglSurface::new(surface.id(), 256, 256).unwrap();
    let egl_surface = unsafe {
        egl.create_window_surface(
            egl_display,
            egl_config,
            wl_egl_surface.ptr() as egl::NativeWindowType,
            None,
        )
        .unwrap()
    };
    egl.make_current(
        egl_display,
        Some(egl_surface),
        Some(egl_surface),
        Some(egl_context),
    )
    .unwrap();
    // The compositor paces this surface with frame callbacks, which is the
    // only pacing a layer surface should have. The default interval of 1 adds
    // the driver's own wait for vsync on top of that, inside every swap.
    // Applies to the surface bound to the current context, so it goes after
    // make_current
    if egl.swap_interval(egl_display, 0).is_err() && debug_enabled() {
        eprintln!("cavawall: swap interval unchanged, frames pace on vsync too");
    }
    gl::load_with(|name| egl.get_proc_address(name).unwrap() as *const std::ffi::c_void);
    // CStr, not CString::from_raw: glGetString returns a pointer into the
    // driver's static string table and from_raw claims ownership, which would
    // free a block Rust never allocated. A null returns None
    let version = unsafe {
        let data = gl::GetString(gl::VERSION);
        if data.is_null() {
            std::borrow::Cow::Borrowed("unknown")
        } else {
            CStr::from_ptr(data.cast()).to_string_lossy()
        }
    };

    println!("OpenGL version: {version}");
    println!("EGL version: {}", egl.version());
    // Resolved once, here, and carried into AppState. draw() then only matches
    // on the stored Option, so a frame costs a null check - no env lookup and
    // no eglGetProcAddress. Reporting the same value that gets used, rather
    // than resolving a second time, keeps one source of truth for it
    let swap_damage = load_swap_with_damage(egl_display);
    if debug_enabled() {
        eprintln!(
            "cavawall: swap-with-damage {}",
            if swap_damage.is_some() {
                "available"
            } else {
                "MISSING - every frame will declare the whole surface"
            }
        );
    }
    let mut mode = configured_mode;
    let mut curve_paths: Vec<curve::PathSpec> = Vec::new();
    let mut path_ssbo: u32 = 0;
    let mut width_ssbo: u32 = 0;
    let mut occ_ssbo: u32 = 0;
    let mut curve_occlude: Vec<curve::Control> = Vec::new();
    let mut curve_fit = FitMode::default();
    let mut curve_image: Option<(u32, u32)> = None;
    // A curve is authored against ONE wallpaper. If the current one has no
    // entry, fall back to bars rather than draw a ridge traced from a
    // different image - which is the whole point of keying them
    if mode == Mode::Curve && active_curve.is_none() {
        mode = Mode::Bars;
    }
    let circle = CircleGeom::from_config(config.circle.as_ref());
    let shader_program = build_program(mode);
    let mut quad_vbo = 0;
    let mut height_vbo = 0;
    let mut vao = 0;
    let mut gradient_colors_ssbo = 0;
    // Ordered once and kept. A live re-resolve reuses this exact Vec rather
    // than walking the HashMap again, which would be free to hand back a
    // different order and silently reshuffle the gradient mid-session
    let color_stops = ordered_stops(&config.colors);
    let follow_colors = config.scheme.as_ref().and_then(|s| s.colors).unwrap_or(false);
    let initial_rgba = resolve_stops(
        &color_stops,
        if follow_colors { scheme::colours() } else { None }.as_ref(),
    );
    debug_palette(
        if follow_colors { "initial (live)" } else { "initial (static)" },
        &initial_rgba,
    );
    assert!(
        !initial_rgba.is_empty(),
        "[colors] needs at least one stop to build a gradient from"
    );
    let buffer_data = gradient_buffer(&initial_rgba);
    // As the GPU sees it, which is not the configured count when there is only
    // one stop. GradientScale below has to agree with the shader's own
    // gradient_colors_size, so both come from here
    let gradient_stops = uploaded_stops(initial_rgba.len()) as u32;
    // One watch over all three files, so a frame costs one read. They are
    // acted on differently: a palette is re-uploaded in place, a bar count or
    // a wallpaper re-execs
    //
    // The wallpaper is watched whenever curve mode was ASKED for, not only
    // when a curve is drawing: an instance that fell back to bars still has to
    // notice the wallpaper it has a curve for coming back
    let watch = scheme::Watch::new(follow_colors, follow_bars, configured_mode == Mode::Curve);

    // Sized from the bar count, which cannot change without a re-exec, so both
    // are allocated once here rather than on every frame
    // cava's frame IS the vertex buffer: two bytes a bar, straight from the
    // pipe to the GPU with nothing in between
    let cava_buffer = vec![0u8; bar_count as usize * 2].into_boxed_slice();
    let prev_frame = cava_buffer.clone();
    let frame_bytes = std::mem::size_of_val(&*cava_buffer) as GLsizeiptr;
    let (bar_width, bar_stride) = bar_geometry(bar_count, config.bars.gap);
    let background_color = array_from_config_color(&config.general.background_color);

    let gradient_scale_name = CString::new("GradientScale").unwrap();
    unsafe {
        gl::GenVertexArrays(1, &mut vao);
        gl::BindVertexArray(vao);
        gl::GenBuffers(1, &mut quad_vbo);
        gl::GenBuffers(1, &mut height_vbo);
        gl::GenBuffers(1, &mut gradient_colors_ssbo);
        gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, gradient_colors_ssbo);
        gl::BufferData(
            gl::SHADER_STORAGE_BUFFER,
            buffer_data.len() as GLsizeiptr,
            buffer_data.as_ptr() as *const ffi::c_void,
            gl::STATIC_DRAW,
        );
        gl::BindBufferBase(gl::SHADER_STORAGE_BUFFER, 0, gradient_colors_ssbo);
        gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, 0);
        // Attribute 0: the quad itself, uploaded once and never touched again
        gl::BindBuffer(gl::ARRAY_BUFFER, quad_vbo);
        gl::BufferData(
            gl::ARRAY_BUFFER,
            std::mem::size_of_val(&UNIT_QUAD) as GLsizeiptr,
            UNIT_QUAD.as_ptr().cast(),
            gl::STATIC_DRAW,
        );
        gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 8, std::ptr::null());
        gl::EnableVertexAttribArray(0);

        // Attribute 1: one height per bar. The divisor is what makes it
        // per-instance rather than per-vertex, and is the whole trick
        gl::BindBuffer(gl::ARRAY_BUFFER, height_vbo);
        gl::BufferData(
            gl::ARRAY_BUFFER,
            frame_bytes,
            std::ptr::null(),
            gl::DYNAMIC_DRAW,
        );
        gl::VertexAttribPointer(1, 1, gl::UNSIGNED_SHORT, gl::TRUE, 2, std::ptr::null());
        gl::EnableVertexAttribArray(1);
        gl::VertexAttribDivisor(1, 1);

        // Render state that never changes, set once instead of per frame.
        // Only clear_and_exit and reexec undo any of it, both on the way out
        //
        // Rests on nothing else binding a vertex array or a program;
        // reload_colors touches only SHADER_STORAGE_BUFFER and resets it.
        // ARRAY_BUFFER is left to draw(), where it is load-bearing
        gl::UseProgram(shader_program);
        // Bar geometry is a pair of constants now, not a buffer full of
        // coordinates. Set once; neither can change without a re-exec
        match mode {
            Mode::Bars => {
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"BarWidth".as_ptr()),
                    bar_width,
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"Stride".as_ptr()),
                    bar_stride,
                );
            }
            Mode::Curve => {
                // Safe: mode was demoted to Bars above when no curve matched.
                // Aspect from the first output's size; place_on re-derives the
                // surface but the shape of the screen does not change under us
                let cfg = active_curve.expect("curve mode implies a matching curve");
                // [x, y] or [x, y, scale]; a short or empty entry is a config
                // typo, and skipping it beats rendering a bar at the origin.
                // Every path resolved up front, each with its own geometry:
                // the shader never learns that paths exist
                curve_paths = cfg
                    .paths()
                    .iter()
                    .map(|path| curve::PathSpec {
                        controls: path
                            .points
                            .iter()
                            .filter(|p| p.len() >= 2)
                            .map(|p| curve::Control {
                                x: p[0],
                                y: p[1],
                                scale: p.get(2).copied().unwrap_or(1.0).max(0.0),
                                angle: p.get(3).copied(),
                            })
                            .collect(),
                        // NDC spans 2.0, so a fraction of the output is twice
                        // that. Per path, because two ridges at different
                        // distances want different reaches
                        bars: path.bars,
                        reach: path.height.or(cfg.height).unwrap_or(0.18).clamp(0.0, 1.0) * 2.0,
                        width: path.width.or(cfg.width).unwrap_or(0.006).clamp(0.0, 1.0) * 2.0,
                        flip: path.flip.or(cfg.flip).unwrap_or(false),
                        upright: path.upright.or(cfg.upright).unwrap_or(false),
                    })
                    .collect();
                // Created empty and bound. Bars cannot be built until a surface
                // exists - their normals and their crop both depend on the
                // output's shape - and configure() fills these before any draw
                gl::GenBuffers(1, &mut path_ssbo);
                gl::GenBuffers(1, &mut width_ssbo);
                upload_bars(&[], path_ssbo, width_ssbo);
                curve_fit = cfg.fit.unwrap_or_default();
                curve_image = wallpaper.as_ref().and_then(|(_, size)| *size);
                if curve_image.is_none() && debug_enabled() {
                    eprintln!("cavawall: wallpaper size unreadable, treating it as output-shaped");
                }
                curve_occlude = cfg.occlude.as_deref().unwrap_or(&[])
                    .iter()
                    .filter(|p| p.len() >= 2)
                    .map(|p| curve::Control { x: p[0], y: p[1], scale: 1.0, angle: None })
                    .collect();
                // Always created and bound, even when empty: an unbound SSBO
                // read is undefined, and the shader guards on the length
                gl::GenBuffers(1, &mut occ_ssbo);
                gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, occ_ssbo);
                // std430 aligns a float array to 4 bytes, not 16, so the
                // horizon starts immediately after the length - no padding.
                // Length zero until an output exists: the shader guards on it,
                // so an unplaced instance draws no occlusion rather than
                // reading an empty buffer
                gl::BufferData(
                    gl::SHADER_STORAGE_BUFFER,
                    4,
                    [0i32].as_ptr().cast(),
                    gl::STATIC_DRAW,
                );
                gl::BindBufferBase(gl::SHADER_STORAGE_BUFFER, 2, occ_ssbo);
                gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, 0);
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"InnerAlpha".as_ptr()),
                    circle.inner_alpha,
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"OuterAlpha".as_ptr()),
                    circle.outer_alpha,
                );
            }
            Mode::Circle => {
                // A slot is one bar plus one gap, and the circle closes, so
                // there are as many gaps as bars - not bars - 1 as on a line
                let step = std::f32::consts::TAU / bar_count as f32;
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"AngleStep".as_ptr()),
                    step,
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"AngularHalf".as_ptr()),
                    step / (1.0 + config.bars.gap) * 0.5,
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"InnerRadius".as_ptr()),
                    circle.inner_radius,
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"RadialSpan".as_ptr()),
                    1.0 - circle.inner_radius,
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"InnerAlpha".as_ptr()),
                    circle.inner_alpha,
                );
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"OuterAlpha".as_ptr()),
                    circle.outer_alpha,
                );
            }
        }
        gl::Enable(gl::BLEND);
        gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
        gl::ClearColor(
            background_color[0],
            background_color[1],
            background_color[2],
            background_color[3],
        );
    }

    // The finish is the same in all three modes, so these three are looked up
    // and set the same way whichever program is bound
    let matte_color_location =
        unsafe { gl::GetUniformLocation(shader_program, c"MatteColor".as_ptr()) };
    let matte_location = unsafe { gl::GetUniformLocation(shader_program, c"Matte".as_ptr()) };
    unsafe {
        let mean = palette_mean(&initial_rgba);
        gl::Uniform3f(matte_color_location, mean[0], mean[1], mean[2]);
        gl::Uniform1f(matte_location, config.bars.matte.unwrap_or(0.0).clamp(0.0, 1.0));
        gl::Uniform1f(
            gl::GetUniformLocation(shader_program, c"Opacity".as_ptr()),
            config.bars.opacity.unwrap_or(1.0).clamp(0.0, 1.0),
        );
    }
    // Direct state access is core from 4.5. The driver decides that, not the
    // build, so it is read back from the context that was granted
    let dsa = unsafe {
        let (mut major, mut minor) = (0, 0);
        gl::GetIntegerv(gl::MAJOR_VERSION, &raw mut major);
        gl::GetIntegerv(gl::MINOR_VERSION, &raw mut minor);
        if debug_enabled() {
            eprintln!("cavawall: GL {major}.{minor}, direct state access {}",
                if major > 4 || (major == 4 && minor >= 5) { "on" } else { "off" });
        }
        major > 4 || (major == 4 && minor >= 5)
    };
    let resolution_location =
        unsafe { gl::GetUniformLocation(shader_program, c"Resolution".as_ptr()) };
    let path_scale_location =
        unsafe { gl::GetUniformLocation(shader_program, c"PathScale".as_ptr()) };
    let path_offset_location =
        unsafe { gl::GetUniformLocation(shader_program, c"PathOffset".as_ptr()) };
    let occ_map_location = unsafe { gl::GetUniformLocation(shader_program, c"OccMap".as_ptr()) };
    let gradient_scale_location =
        unsafe { gl::GetUniformLocation(shader_program, gradient_scale_name.as_ptr()) };

    // CAVAWALL_OUTPUT wins over the config file, and is how an external
    // watcher moves the visualiser between monitors: it relaunches with this
    // set, so argv stays exactly [binary]. The launcher, the shell toggle and
    // that watcher all identify this process by an EXACT argv match, so a flag
    // here breaks all three at once
    //
    // Unset, with no preferred_output, means choose automatically
    let pinned_output = env::var("CAVAWALL_OUTPUT")
        .ok()
        .filter(|s| !s.is_empty())
        .or(config.general.preferred_output);

    let mut simple_window = AppState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        width: 256,
        height: 256,
        layer_shell,
        layer_surface,
        surface,
        cava_reader,
        wl_egl_surface,
        egl_surface,
        egl_config,
        egl_context,
        egl_display,
        height_vbo,
        gradient_scale_location,
        gradient_stops,
        bar_count,
        cava_pid,
        gradient_colors_ssbo,
        color_stops,
        watch,
        prev_frame,
        cava_buffer,
        bar_width,
        bar_stride,
        damage_map: DamageMap::new(bar_count, bar_width, bar_stride, 256, 256),
        frame_bytes,
        swap_damage,
        force_full_damage: true,
        max_height: config.bars.max_height.unwrap_or(1.0),
        mode,
        circle,
        curve_paths: curve_paths.into_boxed_slice(),
        path_ssbo,
        width_ssbo,
        curve_bars: Box::new([]),
        curve_occlude: curve_occlude.into_boxed_slice(),
        curve_horizon: Box::new([]),
        curve_fit,
        curve_key,
        curve_keys,
        occ_ssbo,
        curve_image,
        curve_box: None,
        curve_output: (1, 1),
        resolution_location,
        dsa,
        matte_color_location,
        path_scale_location,
        path_offset_location,
        occ_map_location,
        silent_frames: 0,
        background_color,
        pinned_output,
        placed_on: None,
        placed_size: None,
        startup_settled: false,
        compositor,
        idle: false,
        qh: qh.clone(),
        conn: conn.clone(),
    };
    // Settle the output list before choosing one. A single roundtrip
    // dispatches the whole initial burst of wl_output events, so retarget()
    // below sees every connected monitor at once and places exactly once. The
    // OutputHandler callbacks it fires do nothing while startup_settled is
    // false
    event_queue.roundtrip(&mut simple_window).unwrap();
    simple_window.startup_settled = true;
    simple_window.retarget(&qh);

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .unwrap();
    event_loop
        .run(frame_duration, &mut simple_window, |state| state.poll_resume())
        .unwrap();
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

/// The affine map from OUTPUT NDC into a surface that covers only
/// `(left, top, w, h)` of that output, as `(scale, offset, occ_map)`.
///
/// Margins count from the top and NDC counts from the bottom, so the vertical
/// half is the one that is easy to get wrong. Separated out from configure()
/// to be testable: a wrong sign here puts the bars off screen with nothing to
/// look at
fn surface_map(out: (u32, u32), rect: (u32, u32, u32, u32)) -> ([f32; 2], [f32; 2], [f32; 4]) {
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
struct DamageMap {
    /// Bar index -> bucket, as a table rather than `i * BUCKETS / bars`.
    bucket_of: Box<[u8]>,
    /// Each bucket's pixel x-range, already widened and clamped
    bucket_x: [(i32, i32); DAMAGE_BUCKETS],
    /// Half the surface height, the one NDC-to-pixel factor still needed
    half_h: f32,
    height: i32,
}

impl DamageMap {
    fn new(bar_count: u32, bar_width: f32, bar_stride: f32, width: u32, height: u32) -> Self {
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
fn damage_rects(frame: &[u8], prev: &[u8], map: &DamageMap, out: &mut [egl::Int]) -> usize {
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

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    // All three funnel into retarget(), which re-derives the answer from the
    // live output list rather than from the event
    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // Held until the startup roundtrip is done; see startup_settled
        if self.startup_settled {
            self.retarget(qh);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // Held until the startup roundtrip is done; see startup_settled
        if self.startup_settled {
            self.retarget(qh);
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // Held until the startup roundtrip is done; see startup_settled
        if self.startup_settled {
            self.retarget(qh);
        }
    }
}

delegate_compositor!(AppState);

delegate_output!(AppState);
delegate_registry!(AppState);
delegate_layer!(AppState);

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(conn, qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for AppState {
    /// The compositor has taken the layer surface away - normally because its
    /// output was unplugged. Stop drawing to it and re-run the policy: if
    /// another monitor is still connected, retarget() rebuilds there; if not,
    /// it idles until one appears
    fn closed(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if layer.wl_surface() != &self.surface {
            return;
        }
        self.placed_on = None;
        self.placed_size = None;
        self.retarget(qh);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Ignore a configure for a surface we have already replaced.
        // place_on() builds a new surface on every move, so a late configure
        // for the old one would otherwise resize the NEW surface's EGL
        // surface to the OLD output's dimensions
        if _layer.wl_surface() != &self.surface {
            return;
        }
        let width = configure.new_size.0;
        let height = configure.new_size.1;
        if debug_enabled() {
            eprintln!("cavawall: configure {width}x{height} on {:?}", self.placed_on);
        }
        self.width = width;
        self.height = height;
        // Unbind the context before destroying the surface it is still current
        // on. NVIDIA's EGL leaves a destroyed-while-current surface in a state
        // that makes the freshly created replacement fail eglSwapBuffers with
        // EGL_BAD_SURFACE on the very first draw; Mesa tolerates it, which is
        // why this only reproduces on NVIDIA
        egl.make_current(self.egl_display, None, None, None).ok();
        egl.destroy_surface(self.egl_display, self.egl_surface)
            .unwrap();
        self.wl_egl_surface =
            WlEglSurface::new(self.surface.id(), self.width as i32, self.height as i32).unwrap();
        self.egl_surface = unsafe {
            egl.create_window_surface(
                self.egl_display,
                self.egl_config,
                self.wl_egl_surface.ptr() as egl::NativeWindowType,
                None,
            )
            .unwrap()
        };
        egl.make_current(
            self.egl_display,
            Some(self.egl_surface),
            Some(self.egl_surface),
            Some(self.egl_context),
        )
        .unwrap();
        // A new EGL surface has undefined contents, and the compositor holds
        // nothing for it: the first frame after this has to be whole
        self.force_full_damage = true;
        // Normals are perpendicular ON SCREEN, not in NDC, so they depend on
        // the output's shape - which is only known here. Rebuilt on every
        // configure so a move to a differently proportioned monitor re-leans
        // the bars rather than skewing them
        if self.mode == Mode::Curve && !self.curve_bars.is_empty() {
            let mut blob: Vec<u8> = Vec::with_capacity(4 + self.curve_horizon.len() * 4);
            blob.extend_from_slice(&(self.curve_horizon.len() as i32).to_ne_bytes());
            for h in &self.curve_horizon {
                blob.extend_from_slice(&h.to_ne_bytes());
            }
            // SAFETY: a context is current here, and every buffer was created
            // at startup
            unsafe {
                upload_bars(&self.curve_bars, self.path_ssbo, self.width_ssbo);
                gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, self.occ_ssbo);
                gl::BufferData(
                    gl::SHADER_STORAGE_BUFFER,
                    blob.len() as GLsizeiptr,
                    blob.as_ptr().cast(),
                    gl::STATIC_DRAW,
                );
                gl::BindBufferBase(gl::SHADER_STORAGE_BUFFER, 2, self.occ_ssbo);
                gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, 0);
            }
        }
        // The only moment the bar-to-pixel mapping can change
        self.damage_map =
            DamageMap::new(self.bar_count, self.bar_width, self.bar_stride, self.width, self.height);
        unsafe {
            gl::Viewport(0, 0, self.width as GLsizei, self.height as GLsizei);
            // The only uniform, and the only place its value can change;
            // draw() re-uploaded it every frame. Fine here because the one
            // program is bound at startup and never unbound
            //
            // The stop count folds in with the height so the shader multiplies
            // once instead of converting, multiplying and dividing per
            // fragment. The count cannot change without a re-exec.
            // Bars only. The circle shader indexes the gradient by radius,
            // which the vertex stage already normalises, so it has no such
            // uniform and GetUniformLocation returned -1 for it
            if self.mode == Mode::Curve {
                gl::Uniform2f(self.resolution_location, self.width as f32, self.height as f32);
                let (ow, oh) = (self.curve_output.0 as f32, self.curve_output.1 as f32);
                let (sw, sh) = (self.width as f32, self.height as f32);
                // Identity when the surface IS the output: output NDC needs
                // no mapping and the horizon is already in frame
                let (scale, offset, occ) = match self.curve_box {
                    None => ([1.0, 1.0], [0.0, 0.0], [0.0, 0.0, 1.0, 1.0]),
                    // The compositor is free to hand back a size other than
                    // the one asked for, so the map is built from what this
                    // configure actually granted
                    Some((left, top, _, _)) => surface_map(
                        (ow as u32, oh as u32),
                        (left, top, sw as u32, sh as u32),
                    ),
                };
                gl::Uniform2f(self.path_scale_location, scale[0], scale[1]);
                gl::Uniform2f(self.path_offset_location, offset[0], offset[1]);
                gl::Uniform4f(self.occ_map_location, occ[0], occ[1], occ[2], occ[3]);
            }
            if self.mode == Mode::Bars {
                gl::Uniform1f(
                    self.gradient_scale_location,
                    (self.gradient_stops - 1) as f32 / self.height as f32,
                );
            }
        }
        // Only draw once a real output has been chosen. main() maps a
        // bootstrap surface purely so EGL has a window to build its context
        // against; drawing to it attaches a buffer and MAPS it, which is how
        // pinning a connector that is not plugged in used to put a 256x256
        // box of bars on whatever output the compositor happened to pick
        if self.placed_on.is_some() {
            self.draw(_conn, qh);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let map = DamageMap::new(heights.len() as u32, bw, stride, w, h);
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
            let map = DamageMap::new(bars as u32, bw, stride, w, h);
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

    /// draw() and poll_resume() must not be able to disagree about what silence
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
}
