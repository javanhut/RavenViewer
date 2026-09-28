//! Raven Viewer. `raven-viewer [open] FILE…` opens documents; a second
//! invocation hands its files to the running instance instead of starting
//! another process, so opening from the file manager is instant.

mod config;
mod docx;
mod docxview;
mod pagetiles;
mod pdf;
mod pdfedit;
mod pdftext;
mod pdfview;
mod theme;
mod window;

use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use window::{APP_ID, MIME_TYPES, Window};

const USAGE: &str = "\
Raven Viewer — read, mark up, edit and combine PDF and DOCX files

Usage:
  raven-viewer [FILE…]                  open files (or an empty window)
  raven-viewer open FILE…               same as above
  raven-viewer combine OUT FILE FILE…   join PDFs (or DOCX files) into OUT
  raven-viewer set-default              make Raven Viewer the default for PDF and DOCX
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
        Some("combine") => return combine(&args[2..]),
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

fn combine(args: &[String]) -> glib::ExitCode {
    let [out, inputs @ ..] = args else {
        eprint!("{USAGE}");
        return glib::ExitCode::FAILURE;
    };
    let run = || -> anyhow::Result<()> {
        let mut files = Vec::new();
        for path in inputs {
            let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
            let name = std::path::Path::new(path).file_name().map_or(path.clone(), |n| n.to_string_lossy().into_owned());
            files.push((name, bytes));
        }
        let merged = window::combine_files(&files)?;
        std::fs::write(out, merged).map_err(|e| anyhow::anyhow!("{out}: {e}"))?;
        Ok(())
    };
    match run() {
        Ok(()) => {
            println!("{} files → {out}", inputs.len());
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("raven-viewer: {e}");
            glib::ExitCode::FAILURE
        }
    }
}

/// GTK may only be used from the thread that initialized it, and the test
/// harness runs tests on many threads — so a GTK test run on the wrong one
/// would quietly skip. Every GTK test runs on this one thread instead.
#[cfg(test)]
pub mod gtk_test {
    use std::sync::{Mutex, OnceLock, mpsc};

    type Job = Box<dyn FnOnce() + Send>;
    type Reply = mpsc::Sender<std::thread::Result<()>>;
    type Jobs = mpsc::Sender<(Job, Reply)>;

    static GTK: OnceLock<Mutex<Option<Jobs>>> = OnceLock::new();

    /// Run `test` on the GTK thread; skipped (with a note) only when there
    /// is no display at all.
    pub fn run(test: impl FnOnce() + Send + 'static) {
        let slot = GTK.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<(Job, Reply)>();
            let (ready_tx, ready) = mpsc::channel();
            std::thread::spawn(move || {
                let ok = gtk4::init().is_ok();
                let _ = ready_tx.send(ok);
                if !ok {
                    return;
                }
                for (job, reply) in rx {
                    let _ = reply.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)));
                }
            });
            Mutex::new(ready.recv().unwrap_or(false).then_some(tx))
        });
        let Some(tx) = slot.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            eprintln!("no display: skipping a GTK test");
            return;
        };
        let (reply, answer) = mpsc::channel();
        tx.send((Box::new(test), reply)).expect("the GTK test thread is gone");
        if let Err(panic) = answer.recv().expect("the GTK test thread is gone") {
            std::panic::resume_unwind(panic);
        }
    }
}
