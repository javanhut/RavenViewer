//! Raven Viewer. `raven-viewer [open] FILE…` opens documents; a second
//! invocation hands its files to the running instance instead of starting
//! another process, so opening from the file manager is instant.

mod cfb;
mod config;
mod convert;
mod doc;
mod docx;
mod docxview;
mod emf;
mod fonts;
mod history;
mod look;
mod omml;
mod pagetiles;
mod pdf;
mod pdfedit;
mod pdftext;
mod render;
mod pdfview;
mod theme;
mod window;

use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use window::{APP_ID, MIME_TYPES, Window};

const USAGE: &str = "\
Raven Viewer — read, write, mark up, convert and combine PDFs, Word
documents (.docx and .doc) and text files

Usage:
  raven-viewer [FILE…]                  open files as tabs in the running window
                                        (or a new window, if there is none)
  raven-viewer open FILE…               same as above
  raven-viewer --new-window [FILE…]     a window of its own, in a process of its own
  raven-viewer new FILE                 write an empty document (.docx, .doc, .txt)
  raven-viewer convert IN OUT           convert IN to OUT's kind: .pdf, .docx, .doc or .txt
  raven-viewer combine OUT FILE FILE…   join files into OUT: PDFs page by page,
                                        documents one after another (a mix makes a PDF)
  raven-viewer set-default              make Raven Viewer the default for PDF and Word
  raven-viewer --help | --version
";

fn main() -> glib::ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    // A separate instance: its own process, not handed to the running one.
    let separate = args.iter().skip(1).any(|a| a == "--new-window" || a == "-n");
    args.retain(|a| a != "--new-window" && a != "-n");
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
        Some("convert") => return convert_file(&args[2..]),
        Some("new") => return new_file(&args[2..]),
        // `open` is sugar; GApplication takes the files as plain arguments.
        Some("open") => {
            args.remove(1);
        }
        _ => {}
    }

    let mut flags = gio::ApplicationFlags::HANDLES_OPEN;
    if separate {
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = adw::Application::builder().application_id(APP_ID).flags(flags).build();
    app.connect_startup(|_| theme::apply());
    // Launched again with nothing to open: another window.
    app.connect_activate(|app| Window::new(app).present());
    // Files open as tabs of the window last used, or of a new one.
    app.connect_open(|app, files, _hint| {
        let win = app.active_window().and_then(|w| Window::of(&w)).unwrap_or_else(|| Window::new(app));
        for file in files {
            win.open(file.clone());
        }
        win.present();
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

/// Run a command-line job, reporting how it went.
fn run(job: impl FnOnce() -> anyhow::Result<String>) -> glib::ExitCode {
    match job() {
        Ok(done) => {
            println!("{done}");
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("raven-viewer: {e}");
            glib::ExitCode::FAILURE
        }
    }
}

fn convert_file(args: &[String]) -> glib::ExitCode {
    let [input, output] = args else {
        eprint!("{USAGE}");
        return glib::ExitCode::FAILURE;
    };
    run(|| {
        let bytes = std::fs::read(input).map_err(|e| anyhow::anyhow!("{input}: {e}"))?;
        let out = std::path::Path::new(output);
        let title = out.file_stem().map_or(String::new(), |s| s.to_string_lossy().into_owned());
        let converted = convert::convert(&bytes, convert::Format::of(out), &title)?;
        std::fs::write(out, converted).map_err(|e| anyhow::anyhow!("{output}: {e}"))?;
        Ok(format!("{input} → {output}"))
    })
}

fn new_file(args: &[String]) -> glib::ExitCode {
    let [output] = args else {
        eprint!("{USAGE}");
        return glib::ExitCode::FAILURE;
    };
    run(|| {
        let out = std::path::Path::new(output);
        if out.exists() {
            anyhow::bail!("{output} already exists");
        }
        let blank = docx::blank(convert::local_paper(), &docx::TextDefaults::document());
        let bytes = convert::convert(&blank, convert::Format::of(out), "")?;
        std::fs::write(out, bytes).map_err(|e| anyhow::anyhow!("{output}: {e}"))?;
        Ok(format!("wrote {output}"))
    })
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
        let merged = convert::combine(&files)?;
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
