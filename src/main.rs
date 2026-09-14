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

/// Connector-name prefixes that mean "the machine's own panel". Everything
/// else -- HDMI, DP, DVI, a dock -- counts as external and is preferred.
///
/// Deliberately a policy rather than a per-machine hardware fact. It needs no
/// list to keep in sync across machines, it works on a machine whose dock is
/// DP rather than HDMI, and it survives the panel's connector being renamed --
/// eDP-1 vs eDP-2 has been observed to change across reboots on this hardware
/// with no hardware change at all, which is exactly what a hardcoded name
/// cannot survive.
const BUILTIN_CONNECTOR_PREFIXES: [&str; 3] = ["eDP", "LVDS", "DSI"];

fn is_builtin_connector(name: &str) -> bool {
    BUILTIN_CONNECTOR_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// CAVAWALL_DEBUG, resolved once. Some of the call sites below sit in the
/// per-frame path, and env::var allocates a String and takes the process-wide
/// environment lock on every call -- not something to do 45 times a second
/// just to decide not to print.
fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env::var("CAVAWALL_DEBUG").is_ok_and(|v| v != "0"))
}

extern "C" fn on_terminate(_sig: libc::c_int) {
    // Only async-signal-safe work here: flip a flag, nothing else.
    EXITING.store(true, Ordering::SeqCst);
}
use std::ffi::CString;
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

pub mod app_config;
use app_config::*;
pub mod scheme;
pub mod cli_help;
use cli_help::*;
use std::collections::HashMap;

const VERTEX_SHADER_SRC: &str = include_str!("shaders/vertex_shader.glsl");

const FRAGMENT_SHADER_SRC: &str = include_str!("shaders/fragment_shader.glsl");

