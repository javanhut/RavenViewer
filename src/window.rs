//! The window: a header bar, a sidebar (Contents · Notes) and the document.
//! Files are read and parsed off the main thread so a large book never
//! freezes the UI.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{gdk, gio, glib};
use libadwaita as adw;

use crate::config::ViewerConfig;
use crate::pdf::{self, DocumentInfo};
use crate::pdfview::PdfView;
use crate::{docx, docxview, theme};

pub const APP_ID: &str = "com.ravenviewer.Raven";

enum Doc {
    Pdf {
        path: PathBuf,
        view: PdfView,
        search: Rc<pdf::Searcher>,
    },
    Docx,
}

struct State {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    split: adw::OverlaySplitView,
    content: gtk::Stack,
    toasts: adw::ToastOverlay,
    outline: gtk::ListBox,
    notes: gtk::ListBox,
    pill: gtk::Label,
    zoom_label: gtk::Button,
    search_bar: gtk::SearchBar,
    search: gtk::SearchEntry,
    doc_only: Vec<gtk::Widget>,
    doc: RefCell<Option<Doc>>,
    config: RefCell<ViewerConfig>,
}

#[derive(Clone)]
pub struct Window(Rc<State>);

impl Window {
    pub fn new(app: &adw::Application) -> Self {
        let config = ViewerConfig::load();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Raven Viewer")
            .default_width(1120)
            .default_height(820)
            .css_classes(["raven"])
            .build();
        if theme::glass() {
            window.add_css_class("glass");
        }

        // ── Header bar ──────────────────────────────────────────────────
        let title = adw::WindowTitle::new("Raven Viewer", "");
        let header = adw::HeaderBar::builder().title_widget(&title).build();

        let sidebar_btn = icon_button("sidebar-show-symbolic", "Toggle sidebar (F9)", "win.sidebar");
        let open_btn = icon_button("document-open-symbolic", "Open… (Ctrl+O)", "win.open");
        header.pack_start(&sidebar_btn);
        header.pack_start(&open_btn);

        let menu = gio::Menu::new();
        let view_section = gio::Menu::new();
        view_section.append(Some("Dark Pages"), Some("win.dark-pages"));
        view_section.append(Some("Fit Width"), Some("win.fit"));
        menu.append_section(None, &view_section);
        let app_section = gio::Menu::new();
        app_section.append(Some("Make Default for PDF & DOCX"), Some("win.set-default"));
        app_section.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        app_section.append(Some("About Raven Viewer"), Some("win.about"));
        menu.append_section(None, &app_section);
        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Menu")
            .css_classes(["flat"])
            .build();
        header.pack_end(&menu_btn);

        let search_btn = icon_button("system-search-symbolic", "Find (Ctrl+F)", "win.find");
        header.pack_end(&search_btn);

        let zoom_box = gtk::Box::builder().css_classes(["linked"]).build();
        let zoom_label = gtk::Button::builder()
            .label("100%")
            .tooltip_text("Fit width (Ctrl+0)")
            .action_name("win.fit")
            .css_classes(["flat"])
            .width_request(58)
            .build();
        zoom_box.append(&icon_button("zoom-out-symbolic", "Zoom out (Ctrl+−)", "win.zoom-out"));
        zoom_box.append(&zoom_label);
        zoom_box.append(&icon_button("zoom-in-symbolic", "Zoom in (Ctrl++)", "win.zoom-in"));
        header.pack_end(&zoom_box);

        // ── Search ──────────────────────────────────────────────────────
        let search = gtk::SearchEntry::builder().placeholder_text("Find in document").width_request(320).build();
        let search_bar = gtk::SearchBar::builder().child(&search).show_close_button(true).build();
        search_bar.connect_entry(&search);
        search_bar.set_key_capture_widget(Some(&window));

        // ── Sidebar ─────────────────────────────────────────────────────
        let outline = nav_list();
        let notes = nav_list();
        let side_stack = adw::ViewStack::new();
        side_stack.add_titled_with_icon(&scrolled(&outline), Some("contents"), "Contents", "view-list-symbolic");
        side_stack.add_titled_with_icon(&scrolled(&notes), Some("notes"), "Notes", "user-bookmarks-symbolic");
        let switcher = adw::InlineViewSwitcher::builder().stack(&side_stack).margin_bottom(10).build();
        let sidebar = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["sidebar"]).build();
        sidebar.append(&switcher);
        sidebar.append(&side_stack);

