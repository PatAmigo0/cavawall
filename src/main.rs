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
    output::{OutputHandler, OutputState},
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

use core::{ffi, panic};
use egl::API as egl;
use std::sync::atomic::{AtomicBool, Ordering};

// Set from the SIGTERM/SIGINT handler; the draw loop notices and shuts down
// cleanly instead of being killed mid-frame.
static EXITING: AtomicBool = AtomicBool::new(false);

/// Below this, a bar counts as silence. Shared by draw() and poll_resume():
/// they are the two halves of one decision (park / unpark) and drifting apart
/// would mean parking at one threshold and waking at another.
const SILENCE_THRESHOLD: f32 = 0.005;
/// The same threshold in cava's raw 16-bit units, for comparing without
/// unpacking to f32 first.
const SILENCE_RAW: u16 = (SILENCE_THRESHOLD * 65530.0) as u16;
/// Frames of continuous silence before parking. Measured, not guessed: with
/// monstercat=1.5 and noise_reduction=60 a tone cut from full volume decays
/// below the threshold in 8 frames (0.18s). 23 frames is 0.51s -- roughly 3x
/// the real decay, the remainder being hysteresis so that a gap between tracks
/// does not park and unpark repeatedly.
const SILENT_GRACE_FRAMES: u32 = 23;

/// CAVAWALL_DEBUG=1 reports what the draw loop actually did each frame.
///
/// Worth keeping: the entire point of this fork is *not* drawing things, and a
/// column that is silently skipped looks exactly like a column that is broken.
/// Without a way to see the commit count, the only instrument left is the
/// compositor's damage log.
/// Minimum movement, in pixels, before a column is redrawn.
///
/// 1 means "redraw whenever the top edge moves at all", which sounds right and
/// is nearly useless: a 702px band moves a whole pixel for a 0.14% change in
/// cava's value, and cava's smoothed output jitters more than that on every
/// band while music plays -- measured at 10.97 of 12 columns redrawing per
/// frame, i.e. almost no saving at all.
fn threshold_px() -> i32 {
    static T: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("CAVAWALL_THRESHOLD_PX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .max(1)
    })
}

fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("CAVAWALL_DEBUG").is_ok_and(|v| v != "0"))
}

/// One bar, one layer surface.
///
/// This is the whole reason the fork exists. Measured against Hyprland 0.56.2:
///
///   * a layer surface is damaged by its own GEOMETRY -- the `damage_buffer`
///     rectangles a client declares are ignored outright;
///   * a subsurface's damage is rolled up into its parent's full geometry, so
///     splitting the band into subsurfaces buys exactly nothing;
///   * two separate layer surfaces damage independently, each by its own size.
///
/// So the only lever a Wayland client actually has is how many surfaces it is
/// and how big they are. One surface per bar means a bar that did not change
/// height is simply never committed, and the compositor never repaints it.
struct Column {
    surface: WlSurface,
    layer_surface: LayerSurface,
    wl_egl_surface: WlEglSurface,
    egl_surface: egl::Surface,
    /// Current size, as the compositor confirmed it in `configure`.
    w: u32,
    h: u32,
    /// Height in whole pixels of the last frame actually committed, or -1 if
    /// nothing has been drawn yet.
    ///
    /// The comparison happens in PIXELS, not in cava's floats. cava quantises
    /// to 1/65530, so consecutive frames almost always differ by *something*;
    /// a difference too small to move the top edge by a whole pixel cannot be
    /// seen, and committing it would damage the column for nothing.
    last_px: i32,
    /// Until the compositor has sent a configure, this column has no valid size
    /// and must not be drawn into.
    configured: bool,
}

/// Pixel geometry (left edge, width) of every bar for a given output width.
///
/// `gap` is a fraction OF THE BAR WIDTH (upstream's convention), so n bars and
/// n-1 gaps span `n + (n-1)*gap` bar-widths. Each edge is rounded independently
/// and the width derived from the two rounded edges, so the columns tile the row
/// exactly instead of accumulating a fractional drift across the screen.
///
/// The gaps are not surfaces at all. Under the old single-surface layout they
/// were transparent pixels that still got composited; here they are simply
/// absent, so nothing is drawn or damaged between bars.
fn column_layout(total_width: u32, bar_count: u32, gap: f32) -> Vec<(i32, u32)> {
    let n = bar_count.max(1) as f32;
    let bar_w = total_width as f32 / (n + (n - 1.0) * gap);
    let stride = bar_w * (1.0 + gap);
    (0..bar_count)
        .map(|i| {
            let left = (i as f32 * stride).round() as i32;
            let right = (i as f32 * stride + bar_w).round() as i32;
            (left, (right - left).max(1) as u32)
        })
        .collect()
}