/// Log a resolved palette as hex, which is how it is written in the config and
/// in the scheme -- printing the f32 quads it becomes would be unreadable.
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

    let config_str = fs::read_to_string(config_filename).expect("Unable to read config file");
    let config: Config = match toml::from_str(&config_str) {
        Ok(config) => config,
        Err(error) => panic!("Error parsing config: {}", error.message()),
    };
    // Effective bar count. Startup-only by construction: it is written into
    // the spawned cava's config below and baked into the index buffer further
    // down, so there is no honest way to follow it live. See SchemeConfig::bars.
    let follow_bars = config.scheme.as_ref().and_then(|s| s.bars).unwrap_or(false);
    let bar_count = if follow_bars {
        scheme::bar_count().unwrap_or(config.bars.amount)
    } else {
        config.bars.amount
    };
    let mut cava_output_config: HashMap<String, String> = HashMap::from([
        ("method".into(), "raw".into()),
        ("raw_target".into(), "/dev/stdout".into()),
        ("bit_format".into(), "16bit".into()),
    ]);
    // Only forwarded when set, so leaving it out keeps cava's own default
    // rather than this program quietly picking one.
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
    // CAVAWALL_DEBUG=1 shows exactly what cava is being told. cava is spawned
    // with its config on stdin, so there is no file to inspect afterwards and
    // no other way to check a setting actually got through.
    if debug_enabled() {
        eprintln!("cavawall: cava config >>>\n{string_cava_config}<<<");
    }
    let mut cmd = Command::new("cava");
    cmd.arg("-p").arg("/dev/stdin");
    let cava_process = cmd
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn cava process");
    // Captured before the field moves below leave `cava_process` partially moved
    // and unusable as a whole. Needed so a re-exec can reap this child: see
    // reexec(), where not having it leaked a zombie per bar-count change.
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
    // settled with an explicit roundtrip -- see the note there.
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
    // Empty input region: a wallpaper must never accept pointer input.
    //
    // Without this the surface keeps the default input region (its whole area),
    // so it silently takes pointer focus over the entire screen. It never calls
    // set_cursor, and in Wayland the cursor shape is whatever the focused
    // surface last asked for -- so the shape from the previous window (e.g. the
    // I-beam from a terminal) stays until some other client sets one. Moving
    // onto an "empty" workspace leaves a stale cursor.
    //
    // Being invisible does not help: the silence-skip patch stops it DRAWING,
    // but the surface stays mapped and keeps its input region.
    //
    // set_input_region has copy semantics and the wl_region may be destroyed
    // immediately, so letting it drop after the commit is fine.
    let input_region = Region::new(&compositor).ok();
    if let Some(r) = &input_region {
        layer_surface.set_input_region(Some(r.wl_region()));
    }
    // -1, not the default 0. Zero means "reserve nothing, but stay inside the
    // area other layers have reserved", so any bar with an exclusive zone
    // shifts and shrinks the wallpaper: on a 1920-wide output with a bar, this
    // surface was placed at x=25 and ran 25px off the right edge. -1 means
    // "ignore exclusive zones", which is what a wallpaper wants -- it belongs
    // to the output, not to whatever is left over.
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
    // Ordered once and kept. A live re-resolve reuses this exact Vec rather
    // than walking the HashMap again, which would be free to hand back a
    // different order and silently reshuffle the gradient mid-session.
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
    let buffer_data = gradient_buffer(&initial_rgba);
    // Only watch when the palette actually follows the scheme, so the Option
    // alone says whether live colours are on -- no second flag to disagree.
    let scheme_watch = if follow_colors {
        scheme::Watch::new(scheme::scheme_dir(), scheme::SCHEME_FILE)
    } else {
        None
    };
    // Watched for the same reason the scheme is, but acted on differently: a new
    // bar count cannot be applied in place, so this one re-execs. See reexec().
    let bars_watch = if follow_bars {
        scheme::Watch::new(scheme::shell_dir(), scheme::SHELL_FILE)
    } else {
        None
    };

    let mut indices: Vec<u16> = vec![0; bar_count as usize * 6];
    for i in 0..bar_count as usize {
        indices[i * 6] = i as u16 * 4;
        indices[i * 6 + 1] = i as u16 * 4 + 1;
        indices[i * 6 + 2] = i as u16 * 4 + 2;
        indices[i * 6 + 3] = i as u16 * 4 + 1;
        indices[i * 6 + 4] = i as u16 * 4 + 2;
        indices[i * 6 + 5] = i as u16 * 4 + 3;
    }

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

    // CAVAWALL_OUTPUT wins over the config file, and is how fullscreen-watch
    // moves the visualiser between monitors: it relaunches with this set, so
    // argv stays exactly [binary]. That matters -- the launcher, the fish
    // toggle and fullscreen-watch itself all identify this process by an
    // EXACT argv match, and a --output flag would have silently broken all
    // three at once.
    //
    // Unset (and no preferred_output) means choose automatically.
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
        shader_program,
        vao,
        vbo,
        windows_size_location,
        bar_count,
        cava_pid,
        gradient_colors_ssbo,
        color_stops,
        scheme_watch,
        bars_watch,
        bar_gap: config.bars.gap,
        max_height: config.bars.max_height.unwrap_or(1.0),
        silent_frames: 0,
        background_color: array_from_config_color(config.general.background_color),
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
    // false, which is the point.
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
    shader_program: u32,
    vao: u32,
    vbo: u32,
    windows_size_location: i32,
    bar_count: u32,
    /// Kept so the palette can be re-uploaded in place. Upstream created this
    /// buffer and dropped the handle, which was fine when colours could only
    /// ever be set once.
    gradient_colors_ssbo: u32,
    /// The configured stops, already in gradient order.
    color_stops: Vec<ConfigColor>,
    /// None when colours do not follow the scheme.
    scheme_watch: Option<scheme::Watch>,
    /// None when the bar count does not follow Caelestia's settings.
    bars_watch: Option<scheme::Watch>,
    /// The cava child, kept so a re-exec can kill and reap it.
    cava_pid: u32,
    bar_gap: f32,
    max_height: f32,
    silent_frames: u32,
    background_color: [f32; 4],
    /// Explicit output pin: CAVAWALL_OUTPUT, else the config's
    /// preferred_output. None means choose automatically -- an external
    /// monitor if one is connected, the built-in panel otherwise.
    pinned_output: Option<String>,
    /// Name of the output we currently have a mapped surface on. None means
    /// nothing is drawn: either no output has been chosen yet, or the one we
    /// were on went away.
    placed_on: Option<String>,
    /// Logical size that surface was built for, so an unrelated output
    /// property change does not tear it down and rebuild it for nothing.
    placed_size: Option<(i32, i32)>,
    /// False until the startup roundtrip has enumerated every output. Outputs
    /// are announced one at a time, so acting on the first one to arrive meant
    /// placing on the laptop panel and then moving to the external monitor a
    /// moment later -- a visible flash of bars on the wrong screen at login.
    startup_settled: bool,
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
        unsafe {
            gl::ClearColor(0.0, 0.0, 0.0, 0.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
        let _ = egl.swap_buffers(self.egl_display, self.egl_surface);
        self.surface.commit();
        // Round-trip so the commit actually reaches the compositor before the
        // process goes away and its objects are destroyed.
        let _ = conn.roundtrip();
        std::process::exit(0);
    }

    /// Start over, because the bar count changed and cannot be changed in place.
    ///
    /// It reaches the GPU as an index buffer sized once at startup, and reaches
    /// cava as a config written to that child's stdin at exec time. Neither is
    /// reachable from here, so a fresh process is the only honest way to apply
    /// a new count -- which is why colours update live and this does not.
    ///
    /// exec, rather than spawning cavawall-launch as everything else does. exec
    /// keeps the PID, so every guard built on "is one running" stays true right
    /// through the swap: the launcher's flock and 5s kill-wait, and
    /// fullscreen-watch's instance count. There is never a moment with zero or
    /// two instances, so the stacking race that lock exists for cannot start
    /// here. The environment carries over too, so a CAVAWALL_OUTPUT that
    /// fullscreen-watch set to move us to another monitor survives the restart.
    fn reexec(&mut self) {
        // The same transparent frame clear_and_exit paints, for the same reason:
        // on exec our Wayland connection closes exactly as it would on a kill,
        // and the compositor does not reliably repaint under a layer surface
        // that just disappears. Without this the old bars stay burnt onto the
        // wallpaper until something else damages that strip.
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
        // its stdout pipe closes, so what is left is a zombie -- one per
        // bar-count change, all parented to a process that will not reap them.
        // Measured before this existed: eight changes, seven <defunct> cava.
        //
        // SIGKILL rather than SIGTERM: cava owns no surface, no files and no
        // cleanup worth waiting on, and we want it gone before the exec rather
        // than at some point after it.
        unsafe { libc::kill(self.cava_pid as libc::pid_t, libc::SIGKILL) };
        // Reaps that child and any zombie an earlier re-exec left behind, since
        // those are still ours for the same reason. Terminates on ECHILD.
        loop {
            if unsafe { libc::waitpid(-1, ptr::null_mut(), 0) } <= 0 {
                break;
            }
        }

        // argv[0] before current_exe(): cavawall-launch execs us by absolute
        // path, and after a `cargo install` over a running instance
        // /proc/self/exe reads back as "<path> (deleted)", which will not exec.
        // Both are checked for existence so neither can hand over a dead path.
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
        // repainted by the next draw, so the visible cost is one blank frame.
        eprintln!(
            "cavawall: re-exec failed, keeping {} bars: {}",
            self.bar_count,
            std::io::Error::last_os_error()
        );
    }

    /// Re-resolve the palette against the current scheme and re-upload it.
    ///
    /// Cheap enough to do inline: one small buffer upload, no pipeline rebuild,
    /// no surface reconfigure. Nothing reachable from here can change the stop
    /// COUNT -- a role the scheme lacks falls back to that stop's own hex rather
    /// than dropping it -- so no geometry is invalidated and the next frame
    /// simply draws in the new colours.
    ///
    /// A scheme that will not read or parse leaves the current palette alone
    /// instead of falling back to the static one. The file is written while we
    /// may be reading it, and a momentary flash of the fallback palette every
    /// time the wallpaper changes would be worse than a frame of staleness.
    fn reload_colors(&mut self) {
        let Some(live) = scheme::colours() else {
            return;
        };
        let rgba = resolve_stops(&self.color_stops, Some(&live));
        debug_palette("reloaded", &rgba);
        let buf = gradient_buffer(&rgba);
        unsafe {
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
    /// cava has produced and unparks the moment a sample crosses the threshold.
    ///
    /// Reads are gated on poll() rather than made non-blocking, so the blocking
    /// read_exact in draw() keeps working unchanged. cava writes a whole 24-byte
    /// frame at a time at the configured framerate, so a readable fd means a
    /// frame is there; a partial read would complete within one frame period
    /// anyway.
    pub fn poll_resume(&mut self) {
        // SIGTERM is caught by a handler that only sets EXITING; the checks that
        // act on it live in draw(), which does not run while parked. Without
        // this, a parked instance ignores SIGTERM entirely and has to be killed
        // -- which is exactly what happened once this parking existed.
        if EXITING.load(Ordering::SeqCst) {
            let conn = self.conn.clone();
            self.clear_and_exit(&conn);
        }
        // Parked AND unplaced: the output went away while the audio was
        // silent. Committing here would map a surface belonging to nothing.
        // retarget() restarts the loop when an output comes back.
        if self.placed_on.is_none() {
            return;
        }

        // Deliberately below the placement guard rather than above it. Both of
        // these touch GL -- one re-uploads the SSBO, the other clears the
        // surface before exec'ing -- and unplaced means there is no layer
        // surface and no EGL surface to be current on. The cost is that a
        // scheme or bar-count change arriving while no output is usable is not
        // applied until the next one; the alternative is GL calls against a
        // surface that does not exist, and nothing is on screen to update
        // anyway.
        //
        // Above the idle early-return, though: parked means no draw and no
        // commit, so a change arriving while silent would otherwise sit unread
        // until audio resumed, and the bars would come back stale. Re-uploading
        // costs one buffer write and needs no redraw -- parked implies the bars
        // are already at zero, so there is nothing on screen whose colour
        // anyone could see change.
        //
        // Bars first: a changed count re-execs, which re-reads the scheme on the
        // way up anyway, so resolving colours before that would be thrown away.
        if self.bars_watch.as_ref().is_some_and(|w| w.take_event()) {
            // Every settings change rewrites the whole of shell.json, so most
            // wake-ups here are about something else entirely. Compare before
            // acting -- restarting the visualiser because an unrelated toggle
            // moved would be indefensible.
            if scheme::bar_count().is_some_and(|n| n != self.bar_count) {
                self.reexec();
            }
        }
        if self.scheme_watch.as_ref().is_some_and(|w| w.take_event()) {
            self.reload_colors();
        }

        if !self.idle {
            // Running: the compositor's frame callbacks drive draw(), and this
            // tick has nothing to do. Deliberately NOT a second drive for
            // draw() -- rendering ahead of the callback is what the callback
            // exists to throttle, and cava's pipe is drained by draw() anyway.
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
                // Unpark: one commit restarts the frame-callback loop, and
                // rendering is driven by the compositor again from here.
                self.idle = false;
                self.silent_frames = 0;
                let qh = self.qh.clone();
                self.surface.frame(&qh, self.surface.clone());
                self.surface.commit();
                return;
            }
        }
    }

    /// Rank a connected output; lower wins, None means "not eligible at all".
    ///
    /// A pin excludes everything else outright rather than merely preferring
    /// the pinned output -- when fullscreen-watch says "eDP-1", falling back
    /// to the monitor it just ruled out would defeat the point.
    fn output_rank(&self, name: &str) -> Option<u8> {
        match &self.pinned_output {
            Some(pin) => (pin == name).then_some(0),
            None => Some(u8::from(is_builtin_connector(name))),
        }
    }

    /// The output we should be on, out of everything currently connected.
    ///
    /// Ties break on name purely so the choice is stable: two externals must
    /// not swap between calls and rebuild the surface each time.
    fn choose_output(&self) -> Option<(wl_output::WlOutput, OutputInfo)> {
        self.output_state
            .outputs()
            .filter_map(|o| {
                let info = self.output_state.info(&o)?;
                // No logical size yet means the compositor has not finished
                // describing it; set_size would have nothing to work from.
                info.logical_size?;
                let name = info.name.clone()?;
                let rank = self.output_rank(&name)?;
                Some((rank, name, o, info))
            })
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(_, _, o, info)| (o, info))
    }

    /// Re-run the whole output policy against what is connected right now, and
    /// move if the answer changed. Every OutputHandler callback funnels here so
    /// the three of them cannot drift apart -- which is what happened before,
    /// when update_output rebuilt the surface for any property change at all
    /// and output_destroyed did nothing whatsoever.
    fn retarget(&mut self, qh: &QueueHandle<Self>) {
        // Drop a placement whose output is gone before choosing a new one.
        // Checked against the live output list rather than trusting which
        // output the callback named, so this holds no matter what order
        // OutputState applies the removal in.
        if let Some(current) = self.placed_on.clone() {
            let still_connected = self
                .output_state
                .outputs()
                .filter_map(|o| self.output_state.info(&o))
                .any(|i| i.name.as_deref() == Some(current.as_str()));
            if !still_connected {
                self.placed_on = None;
                self.placed_size = None;
            }
        }

        let Some((output, info)) = self.choose_output() else {
            if self.placed_on.take().is_some() || self.placed_size.take().is_some() {
                eprintln!("cavawall: no usable output, idling until one appears");
            }
            return;
        };
        let name = info.name.clone().unwrap_or_default();
        if self.placed_on.as_deref() == Some(name.as_str()) && self.placed_size == info.logical_size
        {
            return; // already there, same size -- nothing worth rebuilding for
        }
        self.place_on(qh, &output, &info, name);
    }

    /// Build a fresh layer surface on `output` and start drawing to it.
    ///
    /// Moving is always a rebuild: a layer surface belongs to the output it was
    /// created for and there is no request to move one across.
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
        // same empty input region as at startup -- the surface is
        // recreated here, so it would otherwise regain the default one
        let input_region = Region::new(&self.compositor).ok();
        if let Some(r) = &input_region {
            self.layer_surface.set_input_region(Some(r.wl_region()));
        }
        // Only ask for the band the bars can actually reach, anchored to the
        // bottom they grow from.
        //
        // Hyprland damages a layer by its GEOMETRY, not by the buffer damage
        // a client declares -- verified by trying the latter first:
        // eglSwapBuffersWithDamageKHR sent .damage_buffer(0, 377, 1920, 703)
        // 331 times and the damage overlay still showed the whole output.
        // Shrinking the surface moved it immediately. So surface size is the
        // only lever a client has here.
        //
        // max_height caps how far up a full-volume bar goes, as a fraction of
        // the screen, so anything above it is cleared-transparent every frame
        // and recomposited for nothing. With the default 0.65 that is the top
        // 35% of the output.
        //
        // The bar NDC is rescaled to match (see draw) so the bars look
        // identical -- inside a surface that IS the band, they use its full
        // height rather than max_height of it.
        let band = ((self.height as f32 * self.max_height).ceil() as u32).clamp(1, self.height);
        self.layer_surface.set_exclusive_zone(-1); // see note at startup
        self.layer_surface.set_size(self.width, band);
        self.layer_surface.set_anchor(Anchor::BOTTOM);
        self.surface.commit();
        drop(input_region);
        old_surface.destroy();
        // A fresh surface carries no frame callback and nothing parked. The
        // configure this commit provokes is what restarts the loop, and it
        // only draws because placed_on is set here first.
        self.idle = false;
        self.silent_frames = 0;
        self.placed_on = Some(name);
        self.placed_size = info.logical_size;
    }

    pub fn draw(&mut self, _conn: &Connection, qh: &QueueHandle<Self>) {
        let mut cava_buffer: Vec<u8> = vec![0; self.bar_count as usize * 2];
        let mut unpacked_data: Vec<f32> = vec![0.0; self.bar_count as usize];
        if let Err(e) = self.cava_reader.read_exact(&mut cava_buffer) {
            // A signal interrupts the blocking read, which is exactly how we
            // find out it is time to go.
            if EXITING.load(Ordering::SeqCst) {
                self.clear_and_exit(_conn);
            }
            if e.kind() == std::io::ErrorKind::Interrupted {
                return;
            }
            panic!("cava read failed: {e}");
        }
        if EXITING.load(Ordering::SeqCst) {
            self.clear_and_exit(_conn);
        }

        // Drop stale frames and render the newest.
        //
        // cava writes at the configured framerate regardless of whether we are
        // keeping up. Reading exactly one frame per draw means a stall leaves a
        // backlog in the pipe, and on recovery every queued frame is rendered in
        // turn -- the visualiser freezes, then fast-forwards through the audio
        // it missed. Skipping to the newest frame keeps it in step with what is
        // actually playing.
        //
        // The BufReader's own buffer has to be checked as well as the fd: bytes
        // already pulled out of the pipe are invisible to poll(), so polling
        // alone would report "nothing waiting" while a backlog sat in memory.
        let frame_len = cava_buffer.len();
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
            if self.cava_reader.read_exact(&mut cava_buffer).is_err() {
                break;
            }
            skipped += 1;
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
            if debug_enabled() {
                eprintln!("cavawall: parking (silent_frames={})", self.silent_frames);
            }
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

        let bar_width: f32 =
            2.0 / (self.bar_count as f32 + (self.bar_count as f32 - 1.0) * self.bar_gap);
        let bar_gap_width: f32 = bar_width * self.bar_gap;
        let mut vertices: Vec<f32> = vec![0.0; self.bar_count as usize * 8];
        let fwidth: f32 = self.width as f32;
        let fheight: f32 = self.height as f32;
        for i in 0..self.bar_count as usize {
            // NDC space: -1.0 = bottom, +1.0 = top. max_height is NOT applied
            // here any more: the surface has already been sized to that fraction
            // of the screen, so a full-volume bar fills it exactly. Applying it
            // twice would make the bars max_height^2 of the screen -- which is
            // what the first attempt at this looked like, visibly short.
            let bar_height: f32 = 2.0 * unpacked_data[i] - 1.0;
            vertices[i * 8] = bar_gap_width * i as f32 + bar_width * i as f32 - 1.0;
            vertices[i * 8 + 1] = bar_height;
            vertices[i * 8 + 2] = bar_gap_width * i as f32 + bar_width * (i + 1) as f32 - 1.0;
            vertices[i * 8 + 3] = bar_height;
            vertices[i * 8 + 4] = bar_gap_width * i as f32 + bar_width * i as f32 - 1.0;
            vertices[i * 8 + 5] = -1.0;
            vertices[i * 8 + 6] = bar_gap_width * i as f32 + bar_width * (i + 1) as f32 - 1.0;
            vertices[i * 8 + 7] = -1.0;
        }
        unsafe {
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                vertices.as_ptr() as *const _,
                gl::DYNAMIC_DRAW,
            );
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            gl::ClearColor(
                self.background_color[0],
                self.background_color[1],
                self.background_color[2],
                self.background_color[3],
            );
            gl::Clear(gl::COLOR_BUFFER_BIT);
            gl::UseProgram(self.shader_program);
            gl::Uniform2f(self.windows_size_location, fwidth, fheight);
            gl::DrawElements(
                gl::TRIANGLES,
                (self.bar_count as usize * 3 * std::mem::size_of::<u16>()) as gl::types::GLsizei,
                // I don't know why * 3 works here, I thought that it is supposed to be * 6, but it
                // works, so I'll keep it like this for now.
                gl::UNSIGNED_SHORT,
                ptr::null(),
            );
            gl::BindVertexArray(0);
        }
        // Ask for the next callback BEFORE the swap, never after.
        //
        // "The frame request will take effect on the next wl_surface.commit"
        // (wayland.xml) -- it is double-buffered state like a buffer or a
        // damage region, so it needs a commit AFTER it to be applied.
        // eglSwapBuffers is that commit: Mesa attaches the new buffer, adds
        // damage, and commits, all inside the call.
        //
        // Requesting it afterwards instead left the request sitting in pending
        // state with nothing left to apply it, so the loop ran on the callback
        // committed by the PREVIOUS draw -- self-sustaining only once two
        // draws had happened, and dead the moment one draw did not swap (the
        // silence park returns before this point) or the surface holding the
        // in-flight callback was destroyed (new_output does exactly that).
        // Replacing the request with a bare commit() after the swap is worse
        // still: no request is created at all, so the very first draw is the
        // last one, and the trailing bufferless commit re-damages the layer by
        // its geometry for nothing (see the park note above).
        //
        // frame-then-swap is what weston-simple-egl does, and it keeps exactly
        // one callback in flight with no dependence on a second commit.
        self.surface.frame(qh, self.surface.clone());
        egl.swap_buffers(self.egl_display, self.egl_surface)
            .unwrap();
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    // All three funnel into retarget(), which re-derives the answer from the
    // live output list rather than from the event. new_output used to hold the
    // whole policy inline, update_output was a bare alias for it -- so any
    // output property change at all tore the surface down and rebuilt it --
    // and output_destroyed was empty, which left the visualiser stranded on a
    // monitor that had been unplugged.
    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // Held until the startup roundtrip is done; see startup_settled.
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
        // Held until the startup roundtrip is done; see startup_settled.
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
        // Held until the startup roundtrip is done; see startup_settled.
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
    /// The compositor has taken the layer surface away -- normally because its
    /// output was unplugged. Stop drawing to it and re-run the policy: if
    /// another monitor is still connected, retarget() rebuilds there, and if
    /// not it idles until one appears. Previously an empty stub, which left
    /// the loop committing to a surface that no longer had anywhere to go.
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
        // surface to the OLD output's dimensions.
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
        // why this only reproduces on NVIDIA.
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
        unsafe {
            gl::Viewport(0, 0, self.width as GLsizei, self.height as GLsizei);
        }
        // Only draw once a real output has been chosen. main() maps a
        // bootstrap surface purely so EGL has a window to build its context
        // against; drawing to it attaches a buffer and MAPS it, which is how
        // pinning a connector that is not plugged in used to put a 256x256
        // box of bars on whatever output the compositor happened to pick.
        if self.placed_on.is_some() {
            self.draw(_conn, qh);
        }
    }
}
