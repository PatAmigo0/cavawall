# cavawall

A Wayland audio visualiser that draws [cava](https://github.com/karlstav/cava)
over your wallpaper.

A fork of [rs-pro0/wallpaper-cava](https://github.com/rs-pro0/wallpaper-cava),
which did all the hard work; this adds bug fixes, packaging fixes, and two
features. Original by [rs-pro0](https://github.com/rs-pro0). MIT, as upstream.

### Bug fixes

- **NVIDIA `EGL_BAD_SURFACE`.** The EGL context was left current on a surface
  that was then destroyed. NVIDIA leaves that in a state where the replacement
  surface fails `eglSwapBuffers` on its first draw, so nothing renders after a
  resize or output change. Mesa tolerates it, which is why it only ever showed
  up on NVIDIA.
- **Pointer input trap.** The layer surface kept the default input region - its
  whole area, so it silently took pointer focus across the screen. It never
  calls `set_cursor`, and a Wayland cursor keeps whatever shape the focused
  surface last asked for, so moving onto an empty workspace left a stale I-beam.
  Now the input region is empty.
- **Frozen frame on exit.** A hard kill left the last painted frame on the
  background. It now catches `SIGTERM`/`SIGINT`, paints one transparent frame
  and round-trips before exiting. Stop it with `SIGTERM`, not `SIGKILL`.
- **Off-centre placement.** The surface used the default exclusive zone of 0,
  which reserves nothing but still places the surface inside the area *other*
  layers have reserved. With a bar present it sat 25px right of the output and
  ran the same 25px off the far edge. It now sets `-1` and ignores exclusive
  zones, which is what a wallpaper wants.

### Packaging fixes

- **`wayland-rs` submodule removed.** Nothing referenced it - no path
  dependency, no `[patch]`, absent from `Cargo.lock`. It cost a 3.4M checkout
  and was pinned to an SSH URL that fails for anyone without push access to
  Smithay, so `git clone --recursive` broke for every new user.
- **`smithay-client-toolkit` pinned to a revision.** Upstream tracked it without
  one, so cargo resolved to whatever HEAD was; HEAD has since moved the delegate
  macros and the build fails. That is what upstream's `--locked` instruction
  works around. A plain `cargo build` is now reproducible.
- **`target/` untracked**, and a release profile (fat LTO, one codegen unit,
  stripped) for a process that runs for a whole session.

### Features

- **`bars.max_height`** caps how tall bars grow, as a fraction of screen height.
- **Parking during silence.** cava emits frames at the configured rate whether
  or not anything is playing, so an idle machine repainted the band 45x/second
  to draw bars that were all zero. After 0.51s of silence this stops committing
  entirely and waits on cava's pipe from the event loop's own timer.

  It parks with *no commit at all*, not a bufferless one: Hyprland damages a
  layer surface by its geometry on any commit, buffer or not, so bufferless
  commits still repainted the whole band.

## Building

Needs [cava](https://github.com/karlstav/cava) at runtime.

```bash
git clone https://github.com/<you>/cavawall
cd cavawall
cargo build --release
./target/release/cavawall
```

No `--recursive`, no `--locked`, no submodules.

## Configuration

`~/.config/cavawall/config.toml`, or `--config <path>`. If that file is absent
but `~/.config/wallpaper-cava/config.toml` exists it is read instead, with a
notice, so switching over from upstream needs no immediate action.

See [`config.toml`](config.toml) for the annotated defaults.

| key | meaning |
|---|---|
| `general.framerate` | frames per second requested from cava |
| `general.background_color` | usually fully transparent |
| `general.preferred_output` | monitor name, e.g. `eDP-1`; omit for the first |
| `bars.amount` | number of bars |
| `bars.gap` | gap width as a fraction of bar width |
| `bars.max_height` | **fork addition**: bar height cap, fraction of screen |
| `general.channels` | **fork addition**: `mono` or `stereo` (see below) |
| `general.mono_option` | **fork addition**: `average`, `left` or `right` |
| `general.audio_source` | **fork addition**: cava input source; omit for cava's default |
| `colors.*` | gradient stops, bottom to top; names are ignored |
| `smoothing.*` | passed straight through to cava |

`CAVAWALL_DEBUG=1` reports per-frame draw activity on stderr.

### Mirrored bars

cava defaults to `channels = stereo`, and stereo does not give each bar its own
frequency band. It splits the bars in half, drawing the **left channel reversed**
across the left half and the right channel across the right half. Since most
music has near-identical channels, the halves come out as mirror images with the
bass meeting in the middle — a symmetric visualiser rather than a spectrum.

Upstream never exposed this, so there was no way to change it. Set:

```toml
[general]
channels = "mono"
```

for one left-to-right sweep across every bar. Unset, cava's default applies.

`CAVAWALL_DEBUG=1` prints the exact config handed to cava, which is otherwise
unobservable — it is written to cava's stdin, so there is no file to check.

### Compositor notes

The layer-shell namespace is `cavawall`. On Hyprland, skip the map animation:

```
layerrule = noanim, cavawall
```

Without it the surface can strand mid-fade at alpha 0 if the shell is recreated
underneath a running instance
