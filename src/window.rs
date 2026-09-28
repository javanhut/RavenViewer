//! The window: a header bar, a sidebar (Contents · Notes) and the document.
//! Files are read and parsed off the main thread so a large book never
//! freezes the UI.
//!
//! Editing lives here too. A PDF is changed through `pdfedit::Editor`, which
//! hands back the whole new file; the view swaps it in under the pages it is
//! already showing, and the previous file goes on the undo stack. A DOCX is
//! edited in its text view and written out on save.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{gdk, gio, glib};
use libadwaita as adw;

use crate::config::ViewerConfig;
use crate::docx::ParaStyle;
use crate::docxview::{DocxView, Inline};
use crate::pdf::{self, DocumentInfo};
use crate::pdfedit::{self, Edit, Markup};
use crate::pdfview::{PdfView, Spot};
use crate::{docx, theme};

pub const APP_ID: &str = "com.ravenviewer.Raven";

/// A PDF's edit history. `bytes` is the file as it now is, `saved` as it is
/// on disk; they are the same allocation until something changes.
struct PdfEdits {
    editor: pdfedit::Editor,
    bytes: RefCell<Arc<Vec<u8>>>,
    saved: RefCell<Arc<Vec<u8>>>,
    undo: RefCell<Vec<Arc<Vec<u8>>>>,
    redo: RefCell<Vec<Arc<Vec<u8>>>>,
    /// An edit is on its way; another would race it for the undo stack.
    busy: Cell<bool>,
}

impl PdfEdits {
    fn dirty(&self) -> bool {
        !Arc::ptr_eq(&self.bytes.borrow(), &self.saved.borrow())
    }
}

enum Doc {
    Pdf {
        path: PathBuf,
        view: PdfView,
        search: Rc<pdf::Searcher>,
        edits: Rc<PdfEdits>,
    },
    Docx {
        path: PathBuf,
        view: DocxView,
    },
}

impl Doc {
    fn path(&self) -> &Path {
        match self {
            Doc::Pdf { path, .. } | Doc::Docx { path, .. } => path,
        }
    }

    fn dirty(&self) -> bool {
        match self {
            Doc::Pdf { edits, .. } => edits.dirty(),
            Doc::Docx { view, .. } => view.is_modified(),
        }
    }
}

/// The DOCX formatting bar.
struct FormatBar {
    bar: gtk::Revealer,
    style: gtk::DropDown,
    toggles: Vec<(&'static str, gtk::ToggleButton)>,
    /// Set while the bar is being made to match the cursor, so it does not
    /// apply what it is only showing.
    syncing: Cell<bool>,
}

const STYLES: [(&str, ParaStyle); 7] = [
    ("Normal", ParaStyle::Normal),
    ("Title", ParaStyle::Title),
    ("Heading 1", ParaStyle::Heading(1)),
    ("Heading 2", ParaStyle::Heading(2)),
    ("Heading 3", ParaStyle::Heading(3)),
    ("Quote", ParaStyle::Quote),
    ("Bulleted List", ParaStyle::ListItem(0)),
];

const HIGHLIGHT_COLORS: [(&str, &str, [f32; 3]); 4] = [
    ("yellow", "Yellow", [1.0, 0.86, 0.1]),
    ("green", "Green", [0.45, 0.9, 0.35]),
    ("blue", "Blue", [0.4, 0.75, 1.0]),
    ("pink", "Pink", [1.0, 0.5, 0.75]),
];

struct State {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    split: adw::OverlaySplitView,
    content: gtk::Stack,
    /// Where popovers over the document are anchored; it outlives any one
    /// document's view.
    overlay: gtk::Overlay,
    toasts: adw::ToastOverlay,
    outline: gtk::ListBox,
    notes: gtk::ListBox,
    pill: gtk::MenuButton,
    pill_label: gtk::Label,
    page_entry: gtk::Entry,
    page_total: gtk::Label,
    zoom_label: gtk::Button,
    search_bar: gtk::SearchBar,
    search: gtk::SearchEntry,
    pdf_only: Vec<gtk::Widget>,
    doc_only: Vec<gtk::Widget>,
    edit_btn: gtk::ToggleButton,
    format: FormatBar,
    selection_tools: gtk::Popover,
    context_menu: gtk::PopoverMenu,
    /// Where the context menu was opened, for the actions it offers.
    context_spot: Cell<Option<Spot>>,
    context_annotation: Cell<Option<usize>>,
    /// The sidebar lists are refilled on every edit; each refill replaces
    /// its row handler rather than stacking another on top.
    outline_handler: RefCell<Option<glib::SignalHandlerId>>,
    notes_handler: RefCell<Option<glib::SignalHandlerId>>,
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
        let save_btn = icon_button("document-save-symbolic", "Save (Ctrl+S)", "win.save");
        header.pack_start(&sidebar_btn);
        header.pack_start(&open_btn);
        header.pack_start(&save_btn);

        let menu = gio::Menu::new();
        let file_section = gio::Menu::new();
        file_section.append(Some("Save As…"), Some("win.save-as"));
        file_section.append(Some("Combine Files…"), Some("win.combine"));
        menu.append_section(None, &file_section);
        let page_section = gio::Menu::new();
        page_section.append(Some("Rotate Page Right"), Some("win.rotate-right"));
        page_section.append(Some("Rotate Page Left"), Some("win.rotate-left"));
        page_section.append(Some("Move Page Up"), Some("win.move-page-up"));
        page_section.append(Some("Move Page Down"), Some("win.move-page-down"));
        page_section.append(Some("Delete Page"), Some("win.delete-page"));
        menu.append_submenu(Some("Page"), &page_section);
        let colors = gio::Menu::new();
        for (id, label, _) in HIGHLIGHT_COLORS {
            colors.append(Some(label), Some(&format!("win.highlight-color::{id}")));
        }
        menu.append_submenu(Some("Highlight Color"), &colors);
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

        let edit_btn = gtk::ToggleButton::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Edit (Ctrl+E)")
            .action_name("win.edit")
            .css_classes(["flat"])
            .build();
        header.pack_end(&edit_btn);

        // ── DOCX formatting ─────────────────────────────────────────────
        let format = format_bar();

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
        let combine_cta = gtk::Button::builder()
            .label("Combine Files…")
            .action_name("win.combine")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        let ctas = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).halign(gtk::Align::Center).build();
        ctas.append(&open_cta);
        ctas.append(&combine_cta);
        welcome.set_child(Some(&ctas));

        let content = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        content.add_named(&welcome, Some("welcome"));

        // The page counter doubles as "go to page": in a 700-page book the
        // scrollbar is a blunt instrument and not every file has an outline.
        let page_entry = gtk::Entry::builder()
            .input_purpose(gtk::InputPurpose::Digits)
            .max_width_chars(7)
            .width_chars(7)
            .xalign(0.5)
            .build();
        let page_total = gtk::Label::builder().css_classes(["dim-label"]).build();
        let jump = gtk::Box::builder().spacing(8).css_classes(["jump-to-page"]).build();
        jump.append(&gtk::Label::new(Some("Page")));
        jump.append(&page_entry);
        jump.append(&page_total);
        let pill_label = gtk::Label::new(None);
        let pill = gtk::MenuButton::builder()
            .popover(&gtk::Popover::builder().child(&jump).build())
            .child(&pill_label)
            .always_show_arrow(true)
            .tooltip_text("Go to page (Ctrl+G)")
            .css_classes(["page-pill", "flat"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .visible(false)
            .build();
        let overlay = gtk::Overlay::builder().child(&content).build();
        overlay.add_overlay(&pill);

        // What can be done with selected text, offered where it ends.
        let tools = gtk::Box::builder().spacing(2).css_classes(["selection-tools"]).build();
        for (icon, tip, action) in [
            ("edit-copy-symbolic", "Copy (Ctrl+C)", "win.copy"),
            ("marker-symbolic", "Highlight", "win.highlight"),
            ("format-text-underline-symbolic", "Underline", "win.underline"),
            ("format-text-strikethrough-symbolic", "Strike Out", "win.strike"),
            ("document-edit-symbolic", "Add Note", "win.add-note"),
        ] {
            tools.append(&icon_button(icon, tip, action));
        }
        let selection_tools = gtk::Popover::builder()
            .child(&tools)
            .autohide(false)
            .has_arrow(true)
            .position(gtk::PositionType::Bottom)
            .can_focus(false)
            .build();
        selection_tools.set_parent(&overlay);
        let context_menu = gtk::PopoverMenu::builder().has_arrow(false).halign(gtk::Align::Start).build();
        context_menu.set_parent(&overlay);

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
        toolbar.add_top_bar(&format.bar);
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

        let pdf_only: Vec<gtk::Widget> = vec![zoom_box.upcast(), search_btn.upcast()];
        let doc_only: Vec<gtk::Widget> = vec![sidebar_btn.upcast(), save_btn.upcast()];
        let this = Window(Rc::new(State {
            window,
            title,
            split,
            content,
            overlay,
            toasts,
            outline,
            notes,
            pill,
            pill_label,
            page_entry,
            page_total,
            zoom_label,
            search_bar,
            search,
            pdf_only,
            doc_only,
            edit_btn,
            format,
            selection_tools,
            context_menu,
            context_spot: Cell::new(None),
            context_annotation: Cell::new(None),
            outline_handler: RefCell::new(None),
            notes_handler: RefCell::new(None),
            doc: RefCell::new(None),
            config: RefCell::new(config),
        }));
        this.install_actions();
        this.install_drop();
        this.connect_signals();
        this.connect_format_bar();
        this.sync_controls();
        this
    }

    pub fn present(&self) {
        self.0.window.present();
    }

    fn toast(&self, text: &str) {
        self.0.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(4).build());
    }

