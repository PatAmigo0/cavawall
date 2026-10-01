//! Startup: config resolution through to the running event loop.

use super::*;

fn default_config_path() -> PathBuf {
    let home = PathBuf::from(env::var_os("HOME").unwrap_or_else(|| fatal!("HOME is not set, so there is no config to find; pass --config")));
    let own = home.join(".config/cavawall/config.toml");
    if own.exists() {
        return own;
    }
    // The inherited path is still honoured, so a config left where it was
    // keeps working
    let inherited = home.join(".config/wallpaper-cava/config.toml");
    if inherited.exists() {
        say!(
            "using {}\n\
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
            say!("{}: {e}", path.display());
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
    // Someone else owns the surface. 0, so a launcher reads it as stand-down;
    // a person at a terminal is told what is running and what to type
    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        if unsafe { libc::isatty(2) } == 1 {
            let pid = fs::read_to_string(&path).unwrap_or_default();
            eprintln!(
                "cavawall is already running (pid {}).\n\
                 \n  cavawall status    what it is drawing\
                 \n  cavawall tune      edit this wallpaper's figure\
                 \n  cavawall reload    re-read the config\
                 \n  cavawall stop      clear and exit\
                 \n  cavawall help      everything else",
                pid.trim()
            );
        }
        exit(0);
    }
    say!("cannot lock {}", path.display());
    exit(1);
}

/// Bind the globals a wallpaper surface needs, waiting out a starting compositor
///
/// `registry_queue_init` snapshots the global list, so a late advertisement is
/// invisible until the registry is polled again
fn bind_shell(conn: &Connection) -> (GlobalList, EventQueue<AppState>, CompositorState, LayerShell) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (globals, queue) = registry_queue_init::<AppState>(conn)
            .unwrap_or_else(|e| fatal!("cannot read the compositor's globals: {e}"));
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
                if compositor.is_err() {
                    fatal!("the compositor offers no wl_compositor");
                }
                fatal!("the compositor has no wlr-layer-shell, which cavawall draws with (Hyprland, Sway, river and niri have it; GNOME does not)");
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
            say!("migrated curve {key} to {}", path.display());
        }
    }
}

/// Some wallpaper has a file of its own
fn any_wallpaper_settings(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir.join("wallpapers"))
        .is_ok_and(|entries| entries.flatten().any(|e| e.path().extension().is_some_and(|x| x == "toml")))
}

/// A wallpaper's or a path's own palette with every stop that will not parse
/// dropped, each named; None when nothing is left of it, so it inherits
fn usable_palette(palette: Option<&Palette>, what: &str) -> Option<Vec<ConfigColor>> {
    let stops: Vec<ConfigColor> = palette?
        .0
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            let ok = app_config::try_color_from_hex(c.hex(), 1.0).is_some();
            if !ok {
                say!("{what}: colour {} = {:?} is not a colour; dropped", i + 1, c.hex());
            }
            ok
        })
        .map(|(_, c)| c.clone())
        .collect();
    (!stops.is_empty()).then_some(stops)
}