/// Create one column's surface, layer surface and EGL window surface.
fn make_column(
    compositor: &CompositorState,
    layer_shell: &LayerShell,
    qh: &QueueHandle<AppState>,
    output: Option<&wl_output::WlOutput>,
    x: i32,
    w: u32,
    h: u32,
    egl_display: egl::Display,
    egl_config: egl::Config,
) -> Column {
    let surface = compositor.create_surface(qh);
    let layer_surface =
        layer_shell.create_layer_surface(qh, surface.clone(), Layer::Bottom, Some("cavawall"), output);

    // Empty input region: a wallpaper must never accept pointer input.
    //
    // Without this the surface keeps the default input region (its whole area),
    // so it silently takes pointer focus. It never calls set_cursor, and in
    // Wayland the cursor shape is whatever the focused surface last asked for --
    // so the shape from the previous window (e.g. a terminal's I-beam) stays
    // until some other client sets one. Moving onto an "empty" workspace leaves
    // a stale cursor.
    //
    // Being invisible does not help: parking stops the DRAWING, but the surface
    // stays mapped and keeps its input region.
    //
    // set_input_region has copy semantics, so the wl_region may be destroyed
    // immediately after the commit.
    let input_region = Region::new(compositor).ok();
    if let Some(r) = &input_region {
        layer_surface.set_input_region(Some(r.wl_region()));
    }
    layer_surface.set_size(w, h);
    // Bars grow from the bottom, and each column is placed by its left margin.
    layer_surface.set_anchor(Anchor::BOTTOM | Anchor::LEFT);
    layer_surface.set_margin(0, 0, 0, x);
    // -1, not 0. Zero means "reserve nothing, but stay inside the area other
    // layers have reserved", so a bar with an exclusive zone pushes the whole
    // row sideways -- measured at +60px, which shifted the last column 60px off
    // the right edge of the screen. -1 means "ignore exclusive zones entirely",
    // which is what a wallpaper wants: it belongs to the output, not to the
    // leftovers. (Upstream had the same bug in a subtler form: its single
    // full-width surface was centred in the usable area and so sat 25px off.)
    layer_surface.set_exclusive_zone(-1);
    surface.commit();
    drop(input_region);

    let wl_egl_surface = WlEglSurface::new(surface.id(), w as i32, h as i32).unwrap();
    let egl_surface = unsafe {
        egl.create_window_surface(
            egl_display,
            egl_config,
            wl_egl_surface.ptr() as egl::NativeWindowType,
            None,
        )
        .unwrap()
    };

    Column {
        surface,
        layer_surface,
        wl_egl_surface,
        egl_surface,
        w,
        h,
        last_px: -1,
        configured: false,
    }
}

extern "C" fn on_terminate(_sig: libc::c_int) {
    // Only async-signal-safe work here: flip a flag, nothing else.
    EXITING.store(true, Ordering::SeqCst);
}
use std::ffi::CString;
use std::io::Write;
use std::process::{exit, ChildStdout};
use std::os::fd::AsRawFd;
use std::{env, fs, ptr};
use std::{
    io::{BufReader, Read},
    process::{Command, Stdio},
    time::Duration,
};

pub mod app_config;
use app_config::*;
pub mod cli_help;
use cli_help::*;
use std::collections::HashMap;

const VERTEX_SHADER_SRC: &str = include_str!("shaders/vertex_shader.glsl");

const FRAGMENT_SHADER_SRC: &str = include_str!("shaders/fragment_shader.glsl");

