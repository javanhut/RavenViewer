//! Lays a DOCX out as a reflowing text sheet — headings by weight and size,
//! emphasis as tags, lists with bullets, tables as grids — that can be read
//! or edited in place.
//!
//! Each paragraph of the file starts with an invisible mark naming it, so on
//! save every line of the buffer can be traced to the paragraph it came from:
//! unchanged ones are written back as they were, edited ones are rewritten
//! from that original, and lines with no origin are new. Tables and
//! paragraphs holding what cannot be edited as text (equations, fields,
//! charts) are shown but not editable, so they cannot be damaged.
//!
//! Pictures sit in the text as paintables that carry the picture itself, so
//! they can be typed around, deleted, cut and pasted like a character, and
//! are read back from wherever they end up. New ones come from a file, the
//! clipboard or a drop.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use libadwaita as adw;

use crate::history::History;
use crate::docx::{self, Block, Docx, Image, ItemKind, LINE_BREAK, Out, PageSetup, ParaStyle, Run, Table, normalize};
use crate::look::{Line, Look, RunLook, Vert};
use crate::render;

pub use picture::Picture;

mod picture {
    use std::cell::{Cell, OnceCell};

    use gtk4 as gtk;
    use gtk4::prelude::*;
    use gtk4::subclass::prelude::*;
    use gtk4::{gdk, glib, graphene, gsk};

    use crate::docx::{EMU_PER_INCH, Image};

    /// Pictures are shown no wider than the sheet's text.
    const MAX_WIDTH: f64 = 680.0;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct Picture {
            pub image: OnceCell<Image>,
            /// Decoded when first drawn; `None` for what cannot be (a
            /// metafile), which is drawn as a frame of its size.
            pub texture: OnceCell<Option<gdk::Texture>>,
            /// Its size at actual size, and the zoom it is shown at.
            pub size: Cell<(i32, i32)>,
            pub zoom: Cell<f64>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for Picture {
            const NAME: &'static str = "RavenPicture";
            type Type = super::Picture;
            type Interfaces = (gdk::Paintable,);
        }

        impl ObjectImpl for Picture {}

        impl Picture {
            fn zoom(&self) -> f64 {
                let z = self.zoom.get();
                if z > 0.0 { z } else { 1.0 }
            }
        }

        impl PaintableImpl for Picture {
            fn intrinsic_width(&self) -> i32 {
                (self.size.get().0 as f64 * self.zoom()).round() as i32
            }

            fn intrinsic_height(&self) -> i32 {
                (self.size.get().1 as f64 * self.zoom()).round() as i32
            }

            fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
                let Some(snapshot) = snapshot.downcast_ref::<gtk::Snapshot>() else { return };
                let rect = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
                match self.obj().texture() {
                    Some(t) => snapshot.append_scaled_texture(&t, gsk::ScalingFilter::Trilinear, &rect),
                    None => {
                        snapshot.append_color(&gdk::RGBA::new(0.55, 0.57, 0.65, 0.18), &rect);
                        let border = gsk::RoundedRect::from_rect(rect, 0.0);
                        let color = gdk::RGBA::new(0.55, 0.57, 0.65, 0.6);
                        snapshot.append_border(&border, &[1.0; 4], &[color; 4]);
                    }
                }
            }
        }
    }

    /// A Cairo image as a texture GTK can draw.
    pub fn texture_of(mut surface: cairo::ImageSurface) -> Option<gdk::Texture> {
        let (w, h, stride) = (surface.width(), surface.height(), surface.stride() as usize);
        let data = surface.data().ok()?.to_vec();
        let format = if cfg!(target_endian = "little") { gdk::MemoryFormat::B8g8r8a8Premultiplied } else { gdk::MemoryFormat::A8r8g8b8Premultiplied };
        Some(gdk::MemoryTexture::new(w, h, format, &glib::Bytes::from_owned(data), stride).upcast())
    }

    glib::wrapper! {
        /// A picture in the text, carrying the picture it shows.
        pub struct Picture(ObjectSubclass<imp::Picture>) @implements gdk::Paintable;
    }

    impl Picture {
        pub fn new(image: Image) -> Self {
            Self::with_texture(image, None)
        }

        /// A picture whose texture is already decoded.
        pub fn with_texture(image: Image, texture: Option<gdk::Texture>) -> Self {
            let obj: Self = glib::Object::new();
            let imp = obj.imp();
            if let Some(t) = texture {
                let _ = imp.texture.set(Some(t));
            }
            // Its size on the page at 96 dpi, or else its pixels; shrunk to
            // the sheet.
            let (mut w, mut h) = if image.cx > 0 && image.cy > 0 {
                let px = |emu: i64| emu as f64 * 96.0 / EMU_PER_INCH as f64;
                (px(image.cx), px(image.cy))
            } else if let Some((w, h)) = crate::emf::size(&image.data) {
                (w * 96.0 / 72.0, h * 96.0 / 72.0)
            } else {
                let _ = imp.image.set(image.clone());
                obj.texture().map_or((96.0, 96.0), |t| (t.width() as f64, t.height() as f64))
            };
            if w > MAX_WIDTH {
                h *= MAX_WIDTH / w;
                w = MAX_WIDTH;
            }
            imp.size.set(((w.round() as i32).max(8), (h.round() as i32).max(8)));
            let _ = imp.image.set(image);
            obj
        }

        /// Show the picture at a zoom, as the text around it is.
        pub fn set_zoom(&self, zoom: f64) {
            self.imp().zoom.set(zoom);
            self.invalidate_size();
        }

        pub fn image(&self) -> Image {
            self.imp().image.get().cloned().unwrap_or_else(|| Image { data: Default::default(), cx: 0, cy: 0, origin: None, anchor: None })
        }

        fn texture(&self) -> Option<gdk::Texture> {
            let imp = self.imp();
            imp.texture
                .get_or_init(|| {
                    let data = &imp.image.get()?.data;
                    if data.is_empty() {
                        return None;
                    }
                    if crate::emf::is_emf(data) {
                        // A metafile, drawn into pixels at twice its size on
                        // screen, for sharpness.
                        let (w, h) = imp.size.get();
                        let surface = crate::emf::rasterize(data, w * 2, h * 2)?;
                        return texture_of(surface);
                    }
                    gdk::Texture::from_bytes(&glib::Bytes::from(&data[..])).ok()
                })
                .clone()
        }
    }
}

const BULLET_TAG: &str = "bullet";
const LOCKED_TAG: &str = "locked";
const PLACEHOLDER_TAG: &str = "placeholder";

/// Character formatting the toolbar toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub highlight: bool,
}

impl Inline {
    fn of(tags: &[gtk::TextTag]) -> Self {
        let has = |n: &str| tags.iter().any(|t| t.name().as_deref() == Some(n));
        Inline { bold: has("bold"), italic: has("italic"), underline: has("underline"), highlight: has("highlight") }
    }

    fn names(self) -> impl Iterator<Item = &'static str> {
        [(self.bold, "bold"), (self.italic, "italic"), (self.underline, "underline"), (self.highlight, "highlight")]
            .into_iter()
            .filter_map(|(on, n)| on.then_some(n))
    }

    fn get(self, name: &str) -> bool {
        match name {
            "bold" => self.bold,
            "italic" => self.italic,
            "underline" => self.underline,
            _ => self.highlight,
        }
    }

    fn set(&mut self, name: &str, on: bool) {
        match name {
            "bold" => self.bold = on,
            "italic" => self.italic = on,
            "underline" => self.underline = on,
            _ => self.highlight = on,
        }
    }
}

struct Inner {
    doc: Docx,
    widget: gtk::ScrolledWindow,
    sheet: gtk::Box,
    view: gtk::TextView,
    buffer: gtk::TextBuffer,
    /// Set while the view changes its own text (building, bullets), so the
    /// edit handlers leave that alone.
    quiet: Cell<bool>,
    /// The paragraph style where text is being inserted, taken before the
    /// insertion lands.
    inserting_into: Cell<ParaStyle>,
    /// Formatting chosen with nothing selected, for the next thing typed.
    typing: Cell<Option<Inline>>,
    on_format: RefCell<Option<FormatListener>>,
    on_problem: RefCell<Option<ProblemListener>>,
    /// Undo and redo, pictures and formatting included; from after the
    /// document was laid out.
    history: OnceCell<History>,
    clamp: adw::Clamp,
    /// How big the sheet is shown: 1.0 is actual size.
    zoom: Cell<f64>,
    /// Sets this view's text size for its zoom.
    zoom_css: gtk::CssProvider,
    on_zoom: RefCell<Option<ZoomListener>>,
    /// The text view's own font size, in points: what tags scale from.
    base_pt: f64,
    /// Tags made for the document's looks, by what they show.
    para_tags: RefCell<HashMap<String, (gtk::TextTag, Look)>>,
    run_tags: RefCell<HashMap<String, gtk::TextTag>>,
    /// Tables shown in the text: where, what, and the widget showing it —
    /// built again when the zoom changes.
    tables: RefCell<Vec<(gtk::TextChildAnchor, Table, gtk::Widget)>>,
    /// Table cells' shading, as rules for the view's stylesheet.
    cell_css: RefCell<String>,
    next_cell: Cell<u32>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_remove_provider_for_display(&display, &self.zoom_css);
        }
    }
}

