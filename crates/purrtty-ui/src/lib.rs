//! purrtty-ui — GPU rendering and input handling.
//!
//! Owns the wgpu device/surface, renders a `purrtty_term::Grid` via
//! `glyphon`, and translates keyboard/mouse events into PTY bytes.
//!
//! M1: renders a fixed greeting string. Grid-driven rendering in M4.

#![forbid(unsafe_code)]

mod renderer;

pub use renderer::Renderer;
