# purrtty

A Warp-inspired, GPU-accelerated terminal emulator for macOS. Written in Rust.

> Status: **v0.1 / M0 scaffolding** — not usable yet.

## Goals (v0.1)

- Native macOS GUI window (no Electron, no web tech)
- GPU rendering via `wgpu` (Metal backend)
- Basic VT/xterm emulation that can host `zsh`, `vim`, `htop`
- Clean layered architecture so Warp-style block/AI features can layer on later

Features like command blocks, tabs, splits, AI, and command palette are **v2+**.
See [docs/plan.md](docs/plan.md) for the full plan.

## Layout

```
crates/
  purrtty-term/    pure domain — grid, cells, VT parser
  purrtty-pty/     PTY spawn + IO
  purrtty-ui/      wgpu renderer + input
  purrtty-app/     winit event loop, binary
```

## Build & Run

Requires Rust stable (see `rust-toolchain.toml`).

```sh
cargo run -p purrtty-app
```

## License

Dual-licensed under MIT or Apache-2.0 at your option.