/// The sheet's widest at actual size, and where it starts to narrow.
const SHEET_WIDTH: f64 = 820.0;
const SHEET_TIGHTENING: f64 = 600.0;
const MIN_ZOOM: f64 = 0.5;
const MAX_ZOOM: f64 = 4.0;

type FormatListener = Box<dyn Fn(ParaStyle, Inline)>;
type ProblemListener = Box<dyn Fn(&str)>;
type ZoomListener = Box<dyn Fn(f64)>;

#[derive(Clone)]
pub struct DocxView {
    inner: Rc<Inner>,
}

impl DocxView {
    pub fn new(doc: Docx) -> Self {
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        // Sizes in the document are points; the view's own font is the one
        // they are scaled from.
        let font = view.pango_context().font_description().unwrap_or_default();
        let base_pt = match font.size() {
            0 => 11.0,
            s if font.is_size_absolute() => s as f64 / gtk::pango::SCALE as f64 * 72.0 / 96.0,
            s => s as f64 / gtk::pango::SCALE as f64,
        };
        let buffer = view.buffer();
        make_tags(&buffer);

        let sheet = gtk::Box::builder().css_classes(["docx-sheet"]).margin_top(28).margin_bottom(28).build();
        sheet.append(&view);
        view.set_hexpand(true);
        let clamp = adw::Clamp::builder()
            .maximum_size(SHEET_WIDTH as i32)
            .tightening_threshold(SHEET_TIGHTENING as i32)
            .child(&sheet)
            .build();
        // Scrollbars that stay: a sheet zoomed wider than the window is
        // scrolled sideways, and a hidden scrollbar is no help with that.
        let widget = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .overlay_scrolling(false)
            .css_classes(["canvas", "docx-view"])
            .child(&clamp)
            .build();

        // The text's size follows the zoom through a stylesheet of the
        // view's own, by its name.
        static NEXT_VIEW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        view.set_widget_name(&format!("docx-{}", NEXT_VIEW.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let zoom_css = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &zoom_css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        }

        let this = DocxView {
            inner: Rc::new(Inner {
                doc,
                widget,
                sheet,
                view,
                buffer,
                quiet: Cell::new(false),
                inserting_into: Cell::new(ParaStyle::Normal),
                typing: Cell::new(None),
                on_format: RefCell::new(None),
                on_problem: RefCell::new(None),
                history: OnceCell::new(),
                clamp,
                zoom: Cell::new(1.0),
                zoom_css,
                on_zoom: RefCell::new(None),
                base_pt,
                para_tags: RefCell::default(),
                run_tags: RefCell::default(),
                tables: RefCell::default(),
                cell_css: RefCell::default(),
                next_cell: Cell::new(0),
            }),
        };
        this.fill();
        let _ = this.inner.history.set(History::attach(&this.inner.buffer));
        this.connect_editing();
        this.connect_paste();
        this.connect_zoom();
        this
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.widget
    }

    pub fn text_view(&self) -> &gtk::TextView {
        &self.inner.view
    }

    pub fn buffer(&self) -> &gtk::TextBuffer {
        &self.inner.buffer
    }

    pub fn set_editable(&self, on: bool) {
        self.inner.view.set_editable(on);
        self.inner.view.set_cursor_visible(on);
        if on {
            self.inner.sheet.add_css_class("editing");
            self.inner.view.grab_focus();
        } else {
            self.inner.sheet.remove_css_class("editing");
        }
    }

    pub fn can_undo(&self) -> bool {
        self.inner.history.get().is_some_and(History::can_undo)
    }

    pub fn can_redo(&self) -> bool {
        self.inner.history.get().is_some_and(History::can_redo)
    }

    pub fn undo(&self) {
        self.replay(History::undo);
    }

    pub fn redo(&self) {
        self.replay(History::redo);
    }

    /// Play history back without the edit handlers reshaping what it puts
    /// back: it is already as it was.
    fn replay(&self, f: impl FnOnce(&History)) {
        let Some(history) = self.inner.history.get() else { return };
        let was_quiet = self.inner.quiet.replace(true);
        f(history);
        self.inner.quiet.set(was_quiet);
        self.inner.typing.set(None);
        self.report_format();
    }

    /// Fired when what can be undone or redone changes.
    pub fn connect_history_changed(&self, f: impl Fn() + 'static) {
        if let Some(history) = self.inner.history.get() {
            history.connect_changed(f);
        }
    }

    pub fn is_modified(&self) -> bool {
        self.inner.buffer.is_modified()
    }

    pub fn set_unmodified(&self) {
        self.inner.buffer.set_modified(false);
    }

    /// Fired when the formatting under the cursor changes, for the toolbar.
    pub fn connect_format_changed(&self, f: impl Fn(ParaStyle, Inline) + 'static) {
        *self.inner.on_format.borrow_mut() = Some(Box::new(f));
        self.report_format();
    }

    fn fill(&self) {
        let (view, buffer) = (&self.inner.view, &self.inner.buffer);
        // The fonts the document brought, for it to be shown in.
        if let Some(map) = view.pango_context().font_map() {
            crate::fonts::apply(&map);
        }
        self.inner.quiet.set(true);
        buffer.begin_irreversible_action();
        for (i, item) in self.inner.doc.items.iter().enumerate() {
            if !item.visible() {
                continue;
            }
            let start = buffer.end_iter();
            buffer.create_mark(Some(&mark_name(i)), &start, true);
            let from = buffer.create_mark(None, &start, true);
            for block in &item.blocks {
                match block {
                    Block::Paragraph { style, runs, look, .. } => self.append_paragraph(*style, runs, look),
                    Block::Table(t) => {
                        let mut end = buffer.end_iter();
                        let anchor = buffer.create_child_anchor(&mut end);
                        let widget = self.table_widget(t);
                        view.add_child_at_anchor(&widget, &anchor);
                        self.inner.tables.borrow_mut().push((anchor, (**t).clone(), widget));
                        buffer.insert(&mut buffer.end_iter(), "\n");
                    }
                }
            }
            if item.kind != ItemKind::Paragraph {
                // Locked text cannot be edited, so it cannot lose this tag
                // either: it names the item wherever the text ends up.
                let own = gtk::TextTag::new(Some(&lock_name(i)));
                buffer.tag_table().add(&own);
                let (s, e) = (buffer.iter_at_mark(&from), buffer.end_iter());
                buffer.apply_tag(&own, &s, &e);
                buffer.apply_tag_by_name(LOCKED_TAG, &s, &e);
            }
            buffer.delete_mark(&from);
        }
        buffer.end_irreversible_action();
        buffer.set_modified(false);
        buffer.place_cursor(&buffer.start_iter());
        self.inner.quiet.set(false);
        self.load_css();
    }

    // ── How the document looks ──────────────────────────────────────────

    /// The tag showing a paragraph's look: its font, spacing, indents and
    /// alignment. Paragraphs that look alike share one.
    fn para_tag(&self, look: &Look) -> gtk::TextTag {
        let key = format!(
            "{:?}{:?}",
            (&look.run.font, look.run.size, look.run.bold, look.run.italic, look.run.color, look.align, look.shading),
            (look.before, look.after, look.left, look.right, look.first, look.line, look.label.is_some())
        );
        if let Some((tag, _)) = self.inner.para_tags.borrow().get(&key) {
            return tag.clone();
        }
        let tag = gtk::TextTag::new(Some(&format!("para-{}", self.inner.para_tags.borrow().len())));
        tag.set_family(Some(&render::family(&look.run.font)));
        tag.set_scale(look.run.size / self.inner.base_pt);
        tag.set_weight(if look.run.bold { 700 } else { 400 });
        tag.set_style(if look.run.italic { gtk::pango::Style::Italic } else { gtk::pango::Style::Normal });
        tag.set_foreground_rgba(Some(&rgba(look.run.color.unwrap_or(INK))));
        tag.set_justification(match look.align {
            docx::Align::Center => gtk::Justification::Center,
            docx::Align::End => gtk::Justification::Right,
            docx::Align::Justify => gtk::Justification::Fill,
            docx::Align::Start => gtk::Justification::Left,
        });
        if let Some(shade) = look.shading {
            tag.set_paragraph_background_rgba(Some(&rgba(shade)));
        }
        self.space(&tag, look);
        self.inner.buffer.tag_table().add(&tag);
        tag.set_priority(0);
        self.inner.para_tags.borrow_mut().insert(key, (tag.clone(), look.clone()));
        tag
    }

    /// A paragraph tag's spacing and indents, in pixels at the zoom.
    fn space(&self, tag: &gtk::TextTag, look: &Look) {
        let px = |pt: f64| (pt * 96.0 / 72.0 * self.zoom()).round() as i32;
        tag.set_pixels_above_lines(px(look.before));
        tag.set_pixels_below_lines(px(look.after));
        let left = px(look.left);
        let first = px(look.first).max(-left);
        tag.set_left_margin(left.max(0));
        tag.set_right_margin(px(look.right).max(0));
        tag.set_indent(first);
        // A list number hangs in front: a tab takes the text on to where
        // the paragraph's other lines start.
        if look.label.is_some() && first < 0 {
            let mut tabs = gtk::pango::TabArray::new(1, true);
            tabs.set_tab(0, gtk::pango::TabAlign::Left, -first);
            tag.set_tabs(Some(&tabs));
        }
        let natural = look.run.size * 1.17;
        let line = match look.line {
            Line::Auto(m) => m,
            Line::Exact(pt) => pt / natural,
            Line::AtLeast(pt) => (pt / natural).max(1.0),
        };
        if (line - 1.0).abs() > 0.02 {
            tag.set_line_height(line.clamp(0.5, 4.0) as f32);
        }
    }

    /// The tag showing what sets a run apart from its paragraph — beyond
    /// bold, italic, underline and highlight, which have tags of their own.
    fn run_tag(&self, para: &Look, run: &Run) -> Option<gtk::TextTag> {
        let rl = render::run_look(run, para);
        let base = &para.run;
        // Emphasis the run has from a character style, not from itself.
        let bold = rl.bold && !run.bold && !base.bold;
        let italic = rl.italic && !run.italic && !base.italic;
        let underline = rl.underline && !run.underline;
        let highlight = rl.highlight.filter(|_| !run.highlight);
        let font = (rl.font != base.font).then(|| rl.font.clone());
        let size = (rl.size != base.size || rl.vert != Vert::Baseline).then_some(rl.size);
        let color = (rl.color != base.color).then_some(rl.color);
        if !(bold || italic || underline || rl.strike || rl.caps || rl.small_caps || highlight.is_some() || font.is_some() || size.is_some() || color.is_some()) {
            return None;
        }
        let key = format!("{:?}", (bold, italic, underline, rl.strike, rl.caps, rl.small_caps, highlight, &font, size, base.size, rl.vert, color));
        if let Some(tag) = self.inner.run_tags.borrow().get(&key) {
            return Some(tag.clone());
        }
        let tag = gtk::TextTag::new(Some(&format!("run-{}", self.inner.run_tags.borrow().len())));
        if bold {
            tag.set_weight(700);
        }
        if italic {
            tag.set_style(gtk::pango::Style::Italic);
        }
        if underline {
            tag.set_underline(gtk::pango::Underline::Single);
        }
        tag.set_strikethrough(rl.strike);
        if rl.small_caps {
            tag.set_variant(gtk::pango::Variant::SmallCaps);
        } else if rl.caps {
            tag.set_variant(gtk::pango::Variant::AllSmallCaps);
        }
        if let Some(h) = highlight {
            tag.set_background_rgba(Some(&rgba(h)));
        }
        if let Some(f) = &font {
            tag.set_family(Some(&render::family(f)));
        }
        if let Some(size) = size {
            // Tags' scales multiply: the paragraph's is already applied.
            let shown = if rl.vert == Vert::Baseline { size } else { size * 0.65 };
            tag.set_scale(shown / base.size.max(1.0));
            let px = size * 96.0 / 72.0 * gtk::pango::SCALE as f64;
            match rl.vert {
                Vert::Super => tag.set_rise((px * 0.33) as i32),
                Vert::Sub => tag.set_rise((-px * 0.14) as i32),
                Vert::Baseline => {}
            }
        }
        if let Some(c) = color {
            tag.set_foreground_rgba(Some(&rgba(c.unwrap_or(INK))));
        }
        self.inner.buffer.tag_table().add(&tag);
        tag.set_priority(self.inner.buffer.tag_table().size() - 1);
        self.inner.run_tags.borrow_mut().insert(key, tag.clone());
        Some(tag)
    }

    fn append_paragraph(&self, style: ParaStyle, runs: &[Run], look: &Look) {
        let buffer = &self.inner.buffer;
        let start = buffer.create_mark(None, &buffer.end_iter(), true);
        if let Some((label, label_look)) = &look.label {
            let mut tags = vec![buffer.tag_table().lookup(BULLET_TAG).unwrap_or_default()];
            let label_run = Run { look: Some(std::sync::Arc::new(label_look.clone())), ..Default::default() };
            tags.extend(self.run_tag(look, &label_run));
            let refs: Vec<&gtk::TextTag> = tags.iter().collect();
            buffer.insert_with_tags(&mut buffer.end_iter(), &format!("{label}\t"), &refs);
        } else if let ParaStyle::ListItem(level) = style {
            buffer.insert_with_tags_by_name(&mut buffer.end_iter(), bullet_text(level), &[BULLET_TAG]);
        }
        for run in runs {
            if let Some(image) = &run.image {
                let picture = Picture::new(image.clone());
                picture.set_zoom(self.zoom());
                buffer.insert_paintable(&mut buffer.end_iter(), &picture);
                continue;
            }
            if let Some(markup) = &run.math {
                // Equations are shown, not edited: they are kept as they were.
                buffer.insert_markup(&mut buffer.end_iter(), markup);
                continue;
            }
            let mut tags: Vec<gtk::TextTag> = Vec::new();
            let table = buffer.tag_table();
            for (on, name) in [(run.bold, "bold"), (run.italic, "italic"), (run.underline, "underline"), (run.highlight, "highlight"), (run.placeholder, PLACEHOLDER_TAG)] {
                if on && let Some(t) = table.lookup(name) {
                    tags.push(t);
                }
            }
            tags.extend(self.run_tag(look, run));
            let refs: Vec<&gtk::TextTag> = tags.iter().collect();
            let text = run.text.replace('\n', &LINE_BREAK.to_string());
            buffer.insert_with_tags(&mut buffer.end_iter(), &text, &refs);
        }
        buffer.insert(&mut buffer.end_iter(), "\n");
        let (s, e) = (buffer.iter_at_mark(&start), buffer.end_iter());
        buffer.apply_tag_by_name(&block_tag(style), &s, &e);
        buffer.apply_tag(&self.para_tag(look), &s, &e);
        buffer.delete_mark(&start);
    }

    /// A table: a grid of cells, each with its paragraphs (formatted, as
    /// labels), pictures and tables, at the table's column widths.
    fn table_widget(&self, t: &Table) -> gtk::Widget {
        let zoom = self.zoom();
        let px = |pt: f64| pt * 96.0 / 72.0 * zoom;
        let available = px(self.inner.doc.page.width - self.inner.doc.page.margins[1] - self.inner.doc.page.margins[3]);
        let cols = t.columns().max(1);
        let widths: Vec<f64> = if t.widths.len() == cols && t.widths.iter().sum::<f64>() > 1.0 {
            let total: f64 = t.widths.iter().map(|w| px(*w)).sum();
            let k = (available / total).min(1.0);
            t.widths.iter().map(|w| px(*w) * k).collect()
        } else {
            vec![available / cols as f64; cols]
        };
        let grid = gtk::Grid::builder().css_classes(["docx-table"]).margin_top(4).margin_bottom(8).build();
        if t.borders {
            grid.add_css_class("bordered");
        }
        for (r, row) in t.rows.iter().enumerate() {
            let mut col = 0usize;
            for cell in row {
                let span = cell.span.max(1);
                let width: f64 = widths.iter().skip(col).take(span).sum();
                let cell_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(2)
                    .css_classes(["docx-cell"])
                    .width_request(width.max(16.0) as i32)
                    .build();
                if let Some([red, g, b]) = cell.shade {
                    let n = self.inner.next_cell.get();
                    self.inner.next_cell.set(n + 1);
                    let name = format!("{}-cell-{n}", self.inner.view.widget_name());
                    cell_box.set_widget_name(&name);
                    self.inner.cell_css.borrow_mut().push_str(&format!("#{name} {{ background-color: #{red:02X}{g:02X}{b:02X}; }}\n"));
                }
                if !cell.merged {
                    self.fill_box(&cell_box, &cell.blocks, width - 12.0);
                }
                grid.attach(&cell_box, col as i32, r as i32, span as i32, 1);
                col += span;
            }
        }
        grid.upcast()
    }

    /// The content of a table cell: labels for paragraphs, pictures, tables.
    fn fill_box(&self, into: &gtk::Box, blocks: &[Block], width: f64) {
        let zoom = self.zoom();
        for block in blocks {
            match block {
                Block::Paragraph { style, runs, look, .. } => {
                    let look = render::effective(look, *style, &self.inner.doc.page);
                    let text_runs: Vec<&Run> = runs.iter().filter(|r| r.image.is_none()).collect();
                    for image in runs.iter().filter_map(|r| r.image.as_ref()) {
                        let picture = Picture::new(image.clone());
                        picture.set_zoom(zoom);
                        let widget = gtk::Picture::builder().paintable(&picture).can_shrink(true).halign(gtk::Align::Start).build();
                        widget.set_size_request(picture.intrinsic_width().min(width as i32), picture.intrinsic_height());
                        into.append(&widget);
                    }
                    let (mut text, attrs) = render::attributed(&look, &text_runs, &|_| String::new());
                    if text.is_empty() && runs.iter().any(|r| r.image.is_some()) {
                        continue;
                    }
                    let all = pango_all(&attrs, &look.run, zoom);
                    if let Some((label, _)) = &look.label {
                        text = format!("{label}  {text}");
                        let shift = (label.len() + 2) as u32;
                        let shifted = gtk::pango::AttrList::new();
                        for mut a in all.attributes() {
                            let (s, e) = (a.start_index(), a.end_index());
                            if s > 0 || e != u32::MAX {
                                a.set_start_index(s + shift);
                                a.set_end_index(e.saturating_add(shift));
                            }
                            shifted.insert(a);
                        }
                        let label_widget = self.cell_label(&text, &shifted, &look, width);
                        into.append(&label_widget);
                    } else {
                        into.append(&self.cell_label(&text, &all, &look, width));
                    }
                }
                Block::Table(t) => into.append(&self.table_widget(t)),
            }
        }
    }

    fn cell_label(&self, text: &str, attrs: &gtk::pango::AttrList, look: &Look, width: f64) -> gtk::Label {
        let label = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .xalign(match look.align {
                docx::Align::Center => 0.5,
                docx::Align::End => 1.0,
                _ => 0.0,
            })
            .justify(match look.align {
                docx::Align::Center => gtk::Justification::Center,
                docx::Align::End => gtk::Justification::Right,
                _ => gtk::Justification::Left,
            })
            .css_classes(["docx-cell-text"])
            .build();
        label.set_text(text);
        label.set_attributes(Some(attrs));
        label.set_size_request((width - look.left).max(8.0) as i32, -1);
        label.set_margin_start((look.left * 96.0 / 72.0 * self.zoom()) as i32);
        label
    }

    /// The view's stylesheet: its text size for the zoom, its cells' shading.
    fn load_css(&self) {
        let name = self.inner.view.widget_name();
        let css = format!("textview#{name} {{ font-size: {:.1}%; }}\n{}", self.zoom() * 100.0, self.inner.cell_css.borrow());
        self.inner.zoom_css.load_from_string(&css);
    }

    /// Show the sheet as white paper, or turned dark for reading at night.
    pub fn set_dark_pages(&self, on: bool) {
        if on {
            self.inner.sheet.add_css_class("dark-page");
        } else {
            self.inner.sheet.remove_css_class("dark-page");
        }
    }

    pub fn sections(&self) -> Vec<docx::Section> {
        self.inner.doc.sections.clone()
    }

    fn clear_para_look(&self, from: &gtk::TextIter, to: &gtk::TextIter) {
        let names: Vec<String> = self.inner.para_tags.borrow().values().filter_map(|(t, _)| t.name().map(|n| n.to_string())).collect();
        for name in names {
            self.inner.buffer.remove_tag_by_name(&name, from, to);
        }
    }

    fn set_para_look(&self, from: &gtk::TextIter, to: &gtk::TextIter, look: &Look) {
        self.clear_para_look(from, to);
        self.inner.buffer.apply_tag(&self.para_tag(look), from, to);
    }

    // ── Reading the buffer back ─────────────────────────────────────────

    /// The document as the buffer now has it, paragraph by paragraph.
    pub fn to_out(&self) -> Vec<Out> {
        let buffer = &self.inner.buffer;
        let doc = &self.inner.doc;
        let locked = buffer.tag_table().lookup(LOCKED_TAG);
        struct Line {
            origins: Vec<usize>,
            /// The locked item this line shows, if it shows one.
            locked_item: Option<usize>,
            locked: bool,
            style: ParaStyle,
            runs: Vec<Run>,
        }
        let mut lines: Vec<Line> = Vec::new();
        let count = buffer.line_count();
        for n in 0..count {
            let Some(start) = buffer.iter_at_line(n) else { continue };
            let mut end = start;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            // The buffer ends with the last paragraph's newline; the empty
            // "line" after it is not a paragraph.
            if n == count - 1 && start == end && n > 0 {
                break;
            }
            let origins: Vec<usize> = start.marks().iter().filter_map(|m| item_of(m.name()?.as_str())).collect();
            let is_locked = locked.as_ref().is_some_and(|t| start.has_tag(t));
            let locked_item = start.tags().iter().find_map(|t| item_of_lock(t.name()?.as_str()));
            let (style, runs) = if is_locked { (ParaStyle::Normal, Vec::new()) } else { read_line(buffer, start, end) };
            lines.push(Line { origins, locked_item, locked: is_locked, style, runs });
        }

        let original = |i: usize| match doc.items[i].blocks.first() {
            Some(Block::Paragraph { style, runs, .. }) => Some((*style, normalize(runs))),
            _ => None,
        };
        let same = |line: &Line, i: usize| {
            doc.items[i].kind == ItemKind::Paragraph
                && original(i).is_some_and(|(s, r)| s == line.style && r == normalize(&line.runs))
        };

        let mut out = Vec::new();
        let mut claimed: Option<usize> = None;
        let mut last_locked: Option<usize> = None;
        // The list item a new item follows: it carries on that list.
        let mut list: Option<usize> = None;
        for (at, line) in lines.iter().enumerate() {
            let in_list = matches!(line.style, ParaStyle::ListItem(_));
            if line.locked {
                list = None;
                // A locked item may span several lines; it is written once.
                if let Some(i) = line.locked_item
                    && last_locked != Some(i)
                {
                    out.push(Out::Keep(i));
                    last_locked = Some(i);
                }
                continue;
            }
            last_locked = None;
            if let Some(i) = claimed.take() {
                out.push(Out::Keep(i));
                list = in_list.then_some(i);
                continue;
            }
            let editable: Vec<usize> =
                line.origins.iter().copied().filter(|&i| doc.items[i].kind == ItemKind::Paragraph).collect();
            if let Some(&i) = editable.iter().find(|&&i| same(line, i)) {
                out.push(Out::Keep(i));
                list = in_list.then_some(i);
                continue;
            }
            // Enter at the start of a paragraph leaves its mark on the new
            // empty line above; the paragraph itself is the next line.
            if let Some(&i) = editable.last()
                && let Some(next) = lines.get(at + 1)
                && next.origins.is_empty()
                && !next.locked
                && same(next, i)
            {
                claimed = Some(i);
                out.push(Out::Para { style: line.style, runs: line.runs.clone(), base: None });
                continue;
            }
            let base = editable.last().copied().or(if in_list { list } else { None });
            let runs = with_props(&line.runs, base.and_then(|b| match doc.items[b].blocks.first() {
                Some(Block::Paragraph { runs, .. }) => Some(runs.as_slice()),
                _ => None,
            }));
            out.push(Out::Para { style: line.style, runs, base });
            list = if in_list { base } else { None };
        }
        out
    }

    pub fn save(&self) -> anyhow::Result<Vec<u8>> {
        self.inner.doc.save(&self.to_out())
    }

    /// The document as it now reads, for setting on paper or saving as text.
    pub fn blocks(&self) -> Vec<Block> {
        self.inner.doc.blocks_of(&self.to_out())
    }

    pub fn page(&self) -> PageSetup {
        self.inner.doc.page.clone()
    }

    // ── Pictures ────────────────────────────────────────────────────────

    /// Put a picture file in at the cursor, in place of any selection. Kinds
    /// Word cannot show are turned into PNG.
    pub fn insert_picture(&self, data: Vec<u8>) -> Result<(), String> {
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from(&data[..]))
            .map_err(|_| "That file isn’t a picture Raven Viewer can read".to_string())?;
        let data = if docx::picture_type(&data).is_some() { data } else { texture.save_to_png_bytes().to_vec() };
        let image = Image::new(data, (texture.width(), texture.height()), self.inner.doc.text_width_emu());
        let picture = Picture::with_texture(image, Some(texture));
        picture.set_zoom(self.zoom());

        let (view, buffer) = (&self.inner.view, &self.inner.buffer);
        buffer.begin_user_action();
        buffer.delete_selection(true, view.is_editable());
        let mut at = buffer.iter_at_mark(&buffer.get_insert());
        if !at.can_insert(view.is_editable()) {
            buffer.end_user_action();
            return Err("A picture can’t go here — this part of the document can’t be edited".into());
        }
        let line = at.line();
        let style = style_at_line(buffer, line);
        buffer.insert_paintable(&mut at, &picture);
        self.restyle_lines(line, line, style);
        buffer.end_user_action();
        buffer.set_modified(true);
        view.grab_focus();
        Ok(())
    }

