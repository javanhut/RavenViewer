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
    /// Settings' glass theme (black, fog, arctic, midnight, rose, tokyo-neon,
    /// clear, ember, nebula).
    pub glass_theme: String,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::Dark,
            accent: DEFAULT_ACCENT.into(),
            transparency: true,
            glass_theme: String::new(),
        }
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
    /// Where Raven Settings keeps it; watched for live changes.
    pub fn path() -> PathBuf {
        config_dir().join("desktop.toml")
    }

    pub fn load() -> Desktop {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerConfig {
    /// Show the sidebar when a document opens. A PDF with a table of
    /// contents shows it regardless, unless the window is narrow.
    pub show_sidebar: bool,
    /// Page background follows the theme (dark pages in dark mode).
    pub dark_pages: bool,
    /// Last-read page per file, so a reopened book resumes where you were.
    pub remember_position: bool,
    /// Colour for new highlights: yellow, green, blue or pink.
    pub highlight_color: String,
    /// How PDF pages are laid out: continuous, single or double (see
    /// `PageLayout`). A string, like the colour, so a value this version
    /// does not know falls back on its own instead of failing the file.
    pub page_layout: String,
    /// In two-page layout, the first page (the cover) on its own, so pairs
    /// run 2–3, 4–5… with even pages on the left, as in a printed book.
    pub cover_alone: bool,
    pub positions: std::collections::BTreeMap<String, usize>,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            show_sidebar: true,
            dark_pages: false,
            remember_position: true,
            highlight_color: "yellow".into(),
            page_layout: PageLayout::Continuous.id().into(),
            cover_alone: false,
            positions: Default::default(),
        }
    }
}

impl ViewerConfig {
    pub fn page_layout(&self) -> PageLayout {
        PageLayout::from_id(&self.page_layout).unwrap_or_default()
    }

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

/// How a PDF's pages are put in front of the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageLayout {
    /// One long column, scrolled through.
    #[default]
    Continuous,
    /// One page at a time, turned left and right.
    Single,
    /// Two pages side by side, turned a pair at a time.
    Double,
}

impl PageLayout {
    pub const ALL: [PageLayout; 3] = [PageLayout::Continuous, PageLayout::Single, PageLayout::Double];

    pub fn id(self) -> &'static str {
        match self {
            PageLayout::Continuous => "continuous",
            PageLayout::Single => "single",
            PageLayout::Double => "double",
        }
    }

    pub fn from_id(id: &str) -> Option<PageLayout> {
        PageLayout::ALL.into_iter().find(|l| l.id() == id)
    }

    /// Pages turned at a time; 0 for the continuous column, which has no turns.
    pub fn per_spread(self) -> usize {
        match self {
            PageLayout::Continuous => 0,
            PageLayout::Single => 1,
            PageLayout::Double => 2,
        }
    }

    pub fn is_paged(self) -> bool {
        self != PageLayout::Continuous
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("raven")
}