        // ── Content ─────────────────────────────────────────────────────
        let welcome = adw::StatusPage::builder()
            .icon_name(APP_ID)
            .title("Raven Viewer")
            .description("Open a PDF or DOCX, or drop one here.")
            .css_classes(["welcome"])
            .build();
        let open_cta = gtk::Button::builder()
            .label("Open Document…")
            .action_name("win.open")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        welcome.set_child(Some(&open_cta));

        let content = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        content.add_named(&welcome, Some("welcome"));

        let pill = gtk::Label::builder()
            .css_classes(["page-pill"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .visible(false)
            .build();
        let overlay = gtk::Overlay::builder().child(&content).build();
        overlay.add_overlay(&pill);

        let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        body.append(&search_bar);
        body.append(&overlay);

        let split = adw::OverlaySplitView::builder()
            .sidebar(&sidebar)
            .content(&body)
            .min_sidebar_width(220.0)
            .max_sidebar_width(320.0)
            .show_sidebar(false)
            .build();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toasts));
        window.set_content(Some(&toolbar));

        // Collapse the sidebar into an overlay on narrow windows.
        let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Sp,
        ));
        bp.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(bp);

        let doc_only: Vec<gtk::Widget> = vec![zoom_box.upcast(), search_btn.upcast(), sidebar_btn.upcast()];
        let this = Window(Rc::new(State {
            window,
            title,
            split,
            content,
            toasts,
            outline,
            notes,
            pill,
            zoom_label,
            search_bar,
            search,
            doc_only,
            doc: RefCell::new(None),
            config: RefCell::new(config),
        }));
        this.set_doc_controls(false, false);
        this.install_actions();
        this.install_drop();
        this.connect_signals();
        this
    }

    pub fn present(&self) {
        self.0.window.present();
    }

    fn toast(&self, text: &str) {
        self.0.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(4).build());
    }

    fn set_doc_controls(&self, any_doc: bool, pdf: bool) {
        for (i, w) in self.0.doc_only.iter().enumerate() {
            // zoom and search are PDF-only; the sidebar toggle needs a doc.
            w.set_visible(if i < 2 { pdf } else { any_doc });
        }
    }

    // ── Opening ─────────────────────────────────────────────────────────

    pub fn open(&self, file: gio::File) {
        let Some(path) = file.path() else {
            self.toast("Only local files can be opened");
            return;
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let loaded = gio::spawn_blocking(move || load(&p)).await;
            match loaded {
                Ok(Ok(Loaded::Pdf(bytes, info))) => this.show_pdf(path, bytes, info),
                Ok(Ok(Loaded::Docx(blocks))) => this.show_docx(path, &blocks),
                Ok(Err(e)) => this.toast(&format!("Couldn’t open {}: {e}", display_name(&path))),
                Err(_) => this.toast("Couldn’t open the file"),
            }
        });
    }

    fn remember_position(&self) {
        if let Some(Doc::Pdf { path, view, .. }) = self.0.doc.borrow().as_ref() {
            let mut cfg = self.0.config.borrow_mut();
            if cfg.remember_position {
                cfg.positions.insert(path.to_string_lossy().into_owned(), view.current_page());
                let _ = cfg.save();
            }
        }
    }

    fn replace_content(&self, widget: &impl IsA<gtk::Widget>) {
        self.remember_position();
        if let Some(old) = self.0.content.child_by_name("doc") {
            self.0.content.remove(&old);
        }
        self.0.content.add_named(widget, Some("doc"));
        self.0.content.set_visible_child_name("doc");
    }

    fn show_pdf(&self, path: PathBuf, bytes: Arc<Vec<u8>>, info: DocumentInfo) {
        let view = match PdfView::new(bytes.clone(), info.clone()) {
            Ok(v) => v,
            Err(e) => return self.toast(&e.to_string()),
        };
        self.replace_content(view.widget());

        let name = display_name(&path);
        self.0.title.set_title(info.title.as_deref().unwrap_or(&name));
        self.0.window.set_title(Some(&format!("{name} — Raven Viewer")));
        self.fill_outline(&info, &view);
        self.fill_notes(&info, &view);

        let n = view.page_count();
        let (pill, subtitle) = (self.0.pill.clone(), self.0.title.clone());
        let update = move |page: usize| {
            let text = format!("{} of {n}", page + 1);
            pill.set_label(&text);
            subtitle.set_subtitle(&format!("Page {text}"));
        };
        update(0);
        view.connect_page_changed(update);
        let zoom_label = self.0.zoom_label.clone();
        view.connect_zoom_changed(move |z| zoom_label.set_label(&format!("{:.0}%", z * 100.0)));
        self.0.pill.set_visible(true);

        let dark = self.0.config.borrow().dark_pages;
        view.set_dark_pages(dark);
        let resume = {
            let cfg = self.0.config.borrow();
            cfg.remember_position.then(|| cfg.positions.get(path.to_string_lossy().as_ref()).copied()).flatten()
        };
        let has_sidebar = !info.outline.is_empty() || !info.annotations.is_empty();
        self.0.split.set_show_sidebar(has_sidebar && self.0.config.borrow().show_sidebar);
        self.set_doc_controls(true, true);

        let search = Rc::new(pdf::Searcher::spawn(bytes, view.page_count()));
        *self.0.doc.borrow_mut() = Some(Doc::Pdf { path, view: view.clone(), search });

        let this = self.clone();
        glib::idle_add_local_once(move || {
            view.fit_width();
            this.sync_zoom_label();
            if let Some(page) = resume.filter(|&p| p > 0) {
                let v = view.clone();
                glib::idle_add_local_once(move || v.go_to(page, 0.0));
            }
        });
    }

    fn show_docx(&self, path: PathBuf, blocks: &[docx::Block]) {
        self.replace_content(&docxview::build(blocks));
        let name = display_name(&path);
        self.0.title.set_title(&name);
        self.0.title.set_subtitle("Document");
        self.0.window.set_title(Some(&format!("{name} — Raven Viewer")));
        self.0.pill.set_visible(false);
        self.0.outline.remove_all();
        self.0.notes.remove_all();
        self.0.split.set_show_sidebar(false);
        self.set_doc_controls(false, false);
        *self.0.doc.borrow_mut() = Some(Doc::Docx);
    }

    fn fill_outline(&self, info: &DocumentInfo, view: &PdfView) {
        let list = &self.0.outline;
        list.remove_all();
        if info.outline.is_empty() {
            list.set_placeholder(Some(&placeholder("No table of contents")));
        }
        for entry in &info.outline {
            let row = gtk::Box::builder().spacing(8).css_classes(["outline-row"]).build();
            row.add_css_class(&format!("level-{}", entry.level.min(3)));
            let title = gtk::Label::builder()
                .label(&entry.title)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .tooltip_text(&entry.title)
                .build();
            let page = gtk::Label::builder().label((entry.page + 1).to_string()).css_classes(["page-number"]).build();
            row.append(&title);
            row.append(&page);
            list.append(&row);
        }
        let (entries, view) = (info.outline.clone(), view.clone());
        let split = self.0.split.clone();
        list.connect_row_activated(move |_, row| {
            if let Some(e) = entries.get(row.index() as usize) {
                view.go_to(e.page, 0.0);
                if split.is_collapsed() {
                    split.set_show_sidebar(false);
                }
            }
        });
    }

    fn fill_notes(&self, info: &DocumentInfo, view: &PdfView) {
        let list = &self.0.notes;
        list.remove_all();
        if info.annotations.is_empty() {
            list.set_placeholder(Some(&placeholder("No annotations")));
        }
        for a in &info.annotations {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .css_classes(["outline-row"])
                .build();
            let kind = gtk::Label::builder()
                .label(format!("{} · PAGE {}", a.kind.to_uppercase(), a.page + 1))
                .xalign(0.0)
                .css_classes(["annot-kind"])
                .build();
            row.append(&kind);
            if !a.contents.is_empty() {
                row.append(
                    &gtk::Label::builder()
                        .label(&a.contents)
                        .xalign(0.0)
                        .wrap(true)
                        .lines(3)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .build(),
                );
            }
            list.append(&row);
        }
        let (notes, view) = (info.annotations.clone(), view.clone());
        list.connect_row_activated(move |_, row| {
            if let Some(a) = notes.get(row.index() as usize) {
                view.go_to(a.page, a.y_fraction);
            }
        });
    }

    fn with_pdf(&self, f: impl FnOnce(&PdfView)) {
        if let Some(Doc::Pdf { view, .. }) = self.0.doc.borrow().as_ref() {
            f(view);
        }
    }

    fn sync_zoom_label(&self) {
        let label = self.0.zoom_label.clone();
        self.with_pdf(|v| label.set_label(&format!("{:.0}%", v.zoom() * 100.0)));
    }

    // ── Search ──────────────────────────────────────────────────────────

    fn find_next(&self, backwards: bool) {
        let query = self.0.search.text().trim().to_string();
        if query.is_empty() {
            return;
        }
        let (view, search) = match self.0.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, search, .. }) => (view.clone(), search.clone()),
            _ => return,
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let start = view.current_page();
            match search.find(&query, start, backwards).await {
                Some(page) if page == start => this.toast("Only match is on this page"),
                Some(page) => view.go_to(page, 0.0),
                None => this.toast(&format!("No matches for “{query}”")),
            }
        });
    }

    // ── Wiring ──────────────────────────────────────────────────────────

    fn install_actions(&self) {
        let win = &self.0.window;
        let add = |name: &str, f: Box<dyn Fn(&Window)>| {
            let action = gio::SimpleAction::new(name, None);
            let this = self.clone();
            action.connect_activate(move |_, _| f(&this));
            win.add_action(&action);
        };

        add("open", Box::new(|w| w.choose_file()));
        add("zoom-in", Box::new(|w| { w.with_pdf(|v| v.zoom_by(1.2)); w.sync_zoom_label(); }));
        add("zoom-out", Box::new(|w| { w.with_pdf(|v| v.zoom_by(1.0 / 1.2)); w.sync_zoom_label(); }));
        add("fit", Box::new(|w| { w.with_pdf(|v| v.fit_width()); w.sync_zoom_label(); }));
        add("next-page", Box::new(|w| w.with_pdf(|v| v.next_page())));
        add("prev-page", Box::new(|w| w.with_pdf(|v| v.prev_page())));
        add("find", Box::new(|w| {
            let on = !w.0.search_bar.is_search_mode();
            w.0.search_bar.set_search_mode(on);
            if on {
                w.0.search.grab_focus();
            }
        }));
        add("sidebar", Box::new(|w| {
            let show = !w.0.split.shows_sidebar();
            w.0.split.set_show_sidebar(show);
            let mut cfg = w.0.config.borrow_mut();
            cfg.show_sidebar = show;
            let _ = cfg.save();
        }));
        add("set-default", Box::new(|w| w.set_default_app()));
        add("shortcuts", Box::new(|w| w.show_shortcuts()));
        add("about", Box::new(|w| {
            adw::AboutDialog::builder()
                .application_name("Raven Viewer")
                .application_icon(APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Read PDFs and documents on Raven Linux. Free, fast, no browser.")
                .license_type(gtk::License::MitX11)
                .website("https://github.com/javanhut/RavenViewer")
                .build()
                .present(Some(&w.0.window));
        }));
        add("close", Box::new(|w| w.0.window.close()));

        let dark = gio::SimpleAction::new_stateful("dark-pages", None, &self.0.config.borrow().dark_pages.to_variant());
        let this = self.clone();
        dark.connect_activate(move |a, _| {
            let on = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            a.set_state(&on.to_variant());
            this.with_pdf(|v| v.set_dark_pages(on));
            let mut cfg = this.0.config.borrow_mut();
            cfg.dark_pages = on;
            let _ = cfg.save();
        });
        win.add_action(&dark);

        let app = win.application().expect("window has an application");
        for (action, keys) in [
            ("win.open", &["<Ctrl>o"][..]),
            ("win.zoom-in", &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"]),
            ("win.zoom-out", &["<Ctrl>minus", "<Ctrl>KP_Subtract"]),
            ("win.fit", &["<Ctrl>0"]),
            ("win.find", &["<Ctrl>f"]),
            ("win.sidebar", &["F9"]),
            ("win.next-page", &["<Ctrl>Page_Down", "n"]),
            ("win.prev-page", &["<Ctrl>Page_Up", "p"]),
            ("win.shortcuts", &["<Ctrl>question"]),
            ("win.close", &["<Ctrl>w"]),
        ] {
            app.set_accels_for_action(action, keys);
        }
    }

    fn connect_signals(&self) {
        let this = self.clone();
        self.0.search.connect_activate(move |_| this.find_next(false));
        let this = self.clone();
        self.0.search.connect_next_match(move |_| this.find_next(false));
        let this = self.clone();
        self.0.search.connect_previous_match(move |_| this.find_next(true));

        let this = self.clone();
        self.0.window.connect_close_request(move |_| {
            this.remember_position();
            glib::Propagation::Proceed
        });
    }

    fn install_drop(&self) {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let this = self.clone();
        target.connect_drop(move |_, value, _, _| {
            match value.get::<gdk::FileList>().ok().and_then(|l| l.files().into_iter().next()) {
                Some(file) => {
                    this.open(file);
                    true
                }
                None => false,
            }
        });
        self.0.window.add_controller(target);
    }

    fn choose_file(&self) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Documents"));
        for mime in MIME_TYPES {
            filter.add_mime_type(mime);
        }
        filter.add_suffix("pdf");
        filter.add_suffix("docx");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Open Document").filters(&filters).modal(true).build();
        let this = self.clone();
        dialog.open(Some(&self.0.window), gio::Cancellable::NONE, move |res| {
            if let Ok(file) = res {
                this.open(file);
            }
        });
    }

    fn set_default_app(&self) {
        let desktop = format!("{APP_ID}.desktop");
        let ok = MIME_TYPES.iter().all(|mime| {
            std::process::Command::new("xdg-mime")
                .args(["default", &desktop, mime])
                .status()
                .is_ok_and(|s| s.success())
        });
        self.toast(if ok {
            "Raven Viewer now opens PDF and DOCX files"
        } else {
            "Couldn’t set the default (is xdg-mime installed?)"
        });
    }

    fn show_shortcuts(&self) {
        let rows = [
            ("Open", "Ctrl+O"),
            ("Find", "Ctrl+F · Enter for next"),
            ("Zoom in / out", "Ctrl++ / Ctrl+− · Ctrl+scroll"),
            ("Fit width", "Ctrl+0"),
            ("Next / previous page", "N / P · Ctrl+PgDn / PgUp"),
            ("Toggle sidebar", "F9"),
            ("Close window", "Ctrl+W"),
        ];
        let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
        for (what, keys) in rows {
            let row = adw::ActionRow::builder().title(what).build();
            row.add_suffix(&gtk::Label::builder().label(keys).css_classes(["kbd"]).build());
            list.append(&row);
        }
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        page.append(&list);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        adw::Dialog::builder()
            .title("Keyboard Shortcuts")
            .content_width(460)
            .child(&view)
            .build()
            .present(Some(&self.0.window));
    }
}

pub const MIME_TYPES: [&str; 2] =
    ["application/pdf", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"];

enum Loaded {
    Pdf(Arc<Vec<u8>>, DocumentInfo),
    Docx(Vec<docx::Block>),
}

/// Sniff the bytes, not the extension: a PDF saved as .bin still opens.
fn load(path: &Path) -> anyhow::Result<Loaded> {
    let bytes = std::fs::read(path)?;
    if bytes.windows(5).take(1024).any(|w| w == b"%PDF-") {
        let info = pdf::load_info(&bytes)?;
        return Ok(Loaded::Pdf(Arc::new(bytes), info));
    }
    if bytes.starts_with(b"PK") {
        return Ok(Loaded::Docx(docx::load(&bytes)?));
    }
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("doc")) {
        anyhow::bail!("old binary .doc files aren’t supported yet — save it as .docx");
    }
    anyhow::bail!("not a PDF or DOCX file")
}

fn display_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn icon_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).action_name(action).css_classes(["flat"]).build()
}

fn nav_list() -> gtk::ListBox {
    gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build()
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder().vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).child(child).build()
}

fn placeholder(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).css_classes(["dim-label"]).margin_top(24).build()
}