    /// Paste a picture rather than text when the clipboard holds one and no
    /// text — a screenshot, a picture copied from a browser — or holds only
    /// picture files.
    fn connect_paste(&self) {
        let weak = Rc::downgrade(&self.inner);
        self.inner.view.connect_paste_clipboard(move |tv| {
            let Some(inner) = weak.upgrade() else { return };
            let clipboard = tv.clipboard();
            let formats = clipboard.formats();
            let types = formats.mime_types();
            let has_text = types.iter().any(|m| m.starts_with("text/plain")) || formats.contains_type(glib::GString::static_type());
            let has_files = formats.contains_type(gdk::FileList::static_type());
            let has_image = formats.contains_type(gdk::Texture::static_type()) || types.iter().any(|m| m.starts_with("image/"));
            if !(has_files || (has_image && !has_text)) {
                return;
            }
            tv.stop_signal_emission_by_name("paste-clipboard");
            let this = DocxView { inner };
            glib::spawn_future_local(async move {
                let tv = this.inner.view.clone();
                if has_files
                    && let Ok(value) = clipboard.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await
                    && let Ok(list) = value.get::<gdk::FileList>()
                {
                    let files: Vec<std::path::PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
                    if !files.is_empty() && files.iter().all(|p| is_picture_file(p)) {
                        for path in files {
                            match std::fs::read(&path) {
                                Ok(data) => this.report(this.insert_picture(data)),
                                Err(e) => this.report(Err(format!("Couldn’t read {}: {e}", path.display()))),
                            }
                        }
                        return;
                    }
                    // Files that are not pictures paste as their names.
                    tv.buffer().paste_clipboard(&clipboard, None, tv.is_editable());
                    return;
                }
                match read_picture(&clipboard).await {
                    Some(data) => this.report(this.insert_picture(data)),
                    None => tv.buffer().paste_clipboard(&clipboard, None, tv.is_editable()),
                }
            });
        });
    }

