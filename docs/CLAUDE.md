# For Claude (or any agent working in this repo)

A Wayland layer-shell visualiser that draws cava over the wallpaper. Long-lived
background process: it runs for a whole session at the configured framerate, so
per-frame cost matters and startup cost does not.

## Comments

- **Short. Crucial info only.** Say the point once and stop. A comment that
  restates the code is noise; one that says *why* a line is the way it is, or
  what breaks without it, earns its place.
- **A single `-`, never `--`.** In comments, doc comments, printed output and
  prose files alike. Flags like `--config` are untouched.
- Prefer recording a measurement over an adjective: "8 frames (0.18s)" beats
  "quickly".
- **State what IS, never what WAS.** No history in comments: not what upstream
  did, not what the old form allocated, not "used to be". Git holds that.
- No narration, no asides, no justification of a choice against alternatives
  nobody proposed. If a constraint is real, name the constraint.
- Commit messages are where reasoning and history belong.
- **Abstract, not branded.** A comment names the mechanism, not the product.
  Identifiers, paths and file names in code are exempt.
- **No full stop at the end of a comment.** Interior sentences keep theirs.

## Build and install

```bash
cargo build --release
cargo install --path . --force --root "$HOME/.local"
```

`.cargo/config.toml` asks for `target-cpu=native`: cavawall runs on the machine
that built it. A packager overrides it with `RUSTFLAGS` in the environment,
which wins over the file - the PKGBUILD does exactly that. On the author's
machines the real build is `rust-pgo build cavawall` (PGO + BOLT).

## Testing against a live compositor

**Never start a test instance by hand. Use `scripts/test-instance.sh`.**

```bash
scripts/test-instance.sh my-test.toml bash -c '$CTL status; touch $SILENT; sleep 2; $CTL status'
TEST_ENV=CAVAWALL_DEBUG=1 TEST_LOG=1 scripts/test-instance.sh my-test.toml sleep 5
```

It gives the instance its own runtime dir - its own lock and control socket,
so the session's cavawall is never touched - and `scripts/fake-cava` in place
of cava, so nothing has to play sound and frames are loud and moving on
demand (`touch $SILENT` makes them silent, to test parking). The command runs
with `PID`, `CTL` (cavawallctl aimed at the test socket) and `SILENT` set.

The EXIT/INT/TERM trap stops the instance, its cava and the command, and
removes the runtime dir, however the command ends. **This matters because a
forgotten instance is bars drawn on someone's screen** - it happened, from a
measurement whose last step failed before its manual stop. Anything else you
spawn in a test (a tuner, a browser) needs the same treatment: a trap, and a
check afterwards that nothing is left:

```bash
pgrep -af 'release/cavawal[l]|fake-cav[a]|cavawall-tun[e]'   # must be empty
```

Bracket one letter of every `pgrep -f`/`pkill -f` pattern: an unbracketed one
matches the shell running it and kills your own command.

## Running it

`~/.local/bin/cavawall-launch` is the only thing that should ever start the
session's instance. It holds an flock and kills stale instances first.

**argv must stay exactly `[binary]`.** The launcher, `cavawall-theme.fish` and
`fullscreen-watch` identify this process by an exact argv match. New knobs go
in `config.toml` or an env var (`CAVAWALL_OUTPUT`, `CAVAWALL_DEBUG`), never a
CLI flag. `--config` exists for tests only.

**Stop with SIGTERM or `cavawallctl stop`, never SIGKILL.** The exit path
paints one transparent frame and round-trips; a hard kill leaves the last bars
burnt onto the wallpaper. cava is tied to us with `PR_SET_PDEATHSIG` and is
killed on every exit path.

## The loop

Everything is an event source and the loop sleeps with **no timeout**: cava's
pipe (non-blocking), the inotify watch, the control socket, an eventfd the
signal handler writes, and the Wayland connection. `tick()` runs after every
dispatch.

- **A frame is drawn when both halves are in**: a frame callback has arrived
  (`redraw`) and cava has produced a frame since the last draw (`fresh`).
  Whichever arrives second triggers `draw()`, from `tick()` or `on_cava()`.
- **Never draw inside Wayland dispatch.** A swap reads the Wayland socket and
  queues the next callback, so drawing in the frame handler kept dispatch
  busy for as long as audio played and starved every other source - the
  control socket went unanswered (measured: 4s timeouts). The frame handler
  and configure only set flags.
- **Parking** is decided per frame in `on_cava()`, against the one threshold
  in `is_silent()`: after 23 silent frames nothing is drawn or committed, and
  a loud frame unparks. `general.sleep_timer` makes cava itself sleep in long
  silences, so a parked instance barely wakes.
- **One callback in flight.** `frame_pending` guards it; `place_on` resets it,
  since a new surface carries none.

## Hot path

- **No allocation.** `cava_buffer`, `prev_frame` and `cava_scratch` are
  `Box<[u8]>` sized once from the bar count.
- **Per frame: a 46-byte memcpy into the persistent height ring, one clear,
  one instanced draw, one swap.** The ring is `glBufferStorage` + a coherent
  persistent map (GL 4.4); the base instance picks the slot. A frame that
  would draw the same heights is skipped entirely - no commit.
- **GL state is set once** - program, VAO, blend, textures on units 0 and 1.
  Only `MaskPass::rasterise` binds anything else, and it hands the bar program
  and VAO back.
- **Every mode is one draw call.** Curve occluders are rasterised once per
  configure into an R16UI mask (a bit per occluder, XOR parity fill), and the
  curve fragment stage discards against each bar's mask. There is no stencil
  buffer in the EGL config at all.
- **Optional features are compile-time variants.** `radius` and `reveal` are
  `#define`d into the shaders at startup only when used, so a config without
  them runs exactly the shaders it always did.

## Things that look wrong but are not

- `surface.frame()` goes **before** the swap: the request is double-buffered
  state and needs a commit after it, which the swap provides.
- The park path deliberately does **not** commit. Hyprland damages a layer by
  its geometry on any commit, buffer attached or not.
- The bar count is **startup-only**: it is written into cava's config at exec
  time and sizes the instance buffers. Changing it re-execs (`reexec()`),
  which kills and reaps cava first - exec keeps the PID, so it keeps the
  children.
- The swap interval is set on **every** new EGL surface (`rebind_egl`): it
  belongs to the surface, and a new one is back at 1, which on NVIDIA means
  FIFO - a second commit per frame and a vsync wait inside every swap.
- `place_on` keeps the old `LayerSurface` alive until EGL has moved: dropping
  one destroys its `wl_surface` too, and the EGL window still points there.
- Configure only resizes the `wl_egl_window` and marks a redraw. Hyprland
  sends a new surface two configures back to back, and drawing on the first
  commits a frame at a size the second makes stale.
- The fragment shader indexes `gradient_colors_size - 2` with no lower bound.
  That is safe only because `gradient_buffer()` uploads a lone configured stop
  twice; do not "optimise" that duplication away.

## Before calling it done

```bash
cargo clippy --release --all-targets   # must be silent
cargo test --release
```

Tests cover what the GPU cannot: placement maths, damage rects, the occluder
resolution and triangulation, the QOI decoder, the reveal map. Changes to the
GL or placement paths still need a real run through `scripts/test-instance.sh`
and a `grim` of the result.
