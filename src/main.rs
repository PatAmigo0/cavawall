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

    let config_str = fs::read_to_string(config_filename).expect("Unable to read config file");
    let config: Config = match toml::from_str(&config_str) {
        Ok(config) => config,
        Err(error) => panic!("Error parsing config: {}", error.message()),
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
    // CAVAWALL_DEBUG=1 shows exactly what cava is being told. cava is spawned
    // with its config on stdin, so there is no file to inspect afterwards and
    // no other way to check a setting actually got through.
    if std::env::var("CAVAWALL_DEBUG").is_ok_and(|v| v != "0") {
        eprintln!("cavawall: cava config >>>\n{string_cava_config}<<<");
    }
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

    let mut indices: Vec<u16> = vec![0; config.bars.amount as usize * 6];
    for i in 0..config.bars.amount as usize {
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
        egl.swap_buffers(self.egl_display, self.egl_surface)
            .unwrap();
        self.surface.frame(qh, self.surface.clone());
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
            let old_surface = self.surface.clone();
            self.surface = self.compositor.create_surface(qh);
            self.layer_surface = self.layer_shell.create_layer_surface(
                qh,
                self.surface.clone(),
                Layer::Bottom,
                Some("cavawall"),
                Some(&output),
            );
            let logical_size = info.logical_size.unwrap();
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
            let band = ((self.height as f32 * self.max_height).ceil() as u32)
                .clamp(1, self.height);
            self.layer_surface.set_exclusive_zone(-1); // see note at startup
            self.layer_surface.set_size(self.width, band);
            self.layer_surface.set_anchor(Anchor::BOTTOM);
            self.surface.commit();
            drop(input_region);
            old_surface.destroy();
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
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {}

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let width = configure.new_size.0;
        let height = configure.new_size.1;
        println!(
            "LayerSurface configure event: width={}, height={}",
            width, height
        );
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
        self.draw(_conn, qh);
        println!("configure finished");
    }
}