    /// Where problems with pictures are told.
    pub fn connect_problem(&self, f: impl Fn(&str) + 'static) {
        *self.inner.on_problem.borrow_mut() = Some(Box::new(f));
    }

    fn report(&self, result: Result<(), String>) {
        if let (Err(e), Some(f)) = (result, self.inner.on_problem.borrow().as_ref()) {
            f(&e);
        }
    }

    // ── Editing ─────────────────────────────────────────────────────────

    fn connect_editing(&self) {
        let buffer = &self.inner.buffer;

        let weak = Rc::downgrade(&self.inner);
        buffer.connect_insert_text(move |buffer, at, _| {
            if let Some(inner) = weak.upgrade() {
                inner.inserting_into.set(style_at_line(buffer, at.line()));
            }
        });
        let weak = Rc::downgrade(&self.inner);
        buffer.connect_closure(
            "insert-text",
            true,
            glib::closure_local!(move |buffer: gtk::TextBuffer, end: gtk::TextIter, text: &str, _len: i32| {
                let Some(inner) = weak.upgrade() else { return };
                if inner.quiet.get() {
                    return;
                }
                let this = DocxView { inner };
                this.after_insert(&buffer, end, text);
            }),
        );
        let weak = Rc::downgrade(&self.inner);
        buffer.connect_closure(
            "delete-range",
            true,
            glib::closure_local!(move |buffer: gtk::TextBuffer, at: gtk::TextIter, _end: gtk::TextIter| {
                let Some(inner) = weak.upgrade() else { return };
                if inner.quiet.get() {
                    return;
                }
                let this = DocxView { inner };
                // Deleting the line break before locked text would pull it
                // into the paragraph above, which would then be rewritten
                // with it — image and all. Put the break back.
                if !at.starts_line()
                    && let Some(locked) = buffer.tag_table().lookup(LOCKED_TAG)
                    && at.has_tag(&locked)
                {
                    let offset = at.offset();
                    this.inner.quiet.set(true);
                    let mut at = at;
                    buffer.insert(&mut at, "\n");
                    this.inner.quiet.set(false);
                    buffer.place_cursor(&buffer.iter_at_offset(offset));
                }
                let line = buffer.iter_at_mark(&buffer.get_insert()).line();
                let style = style_at_line(&buffer, line);
                this.restyle_lines(line, line, style);
            }),
        );

        let weak = Rc::downgrade(&self.inner);
        buffer.connect_mark_set(move |buffer, _, mark| {
            if mark == &buffer.get_insert()
                && let Some(inner) = weak.upgrade()
            {
                inner.typing.set(None);
                DocxView { inner }.report_format();
            }
        });

        // Backspace just after a bullet ends the list item, as does Enter on
        // an empty one: the bullet itself cannot be deleted as text.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&self.inner);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(inner) = weak.upgrade() else { return glib::Propagation::Proceed };
            if !inner.view.is_editable() || state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK) {
                return glib::Propagation::Proceed;
            }
            let buffer = &inner.buffer;
            if buffer.has_selection() {
                return glib::Propagation::Proceed;
            }
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            let ParaStyle::ListItem(_) = style_at_line(buffer, cursor.line()) else {
                return glib::Propagation::Proceed;
            };
            let this = DocxView { inner: inner.clone() };
            let after_bullet = cursor.offset() == text_start(buffer, cursor.line()).offset();
            let empty_item = after_bullet && cursor.ends_line();
            match key {
                gdk::Key::BackSpace if after_bullet => {
                    this.set_style(ParaStyle::Normal);
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter if empty_item => {
                    this.set_style(ParaStyle::Normal);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.inner.view.add_controller(keys);
    }

    fn after_insert(&self, buffer: &gtk::TextBuffer, end: gtk::TextIter, text: &str) {
        let mut start = end;
        start.backward_chars(text.chars().count() as i32);
        // Typed text takes the formatting of what it follows (or of what it
        // precedes, at the start of a line) unless the toolbar said otherwise.
        let inline = self.inner.typing.get().unwrap_or_else(|| {
            let mut probe = start;
            if !start.starts_line() && probe.backward_char() {
                Inline::of(&probe.tags())
            } else {
                Inline::of(&end.tags())
            }
        });
        for name in ["bold", "italic", "underline", "highlight"] {
            buffer.remove_tag_by_name(name, &start, &end);
        }
        for name in inline.names() {
            buffer.apply_tag_by_name(name, &start, &end);
        }
        for name in [LOCKED_TAG, PLACEHOLDER_TAG, BULLET_TAG] {
            buffer.remove_tag_by_name(name, &start, &end);
        }
        // …and the look of the paragraph and run it went into.
        let mut probe = start;
        let neighbour = if !start.starts_line() && probe.backward_char() { probe } else { end };
        for tag in neighbour.tags() {
            if tag.name().is_some_and(|n| n.starts_with("para-") || n.starts_with("run-")) {
                buffer.apply_tag(&tag, &start, &end);
            }
        }
        // The paragraph it went into keeps its style, and so does the text
        // that followed the insertion point on to its new line. Lines the
        // insertion created are new paragraphs of the same kind — except
        // after a heading, where the next paragraph is ordinary text.
        // Text typed at the start of locked text becomes a paragraph of its
        // own in front of it, rather than part of what cannot be edited.
        let mut end = end;
        if !text.ends_with('\n')
            && let Some(locked) = buffer.tag_table().lookup(LOCKED_TAG)
            && end.has_tag(&locked)
        {
            let offset = end.offset();
            self.inner.quiet.set(true);
            buffer.insert(&mut end, "\n");
            self.inner.quiet.set(false);
            buffer.place_cursor(&buffer.iter_at_offset(offset));
            end = buffer.iter_at_offset(offset);
            start = buffer.iter_at_offset(offset - text.chars().count() as i32);
            self.inner.inserting_into.set(ParaStyle::Normal);
        }
        let style = self.inner.inserting_into.get();
        // Taken before restyling, which may insert bullets and so move
        // every iterator.
        let (first, last) = (start.line(), end.line());
        let carried = !end.ends_line();
        // Lines the insertion made are new paragraphs, saved with their
        // kind's style rather than the properties of the one they were
        // split from; shown so too. A new list item looks like the item it
        // follows, whose list it carries on.
        if last > first
            && let Some(from) = buffer.iter_at_line(first + 1)
        {
            let mut to = buffer.iter_at_line(last).unwrap_or(buffer.end_iter());
            to.forward_line();
            let next = match style {
                ParaStyle::Title | ParaStyle::Heading(_) => ParaStyle::Normal,
                s => s,
            };
            let previous = buffer.iter_at_line(first).and_then(|s| s.tags().into_iter().find(|t| t.name().is_some_and(|n| n.starts_with("para-"))));
            match (next, previous) {
                (ParaStyle::ListItem(_), Some(tag)) => {
                    self.clear_para_look(&from, &to);
                    buffer.apply_tag(&tag, &from, &to);
                }
                _ => self.set_para_look(&from, &to, &self.inner.doc.look_for(next)),
            }
        }
        self.restyle_lines(first, first, style);
        if last > first {
            let next = match style {
                ParaStyle::Title | ParaStyle::Heading(_) => ParaStyle::Normal,
                s => s,
            };
            if last > first + 1 {
                self.restyle_lines(first + 1, last - 1, next);
            }
            // Enter in the middle or at the start of a paragraph carries its
            // text on to the new line, and the text keeps its style.
            self.restyle_lines(last, last, if carried { style } else { next });
        }
    }

    /// Give lines `from..=to` one paragraph style, bullets and all.
    fn restyle_lines(&self, from: i32, to: i32, style: ParaStyle) {
        let buffer = &self.inner.buffer;
        let was_quiet = self.inner.quiet.replace(true);
        for line in from..=to {
            let Some(start) = buffer.iter_at_line(line) else { continue };
            // Stray bullets — a list item joined onto the line above — go.
            let mut end = start;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            let bullet = buffer.tag_table().lookup(BULLET_TAG);
            if let Some(bullet) = &bullet {
                let mut at = start;
                let mut stray = Vec::new();
                while at < end {
                    let mut next = at;
                    if !next.forward_to_tag_toggle(Some(bullet)) || next > end {
                        next = end;
                    }
                    if at.has_tag(bullet) && (at != start || !matches!(style, ParaStyle::ListItem(_))) {
                        stray.push((buffer.create_mark(None, &at, true), buffer.create_mark(None, &next, false)));
                    }
                    at = next;
                }
                for (a, b) in stray {
                    let (mut s, mut e) = (buffer.iter_at_mark(&a), buffer.iter_at_mark(&b));
                    buffer.delete(&mut s, &mut e);
                    buffer.delete_mark(&a);
                    buffer.delete_mark(&b);
                }
            }
            let start = buffer.iter_at_line(line).unwrap_or(buffer.end_iter());
            if let ParaStyle::ListItem(level) = style
                && !bullet.as_ref().is_some_and(|b| start.has_tag(b))
            {
                let mut at = start;
                buffer.insert_with_tags_by_name(&mut at, bullet_text(level), &[BULLET_TAG]);
            }
            let start = buffer.iter_at_line(line).unwrap_or(buffer.end_iter());
            let mut end = start;
            end.forward_line();
            // A paragraph that changes kind takes the look of its new kind;
            // one that keeps its kind keeps its own (the file's) look.
            let changed = style_at_line(buffer, line) != style || !has_para_tag(&start);
            for name in BLOCK_TAGS.iter().copied().chain(LIST_TAGS.iter().copied()) {
                buffer.remove_tag_by_name(name, &start, &end);
            }
            buffer.apply_tag_by_name(&block_tag(style), &start, &end);
            if changed {
                self.set_para_look(&start, &end, &self.inner.doc.look_for(style));
            }
        }
        self.inner.quiet.set(was_quiet);
        self.report_format();
    }

    /// The paragraph style of every line the selection touches.
    pub fn set_style(&self, style: ParaStyle) {
        let buffer = &self.inner.buffer;
        let (a, b) = buffer.selection_bounds().unwrap_or_else(|| {
            let at = buffer.iter_at_mark(&buffer.get_insert());
            (at, at)
        });
        let locked = buffer.tag_table().lookup(LOCKED_TAG);
        buffer.begin_user_action();
        for line in a.line()..=b.line() {
            if let (Some(tag), Some(it)) = (&locked, buffer.iter_at_line(line))
                && it.has_tag(tag)
            {
                continue;
            }
            self.restyle_lines(line, line, style);
        }
        buffer.end_user_action();
        buffer.set_modified(true);
    }

    /// Turn a character format on or off: over the selection if there is
    /// one, otherwise for what is typed next.
    pub fn toggle(&self, name: &str) {
        let buffer = &self.inner.buffer;
        match buffer.selection_bounds() {
            Some((start, end)) => {
                let tag = buffer.tag_table().lookup(name);
                let all = tag.as_ref().is_some_and(|t| covers(start, end, t));
                let locked = buffer.tag_table().lookup(LOCKED_TAG);
                buffer.begin_user_action();
                // Only over editable text: a table or an image paragraph is
                // kept exactly as it was.
                for (s, e) in editable_spans(start, end, locked.as_ref()) {
                    if all {
                        buffer.remove_tag_by_name(name, &s, &e);
                    } else {
                        buffer.apply_tag_by_name(name, &s, &e);
                    }
                }
                buffer.end_user_action();
                buffer.set_modified(true);
            }
            None => {
                let at = buffer.iter_at_mark(&buffer.get_insert());
                let mut current = self.inner.typing.get().unwrap_or_else(|| {
                    let mut probe = at;
                    if probe.backward_char() { Inline::of(&probe.tags()) } else { Inline::of(&at.tags()) }
                });
                current.set(name, !current.get(name));
                self.inner.typing.set(Some(current));
            }
        }
        self.report_format();
    }

    fn report_format(&self) {
        if self.inner.on_format.borrow().is_none() {
            return;
        }
        let (style, inline) = self.format();
        if let Some(cb) = self.inner.on_format.borrow().as_ref() {
            cb(style, inline);
        }
    }

    /// The paragraph style and the character formatting at the cursor (or
    /// over the selection).
    pub fn format(&self) -> (ParaStyle, Inline) {
        let buffer = &self.inner.buffer;
        let at = buffer.iter_at_mark(&buffer.get_insert());
        let inline = self.inner.typing.get().unwrap_or_else(|| match buffer.selection_bounds() {
            Some((s, e)) => {
                let table = buffer.tag_table();
                let all = |n: &str| table.lookup(n).is_some_and(|t| covers(s, e, &t));
                Inline { bold: all("bold"), italic: all("italic"), underline: all("underline"), highlight: all("highlight") }
            }
            None => {
                let mut probe = at;
                if !at.starts_line() && probe.backward_char() { Inline::of(&probe.tags()) } else { Inline::of(&at.tags()) }
            }
        });
        (style_at_line(buffer, at.line()), inline)
    }

    // ── Zoom ────────────────────────────────────────────────────────────

    pub fn zoom(&self) -> f64 {
        self.inner.zoom.get()
    }

    /// Show the sheet bigger or smaller: its text, its pictures and its
    /// width. Zoomed in past the window's width, the sheet keeps its width
    /// and is scrolled sideways rather than reflowed.
    pub fn set_zoom(&self, zoom: f64) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - self.zoom()).abs() < 1e-3 {
            return;
        }
        // Keep the place being read where it is on screen.
        let vadj = self.inner.widget.vadjustment();
        let fraction = if vadj.upper() > 0.0 { (vadj.value() + vadj.page_size() / 2.0) / vadj.upper() } else { 0.0 };

        self.inner.zoom.set(zoom);
        for (tag, look) in self.inner.para_tags.borrow().values() {
            self.space(tag, look);
        }
        // Tables are built at a size; built again at the new one.
        self.inner.cell_css.borrow_mut().clear();
        self.inner.next_cell.set(0);
        let tables: Vec<(gtk::TextChildAnchor, Table, gtk::Widget)> = self.inner.tables.borrow_mut().drain(..).collect();
        for (anchor, table, old) in tables {
            self.inner.view.remove(&old);
            let widget = self.table_widget(&table);
            self.inner.view.add_child_at_anchor(&widget, &anchor);
            self.inner.tables.borrow_mut().push((anchor, table, widget));
        }
        self.load_css();
        let inner = &self.inner;
        inner.clamp.set_maximum_size((SHEET_WIDTH * zoom).round() as i32);
        inner.clamp.set_tightening_threshold((SHEET_TIGHTENING * zoom).round() as i32);
        inner.sheet.set_width_request(if zoom > 1.0 { (SHEET_WIDTH * zoom).round() as i32 } else { -1 });
        let mut at = inner.buffer.start_iter();
        loop {
            if let Some(picture) = at.paintable().and_downcast::<Picture>() {
                picture.set_zoom(zoom);
            }
            if !at.forward_char() {
                break;
            }
        }
        let weak = Rc::downgrade(&self.inner);
        glib::idle_add_local_once(move || {
            if let Some(inner) = weak.upgrade() {
                let vadj = inner.widget.vadjustment();
                vadj.set_value(fraction * vadj.upper() - vadj.page_size() / 2.0);
            }
        });
        if let Some(cb) = self.inner.on_zoom.borrow().as_ref() {
            cb(zoom);
        }
    }

