//! Wayland protocol handlers for `AppState`.

use super::*;

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

delegate_dispatch2!(AppState);
delegate_registry!(AppState);

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

    /// Only records that a frame is owed; `tick()` draws it after dispatch
    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // A callback from a surface already replaced has nothing to draw
        if surface != &self.surface {
            return;
        }
        self.frame_pending = false;
        self.redraw = true;
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
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Ignore a configure for a surface we have already replaced.
        // place_on() builds a new surface on every move, so a late configure
        // for the old one would otherwise resize the NEW surface's EGL
        // surface to the OLD output's dimensions
        if layer.wl_surface() != &self.surface {
            return;
        }
        // Zero on an axis leaves the size to the client; every surface here
        // asks for an explicit one, so keep what is already there
        let width = if configure.new_size.0 > 0 { configure.new_size.0 } else { self.width };
        let height = if configure.new_size.1 > 0 { configure.new_size.1 } else { self.height };
        if debug_enabled() {
            say!("configure {width}x{height} on {:?}", self.placed_on);
        }
        self.width = width;
        self.height = height;
        // Resized, not rebuilt: the EGL surface already belongs to this
        // wl_surface, and the next swap allocates at the new size
        self.wl_egl_surface.resize(width as i32, height as i32, 0, 0);
        // The compositor holds nothing at the new size: the first frame after
        // this has to be whole
        self.force_full_damage = true;
        // Bars and occluders are rebuilt in place_on, where the output is
        // chosen; a configure only has to put them on this surface
        let curve = self.mode == Mode::Curve && !self.curve_bars.is_empty();
        if curve {
            // SAFETY: a context is current here, and both buffers were
            // created at startup
            unsafe { upload_bars(&self.curve_bars, self.path_ssbo, self.width_ssbo) };
        }
        // The only moment the bar-to-pixel mapping can change
        self.damage_map = DamageMap::new(
            self.bar_count,
            self.bar_width,
            self.bar_stride,
            (self.width, self.height),
            self.bars_at.down,
            self.bars_at.mirror,
        );
        // SAFETY: a context is current; every name here was created at startup
        unsafe {
            // Size-dependent uniforms, set only here: the bar program is bound
            // at startup and never unbound
            if curve {
                let (ow, oh) = (self.curve_output.0 as f32, self.curve_output.1 as f32);
                // The aspect and its reciprocal: the shader multiplies by both rather
                // than divide by one
                let aspect = ow / oh.max(1.0);
                gl::Uniform2f(self.aspect_location, aspect, 1.0 / aspect);
                // Identity when the surface IS the output. The compositor is
                // free to grant a size other than the one asked for, so the map
                // is built from what this configure granted
                let (scale, offset, _) = match self.curve_box {
                    None => ([1.0, 1.0], [0.0, 0.0], [0.0, 0.0, 1.0, 1.0]),
                    Some((left, top, _, _)) => {
                        surface_map((ow as u32, oh as u32), (left, top, self.width, self.height))
                    }
                };
                gl::Uniform2f(self.path_scale_location, scale[0], scale[1]);
                gl::Uniform2f(self.path_offset_location, offset[0], offset[1]);
                if let Some(mask) = &self.mask {
                    let fit = self.fit_for(self.curve_output);
                    curve::occluder_triangles_into(&mut self.mask_tris, &self.occluders, fit);
                    mask.rasterise(
                        &self.mask_tris,
                        (self.width, self.height),
                        (scale, offset),
                        (self.program, self.vao),
                    );
                }
            }
            let output = self.placed_size.map_or((self.width, self.height), |(w, h)| {
                (w.max(1) as u32, h.max(1) as u32)
            });
            gl::Uniform2f(self.surface_px_location, self.width as f32, self.height as f32);
            gl::Uniform2f(self.output_px_location, output.0 as f32, output.1 as f32);
            if let Some(image) = self.reveal_size {
                let m = reveal_map(image, output, self.surface_origin, self.height);
                gl::Uniform4f(self.reveal_map_location, m[0], m[1], m[2], m[3]);
            }
            // After the mask pass, which sets its own
            gl::Viewport(0, 0, self.width as GLsizei, self.height as GLsizei);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.height_vbo);
        }
        // Only draw once a real output has been chosen. main() maps a
        // bootstrap surface purely so EGL has a window to build its context
        // against; drawing to it attaches a buffer and MAPS it, putting a
        // 256x256 box of bars on whatever output the compositor picked
        //
        // Owed rather than drawn here: Hyprland sends a new surface two
        // configures back to back (the usable area, then the size asked for),
        // and drawing on the first commits a full-size frame of bars that the
        // second makes stale. tick() draws once the whole batch is in
        if self.placed_on.is_some() {
            self.idle = false;
            self.silent_frames = 0;
            self.redraw = true;
        }
    }
}

