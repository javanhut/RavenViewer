//! `~/.config/raven/desktop.toml` is owned by Raven Settings and read here
//! only for the look, so the viewer matches the rest of the desktop.
//! `~/.config/raven/viewer.toml` is the viewer's own.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_ACCENT: &str = "#7AA2F7";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    #[default]
    Dark,
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme_mode: ThemeMode,
    pub accent: String,
    pub transparency: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self { theme_mode: ThemeMode::Dark, accent: DEFAULT_ACCENT.into(), transparency: true }
    }
}

/// The slice of desktop.toml the viewer cares about. Unknown keys are
/// ignored so Settings can grow without breaking us.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Desktop {
    pub appearance: Appearance,
}

impl Desktop {
    pub fn load() -> Desktop {
        std::fs::read_to_string(config_dir().join("desktop.toml"))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerConfig {
    /// Show the outline/annotations sidebar when a document opens.
    pub show_sidebar: bool,
    /// Page background follows the theme (dark pages in dark mode).
    pub dark_pages: bool,
    /// Last-read page per file, so a reopened book resumes where you were.
    pub remember_position: bool,
    pub positions: std::collections::BTreeMap<String, usize>,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            show_sidebar: true,
            dark_pages: false,
            remember_position: true,
            positions: Default::default(),
        }
    }
}

impl ViewerConfig {
    pub fn path() -> PathBuf {
        config_dir().join("viewer.toml")
    }

    pub fn load() -> ViewerConfig {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = format!(
            "# Raven Viewer preferences. Written by raven-viewer.\n{}",
            toml::to_string_pretty(self)?
        );
        std::fs::write(&path, text)?;
        Ok(())
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("raven")
}