    fn is_pdf(&self) -> bool {
        matches!(self.0.doc.borrow().as_ref(), Some(Doc::Pdf { .. }))
    }

    fn is_docx(&self) -> bool {
        matches!(self.0.doc.borrow().as_ref(), Some(Doc::Docx { .. }))
    }

    fn dirty(&self) -> bool {
        self.0.doc.borrow().as_ref().is_some_and(Doc::dirty)
    }

    /// Show what applies to the open document and enable what can be done
    /// with it. PDF-only actions are *disabled* for a DOCX rather than merely
    /// doing nothing, so their single-key shortcuts (N, P) fall through to
    /// the text being edited.
    fn sync_controls(&self) {
        let (any, pdf, docx) = (self.0.doc.borrow().is_some(), self.is_pdf(), self.is_docx());
        for w in &self.0.pdf_only {
            w.set_visible(pdf);
        }
        for w in &self.0.doc_only {
            w.set_visible(any);
        }
        self.0.edit_btn.set_visible(docx);
        // Search can take over any key typed at the window; a DOCX is typed
        // into instead.
        let capture: Option<gtk::Widget> = pdf.then(|| self.0.window.clone().upcast());
        self.0.search_bar.set_key_capture_widget(capture.as_ref());
        let editing = docx && self.action_state("edit");
        self.0.format.bar.set_reveal_child(editing);

        let (has_selection, can_undo, can_redo) = match self.0.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, edits, .. }) => {
                (view.has_selection(), !edits.undo.borrow().is_empty(), !edits.redo.borrow().is_empty())
            }
            Some(Doc::Docx { view, .. }) => {
                (view.buffer().has_selection(), view.buffer().can_undo(), view.buffer().can_redo())
            }
            None => (false, false, false),
        };
        let pdf_actions = [
            "zoom-in", "zoom-out", "fit", "next-page", "prev-page", "first-page", "last-page", "go-to-page", "find",
            "add-note", "add-text", "rotate-right", "rotate-left", "delete-page", "move-page-up", "move-page-down",
            "highlight-color",
        ];
        for name in pdf_actions {
            self.enable(name, pdf);
        }
        for name in ["underline", "strike"] {
            self.enable(name, pdf && has_selection);
        }
        self.enable("highlight", (pdf && has_selection) || docx);
        self.enable("copy", has_selection);
        self.enable("edit", docx);
        for name in ["bold", "italic", "underline-text"] {
            self.enable(name, editing);
        }
        self.enable("undo", can_undo);
        self.enable("redo", can_redo);
        self.enable("save", self.dirty());
        self.enable("save-as", any);
        self.enable("sidebar", any);
        self.update_title();
    }

    fn enable(&self, name: &str, on: bool) {
        if let Some(a) = self.0.window.lookup_action(name).and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
            a.set_enabled(on);
        }
    }

    fn action_state(&self, name: &str) -> bool {
        self.0.window.lookup_action(name).and_then(|a| a.state()).and_then(|s| s.get::<bool>()).unwrap_or(false)
    }

    fn update_title(&self) {
        let doc = self.0.doc.borrow();
        let Some(doc) = doc.as_ref() else { return };
        let name = display_name(doc.path());
        let mark = if doc.dirty() { "• " } else { "" };
        let shown = match doc {
            Doc::Pdf { view, .. } => view.info().title.clone().unwrap_or_else(|| name.clone()),
            Doc::Docx { .. } => name.clone(),
        };
        self.0.title.set_title(&format!("{mark}{shown}"));
        self.0.window.set_title(Some(&format!("{mark}{name} — Raven Viewer")));
    }

    // ── Opening ─────────────────────────────────────────────────────────

    pub fn open(&self, file: gio::File) {
        let Some(path) = file.path() else {
            self.toast("Only local files can be opened");
            return;
        };
        let this = self.clone();
        self.confirm_discard(move || {
            let (this, path) = (this.clone(), path.clone());
            glib::spawn_future_local(async move {
                let p = path.clone();
                let loaded = gio::spawn_blocking(move || load(&p)).await;
                match loaded {
                    Ok(Ok(Loaded::Pdf(bytes, info))) => this.show_pdf(path, bytes, info),
                    Ok(Ok(Loaded::Docx(doc))) => this.show_docx(path, doc),
                    Ok(Err(e)) => this.toast(&format!("Couldn’t open {}: {e}", display_name(&path))),
                    Err(_) => this.toast("Couldn’t open the file"),
                }
            });
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
        self.0.selection_tools.popdown();
        self.0.context_menu.popdown();
        if let Some(old) = self.0.content.child_by_name("doc") {
            self.0.content.remove(&old);
        }
        self.0.content.add_named(widget, Some("doc"));
        self.0.content.set_visible_child_name("doc");
    }

    fn show_pdf(&self, path: PathBuf, bytes: Arc<Vec<u8>>, info: DocumentInfo) {
        self.remember_position();
        let view = match self.build_pdf_view(bytes.clone(), info.clone()) {
            Ok(v) => v,
            Err(e) => return self.toast(&e.to_string()),
        };
        self.replace_content(view.widget());
        let dark = self.0.config.borrow().dark_pages;
        view.set_dark_pages(dark);
        let resume = {
            let cfg = self.0.config.borrow();
            cfg.remember_position.then(|| cfg.positions.get(path.to_string_lossy().as_ref()).copied()).flatten()
        };
        let has_sidebar = !info.outline.is_empty() || !info.annotations.is_empty();
        self.0.split.set_show_sidebar(has_sidebar && self.0.config.borrow().show_sidebar);

        let search = Rc::new(pdf::Searcher::spawn(bytes.clone(), view.page_count()));
        let edits = Rc::new(PdfEdits {
            editor: pdfedit::Editor::spawn(bytes.clone()),
            bytes: RefCell::new(bytes.clone()),
            saved: RefCell::new(bytes),
            undo: RefCell::default(),
            redo: RefCell::default(),
            busy: Cell::new(false),
        });
        *self.0.doc.borrow_mut() = Some(Doc::Pdf { path, view: view.clone(), search, edits });
        self.set_edit_state(false);
        self.after_pdf_changed(&view);

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

    /// A page view wired to this window.
    fn build_pdf_view(&self, bytes: Arc<Vec<u8>>, info: DocumentInfo) -> anyhow::Result<PdfView> {
        let view = PdfView::new(bytes, info)?;
        let n = view.page_count();
        let (pill, subtitle) = (self.0.pill_label.clone(), self.0.title.clone());
        let update = move |page: usize| {
            let text = format!("{} of {n}", page + 1);
            pill.set_label(&text);
            subtitle.set_subtitle(&format!("Page {text}"));
        };
        update(0);
        view.connect_page_changed(update);
        let zoom_label = self.0.zoom_label.clone();
        view.connect_zoom_changed(move |z| zoom_label.set_label(&format!("{:.0}%", z * 100.0)));

        let this = self.clone();
        view.connect_selection_changed(move |on| {
            if !on {
                this.0.selection_tools.popdown();
            }
            this.sync_controls();
        });
        let (this, weak_view) = (self.clone(), view.widget().downgrade());
        view.connect_selection_done(move |rect| {
            let Some(scroller) = weak_view.upgrade() else { return };
            if let Some(rect) = translate(&scroller, &this.0.overlay, rect) {
                this.0.selection_tools.set_pointing_to(Some(&rect));
                this.0.selection_tools.popup();
            }
        });
        // Scrolling moves the selection out from under the tools.
        let tools = self.0.selection_tools.clone();
        view.widget().vadjustment().connect_value_changed(move |_| tools.popdown());
        let (this, weak_view) = (self.clone(), view.widget().downgrade());
        view.connect_context_menu(move |spot, x, y| {
            let Some(scroller) = weak_view.upgrade() else { return };
            this.show_context_menu(&scroller, spot, x, y);
        });

        self.0.page_total.set_label(&format!("of {n}"));
        self.0.pill.set_visible(true);
        Ok(view)
    }

    /// Refresh everything that is derived from the document's contents.
    fn after_pdf_changed(&self, view: &PdfView) {
        let info = view.info().clone();
        self.fill_outline(&info, view);
        self.fill_notes(&info, view);
        self.sync_controls();
    }

    fn show_docx(&self, path: PathBuf, doc: docx::Docx) {
        self.remember_position();
        let view = DocxView::new(doc);
        self.replace_content(view.widget());
        self.0.title.set_subtitle("Document");
        self.0.pill.set_visible(false);
        self.0.outline.remove_all();
        self.0.notes.remove_all();
        self.0.split.set_show_sidebar(false);

        let format = self.clone();
        view.connect_format_changed(move |style, inline| format.show_format(style, inline));
        let this = self.clone();
        view.buffer().connect_modified_changed(move |_| this.sync_controls());
        let this = self.clone();
        view.buffer().connect_has_selection_notify(move |_| this.sync_controls());
        let this = self.clone();
        view.buffer().connect_can_undo_notify(move |_| this.sync_controls());
        let this = self.clone();
        view.buffer().connect_can_redo_notify(move |_| this.sync_controls());
        self.add_docx_menu(&view);
        self.add_docx_shortcuts(&view);

        *self.0.doc.borrow_mut() = Some(Doc::Docx { path, view });
        self.set_edit_state(false);
        self.sync_controls();
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
        let entries = info.outline.clone();
        let view = view.downgrade();
        let split = self.0.split.clone();
        if let Some(old) = self.0.outline_handler.take() {
            list.disconnect(old);
        }
        let id = list.connect_row_activated(move |_, row| {
            if let (Some(e), Some(view)) = (entries.get(row.index() as usize), view.upgrade()) {
                view.go_to(e.page, 0.0);
                if split.is_collapsed() {
                    split.set_show_sidebar(false);
                }
            }
        });
        *self.0.outline_handler.borrow_mut() = Some(id);
    }

    fn fill_notes(&self, info: &DocumentInfo, view: &PdfView) {
        let list = &self.0.notes;
        list.remove_all();
        if info.annotations.is_empty() {
            list.set_placeholder(Some(&placeholder("No annotations")));
        }
        for (i, a) in info.annotations.iter().enumerate() {
            let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
            let kind = gtk::Label::builder()
                .label(format!("{} · PAGE {}", a.kind.to_uppercase(), a.page + 1))
                .xalign(0.0)
                .css_classes(["annot-kind"])
                .build();
            text.append(&kind);
            if !a.contents.is_empty() {
                text.append(
                    &gtk::Label::builder()
                        .label(&a.contents)
                        .xalign(0.0)
                        .wrap(true)
                        .lines(3)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .build(),
                );
            }
            let row = gtk::Box::builder().spacing(4).css_classes(["outline-row"]).build();
            row.append(&text);
            if a.id.is_some() {
                let delete = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Delete")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .build();
                let this = self.clone();
                delete.connect_clicked(move |_| this.delete_annotation(i));
                row.append(&delete);
            }
            list.append(&row);
        }
        let notes = info.annotations.clone();
        let view = view.downgrade();
        if let Some(old) = self.0.notes_handler.take() {
            list.disconnect(old);
        }
        let id = list.connect_row_activated(move |_, row| {
            if let (Some(a), Some(view)) = (notes.get(row.index() as usize), view.upgrade()) {
                view.go_to(a.page, a.y_fraction);
            }
        });
        *self.0.notes_handler.borrow_mut() = Some(id);
    }

    fn pdf_page_count(&self) -> usize {
        match self.0.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, .. }) => view.page_count(),
            _ => 0,
        }
    }

    fn with_pdf(&self, f: impl FnOnce(&PdfView)) {
        let view = match self.0.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, .. }) => view.clone(),
            _ => return,
        };
        f(&view);
    }

    fn pdf(&self) -> Option<(PdfView, Rc<PdfEdits>)> {
        match self.0.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, edits, .. }) => Some((view.clone(), edits.clone())),
            _ => None,
        }
    }

    fn docx(&self) -> Option<DocxView> {
        match self.0.doc.borrow().as_ref() {
            Some(Doc::Docx { view, .. }) => Some(view.clone()),
            _ => None,
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

    // ── PDF editing ─────────────────────────────────────────────────────

    /// Apply an edit and show the result. The page being read stays where
    /// it is, and the previous file goes on the undo stack.
    fn pdf_edit(&self, edit: Edit, done: &'static str) {
        let Some((_, edits)) = self.pdf() else { return };
        if edits.busy.replace(true) {
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = edits.editor.apply(edit).await;
            match result {
                Ok(bytes) => {
                    let before = edits.bytes.replace(bytes.clone());
                    edits.undo.borrow_mut().push(before);
                    edits.redo.borrow_mut().clear();
                    this.install_pdf(bytes).await;
                    if !done.is_empty() {
                        this.toast(done);
                    }
                }
                Err(e) => this.toast(&format!("Couldn’t change the PDF: {e}")),
            }
            edits.busy.set(false);
            this.sync_controls();
        });
    }

    /// Step through the edit history: `back` for undo, otherwise redo.
    fn pdf_history(&self, back: bool) {
        let Some((_, edits)) = self.pdf() else { return };
        let target = if back { edits.undo.borrow_mut().pop() } else { edits.redo.borrow_mut().pop() };
        let Some(target) = target else { return };
        if edits.busy.replace(true) {
            // Put it back; the history must not lose a step.
            if back { edits.undo.borrow_mut().push(target) } else { edits.redo.borrow_mut().push(target) }
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            match edits.editor.reset(target.clone()).await {
                Ok(bytes) => {
                    let current = edits.bytes.replace(bytes.clone());
                    if back { edits.redo.borrow_mut().push(current) } else { edits.undo.borrow_mut().push(current) }
                    this.install_pdf(bytes).await;
                }
                Err(e) => this.toast(&e),
            }
            edits.busy.set(false);
            this.sync_controls();
        });
    }

    /// Show a new copy of the open PDF, keeping the reader's place.
    async fn install_pdf(&self, bytes: Arc<Vec<u8>>) {
        let b = bytes.clone();
        let info = match gio::spawn_blocking(move || pdf::load_info(&b)).await {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => return self.toast(&format!("The changed PDF didn’t load: {e}")),
            Err(_) => return,
        };
        let Some((view, _)) = self.pdf() else { return };
        let view = if view.reload(bytes.clone(), info.clone()) {
            view
        } else {
            // Pages came or went: build the view afresh, at the same zoom,
            // on the same page.
            let (page, frac) = view.position();
            let (zoom, fit) = (view.zoom(), view.is_fit_width());
            let fresh = match self.build_pdf_view(bytes.clone(), info) {
                Ok(v) => v,
                Err(e) => return self.toast(&e.to_string()),
            };
            fresh.set_dark_pages(self.0.config.borrow().dark_pages);
            self.replace_content(fresh.widget());
            let restored = fresh.clone();
            glib::idle_add_local_once(move || restored.restore(zoom, fit, page, frac));
            fresh
        };
        if let Some(Doc::Pdf { view: v, search, .. }) = self.0.doc.borrow_mut().as_mut() {
            *v = view.clone();
            *search = Rc::new(pdf::Searcher::spawn(bytes, view.page_count()));
        }
        self.after_pdf_changed(&view);
    }

    fn author() -> String {
        let real = glib::real_name().to_string_lossy().into_owned();
        if real.is_empty() || real == "Unknown" { glib::user_name().to_string_lossy().into_owned() } else { real }
    }

    fn highlight_color(&self) -> [f32; 3] {
        let id = self.0.config.borrow().highlight_color.clone();
        HIGHLIGHT_COLORS.iter().find(|(i, _, _)| *i == id).map_or(HIGHLIGHT_COLORS[0].2, |c| c.2)
    }

    fn mark_selection(&self, kind: Markup) {
        let Some((view, _)) = self.pdf() else { return };
        let pieces = view.selection();
        if pieces.is_empty() {
            return;
        }
        let color = match kind {
            Markup::Highlight => self.highlight_color(),
            Markup::Underline => [0.1, 0.45, 0.9],
            Markup::StrikeOut => [0.85, 0.15, 0.15],
        };
        self.0.selection_tools.popdown();
        // One annotation per page the selection covers; each is its own
        // step, so a multi-page highlight is applied as a chain.
        let this = self.clone();
        let edits_for: Vec<Edit> = pieces
            .into_iter()
            .map(|p| Edit::Markup { page: p.page, kind, boxes: p.user_boxes, color, text: p.text, author: Self::author() })
            .collect();
        glib::spawn_future_local(async move {
            for edit in edits_for {
                this.pdf_edit(edit, "");
                // Wait for each to land before the next.
                while this.pdf().is_some_and(|(_, e)| e.busy.get()) {
                    glib::timeout_future(std::time::Duration::from_millis(15)).await;
                }
            }
        });
    }

    /// The page an action applies to: where the context menu was opened, or
    /// the page being read.
    fn target_page(&self) -> Option<usize> {
        let (view, _) = self.pdf()?;
        Some(self.0.context_spot.get().map_or(view.current_page(), |s| s.page))
    }

    fn page_edit(&self, make: impl FnOnce(usize, usize) -> Option<Edit>) {
        let (Some(page), count) = (self.target_page(), self.pdf_page_count()) else { return };
        if let Some(edit) = make(page, count) {
            self.pdf_edit(edit, "");
        }
        self.0.context_spot.set(None);
    }

    /// Ask for text, then place a note or a text box with it at the spot the
    /// menu was opened on — or, from the selection tools, beside the
    /// selection.
    fn add_note(&self, text_box: bool) {
        let Some((view, _)) = self.pdf() else { return };
        let selection = view.selection();
        let spot = self.0.context_spot.take().or_else(|| {
            // Beside the end of the selection, in the margin's direction.
            let last = selection.last()?;
            let b = last.user_boxes.last()?;
            let m = view.info().to_user.get(last.page).copied()?;
            // Invert the fraction→user map for the box's top-right corner.
            let det = m[0] * m[3] - m[1] * m[2];
            if det.abs() < 1e-9 {
                return None;
            }
            let (x, y) = (b[2] as f64 - m[4], b[3] as f64 - m[5]);
            let fx = (m[3] * x - m[2] * y) / det;
            let fy = (-m[1] * x + m[0] * y) / det;
            Some(Spot { page: last.page, x: (fx as f32 + 0.01).min(0.95), y: fy as f32 })
        });
        let spot = spot.unwrap_or(Spot { page: view.current_page(), x: 0.1, y: 0.1 });
        let quoted: String = selection.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ");
        let (heading, body) = if text_box {
            ("Add Text", "The text is drawn onto the page.")
        } else {
            ("Add Note", "A sticky note, shown as an icon on the page.")
        };
        let this = self.clone();
        self.ask_text(heading, body, "", move |text| {
            let text = text.trim().to_string();
            if text.is_empty() {
                return;
            }
            let (x, y) = view.info().user_point(spot.page, spot.x, spot.y);
            let author = Self::author();
            let edit = if text_box {
                Edit::TextBox { page: spot.page, at: (x, y), text, size: 12.0, author }
            } else {
                let text = if quoted.is_empty() { text } else { format!("{text}\n\n“{quoted}”") };
                Edit::Note { page: spot.page, at: (x, y), text, author }
            };
            view.clear_selection();
            this.pdf_edit(edit, "");
        });
    }

    fn edit_annotation(&self, index: usize) {
        let Some((view, _)) = self.pdf() else { return };
        let Some(a) = view.info().annotations.get(index).cloned() else { return };
        let Some(id) = a.id else { return };
        let this = self.clone();
        self.ask_text("Edit Note", &format!("{} on page {}", a.kind, a.page + 1), &a.contents, move |text| {
            this.pdf_edit(Edit::SetContents { annotation: id, text }, "");
        });
    }

    fn delete_annotation(&self, index: usize) {
        let Some((view, _)) = self.pdf() else { return };
        let id = view.info().annotations.get(index).and_then(|a| a.id);
        if let Some(id) = id {
            self.pdf_edit(Edit::RemoveAnnotation { annotation: id }, "Annotation deleted");
        }
    }

    /// A dialog with a text area; `then` gets the text if it is confirmed.
    fn ask_text(&self, heading: &str, body: &str, initial: &str, then: impl Fn(String) + 'static) {
        let area = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        area.buffer().set_text(initial);
        let focus = area.clone();
        let frame = gtk::ScrolledWindow::builder()
            .child(&area)
            .min_content_height(110)
            .max_content_height(260)
            .propagate_natural_height(true)
            .css_classes(["card"])
            .build();
        let dialog = adw::AlertDialog::builder().heading(heading).body(body).extra_child(&frame).build();
        dialog.add_responses(&[("cancel", "Cancel"), ("ok", "Save")]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, response| {
            if response == "ok" {
                let b = area.buffer();
                then(b.text(&b.start_iter(), &b.end_iter(), false).to_string());
            }
        });
        dialog.present(Some(&self.0.window));
        glib::idle_add_local_once(move || {
            focus.grab_focus();
        });
    }

    fn show_context_menu(&self, scroller: &gtk::ScrolledWindow, spot: Option<Spot>, x: f64, y: f64) {
        let Some((view, _)) = self.pdf() else { return };
        self.0.selection_tools.popdown();
        self.0.context_spot.set(spot);
        let annotation = spot.and_then(|s| view.annotation_at(s));
        self.0.context_annotation.set(annotation);

        let menu = gio::Menu::new();
        let text = gio::Menu::new();
        if view.has_selection() {
            text.append(Some("Copy"), Some("win.copy"));
            text.append(Some("Highlight"), Some("win.highlight"));
            text.append(Some("Underline"), Some("win.underline"));
            text.append(Some("Strike Out"), Some("win.strike"));
        }
        text.append(Some("Select Page Text"), Some("win.select-page"));
        menu.append_section(None, &text);
        if spot.is_some() {
            let add = gio::Menu::new();
            add.append(Some("Add Note Here…"), Some("win.add-note"));
            add.append(Some("Add Text Here…"), Some("win.add-text"));
            menu.append_section(None, &add);
        }
        if let Some(a) = annotation.and_then(|i| view.info().annotations.get(i).cloned()).filter(|a| a.id.is_some()) {
            let this_one = gio::Menu::new();
            if matches!(a.kind.as_str(), "Text" | "Highlight" | "Underline" | "StrikeOut" | "Squiggly") {
                this_one.append(Some("Edit Note…"), Some("win.edit-annotation"));
            }
            this_one.append(Some(&format!("Delete {}", friendly_kind(&a.kind))), Some("win.delete-annotation"));
            menu.append_section(None, &this_one);
        }
        if let Some(s) = spot {
            let pages = gio::Menu::new();
            pages.append(Some("Rotate Right"), Some("win.rotate-right"));
            pages.append(Some("Rotate Left"), Some("win.rotate-left"));
            if s.page > 0 {
                pages.append(Some("Move Up"), Some("win.move-page-up"));
            }
            if s.page + 1 < view.page_count() {
                pages.append(Some("Move Down"), Some("win.move-page-down"));
            }
            if view.page_count() > 1 {
                pages.append(Some("Delete Page"), Some("win.delete-page"));
            }
            menu.append_submenu(Some(&format!("Page {}", s.page + 1)), &pages);
        }
        self.0.context_menu.set_menu_model(Some(&menu));
        let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        if let Some(rect) = translate(scroller, &self.0.overlay, rect) {
            self.0.context_menu.set_pointing_to(Some(&rect));
            self.0.context_menu.popup();
        }
    }

    // ── DOCX editing ────────────────────────────────────────────────────

    fn set_edit_state(&self, on: bool) {
        if let Some(a) = self.0.window.lookup_action("edit").and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
            a.set_state(&on.to_variant());
        }
        if let Some(view) = self.docx() {
            view.set_editable(on);
        }
        self.sync_controls();
    }

    fn show_format(&self, style: ParaStyle, inline: Inline) {
        let f = &self.0.format;
        f.syncing.set(true);
        let at = STYLES.iter().position(|(_, s)| match (s, style) {
            (ParaStyle::ListItem(_), ParaStyle::ListItem(_)) => true,
            (ParaStyle::Heading(a), ParaStyle::Heading(b)) => *a == b.min(3),
            (a, b) => *a == b,
        });
        f.style.set_selected(at.unwrap_or(0) as u32);
        for (name, button) in &f.toggles {
            button.set_active(match *name {
                "bold" => inline.bold,
                "italic" => inline.italic,
                "underline" => inline.underline,
                _ => inline.highlight,
            });
        }
        f.syncing.set(false);
    }

    fn connect_format_bar(&self) {
        let f = &self.0.format;
        let this = self.clone();
        f.style.connect_selected_notify(move |drop| {
            if this.0.format.syncing.get() {
                return;
            }
            if let (Some(view), Some((_, style))) = (this.docx(), STYLES.get(drop.selected() as usize)) {
                view.set_style(*style);
                view.text_view().grab_focus();
            }
        });
        for (name, button) in &f.toggles {
            let (this, name) = (self.clone(), *name);
            button.connect_toggled(move |_| {
                if this.0.format.syncing.get() {
                    return;
                }
                if let Some(view) = this.docx() {
                    view.toggle(name);
                    view.text_view().grab_focus();
                }
            });
        }
    }

    /// Highlight in the text view's own right-click menu, next to Copy.
    fn add_docx_menu(&self, view: &DocxView) {
        let menu = gio::Menu::new();
        menu.append(Some("Highlight"), Some("win.highlight"));
        view.text_view().set_extra_menu(Some(&menu));
    }

    /// Bold, italic and underline while editing. On the text view rather
    /// than the window, so they never reach the search box or the PDF view.
    fn add_docx_shortcuts(&self, view: &DocxView) {
        let keys = gtk::ShortcutController::new();
        for (trigger, name) in [("<Control>b", "bold"), ("<Control>i", "italic"), ("<Control>u", "underline")] {
            let v = view.clone();
            keys.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::CallbackAction::new(move |_, _| {
                    if v.text_view().is_editable() {
                        v.toggle(name);
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                })),
            ));
        }
        view.text_view().add_controller(keys);
    }

    // ── Saving ──────────────────────────────────────────────────────────

    /// The document as it now is, ready to write.
    fn current_bytes(&self) -> anyhow::Result<Vec<u8>> {
        match self.0.doc.borrow().as_ref() {
            Some(Doc::Pdf { edits, .. }) => Ok(edits.bytes.borrow().as_ref().clone()),
            Some(Doc::Docx { view, .. }) => view.save(),
            None => anyhow::bail!("nothing is open"),
        }
    }

    fn save(&self, then: Option<Box<dyn FnOnce()>>) {
        let Some(path) = self.0.doc.borrow().as_ref().map(|d| d.path().to_path_buf()) else { return };
        self.save_to(path, then);
    }

    fn save_to(&self, path: PathBuf, then: Option<Box<dyn FnOnce()>>) {
        let bytes = match self.current_bytes() {
            Ok(b) => b,
            Err(e) => return self.toast(&format!("Couldn’t save: {e}")),
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let written = gio::spawn_blocking(move || write_atomically(&p, &bytes)).await;
            match written {
                Ok(Ok(())) => {
                    // The borrow ends before the DOCX buffer is told it is
                    // saved: that emits a signal whose handler reads the
                    // document again.
                    let docx = match this.0.doc.borrow_mut().as_mut() {
                        Some(Doc::Pdf { path: p, edits, .. }) => {
                            *p = path.clone();
                            *edits.saved.borrow_mut() = edits.bytes.borrow().clone();
                            None
                        }
                        Some(Doc::Docx { path: p, view }) => {
                            *p = path.clone();
                            Some(view.clone())
                        }
                        None => None,
                    };
                    if let Some(view) = docx {
                        view.set_unmodified();
                    }
                    this.sync_controls();
                    this.toast(&format!("Saved {}", display_name(&path)));
                    if let Some(then) = then {
                        then();
                    }
                }
                Ok(Err(e)) => this.toast(&format!("Couldn’t save {}: {e}", display_name(&path))),
                Err(_) => this.toast("Couldn’t save"),
            }
        });
    }

    fn save_as(&self) {
        let Some(path) = self.0.doc.borrow().as_ref().map(|d| d.path().to_path_buf()) else { return };
        let dialog = gtk::FileDialog::builder().title("Save As").modal(true).initial_name(display_name(&path)).build();
        if let Some(dir) = path.parent() {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let this = self.clone();
        dialog.save(Some(&self.0.window), gio::Cancellable::NONE, move |res| {
            if let Some(path) = res.ok().and_then(|f| f.path()) {
                this.save_to(path, None);
            }
        });
    }

    /// Run `then` once any unsaved changes are saved or given up — or not at
    /// all if the reader thinks better of it.
    fn confirm_discard(&self, then: impl Fn() + 'static) {
        if !self.dirty() {
            return then();
        }
        let name = self.0.doc.borrow().as_ref().map(|d| display_name(d.path())).unwrap_or_default();
        let dialog = adw::AlertDialog::builder()
            .heading("Save Changes?")
            .body(format!("“{name}” has changes that haven’t been saved."))
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard"), ("save", "Save")]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let this = self.clone();
        let then = Rc::new(then);
        dialog.connect_response(None, move |_, response| match response {
            "discard" => {
                // Forget the changes so the next check lets it through.
                match this.0.doc.borrow().as_ref() {
                    Some(Doc::Pdf { edits, .. }) => *edits.saved.borrow_mut() = edits.bytes.borrow().clone(),
                    Some(Doc::Docx { view, .. }) => view.set_unmodified(),
                    None => {}
                }
                then();
            }
            "save" => {
                let then = then.clone();
                this.save(Some(Box::new(move || then())));
            }
            _ => {}
        });
        dialog.present(Some(&self.0.window));
    }

    // ── Combining ───────────────────────────────────────────────────────

    /// A list of files to put together — the open one first, if it can be —
    /// that can be added to, reordered and trimmed before combining.
    fn combine_dialog(&self) {
        let files: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
        if let Some(doc) = self.0.doc.borrow().as_ref() {
            files.borrow_mut().push(doc.path().to_path_buf());
        }
        let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
        let add = gtk::Button::builder().label("Add Files…").css_classes(["pill"]).halign(gtk::Align::Start).build();
        let go = gtk::Button::builder().label("Combine…").css_classes(["pill", "suggested-action"]).halign(gtk::Align::End).hexpand(true).build();
        let hint = gtk::Label::builder()
            .label("PDFs are joined page by page; DOCX files one after another, each starting on a new page.")
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        let buttons = gtk::Box::builder().spacing(12).build();
        buttons.append(&add);
        buttons.append(&go);
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        page.append(&hint);
        page.append(&list);
        page.append(&buttons);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        let dialog = adw::Dialog::builder().title("Combine Files").content_width(520).child(&view).build();

        // Rebuild the rows from `files` whenever it changes.
        type Refresh = Rc<RefCell<Option<Box<dyn Fn()>>>>;
        let refresh: Refresh = Rc::default();
        let render = {
            let (files, list, go, refresh) = (files.clone(), list.clone(), go.clone(), refresh.clone());
            move || {
                list.remove_all();
                let names = files.borrow().clone();
                if names.is_empty() {
                    list.append(&adw::ActionRow::builder().title("No files yet").css_classes(["dim-label"]).build());
                }
                for (i, path) in names.iter().enumerate() {
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&display_name(path)))
                        .subtitle(glib::markup_escape_text(&path.parent().map(|p| p.display().to_string()).unwrap_or_default()))
                        .build();
                    for (icon, tip, delta) in [("go-up-symbolic", "Move up", -1i64), ("go-down-symbolic", "Move down", 1), ("list-remove-symbolic", "Remove", 0)] {
                        let b = gtk::Button::builder().icon_name(icon).tooltip_text(tip).valign(gtk::Align::Center).css_classes(["flat"]).build();
                        let (files, refresh) = (files.clone(), refresh.clone());
                        let movable = if delta < 0 { i > 0 } else if delta > 0 { i + 1 < names.len() } else { true };
                        b.set_sensitive(movable);
                        b.connect_clicked(move |_| {
                            {
                                let mut f = files.borrow_mut();
                                if delta == 0 {
                                    f.remove(i);
                                } else {
                                    f.swap(i, (i as i64 + delta) as usize);
                                }
                            }
                            if let Some(r) = refresh.borrow().as_ref() {
                                r();
                            }
                        });
                        row.add_suffix(&b);
                    }
                    list.append(&row);
                }
                go.set_sensitive(names.len() >= 2);
            }
        };
        render();
        *refresh.borrow_mut() = Some(Box::new(render));

        let (this, files_add, refresh_add) = (self.clone(), files.clone(), refresh.clone());
        add.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Add Files").modal(true).filters(&document_filters()).build();
            let (files, refresh) = (files_add.clone(), refresh_add.clone());
            dialog.open_multiple(Some(&this.0.window), gio::Cancellable::NONE, move |res| {
                let Ok(chosen) = res else { return };
                for i in 0..chosen.n_items() {
                    if let Some(path) = chosen.item(i).and_downcast::<gio::File>().and_then(|f| f.path()) {
                        files.borrow_mut().push(path);
                    }
                }
                if let Some(r) = refresh.borrow().as_ref() {
                    r();
                }
            });
        });

        let (this, weak_dialog) = (self.clone(), dialog.downgrade());
        go.connect_clicked(move |_| {
            let chosen = files.borrow().clone();
            if let Some(d) = weak_dialog.upgrade() {
                d.close();
            }
            this.combine(chosen);
        });
        dialog.present(Some(&self.0.window));
    }

    fn combine(&self, files: Vec<PathBuf>) {
        let pdf_ext = |p: &PathBuf| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let first_is_pdf = files.first().is_some_and(pdf_ext);
        let open_name = self.0.doc.borrow().as_ref().map(|d| d.path().to_path_buf());
        // The open document is combined as it is on screen, edits included.
        let current = self.current_bytes().ok();
        let suggested = files.first().map(|p| {
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{stem} (combined).{}", if first_is_pdf { "pdf" } else { "docx" })
        });
        let dialog = gtk::FileDialog::builder().title("Save Combined File").modal(true).build();
        if let Some(name) = &suggested {
            dialog.set_initial_name(Some(name));
        }
        if let Some(dir) = files.first().and_then(|p| p.parent()) {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let this = self.clone();
        dialog.save(Some(&self.0.window), gio::Cancellable::NONE, move |res| {
            let Some(out) = res.ok().and_then(|f| f.path()) else { return };
            let (files, current, open_name) = (files.clone(), current.clone(), open_name.clone());
            let this = this.clone();
            glib::spawn_future_local(async move {
                let target = out.clone();
                let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                    let mut loaded = Vec::new();
                    for path in &files {
                        let bytes = match (&current, &open_name) {
                            (Some(b), Some(open)) if open == path => b.clone(),
                            _ => std::fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", display_name(path)))?,
                        };
                        loaded.push((display_name(path), bytes));
                    }
                    let merged = combine_files(&loaded)?;
                    write_atomically(&target, &merged)?;
                    Ok(())
                })
                .await;
                match result {
                    Ok(Ok(())) => {
                        this.toast(&format!("Combined into {}", display_name(&out)));
                        if let Some(app) = this.0.window.application().and_downcast::<adw::Application>() {
                            let win = Window::new(&app);
                            win.open(gio::File::for_path(&out));
                            win.present();
                        }
                    }
                    Ok(Err(e)) => this.toast(&format!("Couldn’t combine: {e}")),
                    Err(_) => this.toast("Couldn’t combine the files"),
                }
            });
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
        add("save", Box::new(|w| w.save(None)));
        add("save-as", Box::new(|w| w.save_as()));
        add("combine", Box::new(|w| w.combine_dialog()));
        add("zoom-in", Box::new(|w| { w.with_pdf(|v| v.zoom_by(1.2)); w.sync_zoom_label(); }));
        add("zoom-out", Box::new(|w| { w.with_pdf(|v| v.zoom_by(1.0 / 1.2)); w.sync_zoom_label(); }));
        add("fit", Box::new(|w| { w.with_pdf(|v| v.fit_width()); w.sync_zoom_label(); }));
        add("next-page", Box::new(|w| w.with_pdf(|v| v.next_page())));
        add("prev-page", Box::new(|w| w.with_pdf(|v| v.prev_page())));
        add("first-page", Box::new(|w| w.with_pdf(|v| v.first_page())));
        add("last-page", Box::new(|w| w.with_pdf(|v| v.last_page())));
        add("go-to-page", Box::new(|w| {
            if w.0.pill.is_visible() {
                w.0.pill.popup();
            }
        }));
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

        // Text.
        add("copy", Box::new(|w| {
            if let Some((view, _)) = w.pdf() {
                if view.copy() {
                    w.0.selection_tools.popdown();
                    w.toast("Copied");
                }
            } else if let Some(view) = w.docx() {
                view.text_view().emit_copy_clipboard();
            }
        }));
        add("select-page", Box::new(|w| w.with_pdf(|v| v.select_page())));
        add("highlight", Box::new(|w| {
            if w.is_pdf() {
                w.mark_selection(Markup::Highlight);
            } else if let Some(view) = w.docx() {
                view.toggle("highlight");
            }
        }));
        add("underline", Box::new(|w| w.mark_selection(Markup::Underline)));
        add("strike", Box::new(|w| w.mark_selection(Markup::StrikeOut)));
        add("add-note", Box::new(|w| w.add_note(false)));
        add("add-text", Box::new(|w| w.add_note(true)));
        add("edit-annotation", Box::new(|w| {
            if let Some(i) = w.0.context_annotation.take() {
                w.edit_annotation(i);
            }
        }));
        add("delete-annotation", Box::new(|w| {
            if let Some(i) = w.0.context_annotation.take() {
                w.delete_annotation(i);
            }
        }));

        // Pages.
        add("rotate-right", Box::new(|w| w.page_edit(|page, _| Some(Edit::Rotate { page, degrees: 90 }))));
        add("rotate-left", Box::new(|w| w.page_edit(|page, _| Some(Edit::Rotate { page, degrees: -90 }))));
        add("move-page-up", Box::new(|w| w.page_edit(|page, _| (page > 0).then(|| Edit::MovePage { from: page, to: page - 1 }))));
        add("move-page-down", Box::new(|w| {
            w.page_edit(|page, count| (page + 1 < count).then(|| Edit::MovePage { from: page, to: page + 1 }))
        }));
        add("delete-page", Box::new(|w| {
            let Some(page) = w.target_page() else { return };
            let dialog = adw::AlertDialog::builder()
                .heading(format!("Delete Page {}?", page + 1))
                .body("You can undo this until you close the file.")
                .build();
            dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_close_response("cancel");
            let this = w.clone();
            dialog.connect_response(Some("delete"), move |_, _| {
                this.0.context_spot.set(Some(Spot { page, x: 0.0, y: 0.0 }));
                this.page_edit(|page, count| (count > 1).then_some(Edit::DeletePage { page }));
            });
            dialog.present(Some(&w.0.window));
        }));

        // History. A DOCX has its text view's own history.
        add("undo", Box::new(|w| {
            if w.is_pdf() {
                w.pdf_history(true);
            } else if let Some(view) = w.docx() {
                view.buffer().undo();
            }
        }));
        add("redo", Box::new(|w| {
            if w.is_pdf() {
                w.pdf_history(false);
            } else if let Some(view) = w.docx() {
                view.buffer().redo();
            }
        }));

        // DOCX formatting from the menu of shortcuts.
        add("bold", Box::new(|w| if let Some(v) = w.docx() { v.toggle("bold") }));
        add("italic", Box::new(|w| if let Some(v) = w.docx() { v.toggle("italic") }));
        add("underline-text", Box::new(|w| if let Some(v) = w.docx() { v.toggle("underline") }));

        add("set-default", Box::new(|w| w.set_default_app()));
        add("shortcuts", Box::new(|w| w.show_shortcuts()));
        add("about", Box::new(|w| {
            adw::AboutDialog::builder()
                .application_name("Raven Viewer")
                .application_icon(APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Read, mark up, edit and combine PDFs and documents on Raven Linux. Free, fast, no browser.")
                .license_type(gtk::License::MitX11)
                .website("https://github.com/javanhut/RavenViewer")
                .build()
                .present(Some(&w.0.window));
        }));
        add("close", Box::new(|w| {
            w.0.window.close();
        }));

        let edit = gio::SimpleAction::new_stateful("edit", None, &false.to_variant());
        let this = self.clone();
        edit.connect_activate(move |a, _| {
            let on = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            this.set_edit_state(on);
        });
        win.add_action(&edit);

        let color = gio::SimpleAction::new_stateful(
            "highlight-color",
            Some(glib::VariantTy::STRING),
            &self.0.config.borrow().highlight_color.to_variant(),
        );
        let this = self.clone();
        color.connect_activate(move |a, value| {
            let Some(id) = value.and_then(|v| v.get::<String>()) else { return };
            a.set_state(&id.to_variant());
            let mut cfg = this.0.config.borrow_mut();
            cfg.highlight_color = id;
            let _ = cfg.save();
        });
        win.add_action(&color);

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
            ("win.save", &["<Ctrl>s"]),
            ("win.save-as", &["<Ctrl><Shift>s"]),
            ("win.edit", &["<Ctrl>e"]),
            ("win.zoom-in", &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"]),
            ("win.zoom-out", &["<Ctrl>minus", "<Ctrl>KP_Subtract"]),
            ("win.fit", &["<Ctrl>0"]),
            ("win.find", &["<Ctrl>f"]),
            ("win.sidebar", &["F9"]),
            ("win.next-page", &["<Ctrl>Page_Down", "n"]),
            ("win.prev-page", &["<Ctrl>Page_Up", "p"]),
            ("win.first-page", &["<Ctrl>Home"]),
            ("win.last-page", &["<Ctrl>End"]),
            ("win.go-to-page", &["<Ctrl>g"]),
            ("win.highlight", &["<Ctrl>h"]),
            ("win.undo", &["<Ctrl>z"]),
            ("win.redo", &["<Ctrl><Shift>z", "<Ctrl>y"]),
            ("win.shortcuts", &["<Ctrl>question"]),
            ("win.close", &["<Ctrl>w"]),
        ] {
            app.set_accels_for_action(action, keys);
        }
    }

    fn connect_signals(&self) {
        let this = self.clone();
        self.0.page_entry.connect_activate(move |entry| {
            let count = this.pdf_page_count();
            match entry.text().trim().parse::<usize>().ok().filter(|&n| n >= 1 && n <= count) {
                Some(page) => {
                    entry.remove_css_class("error");
                    this.with_pdf(|v| v.go_to(page - 1, 0.0));
                    this.0.pill.popdown();
                }
                None => entry.add_css_class("error"),
            }
        });
        self.0.page_entry.connect_changed(|entry| entry.remove_css_class("error"));

        // Open on the page being read, ready to be typed over.
        if let Some(popover) = self.0.pill.popover() {
            let (this, entry) = (self.clone(), self.0.page_entry.clone());
            popover.connect_show(move |_| {
                this.with_pdf(|v| entry.set_text(&(v.current_page() + 1).to_string()));
                entry.remove_css_class("error");
                let entry = entry.clone();
                glib::idle_add_local_once(move || {
                    entry.grab_focus();
                    entry.select_region(0, -1);
                });
            });
        }

        let this = self.clone();
        self.0.search.connect_activate(move |_| this.find_next(false));
        let this = self.clone();
        self.0.search.connect_next_match(move |_| this.find_next(false));
        let this = self.clone();
        self.0.search.connect_previous_match(move |_| this.find_next(true));

        // The context menu's spot is for the actions it offers; once it is
        // gone, actions apply to the page being read again.
        let this = self.clone();
        self.0.context_menu.connect_closed(move |_| {
            let this = this.clone();
            // Closing comes before the chosen action runs.
            glib::idle_add_local_once(move || {
                this.0.context_spot.set(None);
                this.0.context_annotation.set(None);
            });
        });

        let this = self.clone();
        self.0.window.connect_close_request(move |window| {
            this.remember_position();
            if !this.dirty() {
                return glib::Propagation::Proceed;
            }
            let window = window.clone();
            this.confirm_discard(move || window.close());
            glib::Propagation::Stop
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
        let dialog = gtk::FileDialog::builder().title("Open Document").filters(&document_filters()).modal(true).build();
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
            ("Save / Save As", "Ctrl+S / Ctrl+Shift+S"),
            ("Find", "Ctrl+F · Enter for next"),
            ("Select text", "Drag · double-click a word · triple-click a line"),
            ("Copy selection", "Ctrl+C"),
            ("Highlight selection", "Ctrl+H"),
            ("Undo / redo", "Ctrl+Z / Ctrl+Shift+Z"),
            ("Edit a document (DOCX)", "Ctrl+E"),
            ("Bold / italic / underline", "Ctrl+B / Ctrl+I / Ctrl+U"),
            ("Zoom in / out", "Ctrl++ / Ctrl+− · Ctrl+scroll"),
            ("Fit width", "Ctrl+0"),
            ("Next / previous page", "N / P · Ctrl+PgDn / PgUp"),
            ("Go to page", "Ctrl+G"),
            ("First / last page", "Ctrl+Home / Ctrl+End"),
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
        let scroller = gtk::ScrolledWindow::builder()
            .child(&page)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&scroller));
        adw::Dialog::builder()
            .title("Keyboard Shortcuts")
            .content_width(520)
            .child(&view)
            .build()
            .present(Some(&self.0.window));
    }
}

/// The formatting row shown while a DOCX is being edited.
fn format_bar() -> FormatBar {
    let names: Vec<&str> = STYLES.iter().map(|(n, _)| *n).collect();
    let style = gtk::DropDown::from_strings(&names);
    style.set_tooltip_text(Some("Paragraph style"));
    let row = gtk::Box::builder().spacing(6).css_classes(["format-bar"]).halign(gtk::Align::Center).build();
    let undo = gtk::Box::builder().css_classes(["linked"]).build();
    undo.append(&icon_button("edit-undo-symbolic", "Undo (Ctrl+Z)", "win.undo"));
    undo.append(&icon_button("edit-redo-symbolic", "Redo (Ctrl+Shift+Z)", "win.redo"));
    row.append(&undo);
    row.append(&style);
    let marks = gtk::Box::builder().css_classes(["linked"]).build();
    let mut toggles = Vec::new();
    for (name, label, class, tip) in [
        ("bold", "B", "bold-label", "Bold (Ctrl+B)"),
        ("italic", "I", "italic-label", "Italic (Ctrl+I)"),
        ("underline", "U", "underline-label", "Underline (Ctrl+U)"),
    ] {
        let text = gtk::Label::builder().label(label).css_classes([class]).build();
        let b = gtk::ToggleButton::builder().child(&text).tooltip_text(tip).css_classes(["flat"]).width_request(34).build();
        marks.append(&b);
        toggles.push((name, b));
    }
    let highlight = gtk::ToggleButton::builder()
        .icon_name("marker-symbolic")
        .tooltip_text("Highlight (Ctrl+H)")
        .css_classes(["flat"])
        .build();
    marks.append(&highlight);
    toggles.push(("highlight", highlight));
    row.append(&marks);
    let bar = gtk::Revealer::builder().child(&row).transition_type(gtk::RevealerTransitionType::SlideDown).build();
    FormatBar { bar, style, toggles, syncing: Cell::new(false) }
}

fn friendly_kind(kind: &str) -> &str {
    match kind {
        "Text" => "Note",
        "FreeText" => "Text",
        "StrikeOut" => "Strike-Out",
        "Ink" => "Drawing",
        other => other,
    }
}

/// A rectangle in `from`'s coordinates, in `to`'s.
fn translate(from: &impl IsA<gtk::Widget>, to: &impl IsA<gtk::Widget>, r: gdk::Rectangle) -> Option<gdk::Rectangle> {
    let p = from.compute_point(to, &gtk::graphene::Point::new(r.x() as f32, r.y() as f32))?;
    Some(gdk::Rectangle::new(p.x() as i32, p.y() as i32, r.width(), r.height()))
}

/// Replace a file without ever leaving it half-written: write alongside,
/// then rename over it.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
    let tmp = dir.join(format!(".{name}.raven-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    // Keep the original's permissions.
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Combine files of one kind, telling PDF from DOCX by content.
pub fn combine_files(files: &[(String, Vec<u8>)]) -> anyhow::Result<Vec<u8>> {
    if files.len() < 2 {
        anyhow::bail!("choose at least two files");
    }
    let is_pdf = |b: &[u8]| b.windows(5).take(1024).any(|w| w == b"%PDF-");
    let pdfs = files.iter().filter(|(_, b)| is_pdf(b)).count();
    if pdfs == files.len() {
        return pdfedit::merge(files);
    }
    if pdfs == 0 && files.iter().all(|(_, b)| b.starts_with(b"PK")) {
        return docx::merge(files);
    }
    anyhow::bail!("PDFs and DOCX files can’t be combined with each other — combine files of one kind")
}

pub const MIME_TYPES: [&str; 2] =
    ["application/pdf", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"];

fn document_filters() -> gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Documents"));
    for mime in MIME_TYPES {
        filter.add_mime_type(mime);
    }
    filter.add_suffix("pdf");
    filter.add_suffix("docx");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    filters
}

enum Loaded {
    Pdf(Arc<Vec<u8>>, DocumentInfo),
    Docx(docx::Docx),
}

/// Sniff the bytes, not the extension: a PDF saved as .bin still opens.
fn load(path: &Path) -> anyhow::Result<Loaded> {
    let bytes = Arc::new(std::fs::read(path)?);
    if bytes.windows(5).take(1024).any(|w| w == b"%PDF-") {
        let info = pdf::load_info(&bytes)?;
        return Ok(Loaded::Pdf(bytes, info));
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

/// Drives the real window offscreen and saves a PNG after each step:
/// `GDK_BACKEND=broadway RAVEN_DRIVE_DIR=… RAVEN_DRIVE_PDF=… RAVEN_DRIVE_DOCX=…
///  cargo test drive_the_window -- --ignored --nocapture`
#[cfg(test)]
mod drive {
    use super::*;

    fn pump(ms: u64) {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        let ctx = glib::MainContext::default();
        while std::time::Instant::now() < until {
            while ctx.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn shot(w: &Window, name: &str) {
        let dir = std::env::var("RAVEN_DRIVE_DIR").unwrap();
        let win = &w.0.window;
        let content = win.content().unwrap();
        let (width, height) = (content.width() as f64, content.height() as f64);
        // A capture between frames can come back empty; draw and retry.
        let mut node = None;
        for _ in 0..20 {
            content.queue_draw();
            pump(60);
            let snap = gtk::Snapshot::new();
            snap.append_color(&gdk::RGBA::new(0.12, 0.12, 0.14, 1.0), &gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32));
            content.parent().unwrap().snapshot_child(&content, &snap);
            let n = snap.to_node().expect("the window drew nothing");
            let drawn = n.downcast_ref::<gtk::gsk::ContainerNode>().is_some_and(|c| c.n_children() > 1);
            node = Some(n);
            if drawn {
                break;
            }
        }
        let node = node.unwrap();
        let renderer = win.native().unwrap().renderer().unwrap();
        renderer.render_texture(node, None).save_to_png(format!("{dir}/{name}.png")).unwrap();
        eprintln!("shot {name}: title {:?}", win.title());
    }

    #[test]
    #[ignore]
    fn drive_the_window() {
        crate::gtk_test::run(|| {
            adw::init().unwrap();
            crate::theme::apply();
            let app = adw::Application::builder()
                .application_id("com.ravenviewer.RavenDrive")
                .flags(gio::ApplicationFlags::NON_UNIQUE)
                .build();
            app.register(gio::Cancellable::NONE).unwrap();
            let dir = std::env::var("RAVEN_DRIVE_DIR").unwrap();
            let w = Window::new(&app);
            // Offscreen nothing drives the frame clock, so a crossfade
            // would never finish.
            w.0.content.set_transition_type(gtk::StackTransitionType::None);
            w.0.window.set_default_size(1100, 800);
            w.present();
            pump(300);

            // ── PDF ──
            w.open(gio::File::for_path(std::env::var("RAVEN_DRIVE_PDF").unwrap()));
            pump(1500);
            shot(&w, "1-pdf-open");
            let _ = WidgetExt::activate_action(&w.0.window, "win.select-page", None);
            pump(300);
            let (view, _) = w.pdf().unwrap();
            let text = view.selected_text().unwrap_or_default();
            eprintln!("selected {} chars: {:?}", text.len(), text.chars().take(80).collect::<String>());
            assert!(!text.is_empty(), "select-page selected nothing");
            shot(&w, "2-pdf-selected");
            let _ = WidgetExt::activate_action(&w.0.window, "win.copy", None);
            pump(200);
            let clip = w.0.window.clipboard();
            let copied = glib::MainContext::default().block_on(clip.read_text_future()).ok().flatten();
            eprintln!("clipboard has {} chars, matches selection: {}", copied.as_ref().map_or(0, |c| c.len()), copied.as_deref() == Some(text.as_str()));
            let _ = WidgetExt::activate_action(&w.0.window, "win.highlight", None);
            pump(2000);
            shot(&w, "3-pdf-highlighted");
            let notes = w.pdf().unwrap().0.info().annotations.len();
            eprintln!("annotations after highlight: {notes}, dirty: {}", w.dirty());
            // Rotate the page from the menu action, then undo it.
            let _ = WidgetExt::activate_action(&w.0.window, "win.rotate-right", None);
            pump(1500);
            shot(&w, "4-pdf-rotated");
            let _ = WidgetExt::activate_action(&w.0.window, "win.undo", None);
            pump(1500);
            shot(&w, "5-pdf-undo-rotate");
            let saved = format!("{dir}/saved.pdf");
            w.save_to(PathBuf::from(&saved), None);
            pump(800);
            let info = pdf::load_info(&Arc::new(std::fs::read(&saved).unwrap())).unwrap();
            eprintln!("saved PDF: {} pages, annotations {:?}", info.page_sizes.len(),
                info.annotations.iter().map(|a| a.kind.clone()).collect::<Vec<_>>());

            // ── DOCX ──
            w.open(gio::File::for_path(std::env::var("RAVEN_DRIVE_DOCX").unwrap()));
            pump(800);
            shot(&w, "6-docx-open");
            let _ = WidgetExt::activate_action(&w.0.window, "win.edit", None);
            pump(300);
            let view = w.docx().unwrap();
            let buffer = view.buffer().clone();
            let mut at = buffer.iter_at_line(1).unwrap();
            at.forward_to_line_end();
            buffer.place_cursor(&at);
            buffer.insert_interactive_at_cursor(" Typed in the editor.", true);
            // Select "Closing words" line and bold + highlight it.
            let last = buffer.line_count() - 2;
            let s = buffer.iter_at_line(last).unwrap();
            let mut e = s;
            e.forward_to_line_end();
            buffer.select_range(&s, &e);
            let _ = WidgetExt::activate_action(&w.0.window, "win.bold", None);
            let _ = WidgetExt::activate_action(&w.0.window, "win.highlight", None);
            pump(400);
            shot(&w, "7-docx-editing");
            let out = format!("{dir}/saved.docx");
            w.save_to(PathBuf::from(&out), None);
            pump(800);
            let back = docx::load(&std::fs::read(&out).unwrap()).unwrap();
            for b in back.blocks().iter().take(12) {
                if let docx::Block::Paragraph { style, runs } = b {
                    eprintln!("  {style:?} {:?}", runs.iter().map(|r| format!("{}{}{}", if r.bold {"*"} else {""}, if r.highlight {"=="} else {""}, r.text)).collect::<Vec<_>>());
                }
            }
            shot(&w, "8-docx-saved");
        });
    }
}
