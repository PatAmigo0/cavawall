# cavawall

A Wayland audio visualiser that draws [cava](https://github.com/karlstav/cava)
over your wallpaper, as a layer-shell surface behind your windows.

A fork of [rs-pro0/wallpaper-cava](https://github.com/rs-pro0/wallpaper-cava),
which did all the hard work. Original by
[rs-pro0](https://github.com/rs-pro0). MIT, as upstream.

## Requirements

- A Wayland compositor with `wlr-layer-shell` (Hyprland, Sway, river, niri)
- OpenGL 4.3, for compute-era SSBOs
- [cava](https://github.com/karlstav/cava) on `PATH`, at runtime

## Install

No `--recursive`, no `--locked`, no submodules.

```bash
git clone https://github.com/PatAmigo0/cavawall
cd cavawall
cargo install --path . --root ~/.local
```

That installs two binaries into `~/.local/bin`: `cavawall` itself and
`cavawall-curve`, the curve editor. Make sure that directory is on your
`PATH`.

For a build tuned to the machine it will run on:

```bash
RUSTFLAGS="-C target-cpu=native" cargo install --path . --root ~/.local
```

Then copy the annotated defaults and start it:

```bash
mkdir -p ~/.config/cavawall
cp config.toml ~/.config/cavawall/
cavawall
```

`install.sh` does all of the above in one step.

Stop it with `SIGTERM`, never `SIGKILL`: it paints one transparent frame on
the way out, and a hard kill leaves the last frame of bars burned onto your
background until something else repaints it.

An AUR package is planned. Until then, build from source.

## Configuration

`~/.config/cavawall/config.toml`, or `--config <path>`. If that file is absent
but `~/.config/wallpaper-cava/config.toml` exists it is read instead, with a
notice, so switching over from upstream needs no immediate action.

See [`config.toml`](config.toml) for the annotated defaults.

| key | meaning |
|---|---|
| `general.mode` | **fork addition**: `bars`, `circle` or `curve` |
| `general.framerate` | frames per second requested from cava |
| `general.background_color` | usually fully transparent |
| `general.preferred_output` | monitor name, e.g. `eDP-1`; omit to pick automatically |
| `general.channels` | **fork addition**: `mono` or `stereo` |
| `general.mono_option` | **fork addition**: `average`, `left` or `right` |
| `general.audio_source` | **fork addition**: cava input source; omit for cava's default |
| `bars.amount` | number of bars |
| `bars.gap` | gap width as a fraction of bar width |
| `bars.max_height` | **fork addition**: bar height cap, fraction of screen |
| `colors.*` | gradient stops, bottom to top; order matters, names do not |
| `smoothing.*` | passed straight through to cava |
| `circle.*` | **fork addition**: circle mode geometry |
| `curves.*` | **fork addition**: curve mode paths, keyed by wallpaper |

Two environment variables, both deliberately not flags: `CAVAWALL_DEBUG=1`
reports placement and configure activity on stderr, and `CAVAWALL_OUTPUT=<name>`
overrides `preferred_output` without touching the config file. `argv` stays
exactly `[binary]`, so supervisors that identify the process by an exact argv
match keep working.

The bar count and the mode are startup-only. Each mode is a separate GL
program with its own uniforms, the count is written into the spawned cava's
config at exec time, and both are baked into the GPU index buffer.

### Choosing a monitor

With `preferred_output` unset and no `CAVAWALL_OUTPUT`, an external monitor
wins over the machine's own panel - anything whose connector is not `eDP*`,
`LVDS*` or `DSI*`. This is re-evaluated on every hotplug, so unplugging the
external monitor moves the bars to the panel and plugging it back in moves
them home.

A name that matches no connected output maps **nothing**. That is deliberate:
falling back to another monitor would put a visualiser somewhere it was
explicitly not asked for.

### Mirrored bars

cava defaults to `channels = stereo`, and stereo does not give each bar its own
frequency band. It splits the bars in half, drawing the **left channel reversed**
across the left half and the right channel across the right half. Since most
music has near-identical channels, the halves come out as mirror images with
the bass meeting in the middle - a symmetric visualiser rather than a spectrum.

Upstream never exposed this. Set:

```toml
[general]
channels = "mono"
```

for one left-to-right sweep across every bar. Unset, cava's default applies.

`CAVAWALL_DEBUG=1` prints the exact config handed to cava, which is otherwise
unobservable: it is written to cava's stdin, so there is no file to check.

### Compositor notes

The layer-shell namespace is `cavawall`. On Hyprland, skip the map animation:

```
layerrule = noanim, cavawall
```

Without it the surface can strand mid-fade at alpha 0 if the shell is
recreated underneath a running instance.

## Modes

### bars

The default, and upstream's only mode: one row along the bottom of the output.
`bars.max_height` caps how tall they grow, and the surface is only that tall,
which is most of what makes this mode cheap.

### circle

Bars radiate from a ring. The surface is **square**, which is what keeps NDC
square and the circle round with no aspect correction, and it is placed by
anchor plus margin rather than centred. The gradient runs radially: the first
stop is the centre, the last is the rim, and `inner_alpha`/`outer_alpha` fade
it across each bar.

On a 1920x1080 output a 520px circle claims 270k pixels against the bar band's
1.3M.

### curve

Bars stand along a path drawn over the wallpaper, leaning onto its normal, so
they can follow a mountain ridge or a skyline. Per control point they carry a
scale, which is what makes a distant stretch of ridge hold shorter and thinner
bars, and an optional angle override.

A curve is keyed by the wallpaper's **content hash**, not its filename: a path
authored for one image means nothing on another, a rename cannot break it, and
a wallpaper with no entry falls back to bars rather than drawing a curve that
belongs to a different picture.

One curve can hold several disconnected paths, each with its own reach, width,
lean and bar count. Bars split between them by arc length unless a path names
its own number.

An `occlude` silhouette discards anything below it, so bars rise from **behind**
a ridge rather than being positioned to look as though they do. It is a 1-D
horizon - for each x, a y below which nothing draws - sampled into a fixed
array, so it costs one lookup per fragment where a polygon test would cost a
loop.

## The curve editor

```bash
cavawall-curve
```

Serves an editor on localhost and prints the URL. It loads the **current**
wallpaper, so what you draw on is what you will see it on, and it writes the
`[curves.<hash>]` block itself: everything else in your config is left byte
for byte as it was.

- **paint** to trace a ridge freehand, or click to place control points
- **snap active list to the edge** finds the edge that is actually in the
  image, inside a corridor around what you drew. It is confined to that
  corridor on purpose: the strongest edge in a wallpaper is usually the
  subject, not the landscape, so an unconstrained search draws the wrong thing
  confidently
- **occluder** mode draws the silhouette bars hide behind
- per-path `height`, `width`, `upright`, `flip` and bar count
- **thin** simplifies a traced line to points you can still edit by hand

It writes on save and leaves itself running, so draw, look, adjust and save
again is the normal loop.

## What this fork changes

### Bug fixes

- **Gradient stops were shuffled on every launch.** The fragment shader reads
  the SSBO as an ordered ramp, mixing stop *i* into *i+1* down the surface, but
  `[colors]` deserialises into a `HashMap` and upstream fed its iteration
  straight to the GPU: arbitrary order, randomised per process. The config's
  claim that the keys "can be named however you like, only the values matter"
  had it exactly backwards. Stops are now ordered by the number at the end of
  the key, covering both `gradient_color_1..8` and `c1..c8`, and putting `c10`
  after `c9` rather than after `c1`.
- **NVIDIA `EGL_BAD_SURFACE`.** The EGL context was left current on a surface
  that was then destroyed. NVIDIA leaves that in a state where the replacement
  surface fails `eglSwapBuffers` on its first draw, so nothing renders after a
  resize or output change. Mesa tolerates it, which is why it only ever showed
  up on NVIDIA.
- **Pointer input trap.** The layer surface kept the default input region, its
  whole area, so it silently took pointer focus across the screen. It never
  calls `set_cursor`, and a Wayland cursor keeps whatever shape the focused
  surface last asked for, so moving onto an empty workspace left a stale I-beam.
  The input region is now empty.
- **Frozen frame on exit.** A hard kill left the last painted frame on the
  background. It now catches `SIGTERM`/`SIGINT`, paints one transparent frame
  and round-trips before exiting.
- **Off-centre placement.** The surface used the default exclusive zone of 0,
  which reserves nothing but still places the surface inside the area *other*
  layers have reserved. With a bar present it sat 25px right of the output and
  ran the same 25px off the far edge. It now sets `-1` and ignores exclusive
  zones, which is what a wallpaper wants.

### Packaging fixes

- **`wayland-rs` submodule removed.** Nothing referenced it: no path
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

- **Three modes** instead of one, and the **curve editor** that authors the
  third. Both are described above.
- **`bars.max_height`**, capping how tall bars grow.
- **Parking during silence.** cava emits frames at the configured rate whether
  or not anything is playing, so an idle machine repainted the band 45x/second
  to draw bars that were all zero. After 0.51s of silence this stops committing
  entirely and waits on cava's pipe from the event loop's own timer. It parks
  with *no commit at all*, not a bufferless one: Hyprland damages a layer
  surface by its geometry on any commit, buffer or not, so bufferless commits
  still repainted the whole band.
- **Surfaces sized to their content.** Each mode asks for the smallest surface
  that can hold what it draws, because Hyprland recomposites a layer by its
  geometry and ignores the damage a client declares. Curve mode maps the
  output's coordinates into the bounding box of every bar at full volume: on a
  1920x1080 output a ridgeline across the upper third is about 300k pixels
  against 2.1M.
- **Following an external palette** (`[scheme]`, optional). Each `[colors]`
  stop may carry a `role` alongside its `hex`; with `[scheme] colors = true`
  those resolve against Caelestia's live scheme, re-resolved in place whenever
  it changes. The SSBO is re-uploaded directly, so there is no restart and no
  config file is ever rewritten, which is the point: the alternative is a
  script generating colour values into a version-controlled file.

  `[scheme] bars = true` takes the bar count from Caelestia's
  `services.visualiserBars`. That one cannot be applied in place, so cavawall
  **re-execs itself** when it changes. `exec` keeps the PID, so the launcher's
  lock, its kill-wait, and any external "is one running" check never observe
  zero or two instances, and an inherited `CAVAWALL_OUTPUT` survives. The
  outgoing cava is killed and reaped first: exec keeps the children too, and an
  unreaped one is a zombie nothing will ever collect.

  Everything here is optional and every failure falls back rather than throws:
  no scheme, a role it lacks, a value that will not parse. Each stop keeps its
  own `hex`, so the static palette is always the floor.
