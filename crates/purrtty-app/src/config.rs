//! TOML configuration loader.
//!
//! Reads `~/.config/purrtty/config.toml` (or the platform-specific
//! equivalent via `dirs::config_dir`) at startup. Missing fields fall
//! back to sensible defaults — a missing file is also fine and just
//! yields the default config.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use purrtty_ui::{RendererConfig, Theme};
use serde::Deserialize;
use tracing::{info, warn};

/// Top-level config schema.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub window: WindowSection,
    pub font: FontSection,
    pub colors: ColorsSection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WindowSection {
    pub width: f64,
    pub height: f64,
}

impl Default for WindowSection {
    fn default() -> Self {
        Self {
            width: 960.0,
            height: 600.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FontSection {
    /// Specific monospace font family. `None` falls back to the system
    /// generic monospace font (e.g. Menlo on macOS).
    pub family: Option<String>,
    pub size: f32,
    pub line_height: f32,
}

impl Default for FontSection {
    fn default() -> Self {
        Self {
            family: None,
            size: 20.0,
            line_height: 24.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ColorsSection {
    /// Built-in color scheme name. Currently `"dark"` (default) or
    /// `"light"`. Custom palettes are a follow-up item.
    pub scheme: String,
}

impl Default for ColorsSection {
    fn default() -> Self {
        Self {
            scheme: "dark".to_string(),
        }
    }
}

impl Config {
    /// Load from `~/.config/purrtty/config.toml`. Returns `Config::default()`
    /// on missing file or any parse error (with a warning logged).
    pub fn load() -> Self {
        let path = config_path();
        if !path.exists() {
            info!(?path, "no config file, using defaults");
            return Self::default();
        }
        match try_load(&path) {
            Ok(cfg) => {
                info!(?path, "loaded config");
                cfg
            }
            Err(err) => {
                warn!(?path, ?err, "failed to load config; using defaults");
                Self::default()
            }
        }
    }

    /// Convert this config into the renderer's typed config.
    pub fn renderer_config(&self) -> RendererConfig {
        let theme = match self.colors.scheme.as_str() {
            "light" => Theme::light(),
            "dark" => Theme::dark(),
            other => {
                warn!(scheme = other, "unknown color scheme; falling back to dark");
                Theme::dark()
            }
        };
        RendererConfig {
            font_family: self.font.family.clone(),
            font_size: self.font.size,
            line_height: self.font.line_height,
            theme,
        }
    }
}

fn try_load(path: &Path) -> Result<Config> {
    let s = fs::read_to_string(path).context("read config file")?;
    let cfg: Config = toml::from_str(&s).context("parse config TOML")?;
    Ok(cfg)
}

fn config_path() -> PathBuf {
    if let Some(dir) = dirs::config_dir() {
        dir.join("purrtty").join("config.toml")
    } else {
        PathBuf::from(".config/purrtty/config.toml")
    }
}
