//! The look: Raven Glass, the stylesheet shared with Raven Settings, Store
//! and Power (`data/raven-glass.css`), plus the classes only the viewer
//! draws. Accent and light/dark come from desktop.toml.

use gtk4 as gtk;
use libadwaita as adw;

use crate::config::{DEFAULT_ACCENT, Desktop, ThemeMode};

const BASE_CSS: &str = concat!(
    include_str!("../data/raven-glass.css"),
    r#"
/* ── Viewer-only ─────────────────────────────────────────────────────── */
.canvas { background-color: alpha(#000000, 0.18); }
.page {
  background-color: #ffffff;
  border-radius: 3px;
  box-shadow: 0 1px 2px alpha(#000000, 0.30), 0 8px 24px alpha(#000000, 0.28);
}
.page.dark-page { filter: invert(0.88) hue-rotate(180deg); }
/* The page counter is a button now — click it to jump to a page — so the
   pill is drawn on the inner button rather than on the widget itself. */
.page-pill { margin: 14px; }
.page-pill > button {
  background-color: alpha(#26262f, 0.88);
  color: #ffffff;
  border: 1px solid alpha(#ffffff, 0.10);
  border-radius: 999px;
  padding: 4px 8px 4px 12px;
  min-height: 0;
  min-width: 0;
  font-size: 12px;
  font-weight: 600;
  font-feature-settings: "tnum";
  box-shadow: 0 6px 20px alpha(#000000, 0.35);
}
.page-pill > button:hover { background-color: alpha(#3a3a46, 0.94); }
.page-pill > button:active, .page-pill:checked > button { background-color: alpha(#4a4a58, 0.96); }
.jump-to-page { padding: 4px; }
.jump-to-page entry { font-feature-settings: "tnum"; }
.outline-row { padding: 6px 8px; }
.outline-row .page-number { font-size: 12px; font-feature-settings: "tnum"; color: alpha(@window_fg_color, 0.55); }
.outline-row.level-1 { padding-left: 22px; }
.outline-row.level-2 { padding-left: 36px; }
.outline-row.level-3 { padding-left: 50px; }
.annot-kind { font-size: 11px; font-weight: 700; letter-spacing: 0.4px; color: @accent_bg_color; }
.docx-view { background-color: transparent; }
.docx-sheet {
  background-color: @view_bg_color;
  border-radius: 14px;
  border: 1px solid alpha(#ffffff, 0.08);
  padding: 48px 56px;
}
.docx-sheet textview, .docx-sheet textview text { background-color: transparent; }
.welcome .title-1 { font-size: 30px; font-weight: 800; letter-spacing: -0.8px; }
"#
);

pub fn apply() {
    let display = gtk::gdk::Display::default().expect("no display");
    let base = gtk::CssProvider::new();
    base.load_from_string(BASE_CSS);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    let appearance = Desktop::load().appearance;
    adw::StyleManager::default().set_color_scheme(match appearance.theme_mode {
        ThemeMode::Dark => adw::ColorScheme::ForceDark,
        ThemeMode::Light => adw::ColorScheme::ForceLight,
        ThemeMode::Auto => adw::ColorScheme::PreferDark,
    });
    let accent = if is_hex(&appearance.accent) { appearance.accent.as_str() } else { DEFAULT_ACCENT };
    let css = format!(
        "@define-color accent_bg_color {accent};\n@define-color accent_color {accent};\n{}",
        if appearance.theme_mode == ThemeMode::Light { include_str!("../data/raven-glass-light.css") } else { "" }
    );
    let overlay = gtk::CssProvider::new();
    overlay.load_from_string(&css);
    gtk::style_context_add_provider_for_display(&display, &overlay, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
}

/// Whether the desktop asked for translucent windows.
pub fn glass() -> bool {
    Desktop::load().appearance.transparency
}

fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}