    /// Fired when the zoom changes, from the view itself (Ctrl+scroll) too.
    pub fn connect_zoom_changed(&self, f: impl Fn(f64) + 'static) {
        *self.inner.on_zoom.borrow_mut() = Some(Box::new(f));
    }

    /// Ctrl + scroll zooms, as in the PDF view.
    fn connect_zoom(&self) {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&self.inner);
        scroll.connect_scroll(move |ctl, _dx, dy| {
            let ctrl = ctl.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
            match weak.upgrade() {
                Some(inner) if ctrl && dy != 0.0 => {
                    let this = DocxView { inner };
                    this.set_zoom(this.zoom() * if dy < 0.0 { 1.1 } else { 1.0 / 1.1 });
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.inner.widget.add_controller(scroll);
    }
}

/// Whether a file is a picture, by its name.
pub fn is_picture_file(path: &std::path::Path) -> bool {
    let (kind, _) = gio::content_type_guess(Some(path), None);
    kind.starts_with("image/")
}

/// The picture on the clipboard as a file: as it was copied if it is a kind
/// Word shows, otherwise as PNG.
async fn read_picture(clipboard: &gdk::Clipboard) -> Option<Vec<u8>> {
    let offered: Vec<&str> = ["image/png", "image/jpeg", "image/gif", "image/bmp"]
        .into_iter()
        .filter(|m| clipboard.formats().contain_mime_type(m))
        .collect();
    if !offered.is_empty()
        && let Ok((stream, _)) = clipboard.read_future(&offered, glib::Priority::DEFAULT).await
    {
        let mut data = Vec::new();
        while let Ok(chunk) = stream.read_bytes_future(1 << 16, glib::Priority::DEFAULT).await {
            if chunk.is_empty() {
                break;
            }
            data.extend_from_slice(&chunk);
        }
        if gdk::Texture::from_bytes(&glib::Bytes::from(&data[..])).is_ok() {
            return Some(data);
        }
    }
    let texture = clipboard.read_texture_future().await.ok()??;
    Some(texture.save_to_png_bytes().to_vec())
}

fn mark_name(item: usize) -> String {
    format!("item-{item}")
}

fn item_of(mark: &str) -> Option<usize> {
    mark.strip_prefix("item-")?.parse().ok()
}

fn lock_name(item: usize) -> String {
    format!("lock-{item}")
}

fn item_of_lock(tag: &str) -> Option<usize> {
    tag.strip_prefix("lock-")?.parse().ok()
}

/// Which kind of paragraph a line is: markers only — how it looks is in
/// its look tag.
const BLOCK_TAGS: [&str; 9] = ["title", "h1", "h2", "h3", "h4", "h5", "h6", "body", "quote"];

/// Text on the paper: near-black.
const INK: [u8; 3] = [0x1B, 0x1B, 0x1F];

fn rgba([r, g, b]: [u8; 3]) -> gdk::RGBA {
    gdk::RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

fn has_para_tag(at: &gtk::TextIter) -> bool {
    at.tags().iter().any(|t| t.name().is_some_and(|n| n.starts_with("para-")))
}

/// A cell paragraph's attributes over its font, at the zoom.
fn pango_all(attrs: &gtk::pango::AttrList, base: &RunLook, zoom: f64) -> gtk::pango::AttrList {
    let all = gtk::pango::AttrList::new();
    let mut desc = gtk::pango::AttrFontDesc::new(&render::description(base));
    desc.set_start_index(0);
    desc.set_end_index(u32::MAX);
    all.insert(desc);
    let mut scale = gtk::pango::AttrFloat::new_scale(zoom);
    scale.set_start_index(0);
    scale.set_end_index(u32::MAX);
    all.insert(scale);
    for a in attrs.attributes() {
        all.insert(a);
    }
    all
}
const LIST_TAGS: [&str; 6] = ["list0", "list1", "list2", "list3", "list4", "list5"];

fn block_tag(style: ParaStyle) -> String {
    match style {
        ParaStyle::Normal => "body".to_string(),
        ParaStyle::Title => "title".to_string(),
        ParaStyle::Heading(n) => format!("h{}", n.clamp(1, 6)),
        ParaStyle::ListItem(n) => format!("list{}", n.min(5)),
        ParaStyle::Quote => "quote".to_string(),
    }
}

fn style_of_tag(name: &str) -> Option<ParaStyle> {
    Some(match name {
        "body" => ParaStyle::Normal,
        "title" => ParaStyle::Title,
        "quote" => ParaStyle::Quote,
        n if n.starts_with('h') => ParaStyle::Heading(n[1..].parse().ok()?),
        n if n.starts_with("list") => ParaStyle::ListItem(n[4..].parse().ok()?),
        _ => return None,
    })
}

fn style_at_line(buffer: &gtk::TextBuffer, line: i32) -> ParaStyle {
    let Some(start) = buffer.iter_at_line(line) else { return ParaStyle::Normal };
    let mut probe = start;
    // An empty last line has no characters of its own; it takes the style
    // of the paragraph above.
    if probe.is_end() && line > 0 {
        probe.backward_char();
    }
    probe.tags().iter().find_map(|t| style_of_tag(t.name()?.as_str())).unwrap_or(ParaStyle::Normal)
}

fn bullet_text(level: u8) -> &'static str {
    if level.is_multiple_of(2) { "•  " } else { "◦  " }
}

/// Where a line's text starts, past its bullet.
fn text_start(buffer: &gtk::TextBuffer, line: i32) -> gtk::TextIter {
    let mut at = buffer.iter_at_line(line).unwrap_or(buffer.end_iter());
    if let Some(bullet) = buffer.tag_table().lookup(BULLET_TAG)
        && at.has_tag(&bullet)
    {
        at.forward_to_tag_toggle(Some(&bullet));
    }
    at
}

/// A line's style and its text as runs, bullets and placeholders left out.
fn read_line(buffer: &gtk::TextBuffer, start: gtk::TextIter, end: gtk::TextIter) -> (ParaStyle, Vec<Run>) {
    let style = style_at_line(buffer, start.line());
    let skip: Vec<gtk::TextTag> =
        [BULLET_TAG, PLACEHOLDER_TAG].iter().filter_map(|n| buffer.tag_table().lookup(n)).collect();
    let mut runs = Vec::new();
    let mut at = start;
    while at < end {
        let mut next = at;
        if !next.forward_to_tag_toggle(None::<&gtk::TextTag>) || next > end {
            next = end;
        }
        if next == at {
            break;
        }
        if !skip.iter().any(|t| at.has_tag(t)) {
            let flags = Inline::of(&at.tags());
            let run = |text: String| Run {
                text,
                bold: flags.bold,
                italic: flags.italic,
                underline: flags.underline,
                highlight: flags.highlight,
                ..Default::default()
            };
            // The slice, unlike the text, keeps a character for each
            // picture (and each table), where they are.
            let mut text = String::new();
            for (k, c) in buffer.slice(&at, &next, false).chars().enumerate() {
                if c != '\u{FFFC}' {
                    text.push(c);
                    continue;
                }
                let picture = buffer.iter_at_offset(at.offset() + k as i32).paintable().and_downcast::<Picture>();
                if let Some(picture) = picture {
                    if !text.is_empty() {
                        runs.push(run(std::mem::take(&mut text)));
                    }
                    runs.push(Run::picture(picture.image()));
                }
            }
            if !text.is_empty() {
                runs.push(run(text));
            }
        }
        at = next;
    }
    (style, normalize(&runs))
}

/// Give rewritten runs the fonts and sizes of the paragraph they were
/// edited from: each takes the properties of an original run formatted the
/// same way, or of the first one.
fn with_props(runs: &[Run], original: Option<&[Run]>) -> Vec<Run> {
    let Some(original) = original.filter(|o| !o.is_empty()) else { return runs.to_vec() };
    runs.iter()
        .map(|r| {
            let like = original
                .iter()
                .find(|o| (o.bold, o.italic, o.underline, o.highlight) == (r.bold, r.italic, r.underline, r.highlight))
                .unwrap_or(&original[0]);
            Run { props: like.props.clone(), ..r.clone() }
        })
        .collect()
}

fn covers(start: gtk::TextIter, end: gtk::TextIter, tag: &gtk::TextTag) -> bool {
    let mut at = start;
    while at < end {
        if !at.has_tag(tag) && !at.ends_line() {
            return false;
        }
        if !at.forward_char() {
            break;
        }
    }
    true
}

/// The parts of `start..end` not under `locked`.
fn editable_spans(start: gtk::TextIter, end: gtk::TextIter, locked: Option<&gtk::TextTag>) -> Vec<(gtk::TextIter, gtk::TextIter)> {
    let Some(locked) = locked else { return vec![(start, end)] };
    let mut spans = Vec::new();
    let mut at = start;
    while at < end {
        let mut next = at;
        if !next.forward_to_tag_toggle(Some(locked)) || next > end {
            next = end;
        }
        if !at.has_tag(locked) {
            spans.push((at, next));
        }
        if next == at {
            break;
        }
        at = next;
    }
    spans
}

fn make_tags(buffer: &gtk::TextBuffer) {
    let tags = buffer.tag_table();
    let tag = |name: &str, f: &dyn Fn(&gtk::TextTag)| {
        let t = gtk::TextTag::new(Some(name));
        f(&t);
        tags.add(&t);
    };
    for name in BLOCK_TAGS.iter().chain(LIST_TAGS.iter()) {
        tag(name, &|_| {});
    }
    tag("bold", &|t| t.set_weight(700));
    tag("italic", &|t| t.set_style(gtk::pango::Style::Italic));
    tag("underline", &|t| t.set_underline(gtk::pango::Underline::Single));
    tag("highlight", &|t| {
        t.set_background_rgba(Some(&gdk::RGBA::new(1.0, 0.92, 0.0, 1.0)));
    });
    tag(BULLET_TAG, &|t| t.set_editable(false));
    tag(LOCKED_TAG, &|t| t.set_editable(false));
    tag(PLACEHOLDER_TAG, &|t| {
        t.set_style(gtk::pango::Style::Italic);
        t.set_foreground_rgba(Some(&gdk::RGBA::new(0.45, 0.47, 0.55, 1.0)));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::tests::package;

    fn png() -> Vec<u8> {
        let pixbuf = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 4, 2).unwrap();
        pixbuf.fill(0x3366ccff);
        pixbuf.save_to_bufferv("png", &[]).unwrap()
    }

    fn whole(buffer: &gtk::TextBuffer) -> String {
        buffer.slice(&buffer.start_iter(), &buffer.end_iter(), true).to_string()
    }

    fn pictures(buffer: &gtk::TextBuffer) -> usize {
        let mut n = 0;
        let mut at = buffer.start_iter();
        loop {
            n += at.paintable().is_some() as usize;
            if !at.forward_char() {
                return n;
            }
        }
    }

    #[test]
    fn undo_takes_back_words_formatting_and_pictures() {
        crate::gtk_test::run(undo_round_trip);
    }

    fn undo_round_trip() {
        let doc = crate::docx::load(&package("<w:p><w:r><w:t>Hello</w:t></w:r></w:p>", &[])).unwrap();
        let view = DocxView::new(doc);
        view.set_editable(true);
        let buffer = view.buffer().clone();
        let original = whole(&buffer);
        assert!(!view.can_undo(), "laying the document out is not an edit");

        // Typing is undone a word at a time.
        let mut at = buffer.iter_at_line(0).unwrap();
        at.forward_to_line_end();
        buffer.place_cursor(&at);
        for c in " big world".chars() {
            buffer.insert_interactive_at_cursor(&c.to_string(), true);
        }
        assert_eq!(whole(&buffer), "Hello big world\n");
        view.undo();
        assert_eq!(whole(&buffer), "Hello big\n");
        view.undo();
        assert_eq!(whole(&buffer), original);
        view.redo();
        view.redo();
        assert_eq!(whole(&buffer), "Hello big world\n");

        // Formatting is undone too.
        let s = buffer.iter_at_offset(6);
        let e = buffer.iter_at_offset(9);
        buffer.select_range(&s, &e);
        view.toggle("bold");
        let bold = buffer.tag_table().lookup("bold").unwrap();
        assert!(buffer.iter_at_offset(7).has_tag(&bold));
        view.undo();
        assert!(!buffer.iter_at_offset(7).has_tag(&bold), "undo takes the bold off");
        view.redo();
        assert!(buffer.iter_at_offset(7).has_tag(&bold), "redo puts it back");

        // A picture comes and goes, and saves as one.
        buffer.place_cursor(&buffer.iter_at_offset(5));
        view.insert_picture(png()).unwrap();
        assert_eq!(pictures(&buffer), 1);
        view.undo();
        assert_eq!((pictures(&buffer), whole(&buffer)), (0, "Hello big world\n".to_string()));
        view.redo();
        assert_eq!(pictures(&buffer), 1);
        let saved = crate::docx::load(&view.save().unwrap()).unwrap();
        let Some(Block::Paragraph { runs, .. }) = saved.blocks().into_iter().next() else { panic!() };
        let picture = runs.iter().find_map(|r| r.image.clone()).expect("the picture is saved");
        assert_eq!(picture.data.as_slice(), png().as_slice());
        assert_eq!((picture.cx, picture.cy), (4 * 9525, 2 * 9525), "4×2 pixels at 96 dpi");
        let text: String = runs.iter().filter(|r| r.image.is_none()).map(|r| r.text.as_str()).collect();
        assert_eq!(text, "Hello big world");
    }

    #[test]
    fn edits_in_the_buffer_come_back_as_the_right_paragraphs() {
        crate::gtk_test::run(editing_round_trip);
    }

    fn editing_round_trip() {
        let body = r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title here</w:t></w:r></w:p><w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>First para</w:t></w:r></w:p><w:p><w:r><w:drawing/></w:r><w:r><w:t>With image</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p><w:p><w:r><w:t>Last</w:t></w:r></w:p>"#;
        let numbering = r#"<?xml version="1.0"?><w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl></w:abstractNum><w:num w:numId="3"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
        let doc = crate::docx::load(&package(body, &[("word/numbering.xml", numbering)])).unwrap();
        let view = DocxView::new(doc);
        let buffer = view.buffer().clone();

        // Untouched: every paragraph kept as it was.
        assert_eq!(view.to_out(), vec![Out::Keep(0), Out::Keep(1), Out::Keep(2), Out::Keep(3), Out::Keep(4)]);
        assert!(buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).contains("•\tItem"));

        view.set_editable(true);
        // Type at the end of "First para".
        let mut at = buffer.iter_at_line(1).unwrap();
        at.forward_to_line_end();
        buffer.insert_interactive(&mut at, " more", true);
        // Enter at the end of "Item" makes a new bullet.
        let mut at = buffer.iter_at_line(3).unwrap();
        at.forward_to_line_end();
        buffer.insert_interactive(&mut at, "\nSecond item", true);
        // Bold the word "Last".
        let s = buffer.iter_at_line(5).unwrap();
        let mut e = s;
        e.forward_to_line_end();
        buffer.select_range(&s, &e);
        view.toggle("bold");
        // The image paragraph cannot be typed into: typing at its start
        // makes a paragraph of its own in front of it, and backspacing into
        // it from its start is undone.
        let mut at = buffer.iter_at_line(2).unwrap();
        at.forward_char();
        assert!(!buffer.insert_interactive(&mut at, "nope", true), "a locked paragraph took text");
        let mut at = buffer.iter_at_line(2).unwrap();
        buffer.insert_interactive(&mut at, "Before", true);
        let mut s = buffer.iter_at_line(3).unwrap();
        let mut e = s;
        s.backward_char();
        buffer.delete_interactive(&mut s, &mut e, true);

        let out = view.to_out();
        assert_eq!(out[0], Out::Keep(0));
        let Out::Para { style, runs, base } = &out[1] else { panic!("{:?}", out[1]) };
        assert_eq!((*style, runs[0].text.as_str(), *base), (ParaStyle::Normal, "First para more", Some(1)));
        let Out::Para { runs, base: None, .. } = &out[2] else { panic!("{:?}", out[2]) };
        assert_eq!(runs[0].text, "Before");
        assert_eq!(out[3], Out::Keep(2), "the image paragraph is kept verbatim");
        assert_eq!(out[4], Out::Keep(3));
        let Out::Para { style, runs, base } = &out[5] else { panic!("{:?}", out[5]) };
        // A new item carries on the list of the item it follows.
        assert_eq!((*style, runs[0].text.as_str(), *base), (ParaStyle::ListItem(0), "Second item", Some(3)));
        let Out::Para { runs, .. } = &out[6] else { panic!("{:?}", out[6]) };
        assert!(runs[0].bold && runs[0].text == "Last");

        // Enter at the very start of "Title here" pushes it down a line; the
        // heading still comes back as itself rather than as a rewrite.
        let mut at = buffer.start_iter();
        buffer.insert_interactive(&mut at, "\n", true);
        let out = view.to_out();
        assert!(matches!(&out[0], Out::Para { base: None, runs, .. } if runs.is_empty()), "{:?}", out[0]);
        assert_eq!(out[1], Out::Keep(0));

        // Headings change style from the toolbar, and the result saves.
        buffer.place_cursor(&buffer.iter_at_line(2).unwrap());
        assert_eq!(buffer.iter_at_line(2).unwrap().line(), 2);
        view.set_style(ParaStyle::Heading(2));
        let out = view.to_out();
        assert!(matches!(&out[2], Out::Para { style: ParaStyle::Heading(2), base: Some(1), .. }), "{:?}", out[2]);
        let saved = view.save().unwrap();
        let back = crate::docx::load(&saved).unwrap();
        assert!(back.blocks().iter().any(|b| matches!(b, Block::Paragraph { style: ParaStyle::ListItem(0), runs, .. } if runs.iter().any(|r| r.text == "Second item"))));
    }
}