fn main() {
    let config_filename: String;
    let args: Vec<String> = env::args().collect();
    if args.len() == 3 {
        if args[1] != "--config" {
            print_help();
            exit(0);
        }
        config_filename = args[2].clone();
    } else if args.len() != 1 {
        print_help();
        exit(0);
    } else {
        let home_dir = env::var("HOME").expect("Unable to get home directory");
        let own = format!("{}/.config/cavawall/config.toml", home_dir);
        // Upstream's path is still honoured so that anyone switching over from
        // wallpaper-cava keeps a working visualiser before they move anything.
        let inherited = format!("{}/.config/wallpaper-cava/config.toml", home_dir);
        config_filename = if fs::metadata(&own).is_ok() {
            own
        } else if fs::metadata(&inherited).is_ok() {
            eprintln!(
                "cavawall: using {inherited}\n\
                 cavawall: move it to ~/.config/cavawall/config.toml when convenient"
            );
            inherited
        } else {
            "config.toml".to_string()
        }
    }
    // Shut down cleanly on SIGTERM so the surface can be cleared first. A hard
    // kill leaves the last frame burnt into the background: the layer surface
    // goes away, but Hyprland does not reliably repaint underneath it, so a
    // frozen strip of bars stays on the wallpaper until something else forces a
    // redraw. Anything that stops this process (a session manager, a
    // fullscreen watcher) should therefore use SIGTERM, not SIGKILL.
    unsafe {
        libc::signal(libc::SIGTERM, on_terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_terminate as *const () as libc::sighandler_t);
    }

    let cava_output_config: HashMap<String, String> = HashMap::from([
        ("method".into(), "raw".into()),
        ("raw_target".into(), "/dev/stdout".into()),
        ("bit_format".into(), "16bit".into()),
    ]);
    let config_str = fs::read_to_string(config_filename).expect("Unable to read config file");
    let config: Config = match toml::from_str(&config_str) {
        Ok(config) => config,
        Err(error) => panic!("Error parsing config: {}", error.message()),
    };
    let cava_config = CavaConfig {
        general: CavaGeneralConfig {
            framerate: config.general.framerate,
            bars: config.bars.amount,
            autosens: config.general.autosens,
            sensitivity: config.general.sensitivity,
        },
        smoothing: CavaSmoothingConfig {
            monstercat: config.smoothing.monstercat,
            waves: config.smoothing.waves,
            noise_reduction: config.smoothing.noise_reduction,
        },
        output: cava_output_config,
    };
    let string_cava_config: String = toml::to_string(&cava_config).unwrap();
    let mut cmd = Command::new("cava");
    cmd.arg("-p").arg("/dev/stdin");
    let cava_process = cmd
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn cava process");
    let mut cava_stdin = cava_process.stdin.unwrap();
    cava_stdin.write_all(string_cava_config.as_bytes()).unwrap();
    drop(cava_stdin);
    let cava_stdout = cava_process.stdout.unwrap();
    let cava_reader = BufReader::new(cava_stdout);
    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .unwrap();
    let frame_duration = Duration::from_secs(1) / config.general.framerate;
    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell not available");

    // EGL is initialised BEFORE any surface exists, because every column needs
    // the display and config to build its own window surface.
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
    const CONTEXT_ATTRIBUTES: [i32; 7] = [
        egl::CONTEXT_MAJOR_VERSION,
        4,
        egl::CONTEXT_MINOR_VERSION,
        6,
        egl::CONTEXT_OPENGL_PROFILE_MASK,
        egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
        egl::NONE,
    ];

    let egl_context = egl
        .create_context(egl_display, egl_config, None, &CONTEXT_ATTRIBUTES)
        .unwrap();

    // Placeholder geometry. The output size is not known until the compositor
    // reports it, so the columns are laid out again in new_output(); they are
    // invisible until then because a surface with no buffer attached shows
    // nothing, and draw() skips any column that has not been configured.
    let columns: Vec<Column> = (0..config.bars.amount)
        .map(|i| {
            make_column(
                &compositor,
                &layer_shell,
                &qh,
                None,
                i as i32 * 16,
                16,
                16,
                egl_display,
                egl_config,
            )
        })
        .collect();

    // GL setup needs *some* current surface; the first column serves.
    egl.make_current(
        egl_display,
        Some(columns[0].egl_surface),
        Some(columns[0].egl_surface),
        Some(egl_context),
    )
    .unwrap();

    // Never block in eglSwapBuffers.
    //
    // With one surface, waiting for a frame callback was harmless throttling.
    // With one surface per bar it is a deadlock risk: a column the compositor
    // considers hidden may never send a frame callback, and a blocking swap on
    // it would stall the single thread that drives every other column. Pacing
    // comes from cava instead -- draw() blocks on its pipe, which emits at
    // exactly the configured framerate.
    egl.swap_interval(egl_display, 0).ok();
    gl::load_with(|name| egl.get_proc_address(name).unwrap() as *const std::ffi::c_void);
    let version = unsafe {
        let data = gl::GetString(gl::VERSION) as *const i8;
        CString::from_raw(data as *mut _).into_string().unwrap()
    };

    println!("OpenGL version: {}", version);
    println!("EGL version: {}", egl.version());
    let vert_shader_source = CString::new(VERTEX_SHADER_SRC).unwrap();
    let vert_shader = unsafe { gl::CreateShader(gl::VERTEX_SHADER) };
    unsafe {
        gl::ShaderSource(
            vert_shader,
            1,
            &vert_shader_source.as_ptr(),
            std::ptr::null(),
        );
        gl::CompileShader(vert_shader);
    }
    let frag_shader_source = CString::new(FRAGMENT_SHADER_SRC).unwrap();
    let frag_shader = unsafe { gl::CreateShader(gl::FRAGMENT_SHADER) };
    unsafe {
        gl::ShaderSource(
            frag_shader,
            1,
            &frag_shader_source.as_ptr(),
            std::ptr::null(),
        );
        gl::CompileShader(frag_shader);
    }

    let shader_program = unsafe { gl::CreateProgram() };
    unsafe {
        gl::AttachShader(shader_program, vert_shader);
        gl::AttachShader(shader_program, frag_shader);
        gl::LinkProgram(shader_program);
        let mut status = gl::FALSE as gl::types::GLint;
        gl::GetProgramiv(shader_program, gl::LINK_STATUS, &mut status);
        if status != 1 {
            let mut error_log_size: gl::types::GLint = 0;
            gl::GetProgramiv(shader_program, gl::INFO_LOG_LENGTH, &mut error_log_size);
            let mut error_log: Vec<u8> = Vec::with_capacity(error_log_size as usize);
            gl::GetProgramInfoLog(
                shader_program,
                error_log_size,
                &mut error_log_size,
                error_log.as_mut_ptr() as *mut _,
            );

            error_log.set_len(error_log_size as usize);
            let log = String::from_utf8(error_log).unwrap();
            panic!("{}", log);
        }
    }
    let mut vbo = 0;
    let mut vao = 0;
    let mut ebo = 0;
    let mut gradient_colors_ssbo = 0;
    let gradient_colors_rgba: Vec<[f32; 4]> = config
        .colors
        .iter()
        .map(|color| array_from_config_color((color.1).clone()))
        .collect();

    let gradient_colors_size = gradient_colors_rgba.len() as i32;
    let mut buffer_data: Vec<u8> = (gradient_colors_size).to_le_bytes().to_vec();
    buffer_data.extend([0, 0, 0, 0].repeat(3)); // Fix for vec4 alignment
    for color in gradient_colors_rgba.iter() {
        for color_value in color {
            buffer_data.extend_from_slice(&color_value.to_le_bytes());
        }
    }

    // A single quad: two triangles over four corners. Upstream needed
    // bar_count quads because every bar lived in one surface; here each column
    // is drawn into its own surface and is exactly one quad.
    //
    // (Upstream's draw call passed `bar_count * 3 * size_of::<u16>()` as the
    // index COUNT, with a comment wondering why *3 worked when *6 looked right.
    // It worked by accident: size_of::<u16>() is 2, so *3*2 == *6. The argument
    // is a count of indices, not a byte length.)
    let indices: Vec<u16> = vec![0, 1, 2, 1, 2, 3];

    let window_size_string = CString::new("WindowSize").unwrap();
    unsafe {
        gl::GenVertexArrays(1, &mut vao);
        gl::BindVertexArray(vao);
        gl::GenBuffers(1, &mut vbo);
        gl::GenBuffers(1, &mut ebo);
        gl::GenBuffers(1, &mut gradient_colors_ssbo);
        gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
        gl::BindBuffer(gl::ELEMENT_ARRAY_BUFFER, ebo);
        gl::BufferData(
            gl::ELEMENT_ARRAY_BUFFER,
            (indices.len() * std::mem::size_of::<u16>()) as gl::types::GLsizeiptr,
            indices.as_ptr() as *const ffi::c_void,
            gl::STATIC_DRAW,
        );
        gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, gradient_colors_ssbo);
        gl::BufferData(
            gl::SHADER_STORAGE_BUFFER,
            buffer_data.len() as GLsizeiptr,
            buffer_data.as_ptr() as *const ffi::c_void,
            gl::STATIC_DRAW,
        );
        gl::BindBufferBase(gl::SHADER_STORAGE_BUFFER, 0, gradient_colors_ssbo);
        gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, 0);
        gl::VertexAttribPointer(
            0,
            2,
            gl::FLOAT,
            gl::FALSE,
            (2 * std::mem::size_of::<f32>()) as gl::types::GLsizei,
            std::ptr::null(),
        );
        gl::EnableVertexAttribArray(0);
        gl::BindVertexArray(0);
    }

    let windows_size_location =
        unsafe { gl::GetUniformLocation(shader_program, window_size_string.as_ptr()) };

    let mut simple_window = AppState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        width: 256,
        height: 256,
        layer_shell,
        columns,
        band: 16,
        cava_reader,
        egl_config,
        egl_context,
        egl_display,
        shader_program,
        vao,
        vbo,
        windows_size_location,
        bar_count: config.bars.amount,
        bar_gap: config.bars.gap,
        max_height: config.bars.max_height.unwrap_or(1.0),
        silent_frames: 0,
        background_color: array_from_config_color(config.general.background_color),
        preferred_output_name: config.general.preferred_output,
        compositor,
        idle: false,
        qh: qh.clone(),
        conn: conn.clone(),
    };
    event_loop
        .run(frame_duration, &mut simple_window, |state| state.tick())
        .unwrap();
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    width: u32,
    height: u32,
    layer_shell: LayerShell,
    /// One per bar, left to right. Replaced wholesale when the output changes.
    columns: Vec<Column>,
    /// Height of the band the bars occupy, in output pixels: the surface height
    /// of every column.
    band: u32,
    cava_reader: BufReader<ChildStdout>,
    egl_config: egl::Config,
    egl_context: egl::Context,
    egl_display: egl::Display,
    shader_program: u32,
    vao: u32,
    vbo: u32,
    windows_size_location: i32,
    bar_count: u32,
    bar_gap: f32,
    max_height: f32,
    silent_frames: u32,
    background_color: [f32; 4],
    preferred_output_name: Option<String>,
    compositor: CompositorState,
    /// Parked: silent, not committing, waiting for audio on the idle tick.
    idle: bool,
    /// Kept so the idle tick can request a frame callback -- event_loop.run's
    /// callback hands back only &mut AppState, not the QueueHandle.
    qh: QueueHandle<AppState>,
    /// Same reason, for clear_and_exit: SIGTERM must be honoured while parked,
    /// and the parked path has no Connection handed to it.
    conn: Connection,
}

