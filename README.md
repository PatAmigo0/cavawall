# cavawall

A Wayland audio visualiser that draws [cava](https://github.com/karlstav/cava)
over your wallpaper, as a layer-shell surface behind your windows.

https://github.com/user-attachments/assets/704e83af-b01e-4eb2-801c-aa29ee735d7c

A fork of [rs-pro0/wallpaper-cava](https://github.com/rs-pro0/wallpaper-cava),
which did all the hard work. Original by
[rs-pro0](https://github.com/rs-pro0).

## License

GPL-3.0-or-later ([`LICENSE`](LICENSE)). Use it, change it, sell it; ship the
source under the same terms if you pass it on, so it cannot be folded into
something closed.

Upstream was MIT and its notice is kept in [`LICENSE.MIT`](LICENSE.MIT),
covering the code inherited from it. Releases of this repository up to and
including `61be695` were published under MIT and stay available under those
terms.

## Requirements

- A Wayland compositor with `wlr-layer-shell` (Hyprland, Sway, river, niri)
- OpenGL 4.3 for SSBOs; 4.5 and newer also gets direct state access
- [cava](https://github.com/karlstav/cava) on `PATH`, at runtime

## Install

```bash
git clone https://github.com/PatAmigo0/cavawall
cd cavawall
cargo install --path . --root ~/.local
```

every build from this checkout targets the CPU it is built on: `.cargo/config.toml`
sets `-C target-cpu=native`, since cavawall runs on the machine that compiled it
and there is no reason to emit a 2003 baseline and hope. Packagers who need a
portable binary set `RUSTFLAGS` in the environment, which takes precedence.

That installs 3 binaries into `~/.local/bin`: `cavawall` itself, cavawallctl, and
`cavawall-tune`, the editor. Make sure that directory is on your
`PATH`.

Then copy the annotated defaults and start it:

```bash
mkdir -p ~/.config/cavawall
cp config.toml ~/.config/cavawall/
cavawall
```

`install.sh` does all of the above in one step.

an AUR package is planned. until then, build from source

## Configuration

`~/.config/cavawall/config.toml`, or `--config <path>`. If that file is absent
but `~/.config/wallpaper-cava/config.toml` exists it is read instead, with a
notice, so switching over from upstream needs no immediate action

See [`config.toml`](config.toml) for the annotated defaults

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
| `bars.opacity` | **fork addition**: alpha multiplier, every mode |
| `bars.matte` | **fork addition**: flatten the gradient toward its own mean |
| `colors.*` | gradient stops, bottom to top; order matters, names do not |
| `smoothing.*` | passed straight through to cava |
| `circle.*` | **fork addition**: circle mode geometry |
| `curves.*` | **fork addition**: curve mode paths, keyed by wallpaper |

two environment variables: `CAVAWALL_DEBUG=1`
reports placement and configure activity on stderr, and `CAVAWALL_OUTPUT=<name>`
overrides `preferred_output` without touching the config file. `argv` stays
exactly `[binary]`, so supervisors that identify the process by an exact argv
match keep working

The bar count and the mode are startup-only. Each mode is a separate GL
program with its own uniforms, the count is written into the spawned cava's
config at exec time, and both are baked into the GPU index buffer

### Choosing a monitor

With `preferred_output` unset and no `CAVAWALL_OUTPUT`, an external monitor
wins over the machine's own panel - anything whose connector is not `eDP*`,
`LVDS*` or `DSI*`. This is re-evaluated on every hotplug, so unplugging the
external monitor moves the bars to the panel and plugging it back in moves
them home.

a name that matches no connected output maps **nothing**. That is deliberate:
falling back to another monitor would put a visualiser somewhere it was
explicitly not asked for

### Mirrored bars

cava defaults to `channels = stereo` and stereo doesn't give each bar its own
frequency band. It splits the bars in half, drawing the **left channel reversed**
across the left half and the right channel across the right half. Since most
music has near-identical channels, the halves come out as mirror images with
the bass meeting in the middle - a symmetric visualiser rather than a spectrum

change:

```toml
[general]
channels = "mono"
```

for one left-to-right sweep across every bar. Unset, cava's default applies.

`CAVAWALL_DEBUG=1` prints the exact config handed to cava, which is otherwise
unobservable: it is written to cava's stdin

### Compositor notes

The layer-shell namespace is `cavawall`. On Hyprland, skip the map animation:

```
layerrule = noanim, cavawall
```

Without it the surface can strand mid-fade at alpha 0 if the shell is
recreated underneath a running instance.

## Modes

### bars

The default: one row along the bottom of the output.
`bars.max_height` caps how tall they grow

### circle

Bars radiate from a ring. The surface is **square**, which is what keeps NDC
square and the circle round with no aspect correction, and it is placed by
anchor plus margin rather than centred. The gradient runs radially: the first
stop is the centre, the last is the rim, and `inner_alpha`/`outer_alpha` fade
it across each bar

### curve

Bars stand along a path drawn over the wallpaper, leaning onto its normal, so
they can follow a mountain ridge, a skyline, etc. Per control point they carry a
scale, which is what makes a distant stretch of ridge hold shorter and thinner
bars, and an optional angle override.

Points are authored on the **image**, and cavawall maps them onto the output
the same way the wallpaper itself is laid down: scaled to cover, centre-cropped.
So a curve drawn on one monitor lands on the same ridge on a monitor of a
different shape, rather than beside it. set `fit = "stretch"` for a daemon that
stretches instead

A curve is keyed by the wallpaper's **content hash**: a path
authored for one image means nothing on another, a rename cannot break it, and
a wallpaper with no entry falls back to bars instead of drawing a curve that
belongs to a different picture

one curve can hold several disconnected paths, each with its own reach, width,
lean and bar count. Bars split between them by arc length unless a path names
its own number

An `occlude` silhouette discards anything in its area
## The curve editor

```bash
cavawall-tune
```

Serves an editor on localhost and prints the URL. It loads the **current**
wallpaper, so what you draw on is what you will see it on, and it writes the
`[curves.<hash>]` block itself: everything else in your config is left byte
for byte as it was

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
again is the normal loop
