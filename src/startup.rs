//! Startup: config resolution through to the running event loop.

use super::*;

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

/// Hold an exclusive lock for the process lifetime, or stand down
///
/// The lock lives on the descriptor, so the returned file must outlive the
/// process; closing it, including at exit, is what releases it
fn claim_single_instance() -> fs::File {
    let dir = env::var_os("XDG_RUNTIME_DIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let path = dir.join("cavawall.lock");
    let file = match fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cavawall: {}: {e}", path.display());
            exit(1);
        }
    };
    // SAFETY: the descriptor is open and owned for the duration of the call
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // The pid goes in the lock itself, so a wedged instance can still be
        // found when it has stopped answering the socket
        use std::io::Write as _;
        let _ = file.set_len(0);
        let _ = write!(&file, "{}", std::process::id());
        let _ = (&file).flush();
        return file;
    }
    // Someone else owns the surface. 0, so a launcher reads it as stand-down
    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        exit(0);
    }
    eprintln!("cavawall: cannot lock {}", path.display());
    exit(1);
}

/// Bind the globals a wallpaper surface needs, waiting out a starting compositor
///
/// `registry_queue_init` snapshots the global list, so a late advertisement is
/// invisible until the registry is polled again
fn bind_shell(conn: &Connection) -> (GlobalList, EventQueue<AppState>, CompositorState, LayerShell) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (globals, queue) = registry_queue_init::<AppState>(conn).unwrap();
        let qh = queue.handle();
        match (
            CompositorState::bind(&globals, &qh),
            LayerShell::bind(&globals, &qh),
        ) {
            (Ok(compositor), Ok(layer_shell)) => {
                return (globals, queue, compositor, layer_shell)
            }
            _ if Instant::now() < deadline => sleep(Duration::from_millis(50)),
            (compositor, _) => {
                let missing = if compositor.is_err() { "wl_compositor" } else { "layer shell" };
                eprintln!("cavawall: {missing} not available");
                exit(1);
            }
        }
    }
}

/// Config path when no `--config` was given. The inherited path is now built
/// only once the preferred one is ruled out, not unconditionally
/// Give each curve still living in config.toml its own file.
///
/// Writes only what is missing, so it is a no-op once done and safe to repeat.
fn migrate_curves(dir: &std::path::Path, config: &app_config::Config) {
    #[derive(serde::Serialize)]
    struct Migrated<'a> {
        mode: &'static str,
        curve: &'a app_config::CurveConfig,
    }
    let Some(curves) = config.curves.as_ref() else { return };
    for (key, curve) in curves {
        let path = WallpaperConfig::path(dir, key);
        if path.exists() {
            continue;
        }
        let Ok(mut value) = toml::Value::try_from(Migrated { mode: "curve", curve }) else {
            continue;
        };
        app_config::round_floats(&mut value);
        let Ok(body) = toml::to_string(&value) else {
            continue;
        };
        if path.parent().is_some_and(|p| std::fs::create_dir_all(p).is_ok())
            && std::fs::write(&path, body).is_ok()
        {
            eprintln!("cavawall: migrated curve {key} to {}", path.display());
        }
    }
}

/// Wallpaper keys that have a per-wallpaper file.
fn known_wallpapers(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir.join("wallpapers")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "toml").then(|| p.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect()
}