impl AppState {
    /// Paint one fully transparent frame and commit it before exiting, so the
    /// compositor is left with a clean surface rather than our last set of
    /// bars. Without this a hard kill leaves that frame visible on the
    /// background until something else forces a repaint.
    fn clear_and_exit(&mut self, conn: &Connection) -> ! {
        let (display, context) = (self.egl_display, self.egl_context);
        for col in &self.columns {
            if !col.configured {
                continue;
            }
            let _ = egl.make_current(
                display,
                Some(col.egl_surface),
                Some(col.egl_surface),
                Some(context),
            );
            unsafe {
                gl::ClearColor(0.0, 0.0, 0.0, 0.0);
                gl::Clear(gl::COLOR_BUFFER_BIT);
            }
            // swap_buffers attaches and commits, so no explicit commit here.
            let _ = egl.swap_buffers(display, col.egl_surface);
        }
        // Round-trip so the commits actually reach the compositor before the
        // process goes away and its objects are destroyed.
        let _ = conn.roundtrip();
        std::process::exit(0);
    }

    /// Called on every event-loop timeout, whether or not the compositor sent
    /// anything. While parked this is the only thing running: it drains whatever
    /// cava has produced and unparks the moment a sample crosses the threshold.
    ///
    /// Reads are gated on poll() rather than made non-blocking, so the blocking
    /// read_exact in draw() keeps working unchanged. cava writes a whole 24-byte
    /// frame at a time at the configured framerate, so a readable fd means a
    /// frame is there; a partial read would complete within one frame period
    /// anyway.
    /// Called on every event-loop iteration, at least once per frame period.
    ///
    /// This is the only thing that drives rendering. Frame callbacks cannot do
    /// it any more: with one surface per bar, a frame in which no bar moved
    /// commits nothing, so there would be no callback to drive the next frame
    /// and the visualiser would stop. Pacing instead comes from cava -- draw()
    /// blocks on its pipe, which produces exactly one frame per period.
    pub fn tick(&mut self) {
        if EXITING.load(Ordering::SeqCst) {
            let conn = self.conn.clone();
            self.clear_and_exit(&conn);
        }
        if self.idle {
            self.poll_resume();
            return;
        }
        let (conn, qh) = (self.conn.clone(), self.qh.clone());
        self.draw(&conn, &qh);
    }

