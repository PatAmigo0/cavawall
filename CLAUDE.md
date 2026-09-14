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

## Build and install

```bash
export RUSTFLAGS="-C target-cpu=native"   # EXPORTED, see below
cargo build --release
cargo install --path . --force --root "$HOME/.local"
```

`RUSTFLAGS` must be exported, not set per-command: `cargo install` is a separate
invocation and would otherwise install a binary less optimised than the one just
built. Verified by md5. `dotfiles/cavawall/build.sh` does this; match it.

Never commit a `.cargo/config.toml` with `target-cpu=native`. The AUR PKGBUILD
deliberately omits it so the package runs on any machine, and a config file in
the source dir would silently override that during `makepkg`.

## Running it

`~/.local/bin/cavawall-launch` is the only thing that should ever start this.
It holds an flock and kills stale instances first; starting the binary directly
stacks a second layer surface.

**argv must stay exactly `[binary]`.** The launcher, `cavawall-theme.fish` and
`fullscreen-watch` all identify this process by an exact argv match. New knobs
go in `config.toml` or an env var (`CAVAWALL_OUTPUT`, `CAVAWALL_DEBUG`), never a
CLI flag.

**Stop with SIGTERM, never SIGKILL.** The handler paints one transparent frame
and round-trips; a hard kill leaves the last bars burnt onto the wallpaper,
because Hyprland does not reliably repaint under a layer surface that vanishes.

## Hot path: `draw()` and `poll_resume()`

- **No allocation.** `vertices` and `cava_buffer` are `Box<[T]>` on `AppState`,
  sized from the bar count at startup. Adding a `vec![]` or a `format!` here
  puts an allocation back into every frame.
- **GL state is set once in `main`** - program, vertex array, blend mode, clear
  colour - and `Uniform2f` only in `configure()`. `draw()` binds `ARRAY_BUFFER`
  and nothing else. Anything that binds a vertex array or a program, or leaves
  `ARRAY_BUFFER` pointing elsewhere, breaks that invariant silently.
- **Only the two top corners of each bar move.** The x columns and the bottom
  edge come from `static_vertices()` at startup; do not recompute them per frame.
- **Silence is decided in exactly one place**, `is_silent()`, on the raw u16
  samples. `draw()` parks on it and `poll_resume()` unparks on it; two copies
  drift and the visualiser parks at one threshold and wakes at another.

## Things that look wrong but are not

- `DrawElements` takes an **index count**, not bytes: `bar_count * 6`.
- `surface.frame()` goes **before** `swap_buffers`, not after - the request is
  double-buffered state and needs a commit after it, which the swap provides.
- The park path deliberately does **not** commit. Hyprland damages a layer by
  its geometry on any commit, buffer attached or not.
- The bar count is **startup-only**: it is written into cava's config at exec
  time and baked into the index buffer. Changing it re-execs (`reexec()`), which
  must kill and reap cava first - exec keeps the PID, so it keeps the children.

## Before calling it done

```bash
cargo clippy --release --all-targets   # must be silent
cargo test --release
```

Tests cover what the GPU cannot: the quad layout against the index buffer, the
gap ratio, and the silence boundary against the f32 threshold it replaced.
Changes to the GL or placement paths still need a real run - `cavawall-launch`,
then `grim` the bottom band.
