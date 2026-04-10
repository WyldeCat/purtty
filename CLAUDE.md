# purrtty — Developer Guide

## Project overview

GPU-accelerated terminal emulator for macOS with an embedded Claude
coding agent. Written in Rust. Uses wgpu (Metal) for rendering, font-kit
(Core Text) for glyph rasterization, and the Claude CLI for agent
integration.

## Workspace layout

```
crates/
  purrtty-term/    VT parser + grid model (pure logic, no GPU)
  purrtty-pty/     PTY session (portable-pty wrapper)
  purrtty-ui/      wgpu renderer, glyph cache, quad pipeline, theme
  purrtty-app/     winit event loop, key mapping, config, agent runner
```

## Build & run

```sh
cargo run -p purrtty-app           # debug
cargo run -p purrtty-app --release # release (smoother)
cargo run --bin headless           # headless grid dump (no GUI)
```

## Testing

Run **all tests** before every commit:

```sh
# Unit tests (33 tests, instant)
cargo test -p purrtty-term

# Integration smoke tests (16 tests, ~3s, spawns real PTY + font checks)
cargo test -p purrtty-app --test smoke

# Both at once
cargo test -p purrtty-term && cargo test -p purrtty-app --test smoke
```

### What the tests cover

**purrtty-term unit tests (33):**
- Basic text, CRLF, backspace, tab, line wrap, scrolling
- Cursor motion (CUP, CUU/CUD/CUF/CUB, CHA, VPA)
- Erase (ED, EL), line ops (IL/DL), char ops (ICH/DCH/ECH)
- SGR (colors, attrs, 256-color, truecolor, reset)
- Wide chars (CJK 2-cell advance, wrap)
- Scroll region (DECSTBM + line feed within region)
- Cursor save/restore (DECSC/DECRC)
- Alt screen (enter/leave, buffer swap, no scrollback)
- DEC mode 25 (cursor visibility)
- Reverse index (RI at top)
- OSC 7 (cwd parsing, percent-decode)

**Smoke tests (16):**
- Shell prompt appears after PTY spawn
- `ls` produces output in the grid
- `echo` text appears in the grid
- Cursor positioned after prompt
- DA1 response queued on `\e[c`
- DSR/CPR response correct on `\e[6n`
- OSC 7 sets cwd
- Alt screen round-trip preserves primary buffer
- Wide char cursor advance
- Korean echo via real PTY (`echo 안녕하세요`)
- Korean mixed with ASCII (cell-by-cell + WIDE_CONT)
- Korean backspace preserves prompt
- Korean line wrap at grid boundary
- Font: ASCII glyph coverage (primary font)
- Font: Korean glyph coverage (primary + fallbacks)
- Font: Box-drawing / symbol glyph coverage

## Pre-commit checklist

1. `cargo check --workspace` — no errors
2. `cargo test -p purrtty-term` — 33/33 pass
3. `cargo test -p purrtty-app --test smoke` — 9/9 pass
4. `cargo run --bin headless` — prompt appears in grid dump

## Key patterns

- **Grid is pure logic** — `purrtty-term` has zero GPU/windowing deps.
  All VT behavior is unit-testable.
- **Renderer is cell-by-cell** — each grid cell becomes a textured quad
  from a glyph atlas (Core Text on macOS). No paragraph layout engine.
- **Agent uses CLI** — `claude -p "<prompt>" --output-format stream-json`
  with structured event parsing. Not API-direct.
- **Terminal responses** — DA/DSR queries are queued in `Grid::response_queue`
  and drained by the app layer into the PTY.

## Config

`~/.config/purrtty/config.toml` — see `config.example.toml` for schema.
Supports window size, font family/size, and dark/light color scheme.
