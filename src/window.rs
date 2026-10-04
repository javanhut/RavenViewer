//! The window: a header bar, a sidebar (Contents · Notes) and the document.
//! Files are read and parsed off the main thread so a large book never
//! freezes the UI.
//!
//! Editing lives here too. A PDF is changed through `pdfedit::Editor`, which
//! hands back the whole new file; the view swaps it in under the pages it is
//! already showing, and the previous file goes on the undo stack. A Word
//! document — DOCX, Word 97–2003, or a text file read as one — is edited in
//! its text view and written out on save, in the format it came in or the
//! one Save As names.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{gdk, gio, glib};
use libadwaita as adw;

use crate::config::ViewerConfig;
use crate::convert::{self, Format};
use crate::docx::{ParaStyle, TextDefaults};
use crate::docxview::{self, DocxView, Inline};
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
        /// What Save writes: the format the file came in, or nothing yet
        /// for a new document.
        saved_as: Option<Format>,
    },
}

impl Doc {
    fn path(&self) -> &Path {
        match self {
            Doc::Pdf { path, .. } | Doc::Docx { path, .. } => path,
        }
    }

    fn is_new(&self) -> bool {
        matches!(self, Doc::Docx { saved_as: None, .. })
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

/// One open document and everything shown with it, as a tab: its sidebar,
/// its pages or sheet, its page counter and search bar, and the popovers
/// over it. An empty tab shows the welcome page.
struct Tab {
    /// What the tab view holds.
    root: gtk::Box,
    split: adw::OverlaySplitView,
    content: gtk::Stack,
    /// Where popovers over the document are anchored; it outlives any one
    /// document's view.
    overlay: gtk::Overlay,
    outline: gtk::ListBox,
    notes: gtk::ListBox,
    pill: gtk::MenuButton,
    pill_label: gtk::Label,
    page_entry: gtk::Entry,
    page_total: gtk::Label,
    search_bar: gtk::SearchBar,
    search: gtk::SearchEntry,
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
    /// Saving over a Word 97–2003 file was agreed to for this document.
    doc_overwrite_ok: Cell<bool>,
    /// Being edited (a Word document).
    editing: Cell<bool>,
    /// A file is on its way into it: empty, but not to be tidied away.
    loading: Cell<bool>,
    /// What the header says under the title while this tab is shown.
    subtitle: RefCell<String>,
}

/// A row of tabs: two side by side in split view.
struct Group {
    root: gtk::Box,
    bar: adw::TabBar,
    view: adw::TabView,
}

struct State {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    zoom_label: gtk::Button,
    pdf_only: Vec<gtk::Widget>,
    zoomable: Vec<gtk::Widget>,
    doc_only: Vec<gtk::Widget>,
    edit_btn: gtk::ToggleButton,
    format: FormatBar,
    groups: [Group; 2],
    /// Collapses each tab's sidebar on narrow windows.
    narrow: adw::Breakpoint,
    tabs: RefCell<Vec<Rc<Tab>>>,
    /// The tab the header, the menu and the shortcuts act on.
    active: RefCell<Option<Rc<Tab>>>,
    /// The tab a tab's right-click menu was opened on.
    menu_tab: RefCell<Option<Rc<Tab>>>,
    config: RefCell<ViewerConfig>,
}

/// A window, as seen from one of its tabs: the window's own parts are in
/// `.0`, the tab's in `.1`. Whatever a document's views and background work
/// call back into holds the tab they belong to; the header, the menu and
/// the shortcuts act on whichever tab is active.
#[derive(Clone)]
pub struct Window(Rc<State>, Rc<Tab>);

thread_local! {
    /// Every window, for handing files to the one in use.
    static WINDOWS: RefCell<Vec<std::rc::Weak<State>>> = const { RefCell::new(Vec::new()) };
}

impl Window {
    /// The Raven Viewer window that `window` is, as seen from its active tab.
    pub fn of(window: &gtk::Window) -> Option<Window> {
        WINDOWS.with(|all| {
            all.borrow_mut().retain(|w| w.strong_count() > 0);
            let state = all.borrow().iter().filter_map(std::rc::Weak::upgrade).find(|s| s.window.upcast_ref::<gtk::Window>() == window)?;
            let tab = state.active.borrow().clone().or_else(|| state.tabs.borrow().first().cloned())?;
            Some(Window(state, tab))
        })
    }

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
        let new_section = gio::Menu::new();
        new_section.append(Some("New Document"), Some("win.new"));
        new_section.append(Some("New Window"), Some("win.new-window"));
        menu.append_section(None, &new_section);
        let file_section = gio::Menu::new();
        file_section.append(Some("Save As…"), Some("win.save-as"));
        file_section.append(Some("Export as PDF…"), Some("win.export-pdf"));
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
        view_section.append(Some("Split View"), Some("win.split"));
        view_section.append(Some("Dark Pages"), Some("win.dark-pages"));
        view_section.append(Some("Fit Width"), Some("win.fit"));
        menu.append_section(None, &view_section);
        let app_section = gio::Menu::new();
        app_section.append(Some("Make Default for PDF & Word"), Some("win.set-default"));
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

        let split_btn = gtk::ToggleButton::builder()
            .icon_name("view-dual-symbolic")
            .tooltip_text("Split view: two documents side by side (Ctrl+\\)")
            .action_name("win.split")
            .css_classes(["flat"])
            .build();
        header.pack_end(&split_btn);

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

        // ── Tabs: one row, or two side by side ──────────────────────────
        let tab_menu = gio::Menu::new();
        tab_menu.append(Some("Move to Other Side"), Some("win.tab-move"));
        tab_menu.append(Some("Close"), Some("win.tab-close"));
        let groups = [0, 1].map(|_| {
            let view = adw::TabView::builder().vexpand(true).hexpand(true).menu_model(&tab_menu).build();
            // Ctrl+PgUp/PgDn and Ctrl+Home/End turn a PDF's pages here, so
            // only Ctrl+Tab and Alt+digit switch tabs.
            view.set_shortcuts(
                adw::TabViewShortcuts::CONTROL_TAB
                    | adw::TabViewShortcuts::CONTROL_SHIFT_TAB
                    | adw::TabViewShortcuts::ALT_DIGITS
                    | adw::TabViewShortcuts::ALT_ZERO,
            );
            let bar = adw::TabBar::builder().view(&view).autohide(true).build();
            let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["doc-group"]).build();
            root.append(&bar);
            root.append(&view);
            Group { root, bar, view }
        });
        groups[1].root.set_visible(false);
        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&groups[0].root)
            .end_child(&groups[1].root)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .wide_handle(true)
            .build();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&paned));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&format.bar);
        toolbar.set_content(Some(&toasts));
        window.set_content(Some(&toolbar));

        // Collapse the sidebar into an overlay on narrow windows.
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Sp,
        ));
        window.add_breakpoint(narrow.clone());

        let pdf_only: Vec<gtk::Widget> = vec![search_btn.upcast()];
        let zoomable: Vec<gtk::Widget> = vec![zoom_box.upcast()];
        let doc_only: Vec<gtk::Widget> = vec![sidebar_btn.upcast(), save_btn.upcast()];
        let state = Rc::new(State {
            window,
            title,
            toasts,
            zoom_label,
            pdf_only,
            zoomable,
            doc_only,
            edit_btn,
            format,
            groups,
            narrow,
            tabs: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            menu_tab: RefCell::new(None),
            config: RefCell::new(config),
        });
        WINDOWS.with(|all| all.borrow_mut().push(Rc::downgrade(&state)));
        let first = Window::add_tab(&state, 0);
        first.install_actions();
        first.install_drop();
        first.connect_window();
        first.connect_groups();
        first.connect_format_bar();
        first.activate_tab();
        first
    }

    /// A new, empty tab at the end of group `g`, selected there.
    fn add_tab(state: &Rc<State>, g: usize) -> Window {
        let tab = Rc::new(Tab::new());
        state.narrow.add_setter(&tab.split, "collapsed", Some(&true.to_value()));
        state.tabs.borrow_mut().push(tab.clone());
        let page = state.groups[g].view.append(&tab.root);
        page.set_title("New Tab");
        state.groups[g].view.set_selected_page(&page);
        let this = Window(state.clone(), tab);
        this.connect_tab();
        this
    }

    /// The window as seen from its active tab.
    fn active(&self) -> Window {
        let tab = self.0.active.borrow().clone().unwrap_or_else(|| self.1.clone());
        Window(self.0.clone(), tab)
    }

    fn is_active(&self) -> bool {
        self.0.active.borrow().as_ref().is_some_and(|t| Rc::ptr_eq(t, &self.1))
    }

    /// The window as seen from another of its tabs.
    fn with_tab(&self, tab: Rc<Tab>) -> Window {
        Window(self.0.clone(), tab)
    }

    /// Which group a tab is in, and its page there.
    fn page_of(&self, tab: &Tab) -> Option<(usize, adw::TabPage)> {
        self.0.groups.iter().enumerate().find_map(|(g, group)| {
            (0..group.view.n_pages())
                .map(|i| group.view.nth_page(i))
                .find(|p| p.child() == tab.root)
                .map(|p| (g, p))
        })
    }

    fn tab_of(&self, page: &adw::TabPage) -> Option<Rc<Tab>> {
        self.0.tabs.borrow().iter().find(|t| page.child() == t.root).cloned()
    }

    fn is_split(&self) -> bool {
        self.0.groups[1].root.is_visible()
    }

    /// Make this tab the one the window acts on.
    fn activate_tab(&self) {
        let changed = !self.is_active();
        *self.0.active.borrow_mut() = Some(self.1.clone());
        if !changed {
            return;
        }
        let in_group = self.page_of(&self.1).map(|(g, _)| g);
        for (g, group) in self.0.groups.iter().enumerate() {
            if Some(g) == in_group && self.is_split() {
                group.root.add_css_class("focused");
            } else {
                group.root.remove_css_class("focused");
            }
        }
        if let Some(a) = self.0.window.lookup_action("edit").and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
            a.set_state(&self.1.editing.get().to_variant());
        }
        if let Some(view) = self.docx() {
            let (style, inline) = view.format();
            self.show_format(style, inline);
        }
        self.sync_controls();
    }

    pub fn present(&self) {
        self.0.window.present();
    }

    fn toast(&self, text: &str) {
        self.0.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(4).build());
    }

    fn is_pdf(&self) -> bool {
        matches!(self.1.doc.borrow().as_ref(), Some(Doc::Pdf { .. }))
    }

    fn is_docx(&self) -> bool {
        matches!(self.1.doc.borrow().as_ref(), Some(Doc::Docx { .. }))
    }

    fn dirty(&self) -> bool {
        self.1.doc.borrow().as_ref().is_some_and(Doc::dirty)
    }

    /// Bring the tab's label up to date, and the header and the actions to
    /// what the active tab is showing.
    fn sync_controls(&self) {
        self.label_tab();
        let active = self.active();
        if !Rc::ptr_eq(&active.1, &self.1) {
            active.label_tab();
        }
        active.sync_active();
    }

    /// The tab's name in its row, marked when it has unsaved changes.
    fn label_tab(&self) {
        let Some((_, page)) = self.page_of(&self.1) else { return };
        let doc = self.1.doc.borrow();
        let (name, dirty) = match doc.as_ref() {
            Some(d) => (if d.is_new() { "Untitled Document".to_string() } else { display_name(d.path()) }, d.dirty()),
            None => ("New Tab".to_string(), false),
        };
        page.set_title(&format!("{}{name}", if dirty { "• " } else { "" }));
        page.set_tooltip(&glib::markup_escape_text(&doc.as_ref().map(|d| d.path().display().to_string()).unwrap_or_default()));
    }

    /// Show what applies to the active document and enable what can be
    /// done with it.
    fn sync_active(&self) {
        let (any, pdf, docx) = (self.1.doc.borrow().is_some(), self.is_pdf(), self.is_docx());
        for w in &self.0.pdf_only {
            w.set_visible(pdf);
        }
        for w in &self.0.zoomable {
            w.set_visible(any);
        }
        for w in &self.0.doc_only {
            w.set_visible(any);
        }
        self.0.edit_btn.set_visible(docx);
        // Search can take over any key typed at the window — only the
        // active PDF's; a Word document is typed into instead.
        for tab in self.0.tabs.borrow().iter() {
            tab.search_bar.set_key_capture_widget(None::<&gtk::Widget>);
        }
        let capture: Option<gtk::Widget> = pdf.then(|| self.0.window.clone().upcast());
        self.1.search_bar.set_key_capture_widget(capture.as_ref());
        let editing = docx && self.1.editing.get();
        self.0.format.bar.set_reveal_child(editing);
        self.0.title.set_subtitle(&self.1.subtitle.borrow());
        self.sync_zoom_label();

        let (has_selection, can_undo, can_redo) = match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, edits, .. }) => {
                (view.has_selection(), !edits.undo.borrow().is_empty(), !edits.redo.borrow().is_empty())
            }
            Some(Doc::Docx { view, .. }) => {
                (view.buffer().has_selection(), view.can_undo(), view.can_redo())
            }
            None => (false, false, false),
        };
        for name in ["zoom-in", "zoom-out", "fit"] {
            self.enable(name, any);
        }
        let pdf_actions = [
            "next-page", "prev-page", "first-page", "last-page", "go-to-page", "find",
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
        self.enable("export-pdf", docx);
        self.enable("insert-picture", docx);
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

    fn update_title(&self) {
        let doc = self.1.doc.borrow();
        let Some(doc) = doc.as_ref() else {
            self.0.title.set_title("Raven Viewer");
            self.0.window.set_title(Some("Raven Viewer"));
            return;
        };
        let name = if doc.is_new() { "Untitled Document".to_string() } else { display_name(doc.path()) };
        let mark = if doc.dirty() { "• " } else { "" };
        let shown = match doc {
            Doc::Pdf { view, .. } => view.info().title.clone().unwrap_or_else(|| name.clone()),
            Doc::Docx { .. } => name.clone(),
        };
        self.0.title.set_title(&format!("{mark}{shown}"));
        self.0.window.set_title(Some(&format!("{mark}{name} — Raven Viewer")));
    }

    // ── Opening ─────────────────────────────────────────────────────────

    /// Open a file in a tab of its own beside this one — or in this tab,
    /// if it is empty. A file already open is shown where it is.
    pub fn open(&self, file: gio::File) {
        let Some(path) = file.path() else {
            self.toast("Only local files can be opened");
            return;
        };
        let open_in = self.0.tabs.borrow().iter().find(|t| t.doc.borrow().as_ref().is_some_and(|d| d.path() == path)).cloned();
        if let Some(tab) = open_in {
            self.with_tab(tab).select();
            return;
        }
        let this = self.tab_for_document();
        this.1.loading.set(true);
        glib::spawn_future_local(async move {
            let p = path.clone();
            let loaded = gio::spawn_blocking(move || load(&p)).await;
            this.1.loading.set(false);
            match loaded {
                Ok(Ok(Loaded::Pdf(bytes, info))) => this.show_pdf(path, bytes, info),
                Ok(Ok(Loaded::Docx(doc, format))) => this.show_docx(path, *doc, Some(format)),
                Ok(Err(e)) => {
                    this.toast(&format!("Couldn’t open {}: {e}", display_name(&path)));
                    this.tidy_later();
                }
                Err(_) => this.toast("Couldn’t open the file"),
            }
        });
    }

    /// Where a document being opened goes: this tab if it is empty,
    /// otherwise a new tab after it in the same row, shown at once.
    fn tab_for_document(&self) -> Window {
        if self.1.doc.borrow().is_none() && self.page_of(&self.1).is_some() {
            self.select();
            return self.clone();
        }
        let (g, position) = self.page_of(&self.1).map_or((0, None), |(g, page)| (g, Some(self.0.groups[g].view.page_position(&page) + 1)));
        let tab = Window::add_tab(&self.0, g);
        if let (Some(at), Some((_, page))) = (position, tab.page_of(&tab.1)) {
            self.0.groups[g].view.reorder_page(&page, at);
        }
        tab.activate_tab();
        tab
    }

    /// An empty document, ready to type into; Save asks where it goes.
    fn new_document(&self) {
        let blank = docx::blank(convert::local_paper(), &TextDefaults::document());
        match docx::load(&blank) {
            Ok(doc) => {
                let this = self.tab_for_document();
                let dir = glib::user_special_dir(glib::UserDirectory::Documents).unwrap_or_else(glib::home_dir);
                this.show_docx(dir.join("Untitled.docx"), doc, None);
                this.set_edit_state(true);
            }
            Err(e) => self.toast(&format!("Couldn’t make a document: {e}")),
        }
    }

    fn remember_position(&self) {
        if let Some(Doc::Pdf { path, view, .. }) = self.1.doc.borrow().as_ref() {
            let mut cfg = self.0.config.borrow_mut();
            if cfg.remember_position {
                cfg.positions.insert(path.to_string_lossy().into_owned(), view.current_page());
                let _ = cfg.save();
            }
        }
    }

    fn replace_content(&self, widget: &impl IsA<gtk::Widget>) {
        self.1.selection_tools.popdown();
        self.1.context_menu.popdown();
        if let Some(old) = self.1.content.child_by_name("doc") {
            self.1.content.remove(&old);
        }
        self.1.content.add_named(widget, Some("doc"));
        self.1.content.set_visible_child_name("doc");
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
        self.1.split.set_show_sidebar(has_sidebar && self.0.config.borrow().show_sidebar);

        let search = Rc::new(pdf::Searcher::spawn(bytes.clone(), view.page_count()));
        let edits = Rc::new(PdfEdits {
            editor: pdfedit::Editor::spawn(bytes.clone()),
            bytes: RefCell::new(bytes.clone()),
            saved: RefCell::new(bytes),
            undo: RefCell::default(),
            redo: RefCell::default(),
            busy: Cell::new(false),
        });
        *self.1.doc.borrow_mut() = Some(Doc::Pdf { path, view: view.clone(), search, edits });
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
        let this = self.clone();
        let update = move |page: usize| {
            let text = format!("{} of {n}", page + 1);
            this.1.pill_label.set_label(&text);
            *this.1.subtitle.borrow_mut() = format!("Page {text}");
            if this.is_active() {
                this.0.title.set_subtitle(&this.1.subtitle.borrow());
            }
        };
        update(0);
        view.connect_page_changed(update);
        let this = self.clone();
        view.connect_zoom_changed(move |_| this.sync_zoom_label());

        let this = self.clone();
        view.connect_selection_changed(move |on| {
            if !on {
                this.1.selection_tools.popdown();
            }
            this.sync_controls();
        });
        let (this, weak_view) = (self.clone(), view.widget().downgrade());
        view.connect_selection_done(move |rect| {
            let Some(scroller) = weak_view.upgrade() else { return };
            if let Some(rect) = translate(&scroller, &this.1.overlay, rect) {
                this.1.selection_tools.set_pointing_to(Some(&rect));
                this.1.selection_tools.popup();
            }
        });
        // Scrolling moves the selection out from under the tools.
        let tools = self.1.selection_tools.clone();
        view.widget().vadjustment().connect_value_changed(move |_| tools.popdown());
        let (this, weak_view) = (self.clone(), view.widget().downgrade());
        view.connect_context_menu(move |spot, x, y| {
            let Some(scroller) = weak_view.upgrade() else { return };
            this.show_context_menu(&scroller, spot, x, y);
        });

        self.1.page_total.set_label(&format!("of {n}"));
        self.1.pill.set_visible(true);
        Ok(view)
    }

    /// Refresh everything that is derived from the document's contents.
    fn after_pdf_changed(&self, view: &PdfView) {
        let info = view.info().clone();
        self.fill_outline(&info, view);
        self.fill_notes(&info, view);
        self.sync_controls();
    }

    fn show_docx(&self, path: PathBuf, doc: docx::Docx, saved_as: Option<Format>) {
        self.remember_position();
        let view = DocxView::new(doc);
        self.replace_content(view.widget());
        *self.1.subtitle.borrow_mut() = match saved_as {
            None => "New Document",
            Some(Format::Doc) => "Word 97–2003 Document",
            Some(Format::Text) => "Plain Text",
            _ => "Document",
        }
        .to_string();
        self.1.doc_overwrite_ok.set(false);
        self.1.pill.set_visible(false);
        self.1.outline.remove_all();
        self.1.notes.remove_all();
        self.1.split.set_show_sidebar(false);

        let format = self.clone();
        view.connect_format_changed(move |style, inline| format.show_format(style, inline));
        let this = self.clone();
        view.buffer().connect_modified_changed(move |_| this.sync_controls());
        let this = self.clone();
        view.buffer().connect_has_selection_notify(move |_| this.sync_controls());
        let this = self.clone();
        view.connect_history_changed(move || this.sync_controls());
        let this = self.clone();
        view.connect_problem(move |e| this.toast(e));
        let this = self.clone();
        view.connect_zoom_changed(move |_| this.sync_zoom_label());
        view.set_dark_pages(self.0.config.borrow().dark_pages);
        self.add_docx_menu(&view);
        self.add_docx_shortcuts(&view);
        self.install_document_drop(&view);

        *self.1.doc.borrow_mut() = Some(Doc::Docx { path, view, saved_as });
        self.set_edit_state(false);
        self.sync_controls();
    }

    fn fill_outline(&self, info: &DocumentInfo, view: &PdfView) {
        let list = &self.1.outline;
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
        let split = self.1.split.clone();
        if let Some(old) = self.1.outline_handler.take() {
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
        *self.1.outline_handler.borrow_mut() = Some(id);
    }

    fn fill_notes(&self, info: &DocumentInfo, view: &PdfView) {
        let list = &self.1.notes;
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
        if let Some(old) = self.1.notes_handler.take() {
            list.disconnect(old);
        }
        let id = list.connect_row_activated(move |_, row| {
            if let (Some(a), Some(view)) = (notes.get(row.index() as usize), view.upgrade()) {
                view.go_to(a.page, a.y_fraction);
            }
        });
        *self.1.notes_handler.borrow_mut() = Some(id);
    }

    fn pdf_page_count(&self) -> usize {
        match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, .. }) => view.page_count(),
            _ => 0,
        }
    }

    fn with_pdf(&self, f: impl FnOnce(&PdfView)) {
        let view = match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, .. }) => view.clone(),
            _ => return,
        };
        f(&view);
    }

    fn pdf(&self) -> Option<(PdfView, Rc<PdfEdits>)> {
        match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, edits, .. }) => Some((view.clone(), edits.clone())),
            _ => None,
        }
    }

    fn docx(&self) -> Option<DocxView> {
        match self.1.doc.borrow().as_ref() {
            Some(Doc::Docx { view, .. }) => Some(view.clone()),
            _ => None,
        }
    }

    /// The zoom in the header, when this is the active tab.
    fn sync_zoom_label(&self) {
        if !self.is_active() {
            return;
        }
        let zoom = match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, .. }) => view.zoom(),
            Some(Doc::Docx { view, .. }) => view.zoom(),
            None => return,
        };
        self.0.zoom_label.set_label(&format!("{:.0}%", zoom * 100.0));
        self.0.zoom_label.set_tooltip_text(Some(if self.is_pdf() { "Fit width (Ctrl+0)" } else { "Actual size (Ctrl+0)" }));
    }

    /// Zoom by `factor`, or back to fitting (`None`): a PDF to the window's
    /// width, a document to its own size.
    fn zoom(&self, factor: Option<f64>) {
        match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { view, .. }) => match factor {
                Some(f) => view.zoom_by(f),
                None => view.fit_width(),
            },
            Some(Doc::Docx { view, .. }) => view.set_zoom(factor.map_or(1.0, |f| view.zoom() * f)),
            None => {}
        }
        self.sync_zoom_label();
    }

    // ── Search ──────────────────────────────────────────────────────────

    fn find_next(&self, backwards: bool) {
        let query = self.1.search.text().trim().to_string();
        if query.is_empty() {
            return;
        }
        let (view, search) = match self.1.doc.borrow().as_ref() {
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
        if let Some(Doc::Pdf { view: v, search, .. }) = self.1.doc.borrow_mut().as_mut() {
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
        self.1.selection_tools.popdown();
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
        Some(self.1.context_spot.get().map_or(view.current_page(), |s| s.page))
    }

    fn page_edit(&self, make: impl FnOnce(usize, usize) -> Option<Edit>) {
        let (Some(page), count) = (self.target_page(), self.pdf_page_count()) else { return };
        if let Some(edit) = make(page, count) {
            self.pdf_edit(edit, "");
        }
        self.1.context_spot.set(None);
    }

    /// Ask for text, then place a note or a text box with it at the spot the
    /// menu was opened on — or, from the selection tools, beside the
    /// selection.
    fn add_note(&self, text_box: bool) {
        let Some((view, _)) = self.pdf() else { return };
        let selection = view.selection();
        let spot = self.1.context_spot.take().or_else(|| {
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
        self.1.selection_tools.popdown();
        self.1.context_spot.set(spot);
        let annotation = spot.and_then(|s| view.annotation_at(s));
        self.1.context_annotation.set(annotation);

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
        self.1.context_menu.set_menu_model(Some(&menu));
        let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        if let Some(rect) = translate(scroller, &self.1.overlay, rect) {
            self.1.context_menu.set_pointing_to(Some(&rect));
            self.1.context_menu.popup();
        }
    }

    // ── DOCX editing ────────────────────────────────────────────────────

    fn set_edit_state(&self, on: bool) {
        self.1.editing.set(on);
        if self.is_active()
            && let Some(a) = self.0.window.lookup_action("edit").and_then(|a| a.downcast::<gio::SimpleAction>().ok())
        {
            a.set_state(&on.to_variant());
        }
        if let Some(view) = self.docx() {
            view.set_editable(on);
        }
        self.sync_controls();
    }

    fn show_format(&self, style: ParaStyle, inline: Inline) {
        if !self.is_active() {
            return;
        }
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
            if let (Some(view), Some((_, style))) = (this.active().docx(), STYLES.get(drop.selected() as usize)) {
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
                if let Some(view) = this.active().docx() {
                    view.toggle(name);
                    view.text_view().grab_focus();
                }
            });
        }
    }

    /// Highlight and pictures in the text view's own right-click menu, next
    /// to Copy and Paste.
    fn add_docx_menu(&self, view: &DocxView) {
        let menu = gio::Menu::new();
        menu.append(Some("Highlight"), Some("win.highlight"));
        menu.append(Some("Insert Picture…"), Some("win.insert-picture"));
        view.text_view().set_extra_menu(Some(&menu));
    }

    /// Choose picture files and put them in at the cursor.
    fn choose_picture(&self) {
        if self.docx().is_none() {
            return;
        }
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Pictures"));
        filter.add_pixbuf_formats();
        for mime in ["image/png", "image/jpeg", "image/gif", "image/bmp", "image/tiff", "image/webp", "image/svg+xml"] {
            filter.add_mime_type(mime);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Insert Picture").modal(true).filters(&filters).build();
        if let Some(pictures) = glib::user_special_dir(glib::UserDirectory::Pictures) {
            dialog.set_initial_folder(Some(&gio::File::for_path(pictures)));
        }
        let this = self.clone();
        dialog.open_multiple(Some(&self.0.window), gio::Cancellable::NONE, move |res| {
            let Ok(chosen) = res else { return };
            let paths: Vec<PathBuf> =
                (0..chosen.n_items()).filter_map(|i| chosen.item(i).and_downcast::<gio::File>()?.path()).collect();
            this.insert_pictures(&paths);
        });
    }

    /// Put picture files in at the cursor, turning editing on to do it.
    fn insert_pictures(&self, paths: &[PathBuf]) {
        let Some(view) = self.docx() else { return };
        if !self.1.editing.get() {
            self.set_edit_state(true);
        }
        for path in paths {
            let result = std::fs::read(path)
                .map_err(|e| format!("Couldn’t read {}: {e}", display_name(path)))
                .and_then(|data| view.insert_picture(data));
            if let Err(e) = result {
                self.toast(&e);
                break;
            }
        }
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
        match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { edits, .. }) => Ok(edits.bytes.borrow().as_ref().clone()),
            Some(Doc::Docx { view, .. }) => view.save(),
            None => anyhow::bail!("nothing is open"),
        }
    }

    /// Save where the document came from, in the format it came in. A new
    /// document asks where to go first, and a Word 97–2003 file is
    /// overwritten only once that is agreed to.
    fn save(&self, then: Option<Box<dyn FnOnce()>>) {
        let (path, saved_as) = match self.1.doc.borrow().as_ref() {
            Some(Doc::Docx { path, saved_as, .. }) => (path.clone(), *saved_as),
            Some(Doc::Pdf { path, .. }) => (path.clone(), Some(Format::Pdf)),
            None => return,
        };
        match saved_as {
            None => self.save_as(then),
            Some(Format::Doc) if !self.1.doc_overwrite_ok.get() => self.confirm_doc_overwrite(path, then),
            Some(_) => self.save_to(path, then),
        }
    }

    /// Writing a `.doc` keeps what Raven Viewer reads of it — text,
    /// formatting, tables, pictures — but not headers, footers, notes or
    /// comments. Say so before writing over one.
    fn confirm_doc_overwrite(&self, path: PathBuf, then: Option<Box<dyn FnOnce()>>) {
        let dialog = adw::AlertDialog::builder()
            .heading("Save as Word 97–2003?")
            .body(format!(
                "“{}” will be rewritten. Its text, formatting, tables and pictures are kept; headers, footers, notes and comments are not. Saving a copy as DOCX keeps the original as it is.",
                display_name(&path)
            ))
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("docx", "Save as DOCX…"), ("doc", "Save")]);
        dialog.set_response_appearance("doc", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("docx", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("docx"));
        dialog.set_close_response("cancel");
        let this = self.clone();
        let then = RefCell::new(then);
        dialog.connect_response(None, move |_, response| match response {
            "doc" => {
                this.1.doc_overwrite_ok.set(true);
                this.save_to(path.clone(), then.take());
            }
            "docx" => this.save_as_format(Some(Format::Docx), then.take()),
            _ => {}
        });
        dialog.present(Some(&self.0.window));
    }

    /// Write the document to `path` in the format its name asks for. A
    /// Word document written as a PDF is exported: it stays open as it was,
    /// unsaved if it was.
    fn save_to(&self, path: PathBuf, then: Option<Box<dyn FnOnce()>>) {
        let format = Format::of(&path);
        let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let job: Box<dyn FnOnce() -> anyhow::Result<Vec<u8>> + Send> = match self.1.doc.borrow().as_ref() {
            None => return,
            Some(Doc::Pdf { edits, .. }) => {
                if format != Format::Pdf {
                    return self.toast("A PDF can only be saved as a PDF");
                }
                let bytes = edits.bytes.borrow().as_ref().clone();
                Box::new(move || Ok(bytes))
            }
            Some(Doc::Docx { view, .. }) => match format {
                Format::Docx => match view.save() {
                    Ok(bytes) => Box::new(move || Ok(bytes)),
                    Err(e) => return self.toast(&format!("Couldn’t save: {e}")),
                },
                Format::Pdf => {
                    let (blocks, sections) = (view.blocks(), view.sections());
                    Box::new(move || crate::render::pdf(&blocks, &sections, &title))
                }
                Format::Doc => {
                    let (blocks, page) = (view.blocks(), view.page());
                    Box::new(move || crate::doc::write(&blocks, &page))
                }
                Format::Text => {
                    let text = docx::text_of(&view.blocks());
                    Box::new(move || Ok(text.into_bytes()))
                }
            },
        };
        let exporting = self.is_docx() && format == Format::Pdf;
        let this = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let written = gio::spawn_blocking(move || -> anyhow::Result<()> {
                let bytes = job()?;
                write_atomically(&p, &bytes)?;
                Ok(())
            })
            .await;
            match written {
                Ok(Ok(())) if exporting => {
                    this.toast(&format!("Exported {}", display_name(&path)));
                }
                Ok(Ok(())) => {
                    // The borrow ends before the DOCX buffer is told it is
                    // saved: that emits a signal whose handler reads the
                    // document again.
                    let docx = match this.1.doc.borrow_mut().as_mut() {
                        Some(Doc::Pdf { path: p, edits, .. }) => {
                            *p = path.clone();
                            *edits.saved.borrow_mut() = edits.bytes.borrow().clone();
                            None
                        }
                        Some(Doc::Docx { path: p, view, saved_as }) => {
                            *p = path.clone();
                            *saved_as = Some(format);
                            Some(view.clone())
                        }
                        None => None,
                    };
                    if let Some(view) = docx {
                        view.set_unmodified();
                        *this.1.subtitle.borrow_mut() = match format {
                            Format::Doc => "Word 97–2003 Document",
                            Format::Text => "Plain Text",
                            _ => "Document",
                        }
                        .to_string();
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

    fn save_as(&self, then: Option<Box<dyn FnOnce()>>) {
        self.save_as_format(None, then);
    }

    /// Ask where to save, suggesting `format` (or the document's own). The
    /// name's extension decides the format; a name without one gets it.
    fn save_as_format(&self, format: Option<Format>, then: Option<Box<dyn FnOnce()>>) {
        let (path, own, is_pdf) = match self.1.doc.borrow().as_ref() {
            Some(Doc::Pdf { path, .. }) => (path.clone(), Format::Pdf, true),
            Some(Doc::Docx { path, saved_as, .. }) => (path.clone(), saved_as.unwrap_or(Format::Docx), false),
            None => return,
        };
        let format = format.unwrap_or(own);
        let name = path.with_extension(format.extension());
        let dialog = gtk::FileDialog::builder()
            .title(if format == Format::Pdf && !is_pdf { "Export as PDF" } else { "Save As" })
            .modal(true)
            .initial_name(display_name(&name))
            .filters(&save_filters(is_pdf, format))
            .build();
        if let Some(dir) = path.parent().filter(|d| d.is_dir()) {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let this = self.clone();
        dialog.save(Some(&self.0.window), gio::Cancellable::NONE, move |res| {
            if let Some(mut path) = res.ok().and_then(|f| f.path()) {
                if path.extension().is_none() {
                    path.set_extension(format.extension());
                }
                // Choosing to write a .doc here is agreeing to it.
                if Format::of(&path) == Format::Doc {
                    this.1.doc_overwrite_ok.set(true);
                }
                this.save_to(path, then);
            }
        });
    }

    /// Run `then` once any unsaved changes are saved or given up — or not at
    /// all if the reader thinks better of it.
    fn confirm_discard(&self, then: impl Fn() + 'static) {
        if !self.dirty() {
            return then();
        }
        let name = self.1.doc.borrow().as_ref().map(|d| display_name(d.path())).unwrap_or_default();
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
                match this.1.doc.borrow().as_ref() {
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
        if let Some(doc) = self.1.doc.borrow().as_ref() {
            files.borrow_mut().push(doc.path().to_path_buf());
        }
        let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
        let add = gtk::Button::builder().label("Add Files…").css_classes(["pill"]).halign(gtk::Align::Start).build();
        let go = gtk::Button::builder().label("Combine…").css_classes(["pill", "suggested-action"]).halign(gtk::Align::End).hexpand(true).build();
        let hint = gtk::Label::builder()
            .label("PDFs are joined page by page, and Word and text files one after another, each starting on a new page. A mix of the two becomes a PDF.")
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
        let formats: Vec<Format> = files.iter().map(|p| Format::of(p)).collect();
        let out_format = convert::combined_format(&formats);
        let open_name = self.1.doc.borrow().as_ref().map(|d| d.path().to_path_buf());
        // The open document is combined as it is on screen, edits included.
        let current = self.current_bytes().ok();
        let suggested = files.first().map(|p| {
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{stem} (combined).{}", out_format.extension())
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
                    let merged = convert::combine(&loaded)?;
                    write_atomically(&target, &merged)?;
                    Ok(())
                })
                .await;
                match result {
                    Ok(Ok(())) => {
                        this.toast(&format!("Combined into {}", display_name(&out)));
                        this.active().open(gio::File::for_path(&out));
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
        // Every action is for the tab that is active when it is taken.
        let add = |name: &str, f: Box<dyn Fn(&Window)>| {
            let action = gio::SimpleAction::new(name, None);
            let this = self.clone();
            action.connect_activate(move |_, _| f(&this.active()));
            win.add_action(&action);
        };

        add("open", Box::new(|w| w.choose_file()));
        add("new", Box::new(|w| w.new_document()));
        add("save", Box::new(|w| w.save(None)));
        add("save-as", Box::new(|w| w.save_as(None)));
        add("export-pdf", Box::new(|w| w.save_as_format(Some(Format::Pdf), None)));
        add("insert-picture", Box::new(|w| w.choose_picture()));
        add("combine", Box::new(|w| w.combine_dialog()));
        add("zoom-in", Box::new(|w| w.zoom(Some(1.2))));
        add("zoom-out", Box::new(|w| w.zoom(Some(1.0 / 1.2))));
        add("fit", Box::new(|w| w.zoom(None)));
        add("next-page", Box::new(|w| w.with_pdf(|v| v.next_page())));
        add("prev-page", Box::new(|w| w.with_pdf(|v| v.prev_page())));
        add("first-page", Box::new(|w| w.with_pdf(|v| v.first_page())));
        add("last-page", Box::new(|w| w.with_pdf(|v| v.last_page())));
        add("go-to-page", Box::new(|w| {
            if w.1.pill.is_visible() {
                w.1.pill.popup();
            }
        }));
        add("find", Box::new(|w| {
            let on = !w.1.search_bar.is_search_mode();
            w.1.search_bar.set_search_mode(on);
            if on {
                w.1.search.grab_focus();
            }
        }));
        add("sidebar", Box::new(|w| {
            let show = !w.1.split.shows_sidebar();
            w.1.split.set_show_sidebar(show);
            let mut cfg = w.0.config.borrow_mut();
            cfg.show_sidebar = show;
            let _ = cfg.save();
        }));

        // Text.
        add("copy", Box::new(|w| {
            if let Some((view, _)) = w.pdf() {
                if view.copy() {
                    w.1.selection_tools.popdown();
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
            if let Some(i) = w.1.context_annotation.take() {
                w.edit_annotation(i);
            }
        }));
        add("delete-annotation", Box::new(|w| {
            if let Some(i) = w.1.context_annotation.take() {
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
                this.1.context_spot.set(Some(Spot { page, x: 0.0, y: 0.0 }));
                this.page_edit(|page, count| (count > 1).then_some(Edit::DeletePage { page }));
            });
            dialog.present(Some(&w.0.window));
        }));

        // History. A DOCX has its text view's own history.
        add("undo", Box::new(|w| {
            if w.is_pdf() {
                w.pdf_history(true);
            } else if let Some(view) = w.docx() {
                view.undo();
            }
        }));
        add("redo", Box::new(|w| {
            if w.is_pdf() {
                w.pdf_history(false);
            } else if let Some(view) = w.docx() {
                view.redo();
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
                .comments("Read, write, mark up, convert and combine PDFs, Word documents and text on Raven Linux. Free, fast, no browser.")
                .license_type(gtk::License::MitX11)
                .website("https://github.com/javanhut/RavenViewer")
                .build()
                .present(Some(&w.0.window));
        }));
        add("close", Box::new(|w| w.close_tab()));
        add("close-window", Box::new(|w| {
            w.0.window.close();
        }));
        add("new-window", Box::new(|w| {
            if let Some(app) = w.0.window.application().and_downcast::<adw::Application>() {
                Window::new(&app).present();
            }
        }));
        add("tab-move", Box::new(|w| {
            let tab = w.0.menu_tab.borrow_mut().take().unwrap_or_else(|| w.1.clone());
            w.with_tab(tab).move_to_other_side();
        }));
        add("tab-close", Box::new(|w| {
            let tab = w.0.menu_tab.borrow_mut().take().unwrap_or_else(|| w.1.clone());
            w.with_tab(tab).close_tab();
        }));

        let edit = gio::SimpleAction::new_stateful("edit", None, &false.to_variant());
        let this = self.clone();
        edit.connect_activate(move |_, _| {
            let active = this.active();
            active.set_edit_state(!active.1.editing.get());
        });
        win.add_action(&edit);

        let split = gio::SimpleAction::new_stateful("split", None, &false.to_variant());
        let this = self.clone();
        split.connect_activate(move |_, _| {
            let active = this.active();
            active.set_split(!active.is_split());
        });
        win.add_action(&split);

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
            for tab in this.0.tabs.borrow().iter() {
                let w = this.with_tab(tab.clone());
                w.with_pdf(|v| v.set_dark_pages(on));
                if let Some(v) = w.docx() {
                    v.set_dark_pages(on);
                }
            }
            let mut cfg = this.0.config.borrow_mut();
            cfg.dark_pages = on;
            let _ = cfg.save();
        });
        win.add_action(&dark);

        let app = win.application().expect("window has an application");
        for (action, keys) in [
            ("win.open", &["<Ctrl>o"][..]),
            ("win.new", &["<Ctrl>n"]),
            ("win.new-window", &["<Ctrl><Shift>n"]),
            ("win.split", &["<Ctrl>backslash"]),
            ("win.save", &["<Ctrl>s"]),
            ("win.save-as", &["<Ctrl><Shift>s"]),
            ("win.export-pdf", &["<Ctrl><Shift>e"]),
            ("win.edit", &["<Ctrl>e"]),
            ("win.zoom-in", &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"]),
            ("win.zoom-out", &["<Ctrl>minus", "<Ctrl>KP_Subtract"]),
            ("win.fit", &["<Ctrl>0"]),
            ("win.find", &["<Ctrl>f"]),
            ("win.sidebar", &["F9"]),
            // N and P as well, but only away from text: see `connect_window`.
            ("win.next-page", &["<Ctrl>Page_Down"]),
            ("win.prev-page", &["<Ctrl>Page_Up"]),
            ("win.first-page", &["<Ctrl>Home"]),
            ("win.last-page", &["<Ctrl>End"]),
            ("win.go-to-page", &["<Ctrl>g"]),
            ("win.highlight", &["<Ctrl>h"]),
            ("win.undo", &["<Ctrl>z"]),
            ("win.redo", &["<Ctrl><Shift>z", "<Ctrl>y"]),
            ("win.shortcuts", &["<Ctrl>question"]),
            ("win.close", &["<Ctrl>w"]),
            ("win.close-window", &["<Ctrl><Shift>w"]),
        ] {
            app.set_accels_for_action(action, keys);
        }
    }

    /// What belongs to this tab: its page counter, its search, its menus,
    /// and becoming the active tab when it is clicked or focused.
    fn connect_tab(&self) {
        let this = self.clone();
        self.1.page_entry.connect_activate(move |entry| {
            let count = this.pdf_page_count();
            match entry.text().trim().parse::<usize>().ok().filter(|&n| n >= 1 && n <= count) {
                Some(page) => {
                    entry.remove_css_class("error");
                    this.with_pdf(|v| v.go_to(page - 1, 0.0));
                    this.1.pill.popdown();
                }
                None => entry.add_css_class("error"),
            }
        });
        self.1.page_entry.connect_changed(|entry| entry.remove_css_class("error"));

        // Open on the page being read, ready to be typed over.
        if let Some(popover) = self.1.pill.popover() {
            let (this, entry) = (self.clone(), self.1.page_entry.clone());
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
        self.1.search.connect_activate(move |_| this.find_next(false));
        let this = self.clone();
        self.1.search.connect_next_match(move |_| this.find_next(false));
        let this = self.clone();
        self.1.search.connect_previous_match(move |_| this.find_next(true));

        // The context menu's spot is for the actions it offers; once it is
        // gone, actions apply to the page being read again.
        let this = self.clone();
        self.1.context_menu.connect_closed(move |_| {
            let this = this.clone();
            // Closing comes before the chosen action runs.
            glib::idle_add_local_once(move || {
                this.1.context_spot.set(None);
                this.1.context_annotation.set(None);
            });
        });

        // Side by side, the document last clicked or typed into is the one
        // the header and the shortcuts act on.
        let click = gtk::GestureClick::builder().button(0).propagation_phase(gtk::PropagationPhase::Capture).build();
        let this = self.clone();
        click.connect_pressed(move |_, _, _, _| this.activate_tab());
        self.1.root.add_controller(click);
        let focus = gtk::EventControllerFocus::new();
        let this = self.clone();
        focus.connect_enter(move |_| this.activate_tab());
        self.1.root.add_controller(focus);
    }

    /// What belongs to the window as a whole: closing it, and the N and P
    /// page keys.
    fn connect_window(&self) {
        let this = self.clone();
        self.0.window.connect_close_request(move |window| {
            let tabs: Vec<Rc<Tab>> = this.0.tabs.borrow().clone();
            for tab in &tabs {
                this.with_tab(tab.clone()).remember_position();
            }
            // One unsaved document at a time: each asks, and closing again
            // moves on to the next.
            let Some(dirty) = tabs.into_iter().find(|t| t.doc.borrow().as_ref().is_some_and(Doc::dirty)) else {
                return glib::Propagation::Proceed;
            };
            let tab = this.with_tab(dirty);
            tab.select();
            let window = window.clone();
            tab.confirm_discard(move || window.close());
            glib::Propagation::Stop
        });

        // N and P turn the page — unless something that takes typing has
        // the focus, where they are letters: a search, a note, a document.
        // As window shortcuts they would be taken before any text field saw
        // them.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = self.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            let modifiers = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK;
            if state.intersects(modifiers) || !matches!(key, gdk::Key::n | gdk::Key::p) {
                return glib::Propagation::Proceed;
            }
            let typing = gtk::prelude::GtkWindowExt::focus(&this.0.window).is_some_and(|w| {
                w.is::<gtk::Editable>() || w.downcast_ref::<gtk::TextView>().is_some_and(|t| t.is_editable())
            });
            let active = this.active();
            if typing || !active.is_pdf() {
                return glib::Propagation::Proceed;
            }
            active.with_pdf(|v| if key == gdk::Key::n { v.next_page() } else { v.prev_page() });
            glib::Propagation::Stop
        });
        self.0.window.add_controller(keys);
    }

    /// The two rows of tabs: which tab is active, closing tabs (asking
    /// about unsaved changes), tabs moving between the rows.
    fn connect_groups(&self) {
        for (g, group) in self.0.groups.iter().enumerate() {
            let this = self.clone();
            group.view.connect_selected_page_notify(move |view| {
                let Some(tab) = view.selected_page().and_then(|p| this.tab_of(&p)) else { return };
                // The row the active tab is in follows its selection; the
                // other row's selection changes nothing.
                let active_here = this.active().page_of(&this.active().1).map(|(i, _)| i);
                if active_here.is_none_or(|i| i == g) {
                    this.with_tab(tab).activate_tab();
                }
            });
            let this = self.clone();
            group.view.connect_close_page(move |view, page| {
                let Some(tab) = this.tab_of(page) else {
                    view.close_page_finish(page, true);
                    return glib::Propagation::Stop;
                };
                let w = this.with_tab(tab.clone());
                if !w.dirty() {
                    w.remember_position();
                    view.close_page_finish(page, true);
                    w.forget_tab();
                    return glib::Propagation::Stop;
                }
                // Kept until the reader says; closed again once they have.
                view.close_page_finish(page, false);
                w.select();
                let (view, page) = (view.clone(), page.clone());
                w.confirm_discard(move || view.close_page(&page));
                glib::Propagation::Stop
            });
            let this = self.clone();
            group.view.connect_page_attached(move |_, _, _| this.tidy_later());
            let this = self.clone();
            group.view.connect_page_detached(move |_, _, _| this.tidy_later());
            let this = self.clone();
            group.view.connect_setup_menu(move |_, page| {
                *this.0.menu_tab.borrow_mut() = page.and_then(|p| this.tab_of(p));
            });
        }
    }

    /// A tab that has been closed: let go of it — its document first, so a
    /// PDF's render threads stop.
    fn forget_tab(&self) {
        self.1.doc.borrow_mut().take();
        if let Some(old) = self.1.content.child_by_name("doc") {
            self.1.content.remove(&old);
        }
        self.1.selection_tools.unparent();
        self.1.context_menu.unparent();
        self.0.tabs.borrow_mut().retain(|t| !Rc::ptr_eq(t, &self.1));
        if self.is_active() {
            *self.0.active.borrow_mut() = None;
        }
    }

    /// Show this tab in its row.
    fn select(&self) {
        if let Some((g, page)) = self.page_of(&self.1) {
            self.0.groups[g].view.set_selected_page(&page);
        }
        self.activate_tab();
    }

    /// Once tabs have finished moving or closing: a row left empty in
    /// split view folds away, a window left with no tabs gets an empty
    /// one, an empty tab beside documents goes, and some tab is active.
    fn tidy_later(&self) {
        let this = self.clone();
        glib::idle_add_local_once(move || this.tidy());
    }

    fn tidy(&self) {
        let [a, b] = &self.0.groups;
        if self.is_split() {
            if b.view.n_pages() == 0 {
                self.set_split(false);
            } else if a.view.n_pages() == 0 {
                while b.view.n_pages() > 0 {
                    b.view.transfer_page(&b.view.nth_page(0), &a.view, a.view.n_pages());
                }
                self.set_split(false);
            }
        }
        for group in &self.0.groups {
            if group.view.n_pages() > 1 {
                let empty: Vec<adw::TabPage> = (0..group.view.n_pages())
                    .map(|i| group.view.nth_page(i))
                    .filter(|p| self.tab_of(p).is_some_and(|t| t.doc.borrow().is_none() && !t.loading.get()))
                    .collect();
                for page in empty {
                    group.view.close_page(&page);
                }
            }
        }
        if a.view.n_pages() == 0 {
            Window::add_tab(&self.0, 0).activate_tab();
        }
        let active_shown = self.0.active.borrow().as_ref().is_some_and(|t| self.page_of(t).is_some());
        if !active_shown && let Some(tab) = a.view.selected_page().and_then(|p| self.tab_of(&p)) {
            self.with_tab(tab).activate_tab();
        }
        self.sync_controls();
    }

    /// Two rows of tabs side by side, or one.
    fn set_split(&self, on: bool) {
        let [a, b] = &self.0.groups;
        if on == self.is_split() {
            return;
        }
        if let Some(action) = self.0.window.lookup_action("split").and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
            action.set_state(&on.to_variant());
        }
        if on {
            b.root.set_visible(true);
            a.bar.set_autohide(false);
            b.bar.set_autohide(false);
            // The document being read moves over, beside the one before it;
            // with only one open, the other side waits for another.
            let active = self.active();
            match active.page_of(&active.1) {
                Some((0, page)) if a.view.n_pages() > 1 => {
                    a.view.transfer_page(&page, &b.view, 0);
                    b.view.set_selected_page(&page);
                    active.activate_tab();
                }
                _ => Window::add_tab(&self.0, 1).activate_tab(),
            }
            let width = self.0.window.width();
            if let Some(paned) = a.root.parent().and_downcast::<gtk::Paned>()
                && width > 0
            {
                paned.set_position(width / 2);
            }
        } else {
            while b.view.n_pages() > 0 {
                let page = b.view.nth_page(0);
                let empty = self.tab_of(&page).is_some_and(|t| t.doc.borrow().is_none());
                if empty {
                    b.view.close_page(&page);
                } else {
                    b.view.transfer_page(&page, &a.view, a.view.n_pages());
                }
            }
            b.root.set_visible(false);
            a.bar.set_autohide(true);
            b.bar.set_autohide(true);
            for group in &self.0.groups {
                group.root.remove_css_class("focused");
            }
        }
        self.sync_controls();
    }

    /// Move a tab to the other row, splitting the window if it is not.
    fn move_to_other_side(&self) {
        let Some((g, page)) = self.page_of(&self.1) else { return };
        let [a, b] = &self.0.groups;
        if !self.is_split() {
            if a.view.n_pages() < 2 {
                self.set_split(true);
                return;
            }
            b.root.set_visible(true);
            a.bar.set_autohide(false);
            b.bar.set_autohide(false);
            if let Some(action) = self.0.window.lookup_action("split").and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
                action.set_state(&true.to_variant());
            }
        }
        let (from, to) = if g == 0 { (a, b) } else { (b, a) };
        from.view.transfer_page(&page, &to.view, to.view.n_pages());
        to.view.set_selected_page(&page);
        *self.0.active.borrow_mut() = None;
        self.activate_tab();
        self.tidy_later();
    }

    /// Close this tab, asking first if it has unsaved changes. Closing the
    /// only tab, when it is empty, closes the window.
    fn close_tab(&self) {
        let Some((g, page)) = self.page_of(&self.1) else { return };
        let total: i32 = self.0.groups.iter().map(|gr| gr.view.n_pages()).sum();
        if total == 1 && self.1.doc.borrow().is_none() {
            self.0.window.close();
            return;
        }
        self.0.groups[g].view.close_page(&page);
    }

    /// A file dropped on the window opens.
    fn install_drop(&self) {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let this = self.clone();
        target.connect_drop(move |_, value, _, _| this.active().drop_files(value, None));
        self.0.window.add_controller(target);
    }

    /// Pictures dropped on a document go into it where they land (with
    /// editing turned on); anything else dropped opens. The document's own
    /// target sees file drops before its text view, which would otherwise
    /// take them as text and type out their paths.
    fn install_document_drop(&self, view: &DocxView) {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        target.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (this, widget) = (self.clone(), view.widget().downgrade());
        target.connect_drop(move |_, value, x, y| {
            let at = widget.upgrade().map(|w| (w.upcast::<gtk::Widget>(), x, y));
            this.drop_files(value, at)
        });
        view.widget().add_controller(target);
    }

    fn drop_files(&self, value: &glib::Value, at: Option<(gtk::Widget, f64, f64)>) -> bool {
        let files = value.get::<gdk::FileList>().map(|l| l.files()).unwrap_or_default();
        let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
        if let Some(view) = self.docx()
            && !paths.is_empty()
            && paths.iter().all(|p| docxview::is_picture_file(p))
        {
            let tv = view.text_view();
            if let Some((widget, x, y)) = at
                && let Some(p) = widget.compute_point(tv, &gtk::graphene::Point::new(x as f32, y as f32))
            {
                let (bx, by) = tv.window_to_buffer_coords(gtk::TextWindowType::Widget, p.x() as i32, p.y() as i32);
                if let Some(iter) = tv.iter_at_location(bx, by) {
                    view.buffer().place_cursor(&iter);
                }
            }
            self.insert_pictures(&paths);
            return true;
        }
        match files.into_iter().next() {
            Some(file) => {
                self.open(file);
                true
            }
            None => false,
        }
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
            "Raven Viewer now opens PDF and Word files"
        } else {
            "Couldn’t set the default (is xdg-mime installed?)"
        });
    }

    fn show_shortcuts(&self) {
        let rows = [
            ("Open (in a new tab)", "Ctrl+O"),
            ("New document", "Ctrl+N"),
            ("New window", "Ctrl+Shift+N"),
            ("Split view: two documents side by side", "Ctrl+\\"),
            ("Next / previous tab", "Ctrl+Tab / Ctrl+Shift+Tab · Alt+1…9"),
            ("Close tab / close window", "Ctrl+W / Ctrl+Shift+W"),
            ("Save / Save As", "Ctrl+S / Ctrl+Shift+S"),
            ("Export as PDF", "Ctrl+Shift+E"),
            ("Find", "Ctrl+F · Enter for next"),
            ("Select text", "Drag · double-click a word · triple-click a line"),
            ("Copy selection", "Ctrl+C"),
            ("Highlight selection", "Ctrl+H"),
            ("Undo / redo", "Ctrl+Z / Ctrl+Shift+Z"),
            ("Edit a document", "Ctrl+E"),
            ("Bold / italic / underline", "Ctrl+B / Ctrl+I / Ctrl+U"),
            ("Paste a picture", "Ctrl+V · or drop a picture file"),
            ("Zoom in / out (PDFs and documents)", "Ctrl++ / Ctrl+− · Ctrl+scroll"),
            ("Fit width / actual size", "Ctrl+0"),
            ("Scroll sideways when zoomed in", "Shift+scroll · the scrollbar"),
            ("Next / previous page", "N / P (away from text) · Ctrl+PgDn / PgUp"),
            ("Go to page", "Ctrl+G"),
            ("First / last page", "Ctrl+Home / Ctrl+End"),
            ("Toggle sidebar", "F9"),
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

impl Tab {
    fn new() -> Tab {
        // ── Search ──────────────────────────────────────────────────────
        let search = gtk::SearchEntry::builder().placeholder_text("Find in document").width_request(320).build();
        let search_bar = gtk::SearchBar::builder().child(&search).show_close_button(true).build();
        search_bar.connect_entry(&search);

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
            .description("Open a PDF, a Word document or a text file, or drop one here.")
            .css_classes(["welcome"])
            .build();
        let ctas = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).halign(gtk::Align::Center).build();
        for (label, action, suggested) in
            [("Open Document…", "win.open", true), ("New Document", "win.new", false), ("Combine Files…", "win.combine", false)]
        {
            let b = gtk::Button::builder().label(label).action_name(action).halign(gtk::Align::Center).css_classes(["pill"]).build();
            if suggested {
                b.add_css_class("suggested-action");
            }
            ctas.append(&b);
        }
        welcome.set_child(Some(&ctas));

        // Offscreen, in tests, nothing drives the frame clock and a
        // crossfade would never finish.
        let fade = if cfg!(test) { gtk::StackTransitionType::None } else { gtk::StackTransitionType::Crossfade };
        let content = gtk::Stack::builder().transition_type(fade).build();
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
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
        root.append(&split);
        split.set_vexpand(true);

        Tab {
            root,
            split,
            content,
            overlay,
            outline,
            notes,
            pill,
            pill_label,
            page_entry,
            page_total,
            search_bar,
            search,
            selection_tools,
            context_menu,
            context_spot: Cell::new(None),
            context_annotation: Cell::new(None),
            outline_handler: RefCell::new(None),
            notes_handler: RefCell::new(None),
            doc: RefCell::new(None),
            doc_overwrite_ok: Cell::new(false),
            editing: Cell::new(false),
            loading: Cell::new(false),
            subtitle: RefCell::new(String::new()),
        }
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
    row.append(&icon_button("insert-image-symbolic", "Insert Picture… (or paste or drop one)", "win.insert-picture"));
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

/// What Raven Viewer is made the default for. Text files are opened, but
/// left to the text editor.
pub const MIME_TYPES: [&str; 3] =
    ["application/pdf", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "application/msword"];

fn document_filters() -> gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Documents"));
    for mime in MIME_TYPES.iter().chain(&["text/plain", "text/markdown"]) {
        filter.add_mime_type(mime);
    }
    for suffix in ["pdf", "docx", "doc", "txt", "md"] {
        filter.add_suffix(suffix);
    }
    let all = gtk::FileFilter::new();
    all.set_name(Some("All Files"));
    all.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    filters.append(&all);
    filters
}

/// The formats a document can be saved as, the one suggested first.
fn save_filters(is_pdf: bool, first: Format) -> gio::ListStore {
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    let kinds: &[(Format, &str, &[&str])] = if is_pdf {
        &[(Format::Pdf, "PDF", &["pdf"])]
    } else {
        &[
            (Format::Docx, "Word Document (.docx)", &["docx"]),
            (Format::Doc, "Word 97–2003 Document (.doc)", &["doc"]),
            (Format::Pdf, "PDF (.pdf)", &["pdf"]),
            (Format::Text, "Plain Text (.txt)", &["txt", "md"]),
        ]
    };
    let mut ordered: Vec<_> = kinds.iter().filter(|k| k.0 == first).collect();
    ordered.extend(kinds.iter().filter(|k| k.0 != first));
    for (_, name, suffixes) in ordered {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(name));
        for s in *suffixes {
            filter.add_suffix(s);
        }
        filters.append(&filter);
    }
    filters
}

enum Loaded {
    Pdf(Arc<Vec<u8>>, DocumentInfo),
    Docx(Box<docx::Docx>, Format),
}

/// Sniff the bytes, not the extension: a PDF saved as .bin still opens.
fn load(path: &Path) -> anyhow::Result<Loaded> {
    let bytes = std::fs::read(path)?;
    if Format::sniff(&bytes)? == Format::Pdf {
        let bytes = Arc::new(bytes);
        let info = pdf::load_info(&bytes)?;
        return Ok(Loaded::Pdf(bytes, info));
    }
    let (doc, format) = convert::open_document(&bytes)?;
    Ok(Loaded::Docx(Box::new(doc), format))
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

    fn pictures(view: &DocxView) -> usize {
        view.blocks()
            .iter()
            .map(|b| match b {
                docx::Block::Paragraph { runs, .. } => runs.iter().filter(|r| r.image.is_some()).count(),
                _ => 0,
            })
            .sum()
    }

    /// New documents, pictures, every save format, and opening `.doc` and
    /// text files: `GDK_BACKEND=broadway RAVEN_DRIVE_DIR=… RAVEN_DRIVE_FIXTURES=…
    /// cargo test drive_documents -- --ignored --nocapture`, where the
    /// fixtures directory holds `red.png`, `sample.doc` and `notes.txt`.
    #[test]
    #[ignore]
    fn drive_documents() {
        crate::gtk_test::run(|| {
            adw::init().unwrap();
            crate::theme::apply();
            let app = adw::Application::builder()
                .application_id("com.ravenviewer.RavenDriveDocs")
                .flags(gio::ApplicationFlags::NON_UNIQUE)
                .build();
            app.register(gio::Cancellable::NONE).unwrap();
            let dir = PathBuf::from(std::env::var("RAVEN_DRIVE_DIR").unwrap());
            let fx = PathBuf::from(std::env::var("RAVEN_DRIVE_FIXTURES").unwrap());
            let w = Window::new(&app);
            w.1.content.set_transition_type(gtk::StackTransitionType::None);
            w.0.window.set_default_size(1100, 800);
            w.present();
            pump(300);

            // A new document: a heading, text, a picture from a file.
            let _ = WidgetExt::activate_action(&w.0.window, "win.new", None);
            pump(300);
            assert!(w.active().1.doc.borrow().as_ref().is_some_and(Doc::is_new), "win.new made no new document");
            assert!(w.active().1.editing.get(), "a new document opens for editing");
            let view = w.active().docx().unwrap();
            let buffer = view.buffer().clone();
            buffer.insert_interactive_at_cursor("My Report", true);
            view.set_style(ParaStyle::Heading(1));
            buffer.insert_interactive_at_cursor("\nSome body text, then a picture: ", true);
            w.active().insert_pictures(&[fx.join("red.png")]);
            buffer.insert_interactive_at_cursor(" and text after it.", true);
            pump(300);
            assert_eq!(pictures(&view), 1);
            shot(&w, "n1-new-document");

            // A picture pasted from the clipboard.
            let texture = gdk::Texture::from_filename(fx.join("red.png")).unwrap();
            w.0.window.clipboard().set_texture(&texture);
            buffer.insert_interactive_at_cursor("\nPasted: ", true);
            view.text_view().emit_paste_clipboard();
            pump(800);
            assert_eq!(pictures(&view), 2, "the pasted picture is in the document");
            shot(&w, "n2-pasted-picture");

            // Copying a picture within the document, and undoing.
            let at = buffer.iter_at_mark(&buffer.get_insert());
            let mut before = at;
            before.backward_char();
            buffer.select_range(&before, &at);
            view.text_view().emit_copy_clipboard();
            pump(300);
            buffer.place_cursor(&buffer.end_iter());
            view.text_view().emit_paste_clipboard();
            pump(800);
            eprintln!("after copy and paste within: {} pictures", pictures(&view));
            assert_eq!(pictures(&view), 3, "a picture copied within the document pastes as a picture");
            view.undo();
            pump(200);
            assert_eq!(pictures(&view), 2, "undo takes the pasted picture out");
            view.redo();
            pump(200);
            assert_eq!(pictures(&view), 3, "redo puts it back");
            view.undo();
            pump(200);
            // Deleting a picture and undoing brings the picture back, not a
            // placeholder character.
            let text_before = buffer.slice(&buffer.start_iter(), &buffer.end_iter(), true).to_string();
            let mut s = buffer.start_iter();
            while s.paintable().is_none() && s.forward_char() {}
            let mut e = s;
            e.forward_char();
            buffer.delete_interactive(&mut s, &mut e, true);
            assert_eq!(pictures(&view), 1);
            view.undo();
            pump(200);
            assert_eq!(pictures(&view), 2, "the deleted picture comes back as a picture");
            assert_eq!(buffer.slice(&buffer.start_iter(), &buffer.end_iter(), true).to_string(), text_before);

            // Every format: the PDF is an export; the rest save.
            for ext in ["pdf", "txt", "doc", "docx"] {
                w.active().save_to(dir.join(format!("new.{ext}")), None);
                pump(1000);
            }
            assert!(!w.active().dirty(), "saved as DOCX last, the document is clean");
            let back = docx::load(&std::fs::read(dir.join("new.docx")).unwrap()).unwrap();
            let doc_back = crate::doc::read(&std::fs::read(dir.join("new.doc")).unwrap()).unwrap();
            let count = |blocks: &[docx::Block]| -> usize {
                blocks.iter().map(|b| match b {
                    docx::Block::Paragraph { runs, .. } => runs.iter().filter(|r| r.image.is_some()).count(),
                    _ => 0,
                }).sum()
            };
            eprintln!("new.docx: {} pictures; new.doc: {} pictures; new.txt: {:?}", count(&back.blocks()), count(&doc_back.blocks),
                std::fs::read_to_string(dir.join("new.txt")).unwrap());
            assert_eq!((count(&back.blocks()), count(&doc_back.blocks)), (2, 2));
            assert!(matches!(back.blocks()[0], docx::Block::Paragraph { style: ParaStyle::Heading(1), .. }));
            let pdf_info = pdf::load_info(&Arc::new(std::fs::read(dir.join("new.pdf")).unwrap())).unwrap();
            eprintln!("new.pdf: {} pages, outline {:?}", pdf_info.page_sizes.len(), pdf_info.outline.iter().map(|o| &o.title).collect::<Vec<_>>());

            // The Word 97–2003 file it wrote opens with its pictures.
            w.open(gio::File::for_path(dir.join("new.doc")));
            pump(800);
            assert_eq!(w.active().docx().map(|v| pictures(&v)), Some(2));
            assert!(!w.active().1.editing.get(), "a file opens for reading");
            shot(&w, "n3-reopened-doc");

            // A .doc made elsewhere; a picture dropped on it while reading
            // goes in where it lands, editing turned on.
            w.open(gio::File::for_path(fx.join("sample.doc")));
            pump(800);
            shot(&w, "n4-sample-doc");
            let view = w.active().docx().unwrap();
            let before = pictures(&view);
            let dropped = gdk::FileList::from_array(&[gio::File::for_path(fx.join("red.png"))]).to_value();
            let widget = view.widget().clone().upcast::<gtk::Widget>();
            assert!(w.active().drop_files(&dropped, Some((widget, 200.0, 140.0))));
            pump(300);
            assert!(w.active().1.editing.get(), "dropping a picture turns editing on");
            assert_eq!(pictures(&view), before + 1, "the dropped picture is in the document");
            shot(&w, "n4b-dropped-picture");
            view.undo();
            w.active().set_edit_state(false);
            view.set_unmodified();

            // A text file: edited and saved back as text.
            let notes = dir.join("notes.txt");
            std::fs::copy(fx.join("notes.txt"), &notes).unwrap();
            w.open(gio::File::for_path(&notes));
            pump(800);
            shot(&w, "n5-text-file");
            w.active().set_edit_state(true);
            let view = w.active().docx().unwrap();
            let buffer = view.buffer().clone();
            buffer.place_cursor(&buffer.end_iter());
            buffer.insert_interactive_at_cursor("Added in Raven Viewer\n", true);
            let _ = WidgetExt::activate_action(&w.0.window, "win.save", None);
            pump(800);
            let text = std::fs::read_to_string(&notes).unwrap();
            eprintln!("notes.txt now: {text:?}");
            assert!(text.contains("Added in Raven Viewer") && text.starts_with("Shopping list\n"));
        });
    }

    /// Tabs, split view, zoom with horizontal scrolling, and windows:
    /// `GDK_BACKEND=broadway RAVEN_DRIVE_DIR=… RAVEN_DRIVE_PDF=a.pdf
    /// RAVEN_DRIVE_DOCX=b.docx cargo test drive_tabs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn drive_tabs() {
        crate::gtk_test::run(|| {
            adw::init().unwrap();
            crate::theme::apply();
            let app = adw::Application::builder()
                .application_id("com.ravenviewer.RavenDriveTabs")
                .flags(gio::ApplicationFlags::NON_UNIQUE)
                .build();
            app.register(gio::Cancellable::NONE).unwrap();
            let w = Window::new(&app);
            w.0.window.set_default_size(1280, 800);
            w.present();
            pump(300);
            let pages = |g: usize| w.0.groups[g].view.n_pages();
            let path_of = |t: &Window| t.1.doc.borrow().as_ref().map(|d| display_name(d.path())).unwrap_or_default();

            // Two documents: two tabs in one window, the second shown.
            let pdf_path = PathBuf::from(std::env::var("RAVEN_DRIVE_PDF").unwrap());
            let docx_path = PathBuf::from(std::env::var("RAVEN_DRIVE_DOCX").unwrap());
            w.open(gio::File::for_path(&pdf_path));
            pump(1200);
            w.active().open(gio::File::for_path(&docx_path));
            pump(800);
            assert_eq!(pages(0), 2, "each document in a tab of its own");
            assert_eq!(path_of(&w.active()), display_name(&docx_path));
            // Opening one again shows the tab it is in.
            w.active().open(gio::File::for_path(&pdf_path));
            pump(300);
            assert_eq!((pages(0), path_of(&w.active())), (2, display_name(&pdf_path)));
            assert!(w.0.groups[0].bar.is_tabs_revealed(), "with two documents the tabs show");
            shot(&w, "t1-two-tabs");

            // Side by side: the document being read moves to the right.
            let _ = WidgetExt::activate_action(&w.0.window, "win.split", None);
            pump(600);
            assert!(w.active().is_split());
            assert_eq!((pages(0), pages(1)), (1, 1));
            assert_eq!(w.active().page_of(&w.active().1).map(|(g, _)| g), Some(1));
            assert_eq!(path_of(&w.active()), display_name(&pdf_path));
            // Clicking into the left one makes it the one acted on.
            let left = w.0.groups[0].view.nth_page(0);
            w.with_tab(w.tab_of(&left).unwrap()).activate_tab();
            pump(200);
            assert_eq!(path_of(&w.active()), display_name(&docx_path));
            assert_eq!(w.0.title.title().as_str().trim_start_matches("• "), display_name(&docx_path));
            assert!(w.0.edit_btn.is_visible(), "the header shows what the active document offers");

            // A document zoomed past the window scrolls sideways, with a
            // scrollbar that is there to be seen.
            for _ in 0..4 {
                let _ = WidgetExt::activate_action(&w.0.window, "win.zoom-in", None);
            }
            pump(800);
            let docx = w.active().docx().unwrap();
            let sc = docx.widget().clone();
            let h = sc.hadjustment();
            // Offscreen, layout lands when it lands.
            for _ in 0..10 {
                if h.upper() > h.page_size() + 1.0 {
                    break;
                }
                sc.queue_resize();
                pump(200);
            }
            eprintln!("document at {:.0}%: content {} wide in {} | scrollbar shown {} overlay {}", docx.zoom() * 100.0, h.upper(), h.page_size(), sc.hscrollbar().is_visible(), sc.is_overlay_scrolling());
            assert!(h.upper() > h.page_size() + 1.0, "zoomed in, the sheet is wider than the window");
            assert!(sc.hscrollbar().is_visible() && !sc.is_overlay_scrolling());
            assert_eq!(w.0.zoom_label.label().as_deref(), Some("207%"));
            shot(&w, "t2-split-zoomed-document");
            let _ = WidgetExt::activate_action(&w.0.window, "win.fit", None);
            pump(400);
            assert_eq!(docx.zoom(), 1.0);

            // The PDF on the right, zoomed in, too.
            let right = w.0.groups[1].view.nth_page(0);
            w.with_tab(w.tab_of(&right).unwrap()).activate_tab();
            w.active().with_pdf(|v| v.set_zoom(3.0));
            pump(900);
            let (pdf_view, _) = w.active().pdf().unwrap();
            let sc = pdf_view.widget().clone();
            let h = sc.hadjustment();
            for _ in 0..10 {
                if h.upper() > h.page_size() + 1.0 {
                    break;
                }
                pump(200);
            }
            eprintln!("pdf at 300%: content {} wide in {} | scrollbar shown {}", h.upper(), h.page_size(), sc.hscrollbar().is_visible());
            assert!(h.upper() > h.page_size() + 1.0 && sc.hscrollbar().is_visible() && !sc.is_overlay_scrolling());
            shot(&w, "t3-split-zoomed-pdf");
            // At fit width there is nothing to scroll sideways.
            let _ = WidgetExt::activate_action(&w.0.window, "win.fit", None);
            pump(900);
            let h = pdf_view.widget().hadjustment();
            assert!(h.upper() <= h.page_size() + 0.5, "fit width fits: {} in {}", h.upper(), h.page_size());

            // N and P are page keys only away from text.
            let accels = app.accels_for_action("win.prev-page");
            assert!(!accels.iter().any(|a| a == "p"), "P is not a window shortcut: {accels:?}");

            // Moving the only tab on one side across folds the split away.
            w.active().move_to_other_side();
            pump(500);
            assert!(!w.active().is_split());
            assert_eq!((pages(0), pages(1)), (2, 0));

            // Ctrl+W closes a tab; closing the last empty one closes the window.
            let _ = WidgetExt::activate_action(&w.0.window, "win.close", None);
            pump(400);
            assert_eq!(pages(0), 1);
            let _ = WidgetExt::activate_action(&w.0.window, "win.close", None);
            pump(400);
            assert_eq!(pages(0), 1, "an empty tab takes the last one's place");
            assert!(w.active().1.doc.borrow().is_none());
            shot(&w, "t4-welcome-again");

            // Another window, in the same instance.
            let _ = WidgetExt::activate_action(&w.0.window, "win.new-window", None);
            pump(300);
            assert_eq!(app.windows().len(), 2);
        });
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
            w.1.content.set_transition_type(gtk::StackTransitionType::None);
            w.0.window.set_default_size(1100, 800);
            w.present();
            pump(300);

            // ── PDF ──
            w.open(gio::File::for_path(std::env::var("RAVEN_DRIVE_PDF").unwrap()));
            pump(1500);
            shot(&w, "1-pdf-open");
            let _ = WidgetExt::activate_action(&w.0.window, "win.select-page", None);
            pump(300);
            let (view, _) = w.active().pdf().unwrap();
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
            let notes = w.active().pdf().unwrap().0.info().annotations.len();
            eprintln!("annotations after highlight: {notes}, dirty: {}", w.active().dirty());
            // Rotate the page from the menu action, then undo it.
            let _ = WidgetExt::activate_action(&w.0.window, "win.rotate-right", None);
            pump(1500);
            shot(&w, "4-pdf-rotated");
            let _ = WidgetExt::activate_action(&w.0.window, "win.undo", None);
            pump(1500);
            shot(&w, "5-pdf-undo-rotate");
            let saved = format!("{dir}/saved.pdf");
            w.active().save_to(PathBuf::from(&saved), None);
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
            let view = w.active().docx().unwrap();
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
            w.active().save_to(PathBuf::from(&out), None);
            pump(800);
            let back = docx::load(&std::fs::read(&out).unwrap()).unwrap();
            for b in back.blocks().iter().take(12) {
                if let docx::Block::Paragraph { style, runs, .. } = b {
                    eprintln!("  {style:?} {:?}", runs.iter().map(|r| format!("{}{}{}", if r.bold {"*"} else {""}, if r.highlight {"=="} else {""}, r.text)).collect::<Vec<_>>());
                }
            }
            shot(&w, "8-docx-saved");
        });
    }
}
