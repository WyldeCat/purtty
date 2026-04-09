# purrtty

A Warp-inspired, GPU-accelerated terminal emulator for macOS. Written in Rust.

> Status: **v0.1** — usable for everyday shell work, vim, htop, less.
> See [docs/plan.md](docs/plan.md) for what's done and what's next.

## What works today

- Native macOS GUI window via `winit` + `wgpu` (Metal backend)
- Spawns `$SHELL` (defaults to `zsh`) in a real PTY
- VT/xterm emulation: cursor motion, scroll regions, alt screen,
  cursor save/restore, IL/DL/ICH/DCH/SU/SD, SGR colors (16/256/truecolor),
  reverse video, wide characters (CJK/emoji)
- 10k-row scrollback with mouse-wheel scrolling
- Foreground colors and per-cell backgrounds via a single `glyphon`
  buffer + a small wgpu quad pipeline
- Block cursor, modifier keys (Ctrl + letter, Alt prefix, Cmd swallowed)
- Korean IME commit (preedit overlay still TODO)
- TOML config (`~/.config/purrtty/config.toml`) for window size, font,
  and a built-in dark / light color scheme

## What's not in v0.1

- Command blocks, tabs, splits, command palette (Warp signature features)
- AI integration
- Per-cell glyph positioning (CJK alignment is approximate)
- IME preedit overlay
- Cursor blink, copy/paste, font zoom
- Custom (non-builtin) color schemes
- Code signing / notarized release

These are tracked in [docs/plan.md](docs/plan.md) and [docs/perf.md](docs/perf.md).

## Layout

```
crates/
  purrtty-term/    pure domain — grid, cells, VT parser (31 unit tests)
  purrtty-pty/     PTY spawn + reader thread
  purrtty-ui/      wgpu renderer + glyphon text + quad pipeline + theme
  purrtty-app/     winit event loop, key mapping, config loader
```

## Build & Run

Requires Rust stable (see `rust-toolchain.toml`).

```sh
cargo run -p purrtty-app           # debug build
cargo run -p purrtty-app --release # release build (smoother)
```

The binary name is `purrtty`.

## Configuration

purrtty looks for `~/.config/purrtty/config.toml` at startup. A missing
file just yields the defaults. See [`config.example.toml`](config.example.toml)
for the full schema; here's the gist:

```toml
[window]
width  = 960
height = 600

[font]
# family    = "JetBrains Mono"   # optional, falls back to system monospace
size        = 18.0
line_height = 22.0

[colors]
scheme = "dark"   # or "light"
```

## Tests

```sh
cargo test -p purrtty-term   # 31 unit tests for the VT parser / grid
cargo check --workspace      # compiles everything
```

## License

Dual-licensed under MIT or Apache-2.0 at your option.