pub(crate) fn run() {
    dispatch();
    let mut args = env::args_os().skip(1);
    let config_filename = match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(path), None) if flag == "--config" => PathBuf::from(path),
        (None, _, _) => default_config_path(),
        _ => {
            say!("--config takes one path");
            exit(2);
        }
    };
    // Before cava is spawned, so a duplicate costs nothing
    let _instance = claim_single_instance();
    // After the lock: a second instance, which stands down at once, must not
    // rotate the running one's log away from under it
    cavawall::log::init();
    say!("starting, version {}, config {}", env!("CARGO_PKG_VERSION"), config_filename.display());
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

    // A typo in a hand-edited file is the likeliest failure here: said with
    // the file, line and column toml reports, then exit 1, never a panic
    let config_str = fs::read_to_string(&config_filename)
        .unwrap_or_else(|e| fatal!("cannot read {}: {e}", config_filename.display()));
    let config: Config = toml::from_str(&config_str)
        .unwrap_or_else(|e| fatal!("{}: {e}", config_filename.display()));
    cavawall::notify::configure(config.notify.as_ref());
    // Before the first EGL call, which is when the loader reads it. An
    // explicit variable in the environment wins
    if let Some(driver) = config.general.gl_driver.as_deref() {
        let file = match driver {
            "nvidia" => Some("10_nvidia.json"),
            "mesa" => Some("50_mesa.json"),
            other => {
                say!("unknown gl_driver {other:?}: nvidia or mesa; loading every driver");
                None
            }
        };
        let path = file.map(|f| PathBuf::from("/usr/share/glvnd/egl_vendor.d").join(f));
        match path {
            Some(p) if !p.exists() => say!("gl_driver = {driver:?}, but {} is not installed; loading every driver", p.display()),
            // SAFETY: no thread exists yet; the first notify spawns one below
            Some(p) if env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_none() => unsafe { env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", p) },
            _ => {}
        }
    }
    // Where the palette and the wallpaper come from, before either is read
    let scheme = config.scheme.as_ref();
    scheme::configure_colours(scheme.and_then(|s| s.source.as_deref()), scheme.and_then(|s| s.path.as_deref()));
    cavawall::wallpaper::configure(config.wallpaper.as_ref());
    // Taken before any notify, whose waiter thread would make remove_var unsound
    let reexeced = env::var_os("CAVAWALL_REEXEC").is_some();
    if reexeced {
        // SAFETY: no thread exists yet
        unsafe { env::remove_var("CAVAWALL_REEXEC") };
    }
    if let Some(crash) = cavawall::log::previous_crash() {
        say!("the previous instance crashed: {crash}");
        cavawall::notify::send(NotifyEvent::Crash, "the previous run crashed. `cavawall log last-exit` says how");
    }
    if !reexeced {
        cavawall::notify::send(NotifyEvent::Start, "started");
    }
    // Colours are checked here, once, so a typo is a message naming the key
    // rather than a panic wherever the value is first used
    for (key, colour) in config.colors.iter().map(|(k, c)| (format!("colors.{k}"), c)).chain(std::iter::once((
        "general.background_color".to_owned(),
        &config.general.background_color,
    ))) {
        if app_config::try_color_from_hex(colour.hex(), 1.0).is_none() {
            fatal!("{}: {key} = {:?} is not a colour; write it as \"#rrggbb\"", config_filename.display(), colour.hex());
        }
    }
    // The bar count is startup-only: it is written into the spawned cava's
    // config and sizes the instance buffers, so a change re-execs
    let follow_bars = config.scheme.as_ref().and_then(|s| s.bars).unwrap_or(false);
    // One read of the wallpaper answers both questions asked of it: which
    // per-wallpaper file applies, and how the image crops onto the output
    let config_dir = config_filename
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    // Read unconditionally: a per-wallpaper file may choose the figure, so the
    // key is needed before the mode is
    let wallpaper = curve::current_wallpaper().and_then(|w| curve::describe(&w));
    let curve_key = wallpaper.as_ref().map(|(key, _)| key.clone());
    migrate_curves(&config_dir, &config);
    // One wallpaper's typo must not stop the visualiser: said, and skipped
    let per_wallpaper = curve_key.as_ref().and_then(|key| {
        WallpaperConfig::load(&config_dir, key).unwrap_or_else(|e| {
            say!("{e}; drawing config.toml's defaults on this wallpaper");
            None
        })
    });
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
        .or_else(|| config.general.preferred_output.clone());
    let configured_mode = per_wallpaper
        .as_ref()
        .and_then(|w| w.mode)
        .or(config.general.mode)
        .unwrap_or_default();
    // cava's rate, so startup-only like the bar count; cava refuses 0
    let (framerate, framerate_from) = match per_wallpaper.as_ref().and_then(|w| w.framerate) {
        Some(n) => (n.clamp(1, 360), "wallpaper"),
        None => (config.general.framerate, "config"),
    };
    let bars_config = per_wallpaper
        .as_ref()
        .and_then(|w| w.bars.as_ref())
        .map_or_else(|| config.bars.clone(), |o| o.apply(&config.bars));
    let circle_config = per_wallpaper
        .as_ref()
        .and_then(|w| w.circle.as_ref())
        .or(config.circle.as_ref());
    let legacy_curves: HashSet<String> = config.curves.iter().flat_map(|m| m.keys().cloned()).collect();
    let own_settings = per_wallpaper.is_some() || curve_key.as_ref().is_some_and(|k| legacy_curves.contains(k));
    let active_curve = (configured_mode == Mode::Curve)
        .then(|| {
            let key = curve_key.clone()?;
            let found = per_wallpaper
                .as_ref()
                .and_then(|w| w.curve.as_ref())
                .or_else(|| config.curves.as_ref()?.get(&key));
            if found.is_none() && debug_enabled() {
                say!("no curve for wallpaper {key}, falling back to bars");
            }
            found
        })
        .flatten();
    // Most specific first: the figure's own count, then this wallpaper's
    // `[bars] amount`, then the shell's setting, then config.toml. A count
    // set for one wallpaper has to beat the shell's global one, or editing it
    // does nothing while `[scheme] bars` is on
    let figure_bars = match configured_mode {
        Mode::Circle => circle_config.and_then(|c| c.bars).map(|n| (n, "circle")),
        Mode::Curve => active_curve.and_then(CurveConfig::total_bars).map(|n| (n, "curve")),
        Mode::Bars => None,
    };
    let own_bars = figure_bars.or_else(|| {
        per_wallpaper
            .as_ref()
            .and_then(|w| w.bars.as_ref()?.amount)
            .map(|n| (n, "wallpaper"))
    });
    // Watched whenever nothing more specific set the count, even if the shell
    // has none yet, so setting one there takes effect
    let bars_follow_shell = follow_bars && own_bars.is_none();
    let (bar_count, bars_from) = own_bars
        .or_else(|| {
            bars_follow_shell
                .then(scheme::bar_count)
                .flatten()
                .map(|n| (n, "shell"))
        })
        .unwrap_or((config.bars.amount, "config"));
    // Zero divides by zero in the bar-width maths. The ceiling is a sanity
    // bound: 4096 bars is already sub-pixel on any real monitor. Clamped, not
    // asserted: one bad per-wallpaper value must not take the visualiser down
    const MAX_BARS: u32 = 4096;
    if !(1..=MAX_BARS).contains(&bar_count) {
        say!("bar count {bar_count} from {bars_from} is outside 1..={MAX_BARS}, clamping");
    }
    let bar_count = bar_count.clamp(1, MAX_BARS);
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
            framerate,
            bars: bar_count,
            autosens: config.general.autosens,
            sensitivity: config.general.sensitivity,
            sleep_timer: config.general.sleep_timer,
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
        say!("cava config >>>\n{string_cava_config}<<<");
    }
    let mut cmd = Command::new("cava");
    cmd.arg("-p").arg("/dev/stdin");
    // Dies with us, however we die. Blocked writing into a full pipe, cava
    // does not act on SIGTERM, and asleep it writes too rarely to learn the
    // pipe is gone
    // SAFETY: prctl is async-signal-safe and touches only the child
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    // The `Child` is taken apart rather than kept: exec keeps our PID and so
    // keeps this child, so what must survive is the raw pid. reexec() kills and
    // waits before replacing our image, and a cava that dies on its own takes
    // draw() out through clear_and_exit(), after which init collects it
    #[allow(clippy::zombie_processes)]
    let cava_process = cmd
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| fatal!("cannot start cava ({e}); is it installed and on PATH?"));
    // Captured before the field moves below leave `cava_process` partially moved
    // and unusable as a whole. Needed so a re-exec can reap this child: see
    // reexec(), where not having it leaked a zombie per bar-count change
    let cava_pid = cava_process.id();
    let mut cava_stdin = cava_process.stdin.expect("stdin was piped");
    if let Err(e) = cava_stdin.write_all(string_cava_config.as_bytes()) {
        fatal!("cava exited before reading its config ({e}); run cava by hand to see why");
    }
    drop(cava_stdin);
    let cava_stdout = cava_process.stdout.expect("stdout was piped");
    // Non-blocking, and read only when the event loop says it is readable:
    // nothing ever waits on cava, so no frame, signal or order waits either
    let cava_fd = cava_stdout.as_raw_fd();
    // SAFETY: plain fcntl on a descriptor this process owns
    unsafe {
        let flags = libc::fcntl(cava_fd, libc::F_GETFL);
        libc::fcntl(cava_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    // The one failure a person meets by running this outside a Wayland
    // session: a message, not a panic. cava dies with us on exit
    let conn = Connection::connect_to_env()
        .unwrap_or_else(|e| fatal!("cannot reach a Wayland compositor ({e}); is WAYLAND_DISPLAY set?"));
    let (globals, mut event_queue, compositor, layer_shell) = bind_shell(&conn);
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    // WaylandSource is inserted further down, AFTER the output list has been
    // settled with an explicit roundtrip - see the note there
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
    egl.bind_api(egl::OPENGL_API)
        .unwrap_or_else(|e| fatal!("EGL has no desktop OpenGL API ({e}); is the GPU driver's EGL installed?"));
    let egl_display = unsafe { egl.get_display(conn.display().id().as_ptr().cast::<std::ffi::c_void>()) }
        .unwrap_or_else(|| fatal!("EGL has no display for this Wayland connection"));
    egl.initialize(egl_display).unwrap_or_else(|e| fatal!("EGL will not initialise ({e})"));
    // Colour only. Occluders live in a texture filled once per configure, so
    // there is no stencil or depth buffer to allocate, clear or scan out beside
    // every frame
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
        .ok()
        .flatten()
        .unwrap_or_else(|| fatal!("EGL offers no 8-bit RGBA config"));
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
        .unwrap_or_else(|| fatal!("the GPU driver gives no OpenGL 4.3 core context, which cavawall needs"));

    let wl_egl_surface = WlEglSurface::new(surface.id(), 256, 256)
        .unwrap_or_else(|e| fatal!("cannot create the EGL window ({e})"));
    let egl_surface = unsafe {
        egl.create_window_surface(
            egl_display,
            egl_config,
            wl_egl_surface.ptr() as egl::NativeWindowType,
            None,
        )
    }
    .unwrap_or_else(|e| fatal!("cannot create the EGL surface ({e})"));
    egl.make_current(
        egl_display,
        Some(egl_surface),
        Some(egl_surface),
        Some(egl_context),
    )
    .unwrap_or_else(|e| fatal!("cannot make the GL context current ({e})"));
    // The compositor paces this surface with frame callbacks, which is the
    // only pacing a layer surface should have. The default interval of 1 adds
    // the driver's own wait for vsync on top of that, inside every swap.
    // Applies to the surface bound to the current context, so it goes after
    // make_current
    if egl.swap_interval(egl_display, 0).is_err() && debug_enabled() {
        say!("swap interval unchanged, frames pace on vsync too");
    }
    // Null for what the driver lacks: gl asks for every function it knows,
    // and a driver missing one never called must not stop the start
    gl::load_with(|name| egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const std::ffi::c_void));
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

    say!("OpenGL {version}");
    // Immutable buffer storage, and so a persistent mapping, is core from 4.4.
    // The driver decides that, not the build, so it is read from the context
    // that was granted
    let persistent = unsafe {
        let (mut major, mut minor) = (0, 0);
        gl::GetIntegerv(gl::MAJOR_VERSION, &raw mut major);
        gl::GetIntegerv(gl::MINOR_VERSION, &raw mut minor);
        let ok = major > 4 || (major == 4 && minor >= 4);
        if debug_enabled() {
            say!("GL {major}.{minor}, persistent height ring {}", if ok { "on" } else { "off" });
        }
        ok
    };
    say!("EGL {}", egl.version());
    // Resolved once, here, and carried into AppState. draw() then only matches
    // on the stored Option, so a frame costs a null check - no env lookup and
    // no eglGetProcAddress. Reporting the same value that gets used, rather
    // than resolving a second time, keeps one source of truth for it
    let swap_damage = load_swap_with_damage(egl_display);
    if debug_enabled() {
        say!(
            "swap-with-damage {}",
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
    let mut palette_ssbo: u32 = 0;
    let mut occluders: Vec<curve::Occluder> = Vec::new();
    let mut common_mask = 0u16;
    let mut curve_fit = FitMode::default();
    let mut curve_image: Option<(u32, u32)> = None;
    // A curve is authored against ONE wallpaper. If the current one has no
    // entry, fall back to bars rather than draw a ridge traced from a
    // different image - which is the whole point of keying them
    if mode == Mode::Curve && active_curve.is_none() {
        mode = Mode::Bars;
    }
    // The wallpaper's own palette wins over [colors], and each curve path may
    // carry one more. Ordered once and kept: a live re-resolve reuses these
    // exact Vecs rather than walking a HashMap again, which would be free to
    // hand back a different order and silently reshuffle the gradient
    let wallpaper_file = || format!("wallpapers/{}.toml", curve_key.as_deref().unwrap_or_default());
    let own_palette = usable_palette(per_wallpaper.as_ref().and_then(|w| w.colors.as_ref()), &wallpaper_file());
    let palette_from = if own_palette.is_some() { "wallpaper" } else { "config" };
    let mut palettes = vec![own_palette.unwrap_or_else(|| ordered_stops(&config.colors))];
    if palettes[0].is_empty() {
        fatal!("{}: [colors] needs at least one stop to build a gradient from", config_filename.display());
    }
    if let Some(cfg) = active_curve.filter(|_| mode == Mode::Curve) {
        // Every path resolved up front, each with its own geometry, the bits
        // of the occluders that cut it and its palette: the shader never
        // learns that paths exist
        let occlusion = cfg.occlusion();
        let point = |p: &Vec<f32>| curve::Control { x: p[0], y: p[1], scale: 1.0, angle: None };
        occluders = occlusion
            .shapes
            .iter()
            .map(|(pts, shape)| curve::Occluder {
                points: pts.iter().filter(|p| p.len() >= 2).map(point).collect(),
                closed: *shape == OccluderShape::Closed,
            })
            .collect();
        curve_paths = cfg
            .paths()
            .iter()
            .zip(occlusion.masks.iter().copied())
            .enumerate()
            .map(|(i, (path, mask))| {
                let own = path.is_drawn().then(|| usable_palette(path.colors.as_ref(), &format!("{} path {}", wallpaper_file(), i + 1))).flatten();
                let palette = own.map_or(0, |p| {
                    palettes.push(p);
                    (palettes.len() - 1) as u16
                });
                curve::PathSpec::from_config(path, cfg, mask, palette)
            })
            .collect();
        // The occluders every bar tests. Only those may stop the surface
        // short: one that a single path ignores cannot
        common_mask = curve_paths
            .iter()
            .filter(|p| p.controls.len() >= 2)
            .fold(u16::MAX, |m, p| m & p.mask);
        if curve_paths.iter().all(|p| p.controls.len() < 2) {
            common_mask = 0;
        }
        curve_fit = cfg.fit.unwrap_or_default();
        curve_image = wallpaper.as_ref().and_then(|(_, size)| *size);
        if curve_image.is_none() && debug_enabled() {
            say!("wallpaper size unreadable, treating it as output-shaped");
        }
    }
    let path_palettes = palettes.len() > 1;
    let palette_slots = app_config::palette_slots(&palettes.iter().map(Vec::len).collect::<Vec<_>>());
    let circle = CircleGeom::from_config(circle_config);
    // Rounding and the reveal are compiled in only when used, so a config
    // without them runs the exact shaders it always did
    let radius = bars_config.radius.unwrap_or(0.0).clamp(0.0, 0.5);
    let round = radius > 0.0 && mode != Mode::Circle;
    let reveal_mix = bars_config.reveal.unwrap_or(0.0).clamp(0.0, 1.0);
    let reveal_image = curve_key
        .as_deref()
        .filter(|_| reveal_mix > 0.0)
        .and_then(|key| fs::read(config_dir.join("wallpapers").join(format!("{key}.reveal.qoi"))).ok())
        .and_then(|bytes| cavawall::qoi::decode(&bytes));
    if reveal_mix > 0.0 && reveal_image.is_none() {
        say!("reveal is set but wallpapers/<key>.reveal.qoi will not read; drawing the gradient");
    }
    let reveal_size = reveal_image.as_ref().map(|i| (i.width, i.height));
    // Styles likewise: each is a variant, compiled in only when set
    let mirror = bars_config.mirror.unwrap_or(false) && mode != Mode::Circle;
    let blocks = bars_config.blocks.unwrap_or(0).min(256);
    let along_row = bars_config.gradient == Some(GradientAxis::Row);
    let pulse = bars_config.reveal_pulse.unwrap_or(false) && reveal_image.is_some();
    // Finishes that would do nothing per pixel at their defaults are left
    // out: a matte of 0, an opacity of 1, an alpha ramp from 1 to 1, and an
    // occluder test on a curve with no occluders
    let matte = bars_config.matte.unwrap_or(0.0).clamp(0.0, 1.0) > 0.0;
    let opacity = bars_config.opacity.unwrap_or(1.0).clamp(0.0, 1.0) < 1.0;
    let ramp = mode != Mode::Bars && !(circle.inner_alpha >= 1.0 && circle.outer_alpha >= 1.0);
    let occlude = !occluders.is_empty();
    let mut defines = String::new();
    for (on, name) in [
        (round, "ROUND"),
        (reveal_image.is_some(), "REVEAL"),
        (mirror, "MIRROR"),
        (blocks > 0, "BLOCKS"),
        (along_row, "GRADIENT_ROW"),
        (pulse, "REVEAL_PULSE"),
        (matte, "MATTE"),
        (opacity, "OPACITY"),
        (ramp, "RAMP"),
        (occlude, "OCCLUDE"),
        (path_palettes, "PATH_PALETTES"),
    ] {
        if on {
            defines.push_str("#define ");
            defines.push_str(name);
            defines.push('\n');
        }
    }
    let shader_program = build_program(mode, &defines);
    let mut quad_vbo = 0;
    let mut height_vbo = 0;
    let mut vao = 0;
    let mut gradient_colors_ssbo = 0;
    let ring: Option<HeightRing>;
    // Followed only when some stop takes a role from it: a new scheme moves
    // no other
    let follow_colors = config.scheme.as_ref().and_then(|s| s.colors).unwrap_or(false)
        && palettes.iter().flatten().any(ConfigColor::has_role);
    let initial_rgba: Vec<Vec<[f32; 4]>> = {
        let live = if follow_colors { scheme::colours() } else { None };
        palettes.iter().map(|p| resolve_stops(p, live.as_ref())).collect()
    };
    debug_palette(
        if follow_colors { "initial (live)" } else { "initial (static)" },
        &initial_rgba[0],
    );
    if path_palettes && debug_enabled() {
        say!("{} paths draw in palettes of their own", palettes.len() - 1);
    }
    let buffer_data = gradient_buffer(&initial_rgba);
    // One watch over all three files, so a frame costs one read. They are
    // acted on differently: a palette is re-uploaded in place, a bar count or
    // a wallpaper re-execs
    //
    // The wallpaper is watched whenever any wallpaper has settings of its own,
    // whatever this one draws: a per-wallpaper file can pick the figure, so
    // moving onto or off one of them can change everything
    let watch = scheme::Watch::new(
        follow_colors,
        bars_follow_shell,
        !legacy_curves.is_empty() || any_wallpaper_settings(&config_dir),
    );

    // Sized from the bar count, which cannot change without a re-exec, so both
    // are allocated once here rather than on every frame
    // cava's frame IS the vertex buffer: two bytes a bar, straight from the
    // pipe to the GPU with nothing in between
    let cava_buffer = vec![0u8; bar_count as usize * 2].into_boxed_slice();
    let prev_frame = cava_buffer.clone();
    let frame_bytes = std::mem::size_of_val(&*cava_buffer) as GLsizeiptr;
    let (bar_width, bar_stride) = bar_geometry(bar_count, bars_config.gap);
    let bars_at = BarPlacement::from_config(&bars_config);
    let background_color = array_from_config_color(&config.general.background_color);

    unsafe {
        gl::GenVertexArrays(1, &raw mut vao);
        gl::BindVertexArray(vao);
        gl::GenBuffers(1, &raw mut quad_vbo);
        gl::GenBuffers(1, &raw mut height_vbo);
        gl::GenBuffers(1, &raw mut gradient_colors_ssbo);
        gl::BindBuffer(gl::SHADER_STORAGE_BUFFER, gradient_colors_ssbo);
        gl::BufferData(
            gl::SHADER_STORAGE_BUFFER,
            buffer_data.len() as GLsizeiptr,
            buffer_data.as_ptr().cast::<ffi::c_void>(),
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
        ring = if persistent { HeightRing::map(frame_bytes) } else { None };
        if ring.is_none() {
            // A failed mapping leaves immutable storage behind, which cannot
            // be respecified: start over on a fresh name
            gl::DeleteBuffers(1, &raw const height_vbo);
            gl::GenBuffers(1, &raw mut height_vbo);
            gl::BindBuffer(gl::ARRAY_BUFFER, height_vbo);
            gl::BufferData(gl::ARRAY_BUFFER, frame_bytes, std::ptr::null(), gl::DYNAMIC_DRAW);
        }
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
                gl::Uniform1f(
                    gl::GetUniformLocation(shader_program, c"Grow".as_ptr()),
                    if bars_at.down { -1.0 } else { 1.0 },
                );
            }
            Mode::Curve => {
                // Created empty and bound. Bars cannot be built until a surface
                // exists - their normals and their crop both depend on the
                // output's shape - and configure() fills these before any draw
                gl::GenBuffers(1, &raw mut path_ssbo);
                gl::GenBuffers(1, &raw mut width_ssbo);
                if path_palettes {
                    gl::GenBuffers(1, &raw mut palette_ssbo);
                }
                upload_bars(&[], path_ssbo, width_ssbo, (palette_ssbo, &palette_slots));
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
        if round {
            gl::Uniform1f(gl::GetUniformLocation(shader_program, c"Radius".as_ptr()), radius);
        }
        if blocks > 0 {
            gl::Uniform1f(gl::GetUniformLocation(shader_program, c"Blocks".as_ptr()), blocks as f32);
        }
        if along_row {
            gl::Uniform1f(gl::GetUniformLocation(shader_program, c"InvCount".as_ptr()), 1.0 / bar_count as f32);
        }
        // Taken by value: the pixels are freed once GL has its copy, where a
        // local of this function would live as long as the process
        if let Some(img) = reveal_image {
            // Unit 1; unit 0 is the occluder mask's. Filtered, since the image
            // is scaled onto the output; clamped, so the crop never wraps
            let mut texture = 0u32;
            gl::GenTextures(1, &raw mut texture);
            gl::ActiveTexture(gl::TEXTURE1);
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA8 as i32,
                img.width as GLsizei,
                img.height as GLsizei,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                img.rgba.as_ptr().cast(),
            );
            gl::ActiveTexture(gl::TEXTURE0);
            gl::Uniform1i(gl::GetUniformLocation(shader_program, c"Reveal".as_ptr()), 1);
            gl::Uniform1f(gl::GetUniformLocation(shader_program, c"RevealMix".as_ptr()), reveal_mix);
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
        let mean = palette_mean(&initial_rgba[0]);
        gl::Uniform3f(matte_color_location, mean[0], mean[1], mean[2]);
        gl::Uniform1f(matte_location, bars_config.matte.unwrap_or(0.0).clamp(0.0, 1.0));
        gl::Uniform1f(
            gl::GetUniformLocation(shader_program, c"Opacity".as_ptr()),
            bars_config.opacity.unwrap_or(1.0).clamp(0.0, 1.0),
        );
    }
    let aspect_location =
        unsafe { gl::GetUniformLocation(shader_program, c"Aspect".as_ptr()) };
    let surface_px_location =
        unsafe { gl::GetUniformLocation(shader_program, c"SurfacePx".as_ptr()) };
    let output_px_location =
        unsafe { gl::GetUniformLocation(shader_program, c"OutputPx".as_ptr()) };
    let reveal_map_location =
        unsafe { gl::GetUniformLocation(shader_program, c"RevealMap".as_ptr()) };
    let path_scale_location =
        unsafe { gl::GetUniformLocation(shader_program, c"PathScale".as_ptr()) };
    let path_offset_location =
        unsafe { gl::GetUniformLocation(shader_program, c"PathOffset".as_ptr()) };
    // The occluder mask. Curve bars sample it on unit 0 whatever happens, so a
    // curve without occluders still gets one texel of zero: a test that never
    // hides anything, where an unbound sampler would be undefined
    let mask = (mode == Mode::Curve).then(|| {
        // SAFETY: a context is current and nothing else uses texture unit 0
        unsafe {
            let mut texture = 0u32;
            gl::GenTextures(1, &raw mut texture);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, texture);
            // Integer textures cannot be filtered, and a filtered one is
            // incomplete: nearest in both directions, no mipmaps
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::R16UI as i32,
                1,
                1,
                0,
                gl::RED_INTEGER,
                gl::UNSIGNED_SHORT,
                [0u16].as_ptr().cast(),
            );
            gl::Uniform1i(gl::GetUniformLocation(shader_program, c"Occluders".as_ptr()), 0);
            let pass = (!occluders.is_empty()).then(|| MaskPass::new(texture));
            // The pass's constructor bound its own VAO and buffer
            gl::UseProgram(shader_program);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, height_vbo);
            pass
        }
    }).flatten();

    // Opt-in: with the policy at ignore, Hyprland's socket is never opened.
    // The first reading comes before the first placement, so an instance
    // started under a game does not flash onto its monitor first
    let on_fullscreen = config.general.on_fullscreen.unwrap_or_default();
    let hypr_events = (on_fullscreen != FullscreenPolicy::Ignore).then(hypr::events).flatten();
    let covered = hypr_events.as_ref().and_then(|_| hypr::covered()).unwrap_or_default();

    let mut simple_window = AppState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        width: 256,
        height: 256,
        layer_shell,
        layer_surface,
        surface,
        cava_fd,
        // Eight frames: a backlog after a stall drains in few reads
        cava_scratch: vec![0u8; bar_count as usize * 2 * 8].into_boxed_slice(),
        cava_partial: 0,
        fresh: false,
        wl_egl_surface,
        egl_surface,
        egl_config,
        egl_context,
        egl_display,
        height_vbo,
        bar_count,
        cava_pid,
        gradient_colors_ssbo,
        palettes: palettes.into_boxed_slice(),
        palette_slots: palette_slots.into_boxed_slice(),
        palette_ssbo,
        palette_from,
        follow_colors,
        watch,
        prev_frame,
        cava_buffer,
        bar_width,
        bar_stride,
        damage_map: DamageMap::new(bar_count, bar_width, bar_stride, (256, 256), bars_at.down, bars_at.mirror),
        frame_bytes,
        swap_damage,
        force_full_damage: true,
        bars_at,
        mode,
        circle,
        curve_paths: curve_paths.into_boxed_slice(),
        path_ssbo,
        width_ssbo,
        curve_bars: Vec::new(),
        occluders: occluders.into_boxed_slice(),
        common_mask,
        mask,
        curve_horizon: Vec::new(),
        horizon_scratch: Vec::new(),
        mask_tris: Vec::new(),
        curve_fit,
        curve_key,
        own_settings,
        legacy_curves,
        curve_image,
        curve_box: None,
        curve_output: (1, 1),
        aspect_location,
        surface_px_location,
        output_px_location,
        surface_origin: (0, 0),
        reveal_size,
        reveal_map_location,
        ring,
        matte_color_location,
        path_scale_location,
        path_offset_location,
        vao,
        program: shader_program,
        silent_frames: 0,
        background_color,
        config_path: config_filename,
        config_dir,
        bars_from,
        framerate,
        framerate_from,
        bars_follow_shell,
        frame_pending: false,
        redraw: false,
        pinned_output,
        on_fullscreen,
        covered,
        hypr_partial: Vec::new(),
        hypr_reply: Vec::new(),
        hypr_busy: false,
        hypr_again: false,
        loop_handle: loop_handle.clone(),
        cava_stopped: false,
        toplevels: toplevel::Toplevels::default(),
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
    event_queue
        .roundtrip(&mut simple_window)
        .unwrap_or_else(|e| fatal!("the compositor dropped the connection during setup ({e})"));
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
        Err(e) => say!("no control socket: {e}"),
    }

    // Every input is an event source, so the loop sleeps with no timeout and
    // an idle instance wakes only when something happens
    loop_handle
        .insert_source(
            Generic::new(cava_stdout, Interest::READ, CalloopMode::Level),
            |_, _, state: &mut AppState| {
                state.on_cava();
                Ok(PostAction::Continue)
            },
        )
        .unwrap();
    if let Some(fd) = simple_window.watch.as_ref().and_then(scheme::Watch::event_fd) {
        loop_handle
            .insert_source(Generic::new(fd, Interest::READ, CalloopMode::Level), |_, _, state| {
                state.poll_external();
                Ok(PostAction::Continue)
            })
            .unwrap();
    }
    // Elsewhere, the standard protocol: bound only when the policy is on and
    // Hyprland's IPC is not answering it more precisely
    if on_fullscreen != FullscreenPolicy::Ignore && hypr_events.is_none() {
        match globals.bind::<wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, toplevel::ManagerData) {
            Ok(_) => say!("following fullscreen windows through foreign-toplevel"),
            Err(_) => say!("on_fullscreen is set, but the compositor offers neither Hyprland IPC nor foreign-toplevel; ignoring fullscreen windows"),
        }
    }
    if let Some(events) = hypr_events {
        loop_handle
            .insert_source(Generic::new(events, Interest::READ, CalloopMode::Level), |_, events, state: &mut AppState| {
                // SAFETY: the stream is only read, and only here
                Ok(if state.on_hypr(unsafe { events.get_mut() }) { PostAction::Continue } else { PostAction::Remove })
            })
            .unwrap();
    }
    // The signal handler writes here, so SIGTERM wakes a loop that is asleep
    // SAFETY: eventfd returns a fresh descriptor or -1, checked below
    let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake >= 0 {
        WAKE.store(wake, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: just created, and owned by nothing else
        let wake = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(wake) };
        loop_handle
            .insert_source(Generic::new(wake, Interest::READ, CalloopMode::Level), |_, _, state| {
                state.clear_and_exit("SIGTERM or SIGINT")
            })
            .unwrap();
    }
    WaylandSource::new(conn, event_queue)
        .insert(loop_handle)
        .unwrap();
    // Startup is done: config text, shader sources, the curve's dense
    // samples and the driver's setup scratch are freed, and this returns the
    // pages to the kernel instead of keeping them for a run that never
    // allocates again
    // SAFETY: malloc_trim only walks glibc's own free lists
    unsafe { libc::malloc_trim(0) };
    if let Err(e) = event_loop.run(None, &mut simple_window, AppState::tick) {
        // A compositor that restarts or crashes ends up here
        fatal!("the event loop stopped: {e}; the compositor connection most likely closed");
    }
}