    pub fn poll_resume(&mut self) {
        // SIGTERM is caught by a handler that only sets EXITING; the checks that
        // act on it live in draw(), which does not run while parked. Without
        // this, a parked instance ignores SIGTERM entirely and has to be killed
        // -- which is exactly what happened once this parking existed.
        if EXITING.load(Ordering::SeqCst) {
            let conn = self.conn.clone();
            self.clear_and_exit(&conn);
        }
        if !self.idle {
            return;
        }
        let fd = self.cava_reader.get_ref().as_raw_fd();
        let mut buf = vec![0u8; self.bar_count as usize * 2];
        loop {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 0) } <= 0 || pfd.revents & libc::POLLIN == 0 {
                return; // nothing waiting; stay parked
            }
            if self.cava_reader.read_exact(&mut buf).is_err() {
                return;
            }
            if buf
                .chunks_exact(2)
                .any(|c| u16::from_le_bytes([c[0], c[1]]) > SILENCE_RAW)
            {
                // Unpark. Nothing to commit: rendering is driven by the event
                // loop's own timeout, not by frame callbacks, so clearing the
                // flag is the entire operation. (It used to commit here purely
                // to restart the frame-callback loop, which cost one full-band
                // damage every time audio resumed.)
                self.idle = false;
                self.silent_frames = 0;
                return;
            }
        }
    }

    pub fn draw(&mut self, conn: &Connection, _qh: &QueueHandle<Self>) {
        let mut cava_buffer: Vec<u8> = vec![0; self.bar_count as usize * 2];
        let mut unpacked_data: Vec<f32> = vec![0.0; self.bar_count as usize];
        if let Err(e) = self.cava_reader.read_exact(&mut cava_buffer) {
            // A signal interrupts the blocking read, which is exactly how we
            // find out it is time to go.
            if EXITING.load(Ordering::SeqCst) {
                self.clear_and_exit(conn);
            }
            if e.kind() == std::io::ErrorKind::Interrupted {
                return;
            }
            panic!("cava read failed: {e}");
        }
        if EXITING.load(Ordering::SeqCst) {
            self.clear_and_exit(conn);
        }
        for (unpacked_data_index, i) in (0..cava_buffer.len()).step_by(2).enumerate() {
            let num = u16::from_le_bytes([cava_buffer[i], cava_buffer[i + 1]]);
            unpacked_data[unpacked_data_index] = (num as f32) / 65530.0;
        }
        // Skip GPU work while the audio is silent. cava emits frames at the
        // configured framerate whether or not anything is playing, so without
        // this the full-screen surface is recomposited 60x/sec forever just to
        // draw bars that are all zero -- measurably pinning an integrated GPU.
        //
        // Commit with no new buffer instead of drawing: that still schedules
        // Grace before parking, so the bars finish falling to zero rather than
        // freezing part-way down.
        //
        // Measured, not guessed: with monstercat=1.5 and noise_reduction=60, a
        // tone cut from full volume decays below the threshold in 8 frames --
        // 0.18s. The original 90 (2.0s) was 11x that. 23 frames is 0.51s, still
        // ~3x the real decay.
        //
        // The remainder is hysteresis rather than decay: a quiet passage or a
        // gap between tracks would otherwise park and unpark repeatedly. That
        // costs almost nothing -- parking sets a flag, unparking is one commit
        // -- and is invisible, since the bars are already at zero whenever it
        // happens.
        if unpacked_data.iter().all(|&v| v < SILENCE_THRESHOLD) {
            self.silent_frames = self.silent_frames.saturating_add(1);
        } else {
            self.silent_frames = 0;
        }
        if self.silent_frames > SILENT_GRACE_FRAMES {
            // PARK. No draw, and critically no commit either.
            //
            // A bufferless commit is free for us but not for the compositor:
            // Hyprland damages a layer by its GEOMETRY on any commit, buffer
            // attached or not, so every one recomposited the whole band. The
            // original comment here claimed it "produces no damage" -- false,
            // and visible in the damage overlay as a flash on an idle workspace
            // with no audio playing.
            //
            // Committing was only ever there to keep frame callbacks coming, so
            // that audio returning would be noticed. That job moves to
            // poll_resume(), driven by the timeout event_loop.run already has,
            // which owes nothing to the compositor. So while silent this draws
            // nothing, commits nothing, and damages nothing.
            self.idle = true;
            return;
        }

        // Draw only the columns whose top edge actually moved.
        //
        // This is where the fork earns its keep. Each column is its own layer
        // surface, so committing one damages only that column's geometry; a
        // column that is skipped is not committed, and the compositor does not
        // repaint it at all.
        let (display, context) = (self.egl_display, self.egl_context);
        let (program, vao, vbo, wsl) = (
            self.shader_program,
            self.vao,
            self.vbo,
            self.windows_size_location,
        );
        let bg = self.background_color;
        let mut drawn = 0u32;
        let mut unconfigured = 0u32;

        for (i, col) in self.columns.iter_mut().enumerate() {
            if !col.configured {
                unconfigured += 1;
                continue;
            }
            // Quantise to whole pixels BEFORE deciding whether to redraw, so
            // the test asks the only question that matters: would this look
            // any different?
            let px = (unpacked_data[i] * col.h as f32).round().clamp(0.0, col.h as f32) as i32;
            // Redraw only if the top edge moved far enough to be worth a
            // commit. last_px < 0 means "never drawn", which always draws.
            if col.last_px >= 0 && (px - col.last_px).abs() < threshold_px() {
                continue;
            }
            col.last_px = px;
            drawn += 1;

            egl.make_current(
                display,
                Some(col.egl_surface),
                Some(col.egl_surface),
                Some(context),
            )
            .unwrap();

            // NDC: -1.0 is the bottom of the column, +1.0 the top. The bar
            // spans the column's full width, because the gaps between bars are
            // not part of any surface any more.
            //
            // max_height is NOT applied here: the surface has already been
            // sized to that fraction of the screen, so a full-volume bar fills
            // it exactly. Applying it twice makes bars reach max_height^2 of
            // the screen -- visibly short, which is what the first attempt at
            // this looked like.
            let top = 2.0 * (px as f32 / col.h as f32) - 1.0;
            let vertices: [f32; 8] = [-1.0, top, 1.0, top, -1.0, -1.0, 1.0, -1.0];

            unsafe {
                // Columns can differ by a pixel in width after rounding, and
                // the viewport does not follow eglMakeCurrent, so it has to be
                // set per column. The gradient reads gl_FragCoord.y against
                // WindowSize.y, so getting this wrong would tilt the colours.
                gl::Viewport(0, 0, col.w as GLsizei, col.h as GLsizei);
                gl::BindVertexArray(vao);
                gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (vertices.len() * std::mem::size_of::<f32>()) as GLsizeiptr,
                    vertices.as_ptr() as *const _,
                    gl::DYNAMIC_DRAW,
                );
                gl::Enable(gl::BLEND);
                gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
                gl::ClearColor(bg[0], bg[1], bg[2], bg[3]);
                gl::Clear(gl::COLOR_BUFFER_BIT);
                gl::UseProgram(program);
                // Only .y is read by the fragment shader: the gradient runs
                // vertically, which is exactly why splitting the band into
                // vertical columns cannot produce a seam. Every column is the
                // same height and so resolves the identical gradient.
                gl::Uniform2f(wsl, col.w as f32, col.h as f32);
                gl::DrawElements(gl::TRIANGLES, 6, gl::UNSIGNED_SHORT, ptr::null());
                gl::BindVertexArray(0);
            }
            // swap_buffers attaches the new buffer and commits the surface.
            egl.swap_buffers(display, col.egl_surface).unwrap();
        }
        if debug_enabled() {
            eprintln!(
                "cavawall: {}/{} columns committed ({} not yet configured)",
                drawn,
                self.columns.len(),
                unconfigured
            );
        }
        // Push the commits out now rather than waiting for the event loop to
        // come back around.
        let _ = conn.flush();
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let info = self.output_state.info(&output).unwrap();
        let mut need_configuration = false;
        if let Some(output_name) = info.name {
            if let Some(preffered_output_name) = self.preferred_output_name.clone() {
                if output_name == preffered_output_name {
                    need_configuration = true;
                }
            }
        }
        if self.preferred_output_name.is_none() {
            need_configuration = true;
        }
        if need_configuration {
            let logical_size = info.logical_size.unwrap();
            self.width = logical_size.0 as u32;
            self.height = logical_size.1 as u32;

            // Only the band the bars can actually reach, anchored to the bottom
            // they grow from.
            //
            // max_height caps how far up a full-volume bar goes as a fraction of
            // screen height, so everything above it used to be cleared to
            // transparent every frame and recomposited for nothing -- with the
            // default 0.65, the top 35% of the output.
            let band = ((self.height as f32 * self.max_height).ceil() as u32)
                .clamp(1, self.height);
            self.band = band;

            let layout = column_layout(self.width, self.bar_count, self.bar_gap);
            let (display, config) = (self.egl_display, self.egl_config);

            // Replace the columns wholesale. Unbind the context first: NVIDIA's
            // EGL leaves a destroyed-while-current surface in a state that makes
            // the replacement fail eglSwapBuffers with EGL_BAD_SURFACE on its
            // very first draw. Mesa tolerates it, which is why this only ever
            // reproduced on NVIDIA.
            egl.make_current(display, None, None, None).ok();
            for col in self.columns.drain(..) {
                // Teardown order is load-bearing, innermost first. Destroying
                // the wl_surface while its zwlr_layer_surface_v1 still refers
                // to it is a protocol error ("invalid object"), which kills the
                // connection and makes the very next eglCreateWindowSurface
                // fail with BadAlloc.
                egl.destroy_surface(display, col.egl_surface).ok();
                let Column {
                    surface,
                    layer_surface,
                    wl_egl_surface,
                    ..
                } = col;
                drop(wl_egl_surface); // wl_egl_window
                drop(layer_surface); // zwlr_layer_surface_v1
                surface.destroy(); // wl_surface, last
            }
            for (x, w) in layout {
                self.columns.push(make_column(
                    &self.compositor,
                    &self.layer_shell,
                    qh,
                    Some(&output),
                    x,
                    w,
                    band,
                    display,
                    config,
                ));
            }
        }
    }

    // For now update_output is same as new_output, because I'm not really sure what to do with it
    fn update_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.new_output(_conn, qh, output);
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
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

    /// Deliberately empty.
    ///
    /// Rendering used to be driven by frame callbacks on the one surface. With
    /// a surface per bar that no longer works: the loop would only be driven by
    /// whichever columns happened to commit, and a frame in which nothing
    /// changed would commit nothing and stall it outright. The event loop's own
    /// timeout drives drawing instead -- see tick().
    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
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
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {}

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if w == 0 || h == 0 {
            return;
        }
        let (display, config) = (self.egl_display, self.egl_config);
        let Some(col) = self
            .columns
            .iter_mut()
            .find(|c| c.layer_surface.wl_surface() == layer.wl_surface())
        else {
            return;
        };
        if col.configured && col.w == w && col.h == h {
            return;
        }

        // Unbind before destroying the surface the context is current on -- see
        // the note in new_output(); this is the NVIDIA EGL_BAD_SURFACE path.
        egl.make_current(display, None, None, None).ok();
        egl.destroy_surface(display, col.egl_surface).ok();

        col.w = w;
        col.h = h;
        col.wl_egl_surface = WlEglSurface::new(col.surface.id(), w as i32, h as i32).unwrap();
        col.egl_surface = unsafe {
            egl.create_window_surface(
                display,
                config,
                col.wl_egl_surface.ptr() as egl::NativeWindowType,
                None,
            )
            .unwrap()
        };
        // Size changed, so whatever was on screen is gone: force a redraw
        // rather than trusting the cached pixel height.
        col.last_px = -1;
        col.configured = true;
    }
}
