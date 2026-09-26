//! The look: Raven Glass, the stylesheet shared with Raven Settings, Store
//! and Power (`data/raven-glass.css`), plus the classes only the viewer
//! draws. Accent and light/dark come from desktop.toml.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
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

/// The viewer's own light overrides, laid over `raven-glass-light.css`: the
/// canvas, the page pill and the sheet's hairline are drawn for a dark ground.
const LIGHT_CSS: &str = r#"
.canvas { background-color: alpha(#000000, 0.05); }
.page { box-shadow: 0 1px 2px alpha(#000000, 0.18), 0 8px 24px alpha(#000000, 0.14); }
.page-pill > button {
  background-color: alpha(#ffffff, 0.92);
  color: @window_fg_color;
  border-color: alpha(#000000, 0.10);
  box-shadow: 0 6px 20px alpha(#000000, 0.16);
}
.page-pill > button:hover { background-color: #ffffff; }
.page-pill > button:active, .page-pill:checked > button { background-color: alpha(#e6e6ec, 0.98); }
.docx-sheet { border-color: alpha(#000000, 0.08); }
"#;

/// How long desktop.toml has to be quiet before it is re-read: one save is
/// a burst of events (create, write, rename).
const DESKTOP_SETTLE: Duration = Duration::from_millis(150);

thread_local! {
    static OVERLAY: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static DESKTOP_MONITOR: RefCell<Option<gio::FileMonitor>> = const { RefCell::new(None) };
}

/// Load the stylesheet once, apply the desktop's look, and follow it.
pub fn apply() {
    let display = gtk::gdk::Display::default().expect("no display");
    let base = gtk::CssProvider::new();
    base.load_from_string(BASE_CSS);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    refresh();
    watch_desktop();
}

/// Re-read desktop.toml and apply it: light/dark, the accent, and glass on
/// the open windows. The overlay provider is replaced, never stacked.
fn refresh() {
    let appearance = Desktop::load().appearance;
    adw::StyleManager::default().set_color_scheme(match appearance.theme_mode {
        ThemeMode::Dark => adw::ColorScheme::ForceDark,
        ThemeMode::Light => adw::ColorScheme::ForceLight,
        ThemeMode::Auto => adw::ColorScheme::PreferDark,
    });
    let accent = if is_hex(&appearance.accent) { appearance.accent.as_str() } else { DEFAULT_ACCENT };
    let light = appearance.theme_mode == ThemeMode::Light;
    let css = format!(
        "@define-color accent_bg_color {accent};\n@define-color accent_color {accent};\n{}{}",
        if light { include_str!("../data/raven-glass-light.css") } else { "" },
        if light { LIGHT_CSS } else { "" }
    );
    if let Some(display) = gtk::gdk::Display::default() {
        OVERLAY.with(|slot| {
            if let Some(old) = slot.borrow_mut().take() {
                gtk::style_context_remove_provider_for_display(&display, &old);
            }
            let overlay = gtk::CssProvider::new();
            overlay.load_from_string(&css);
            gtk::style_context_add_provider_for_display(
                &display,
                &overlay,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
            *slot.borrow_mut() = Some(overlay);
        });
    }

    // Glass is the material of the viewer's own windows; dialogs stay opaque.
    let toplevels = gtk::Window::toplevels();
    for i in 0..toplevels.n_items() {
        let Some(window) = toplevels.item(i).and_downcast::<gtk::Window>() else {
            continue;
        };
        if !window.has_css_class("raven") {
            continue;
        }
        if appearance.transparency && window.transient_for().is_none() {
            window.add_css_class("glass");
        } else {
            window.remove_css_class("glass");
        }
    }
}

/// Follow desktop.toml, so a change made in Raven Settings shows here at
/// once. The directory is watched rather than the file: the file may not
/// exist yet, and is replaced by renaming a new one over it.
fn watch_desktop() {
    let path = Desktop::path();
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let name = name.to_os_string();
    let Ok(monitor) = gio::File::for_path(dir)
        .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
    else {
        return;
    };
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    monitor.connect_changed(move |_, file, other, event| {
        if matches!(
            event,
            gio::FileMonitorEvent::AttributeChanged
                | gio::FileMonitorEvent::PreUnmount
                | gio::FileMonitorEvent::Unmounted
        ) {
            return;
        }
        let names_desktop = |f: Option<&gio::File>| {
            f.and_then(|f| f.basename()).is_some_and(|b| b.as_os_str() == name.as_os_str())
        };
        if !names_desktop(Some(file)) && !names_desktop(other) {
            return;
        }
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        let fired = pending.clone();
        let id = glib::timeout_add_local_once(DESKTOP_SETTLE, move || {
            fired.borrow_mut().take();
            refresh();
        });
        *pending.borrow_mut() = Some(id);
    });
    DESKTOP_MONITOR.with(|m| *m.borrow_mut() = Some(monitor));
}

/// Whether the desktop asked for translucent windows.
pub fn glass() -> bool {
    Desktop::load().appearance.transparency
}

fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}
