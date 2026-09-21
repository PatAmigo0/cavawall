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
            // One fan per silhouette, concatenated. Closed to the bottom edge
            // and filled by parity, so an outline that doubles back is a shape
            // rather than an impossible height-per-x
            let fit = self.fit_for(self.curve_output);
            let mut verts: Vec<[f32; 2]> = Vec::new();
            let mut fans: Vec<(i32, i32)> = Vec::with_capacity(self.curve_occluders.len());
            for outline in &self.curve_occluders {
                let poly = curve::occluder_outline(outline, fit);
                let first = i32::try_from(verts.len()).unwrap_or(0);
                let count = i32::try_from(poly.len()).unwrap_or(0);
                verts.extend_from_slice(&poly);
                fans.push((first, count));
            }
            self.occ_fans = fans.into();
            // SAFETY: a context is current here, and every buffer was created
            // at startup
            unsafe {
                upload_bars(&self.curve_bars, self.path_ssbo, self.width_ssbo);
                gl::BindBuffer(gl::ARRAY_BUFFER, self.stencil_vbo);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    std::mem::size_of_val(verts.as_slice()) as GLsizeiptr,
                    verts.as_ptr().cast(),
                    gl::STATIC_DRAW,
                );
                gl::BindBuffer(gl::ARRAY_BUFFER, self.height_vbo);
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
                let (scale, offset, _occ) = match self.curve_box {
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
                // The fans are authored in the same output NDC as the bars
                gl::UseProgram(self.stencil_program);
                gl::Uniform2f(self.stencil_scale_location, scale[0], scale[1]);
                gl::Uniform2f(self.stencil_offset_location, offset[0], offset[1]);
                gl::UseProgram(self.program);
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

