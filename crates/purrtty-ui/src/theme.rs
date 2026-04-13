//! Color theme for the renderer.
//!
//! A `Theme` carries a default foreground, a surface background, and the
//! 16-entry ANSI palette. The 256-color cube (16..=231) and grayscale
//! ramp (232..=255) are computed deterministically and stay outside the
//! theme so users only have to override the colors that actually differ
//! between schemes.

use glyphon::Color as GlyphColor;

/// Solid background color stored as RGBA in `[0,1]` so it can flow into
/// both `wgpu::Color` (clear color) and the quad pipeline.
#[derive(Debug, Clone, Copy)]
pub struct ThemeBg {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl ThemeBg {
    /// Build a background color from sRGB bytes. The stored value is
    /// converted to linear space because the wgpu surface we render
    /// into is sRGB-encoded — wgpu expects clear colors and fragment
    /// outputs in linear space and handles the sRGB encoding itself.
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: srgb_to_linear(r as f32 / 255.0),
            g: srgb_to_linear(g as f32 / 255.0),
            b: srgb_to_linear(b as f32 / 255.0),
            a: 1.0,
        }
    }

    pub fn as_wgpu(self) -> wgpu::Color {
        wgpu::Color {
            r: self.r as f64,
            g: self.g as f64,
            b: self.b as f64,
            a: self.a as f64,
        }
    }

    pub fn as_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

/// Convert one sRGB channel value in `[0, 1]` to linear space using the
/// standard sRGB transfer function.
pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// A complete color scheme.
#[derive(Debug, Clone)]
pub struct Theme {
    pub foreground: GlyphColor,
    pub background: ThemeBg,
    /// ANSI 0..=15. Indices 0..=7 are normal, 8..=15 are bright.
    pub palette: [GlyphColor; 16],
}

impl Theme {
    /// VS Code Dark+ — the default dark theme shipped with VS Code.
    pub fn dark() -> Self {
        Self {
            // #cccccc — VS Code's `editor.foreground` default.
            foreground: GlyphColor::rgb(204, 204, 204),
            // #1e1e1e — VS Code's `editor.background` default.
            background: ThemeBg::rgb(30, 30, 30),
            palette: [
                GlyphColor::rgb(0, 0, 0),         // 000000 black
                GlyphColor::rgb(205, 49, 49),     // cd3131 red
                GlyphColor::rgb(13, 188, 121),    // 0dbc79 green
                GlyphColor::rgb(229, 229, 16),    // e5e510 yellow
                GlyphColor::rgb(36, 114, 200),    // 2472c8 blue
                GlyphColor::rgb(188, 63, 188),    // bc3fbc magenta
                GlyphColor::rgb(17, 168, 205),    // 11a8cd cyan
                GlyphColor::rgb(229, 229, 229),   // e5e5e5 white
                GlyphColor::rgb(102, 102, 102),   // 666666 bright black
                GlyphColor::rgb(241, 76, 76),     // f14c4c bright red
                GlyphColor::rgb(35, 209, 139),    // 23d18b bright green
                GlyphColor::rgb(245, 245, 67),    // f5f543 bright yellow
                GlyphColor::rgb(59, 142, 234),    // 3b8eea bright blue
                GlyphColor::rgb(214, 112, 214),   // d670d6 bright magenta
                GlyphColor::rgb(41, 184, 219),    // 29b8db bright cyan
                GlyphColor::rgb(229, 229, 229),   // e5e5e5 bright white
            ],
        }
    }

    /// Solarized-ish light theme.
    pub fn light() -> Self {
        Self {
            foreground: GlyphColor::rgb(40, 40, 40),
            background: ThemeBg::rgb(253, 246, 227),
            palette: [
                GlyphColor::rgb(7, 54, 66),
                GlyphColor::rgb(220, 50, 47),
                GlyphColor::rgb(133, 153, 0),
                GlyphColor::rgb(181, 137, 0),
                GlyphColor::rgb(38, 139, 210),
                GlyphColor::rgb(211, 54, 130),
                GlyphColor::rgb(42, 161, 152),
                GlyphColor::rgb(238, 232, 213),
                GlyphColor::rgb(0, 43, 54),
                GlyphColor::rgb(203, 75, 22),
                GlyphColor::rgb(88, 110, 117),
                GlyphColor::rgb(101, 123, 131),
                GlyphColor::rgb(131, 148, 150),
                GlyphColor::rgb(108, 113, 196),
                GlyphColor::rgb(147, 161, 161),
                GlyphColor::rgb(253, 246, 227),
            ],
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// What the app passes to `Renderer::new` — font + theme together.
#[derive(Debug, Clone)]
pub struct RendererConfig {
    /// Specific monospace font family name (e.g. `"Menlo"`,
    /// `"JetBrains Mono"`). `None` falls back to the system's generic
    /// monospace face.
    pub font_family: Option<String>,
    /// Font size in physical pixels.
    pub font_size: f32,
    /// Line height in physical pixels. Should be slightly larger than
    /// `font_size` (≈ 1.2 ×).
    pub line_height: f32,
    pub theme: Theme,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: 20.0,
            line_height: 24.0,
            theme: Theme::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The surface format we use is sRGB, so wgpu interprets clear
    /// colors and fragment output as LINEAR values. If we feed raw
    /// sRGB values (e.g. `30 / 255.0` for #1e1e1e), wgpu treats them
    /// as linear intensities and re-encodes them on write, producing
    /// a much lighter display color than we intended.
    ///
    /// This test pins the expected behavior: `ThemeBg::rgb(30, 30, 30)`
    /// must produce a linear color whose sRGB re-encoding round-trips
    /// back to the input `30 / 255`. Before the fix it returned the
    /// raw sRGB values, causing the background to display as ~#636363
    /// instead of #1e1e1e.
    #[test]
    fn theme_bg_is_linear_for_srgb_surface() {
        let bg = ThemeBg::rgb(30, 30, 30);
        // Round-trip: linear → sRGB back to the original 8-bit value.
        let round_trip = |v: f32| -> u8 {
            let srgb = if v <= 0.003_130_8 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
            (srgb * 255.0).round() as u8
        };
        assert_eq!(
            round_trip(bg.r),
            30,
            "background red should round-trip through sRGB encoding back to 30"
        );
        assert_eq!(round_trip(bg.g), 30);
        assert_eq!(round_trip(bg.b), 30);
    }
}