pub(crate) fn run() {
    let mut args = env::args_os().skip(1);
    let config_filename = match (args.next(), args.next(), args.next()) {
        (None, _, _) => default_config_path(),
        (Some(flag), Some(path), None) if flag == "--config" => PathBuf::from(path),
        _ => {
            print_help();
            exit(0);
        }
    };
    // Before cava is spawned, so a duplicate costs nothing
    let _instance = claim_single_instance();
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

    // Resolved here, not at the curve setup below, because the bar count
    // comes out of it. It has to be the curve for the CURRENT wallpaper:
    // HashMap order is undefined, so any other choice is arbitrary
    // One read of the wallpaper answers both questions it is asked: which
    // curve this is, and how the image crops onto the output
    let config_dir = config_filename
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    // Read unconditionally: a per-wallpaper file may choose the figure, so the
    // key is needed before the mode is
    let wallpaper = curve::current_wallpaper().and_then(|w| curve::describe(&w));
    let curve_key = wallpaper.as_ref().map(|(key, _)| key.clone());
    migrate_curves(&config_dir, &config);
    let per_wallpaper = curve_key
        .as_ref()
        .and_then(|key| WallpaperConfig::load(&config_dir, key));
    let configured_mode = per_wallpaper
        .as_ref()
        .and_then(|w| w.mode)
        .or(config.general.mode)
        .unwrap_or_default();
    let bars_config = per_wallpaper
        .as_ref()
        .and_then(|w| w.bars.as_ref())
        .map_or_else(|| config.bars.clone(), |o| o.apply(&config.bars));
    let circle_config = per_wallpaper
        .as_ref()
        .and_then(|w| w.circle.as_ref())
        .or(config.circle.as_ref());
    let curve_keys: HashSet<String> = config
        .curves
        .iter()
        .flat_map(|m| m.keys().cloned())
        .chain(known_wallpapers(&config_dir))
        .collect();
    let active_curve = (configured_mode == Mode::Curve)
        .then(|| {
            let key = curve_key.clone()?;
            let found = per_wallpaper
                .as_ref()
                .and_then(|w| w.curve.as_ref())
                .or_else(|| config.curves.as_ref()?.get(&key));
            if found.is_none() && debug_enabled() {
                eprintln!("cavawall: no curve for wallpaper {key}, falling back to bars");
            }
            found
        })
        .flatten();
    let bar_count = match configured_mode {
        Mode::Circle => circle_config.and_then(|c| c.bars),
        Mode::Curve => active_curve.and_then(CurveConfig::total_bars),
        Mode::Bars => None,
    }
    .unwrap_or(if follow_bars {
        scheme::bar_count().unwrap_or(bars_config.amount)
    } else {
        bars_config.amount
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
    let (globals, mut event_queue, compositor, layer_shell) = bind_shell(&conn);
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    // WaylandSource is inserted further down, AFTER the output list has been
    // settled with an explicit roundtrip - see the note there
    let frame_duration = Duration::from_secs(1) / config.general.framerate;
    let surface = compositor.create_surface(&qh);
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
    // Stencil carries the occluders: a silhouette is filled by parity into its
    // own bit, and bars test it before the fragment shader runs. No config here
    // offers stencil without depth, so a depth buffer comes along unused
    const ATTRIBUTES: [i32; 11] = [
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::STENCIL_SIZE,
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
    let circle = CircleGeom::from_config(circle_config);
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
    let (bar_width, bar_stride) = bar_geometry(bar_count, bars_config.gap);
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
                        clip: path.clip.unwrap_or(true),
                        occlude: path.occlude.as_deref().map(|pts| {
                            pts.iter()
                                .filter(|p| p.len() >= 2)
                                .map(|p| curve::Control {
                                    x: p[0],
                                    y: p[1],
                                    scale: 1.0,
                                    angle: None,
                                })
                                .collect()
                        }),
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
                    step / (1.0 + bars_config.gap) * 0.5,
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
        gl::Uniform1f(matte_location, bars_config.matte.unwrap_or(0.0).clamp(0.0, 1.0));
        gl::Uniform1f(
            gl::GetUniformLocation(shader_program, c"Opacity".as_ptr()),
            bars_config.opacity.unwrap_or(1.0).clamp(0.0, 1.0),
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
    let instance_offset_location =
        unsafe { gl::GetUniformLocation(shader_program, c"InstanceOffset".as_ptr()) };
    // Its own VAO: the main one carries the quad and the per-bar heights, and
    // a fan wants neither
    let (stencil_program, stencil_scale_location, stencil_offset_location, stencil_vbo, stencil_vao) = unsafe {
        let program = link_program(STENCIL_VERTEX_SHADER_SRC, STENCIL_FRAGMENT_SHADER_SRC);
        let scale = gl::GetUniformLocation(program, c"PathScale".as_ptr());
        let offset = gl::GetUniformLocation(program, c"PathOffset".as_ptr());
        let (mut fan_vbo, mut fan_vao) = (0u32, 0u32);
        gl::GenBuffers(1, &mut fan_vbo);
        gl::GenVertexArrays(1, &mut fan_vao);
        gl::BindVertexArray(fan_vao);
        gl::BindBuffer(gl::ARRAY_BUFFER, fan_vbo);
        gl::EnableVertexAttribArray(0);
        gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 8, std::ptr::null());
        // Back to the one the draw loop assumes is bound: this program sets GL
        // state once at startup and never rebinds per frame
        gl::BindVertexArray(vao);
        gl::BindBuffer(gl::ARRAY_BUFFER, height_vbo);
        (program, scale, offset, fan_vbo, fan_vao)
    };
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
        max_height: bars_config.max_height.unwrap_or(1.0),
        mode,
        circle,
        curve_paths: curve_paths.into_boxed_slice(),
        path_ssbo,
        width_ssbo,
        curve_bars: Box::new([]),
        curve_occlude: curve_occlude.into_boxed_slice(),
        curve_draws: Box::new([]),
        curve_occluders: Box::new([]),
        curve_horizon: Box::new([]),
        curve_fit,
        curve_key,
        curve_keys,
        curve_image,
        curve_box: None,
        curve_output: (1, 1),
        resolution_location,
        dsa,
        matte_color_location,
        path_scale_location,
        path_offset_location,
        vao,
        program: shader_program,
        instance_offset_location,
        stencil_program,
        stencil_scale_location,
        stencil_offset_location,
        stencil_vbo,
        stencil_vao,
        occ_fans: Box::new([]),
        silent_frames: 0,
        background_color,
        config_path: config_filename.clone(),
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

    match control::bind() {
        Ok(listener) => {
            loop_handle
                .insert_source(
                    Generic::new(listener, Interest::READ, CalloopMode::Level),
                    |_, listener, state: &mut AppState| {
                        while let Ok((stream, _)) = listener.accept() {
                            state.serve_control(&stream);
                        }
                        Ok(PostAction::Continue)
                    },
                )
                .unwrap();
        }
        Err(e) => eprintln!("cavawall: no control socket: {e}"),
    }

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .unwrap();
    event_loop
        .run(frame_duration, &mut simple_window, |state| state.poll_resume())
        .unwrap();
}
