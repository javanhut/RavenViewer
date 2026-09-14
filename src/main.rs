//! Raven Viewer. `raven-viewer [open] FILE…` opens documents; a second
//! invocation hands its files to the running instance instead of starting
//! another process, so opening from the file manager is instant.

mod config;
mod docx;
mod docxview;
mod pdf;
mod pdfview;
mod theme;
mod window;

use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use window::{APP_ID, MIME_TYPES, Window};

const USAGE: &str = "\
Raven Viewer — read PDF and DOCX files

Usage:
  raven-viewer [FILE…]          open files (or an empty window)
  raven-viewer open FILE…       same as above
  raven-viewer set-default      make Raven Viewer the default for PDF and DOCX
  raven-viewer --help | --version
";

fn main() -> glib::ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return glib::ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("raven-viewer {}", env!("CARGO_PKG_VERSION"));
            return glib::ExitCode::SUCCESS;
        }
        Some("set-default") => return set_default(),
        // `open` is sugar; GApplication takes the files as plain arguments.
        Some("open") => {
            args.remove(1);
        }
        _ => {}
    }

    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_OPEN).build();
    app.connect_startup(|_| theme::apply());
    app.connect_activate(|app| {
        if let Some(win) = app.active_window() {
            win.present();
        } else {
            Window::new(app).present();
        }
    });
    app.connect_open(|app, files, _hint| {
        for file in files {
            let win = Window::new(app);
            win.open(file.clone());
            win.present();
        }
    });
    app.run_with_args(&args)
}

fn set_default() -> glib::ExitCode {
    let desktop = format!("{APP_ID}.desktop");
    for mime in MIME_TYPES {
        let ok = std::process::Command::new("xdg-mime")
            .args(["default", &desktop, mime])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("raven-viewer: could not set the default for {mime} (is xdg-mime installed?)");
            return glib::ExitCode::FAILURE;
        }
        println!("{mime} → {desktop}");
    }
    glib::ExitCode::SUCCESS
}
