//! The continuous page view. Every page is a placeholder sized from the PDF
//! up front, so scrolling and the scrollbar are right before anything is
//! rasterized; only pages near the viewport are rendered, and a zoom keeps
//! showing the old pixels (stretched) until the sharp ones arrive.
//!
//! Page tops are kept as a running total rather than re-summed per page, so
//! locating the viewport in a 700-page book is a binary search and not a
//! walk — scrolling one is the same cost as scrolling a pamphlet.
//!
//! Past the zoom where a whole page is a sensible thing to rasterize, pages
//! are drawn as tiles and only the tiles over the viewport are asked for, so
//! zooming in costs no more than the screen it fills.
//!
//! Text can be selected like in any reader: drag across it, double-click a
//! word, triple-click a line, Shift+click to extend. Where the characters are
//! comes from `pdftext`, asked for page by page as pages come on screen.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};

use crate::pagetiles::PageTiles;
use crate::pdf::{DocumentInfo, Grid, RenderedTile, Renderer, TILE, TileKey};
use crate::pdftext::{PageText, TextLayer};

const PAGE_GAP: i32 = 18;
const MARGIN: i32 = 24;
/// Pages kept rasterized on each side of the viewport.
const KEEP: usize = 4;
/// Tiles are asked for this far outside the viewport as well, so panning a
/// zoomed page has something to show before the sharp version lands.
const TILE_MARGIN: f64 = TILE as f64 / 2.0;
/// …but no more than this many bytes of texture in total, so zooming right
/// in on a big book cannot eat the machine.
const TEXTURE_BUDGET: usize = 384 << 20;
/// `RAVEN_DEBUG_TILES=1` reports what the view is asking the renderer for.
/// A renderer you cannot see the working of is hard to keep honest.
static TRACE: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("RAVEN_DEBUG_TILES").is_some());
const MIN_ZOOM: f64 = 0.1;
const MAX_ZOOM: f64 = 8.0;
/// How close to the top or bottom edge a selection drag starts scrolling.
const AUTOSCROLL_EDGE: f64 = 36.0;

#[derive(Clone, Copy, PartialEq)]
enum Fit {
    Width,
    Manual,
}

/// A point on a page, as fractions of the page as displayed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    pub page: usize,
    pub x: f32,
    pub y: f32,
}

/// A caret: before character `.1` of page `.0`.
type Caret = (usize, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: Caret,
    focus: Caret,
}

impl Selection {
    fn ordered(&self) -> (Caret, Caret) {
        if self.anchor <= self.focus { (self.anchor, self.focus) } else { (self.focus, self.anchor) }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Selected text on one page, for copying and marking up.
pub struct PageSelection {
    pub page: usize,
    pub text: String,
    /// Line pieces in PDF user space.
    pub user_boxes: Vec<[f32; 4]>,
}

/// Where a press started, for telling a double- or triple-click from a
/// fresh press, and how far the drag has got.
#[derive(Clone, Copy, Default)]
struct Press {
    time: u32,
    x: f64,
    y: f64,
    count: u32,
}

struct Inner {
    info: RefCell<DocumentInfo>,
    renderer: RefCell<Renderer>,
    results: async_channel::Sender<RenderedTile>,
    scroller: gtk::ScrolledWindow,
    column: gtk::Box,
    pages: Vec<gtk::Picture>,
    /// The textures behind each page, one painter per page.
    tiles: Vec<PageTiles>,
    /// Page tops at the current zoom; see `page_offsets`.
    offsets: RefCell<Vec<f64>>,
    zoom: Cell<f64>,
    fit: Cell<Fit>,
    /// Pages holding textures, and how many bytes each is holding.
    rendered: RefCell<HashMap<usize, usize>>,
    current: Cell<usize>,

    text: RefCell<Rc<TextLayer>>,
    texts: RefCell<HashMap<usize, Arc<PageText>>>,
    asked: RefCell<HashSet<usize>>,
    /// Bumped when the document is swapped, so text still arriving for the
    /// old copy is dropped.
    text_epoch: Cell<u64>,
    selection: Cell<Option<Selection>>,
    /// Pages with selection boxes drawn on them.
    marked: RefCell<HashSet<usize>>,
    press: Cell<Press>,
    /// The pointer during a selection drag, in scroller coordinates.
    drag_at: Cell<Option<(f64, f64)>>,
    autoscroll: RefCell<Option<glib::SourceId>>,
    hovered_annotation: Cell<Option<usize>>,

    on_page_changed: Callback<usize>,
    on_zoom_changed: Callback<f64>,
    on_selection_changed: Callback<bool>,
    on_selection_done: Callback<gdk::Rectangle>,
    on_context_menu: Callback<(Option<Spot>, f64, f64)>,
}

/// A listener the window sets once.
type Callback<T> = RefCell<Option<Box<dyn Fn(T)>>>;

#[derive(Clone)]
pub struct PdfView {
    inner: Rc<Inner>,
}

/// A handle that does not keep a view (and its render threads) alive.
#[derive(Clone)]
pub struct WeakPdfView(std::rc::Weak<Inner>);

impl WeakPdfView {
    pub fn upgrade(&self) -> Option<PdfView> {
        self.0.upgrade().map(|inner| PdfView { inner })
    }
}

impl PdfView {
    pub fn new(bytes: Arc<Vec<u8>>, info: DocumentInfo) -> anyhow::Result<Self> {
        let (tx, rx) = async_channel::unbounded::<RenderedTile>();
        let renderer = Renderer::spawn(bytes.clone(), tx.clone())?;

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(PAGE_GAP)
            .halign(gtk::Align::Center)
            .margin_top(MARGIN)
            .margin_bottom(MARGIN)
            .margin_start(MARGIN)
            .margin_end(MARGIN)
            // Focusable so Ctrl+C and friends reach the view once the
            // reader has clicked into it.
            .focusable(true)
            .build();
        let tiles: Vec<PageTiles> = info.page_sizes.iter().map(|_| PageTiles::new()).collect();
        let pages: Vec<gtk::Picture> = tiles
            .iter()
            .map(|painter| {
                let p = gtk::Picture::builder()
                    .can_shrink(true)
                    .content_fit(gtk::ContentFit::Fill)
                    // Each page is exactly its own width: without this a
                    // narrow page in a document that also holds a wide one
                    // would be stretched to the widest page's width.
                    .halign(gtk::Align::Center)
                    .css_classes(["page"])
                    .paintable(painter)
                    .build();
                column.append(&p);
                p
            })
            .collect();

        let scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .css_classes(["canvas"])
            .child(&column)
            .build();

        let mut accent = crate::theme::accent();
        accent.set_alpha(0.32);
        let inner = Rc::new(Inner {
            info: RefCell::new(info),
            renderer: RefCell::new(renderer),
            results: tx,
            scroller,
            column,
            pages,
            tiles,
            offsets: Default::default(),
            zoom: Cell::new(1.0),
            fit: Cell::new(Fit::Width),
            rendered: Default::default(),
            current: Cell::new(0),
            text: RefCell::new(Rc::new(TextLayer::spawn(bytes))),
            texts: Default::default(),
            asked: Default::default(),
            text_epoch: Cell::new(0),
            selection: Cell::new(None),
            marked: Default::default(),
            press: Cell::new(Press::default()),
            drag_at: Cell::new(None),
            autoscroll: RefCell::new(None),
            hovered_annotation: Cell::new(None),
            on_page_changed: Default::default(),
            on_zoom_changed: Default::default(),
            on_selection_changed: Default::default(),
            on_selection_done: Default::default(),
            on_context_menu: Default::default(),
        });
        SELECTION_COLOR.with(|c| c.set(accent));
        let view = PdfView { inner };
        view.apply_sizes();
        view.connect_signals();
        view.connect_selection();

        let weak = Rc::downgrade(&view.inner);
        glib::spawn_future_local(async move {
            while let Ok(done) = rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                PdfView { inner }.accept(done);
            }
        });
        Ok(view)
    }

    /// Swap in another copy of the document with the same pages — the same
    /// file after an annotation was added. The pages on screen keep showing
    /// what they showed until the new renders land, so nothing flashes.
    /// Returns false when the pages differ and the view must be rebuilt.
    pub fn reload(&self, bytes: Arc<Vec<u8>>, info: DocumentInfo) -> bool {
        if info.page_sizes != self.inner.info.borrow().page_sizes {
            return false;
        }
        let Ok(renderer) = Renderer::spawn(bytes.clone(), self.inner.results.clone()) else { return false };
        renderer.continue_from(self.inner.renderer.borrow().generation());
        *self.inner.renderer.borrow_mut() = renderer;
        *self.inner.info.borrow_mut() = info;
        *self.inner.text.borrow_mut() = Rc::new(TextLayer::spawn(bytes));
        self.inner.text_epoch.set(self.inner.text_epoch.get() + 1);
        self.inner.texts.borrow_mut().clear();
        self.inner.asked.borrow_mut().clear();
        self.set_selection(None);
        self.update_visible();
        true
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.scroller
    }

    pub fn downgrade(&self) -> WeakPdfView {
        WeakPdfView(Rc::downgrade(&self.inner))
    }

    pub fn info(&self) -> std::cell::Ref<'_, DocumentInfo> {
        self.inner.info.borrow()
    }

    pub fn page_count(&self) -> usize {
        self.inner.pages.len()
    }

    pub fn current_page(&self) -> usize {
        self.inner.current.get()
    }

    pub fn connect_page_changed(&self, f: impl Fn(usize) + 'static) {
        *self.inner.on_page_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Fired for every zoom change: buttons, Ctrl+scroll, pinch, re-fit.
    pub fn connect_zoom_changed(&self, f: impl Fn(f64) + 'static) {
        *self.inner.on_zoom_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Fired when there comes to be a selection, or stops being one.
    pub fn connect_selection_changed(&self, f: impl Fn(bool) + 'static) {
        *self.inner.on_selection_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Fired when a drag or a multi-click has selected something, with the
    /// end of the selection in the widget's coordinates — where to offer
    /// what can be done with it.
    pub fn connect_selection_done(&self, f: impl Fn(gdk::Rectangle) + 'static) {
        *self.inner.on_selection_done.borrow_mut() = Some(Box::new(f));
    }

    /// Fired on a right-click: the spot on a page (if it was on one) and the
    /// point in the widget's coordinates.
    pub fn connect_context_menu(&self, f: impl Fn(Option<Spot>, f64, f64) + 'static) {
        *self.inner.on_context_menu.borrow_mut() = Some(Box::new(move |(spot, x, y)| f(spot, x, y)));
    }

    pub fn zoom(&self) -> f64 {
        self.inner.zoom.get()
    }

    pub fn set_zoom(&self, zoom: f64) {
        self.inner.fit.set(Fit::Manual);
        self.zoom_keeping_position(zoom);
    }

    pub fn zoom_by(&self, factor: f64) {
        self.set_zoom(self.zoom() * factor);
    }

    pub fn fit_width(&self) {
        self.inner.fit.set(Fit::Width);
        self.zoom_keeping_position(self.fit_width_zoom());
    }

    /// Scroll so `page` is at the top, `y_fraction` of the way down it.
    pub fn go_to(&self, page: usize, y_fraction: f64) {
        let page = page.min(self.page_count().saturating_sub(1));
        let (top, height) = self.page_extent(page);
        // Land a little above an annotation so it is not against the edge.
        let lead = if y_fraction > 0.0 { 60.0 } else { 0.0 };
        self.scroll_to(top + height * y_fraction - lead);
    }

    /// Where the view is scrolled to, as a page and how far down it — to put
    /// a rebuilt view back where this one was.
    pub fn position(&self) -> (usize, f64) {
        let y = self.inner.scroller.vadjustment().value();
        let page = self.page_at(y);
        let (top, height) = self.page_extent(page);
        (page, if height > 0.0 { ((y - top) / height).max(0.0) } else { 0.0 })
    }

    /// Put the view at a position taken from `position`, at a zoom, exactly.
    pub fn restore(&self, zoom: f64, fit_width: bool, page: usize, y_fraction: f64) {
        if fit_width {
            self.inner.fit.set(Fit::Width);
            self.inner.zoom.set(self.fit_width_zoom());
        } else {
            self.inner.fit.set(Fit::Manual);
            self.inner.zoom.set(zoom.clamp(MIN_ZOOM, MAX_ZOOM));
        }
        if let Some(cb) = self.inner.on_zoom_changed.borrow().as_ref() {
            cb(self.zoom());
        }
        self.inner.renderer.borrow().bump();
        self.apply_sizes();
        let page = page.min(self.page_count().saturating_sub(1));
        let view = self.clone();
        glib::idle_add_local_once(move || {
            let (top, height) = view.page_extent(page);
            view.scroll_to(top + height * y_fraction);
        });
    }

    pub fn is_fit_width(&self) -> bool {
        self.inner.fit.get() == Fit::Width
    }

    pub fn next_page(&self) {
        self.go_to(self.current_page() + 1, 0.0);
    }

    pub fn prev_page(&self) {
        self.go_to(self.current_page().saturating_sub(1), 0.0);
    }

    pub fn first_page(&self) {
        self.go_to(0, 0.0);
    }

    pub fn last_page(&self) {
        self.go_to(self.page_count().saturating_sub(1), 0.0);
    }

    fn fit_width_zoom(&self) -> f64 {
        let avail = self.inner.scroller.width() as f64 - 2.0 * MARGIN as f64 - 16.0;
        let widest = self.inner.info.borrow().page_sizes.iter().map(|s| s.0).fold(1.0, f32::max) as f64;
        if avail <= 0.0 { 1.0 } else { (avail / widest).clamp(MIN_ZOOM, MAX_ZOOM) }
    }

    /// Set the scroll position, re-applying it while GTK clamps it short.
    /// A jump to page 600 right after opening, or straight after a zoom,
    /// asks for a position past the column height GTK has measured so far;
    /// without this the view quietly stops at the bottom of the old layout.
    fn scroll_to(&self, y: f64) {
        self.scroll_to_within(y, 4);
    }

    fn scroll_to_within(&self, y: f64, tries: u8) {
        let adj = self.inner.scroller.vadjustment();
        let y = y.max(0.0);
        adj.set_value(y);
        if tries == 0 || (adj.value() - y).abs() < 0.5 {
            self.update_visible();
            return;
        }
        let view = self.clone();
        glib::idle_add_local_once(move || view.scroll_to_within(y, tries - 1));
    }

    fn zoom_keeping_position(&self, zoom: f64) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - self.zoom()).abs() < 1e-4 {
            // Nothing moves, but this is also the path a first fit-width
            // takes when the document happens to already be at that zoom —
            // it still needs its first pages asked for.
            self.update_visible();
            return;
        }
        // Anchor on what is in the middle of the screen, not on the top edge:
        // zooming then grows the page around what is being read.
        let adj = self.inner.scroller.vadjustment();
        let anchor = adj.value() + adj.page_size() * 0.5;
        let page = self.page_at(anchor);
        let (top, height) = self.page_extent(page);
        let frac = if height > 0.0 { (anchor - top) / height } else { 0.0 };

        self.inner.zoom.set(zoom);
        if let Some(cb) = self.inner.on_zoom_changed.borrow().as_ref() {
            cb(zoom);
        }
        // Every texture is now the wrong size; they stay on screen stretched
        // until the sharp ones land, but none of them counts as done.
        self.inner.renderer.borrow().bump();
        self.apply_sizes();
        // Sizes land on the next layout pass; restore the position after it.
        let view = self.clone();
        glib::idle_add_local_once(move || {
            let (top, height) = view.page_extent(page);
            let half = view.inner.scroller.vadjustment().page_size() * 0.5;
            view.scroll_to(top + height * frac - half);
        });
    }

    fn apply_sizes(&self) {
        let zoom = self.zoom();
        let info = self.inner.info.borrow();
        for (pic, (w, h)) in self.inner.pages.iter().zip(&info.page_sizes) {
            pic.set_size_request((*w as f64 * zoom).round() as i32, (*h as f64 * zoom).round() as i32);
        }
        *self.inner.offsets.borrow_mut() = page_offsets(&info.page_sizes, zoom);
    }

    /// (top offset, height) of a page in scroller coordinates, from the model
    /// rather than widget allocation so it is right before layout happens.
    fn page_extent(&self, page: usize) -> (f64, f64) {
        let offsets = self.inner.offsets.borrow();
        let top = offsets[page];
        (top, offsets[page + 1] - top - PAGE_GAP as f64)
    }

    fn page_at(&self, y: f64) -> usize {
        page_at(&self.inner.offsets.borrow(), y)
    }

    fn connect_signals(&self) {
        let weak = Rc::downgrade(&self.inner);
        self.inner.scroller.vadjustment().connect_value_changed(move |_| {
            if let Some(inner) = weak.upgrade() {
                PdfView { inner }.update_visible();
            }
        });

        // Width changes re-fit while in fit-width mode. `notify::width`
        // is not emitted for allocations, so watch the page-size adjustment.
        // That fires *during* size allocation, and resizing the pages there
        // makes GTK measure a stale layout; so the re-fit waits for idle,
        // coalescing a window drag's many notifications into one.
        let weak = Rc::downgrade(&self.inner);
        let pending = Rc::new(Cell::new(false));
        self.inner.scroller.hadjustment().connect_page_size_notify(move |_| {
            if pending.replace(true) {
                return;
            }
            let (weak, pending) = (weak.clone(), pending.clone());
            glib::idle_add_local_once(move || {
                pending.set(false);
                if let Some(inner) = weak.upgrade() {
                    let view = PdfView { inner };
                    if view.inner.fit.get() == Fit::Width {
                        view.zoom_keeping_position(view.fit_width_zoom());
                    }
                    view.update_visible();
                }
            });
        });

        // Ctrl + scroll zooms, like every other reader.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(&self.inner);
        scroll.connect_scroll(move |ctl, _dx, dy| {
            let ctrl = ctl.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
            match weak.upgrade() {
                Some(inner) if ctrl => {
                    PdfView { inner }.zoom_by(if dy < 0.0 { 1.1 } else { 1.0 / 1.1 });
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.inner.scroller.add_controller(scroll);

        let pinch = gtk::GestureZoom::new();
        let weak = Rc::downgrade(&self.inner);
        let start = Rc::new(Cell::new(1.0));
        let start_begin = start.clone();
        let weak_begin = weak.clone();
        pinch.connect_begin(move |_, _| {
            if let Some(inner) = weak_begin.upgrade() {
                start_begin.set(inner.zoom.get());
            }
        });
        pinch.connect_scale_changed(move |_, scale| {
            if let Some(inner) = weak.upgrade() {
                PdfView { inner }.set_zoom(start.get() * scale);
            }
        });
        self.inner.scroller.add_controller(pinch);
    }

    fn update_visible(&self) {
        let n = self.page_count();
        if n == 0 {
            return;
        }
        let adj = self.inner.scroller.vadjustment();
        let height = adj.page_size().max(1.0);
        let first = self.page_at(adj.value());
        let last = self.page_at(adj.value() + height);
        let current = self.page_at(adj.value() + height * 0.35);

        if current != self.inner.current.replace(current)
            && let Some(cb) = self.inner.on_page_changed.borrow().as_ref()
        {
            cb(current);
        }

        // Which pages to draw, in the order they matter: the page being read,
        // the rest of the viewport, then outwards. The renderer takes the
        // list as a *replacement*, so pages left behind by a jump stop
        // competing with the one now on screen.
        // Read ahead several pages at a reading zoom, where a page is cheap
        // and scrolling is quick. Once a page is tiled it fills the screen on
        // its own, nobody scrolls four of them without the view catching up,
        // and a page's worth of backing render is no longer pocket change.
        let factor = self.inner.scroller.scale_factor() as f64;
        let scale = (self.zoom() * factor) as f32;
        let current_size = self.inner.info.borrow().page_sizes[current];
        let ahead = if Grid::new(current_size, scale).tiled() { 1 } else { KEEP };

        let mut order: Vec<usize> = Vec::with_capacity(last - first + 1 + 2 * ahead);
        let mut want = |page: usize| {
            if page < n && !order.contains(&page) {
                order.push(page);
            }
        };
        want(current);
        for page in first..=last {
            want(page);
        }
        for step in 1..=ahead {
            want(last + step);
            want(first.wrapping_sub(step)); // underflows past 0, filtered by `page < n`
        }

        let generation = self.inner.renderer.borrow().generation();
        let mut wanted: Vec<TileKey> = Vec::new();
        for &page in &order {
            let on_screen = (first..=last).contains(&page);
            let pieces = self.pieces_of(page, on_screen, factor);
            for &key in &pieces {
                if !self.inner.tiles[page].has(key, generation) {
                    wanted.push(key);
                }
            }
            // Drop the tiles that have been panned off; the whole-page render
            // stays behind as the backing.
            self.inner.tiles[page].retain(&pieces);
            if let Some(bytes) = self.inner.rendered.borrow_mut().get_mut(&page) {
                *bytes = self.inner.tiles[page].bytes();
            }
        }
        if *TRACE {
            let grid = Grid::new(current_size, scale);
            let held: usize = self.inner.rendered.borrow().values().sum();
            eprintln!(
                "[tiles] zoom {:.2} scale {scale:.1} | page {current} is {}x{} tiles ({}x{}px), \
                 {} to draw it whole | asked for {} | holding {} MB over {} pages",
                self.zoom(),
                grid.cols,
                grid.rows,
                grid.width,
                grid.height,
                grid.cols as usize * grid.rows as usize,
                wanted.len(),
                held / (1 << 20),
                self.inner.rendered.borrow().len(),
            );
        }
        self.inner.renderer.borrow().submit(scale, wanted);

        // The text of what is on screen, so it can be selected the moment
        // the reader reaches for it.
        for page in first..=last.min(n - 1) {
            self.request_text(page);
        }

        self.evict(first, last, ahead);
    }

    /// The pieces of a page worth having: always the whole-page render, plus
    /// — once the page is tiled — the tiles over the viewport. A page off
    /// screen gets only the whole-page render; drawing sharp tiles for pages
    /// nobody is looking at is what makes a zoomed-in book crawl.
    fn pieces_of(&self, page: usize, on_screen: bool, factor: f64) -> Vec<TileKey> {
        let size = self.inner.info.borrow().page_sizes[page];
        let grid = Grid::new(size, (self.zoom() * factor) as f32);
        let backing = TileKey { page, tile: None };
        if !grid.tiled() || !on_screen {
            return vec![backing];
        }

        // The slice of this page the viewport covers, as fractions of it.
        // Horizontally the page is centred in the scrolled content, which is
        // exact from the adjustment alone — no waiting for an allocation.
        let zoom = self.zoom();
        let (top, page_height) = self.page_extent(page);
        let page_width = (size.0 as f64 * zoom).round().max(1.0);
        let vadj = self.inner.scroller.vadjustment();
        let hadj = self.inner.scroller.hadjustment();
        let left = (hadj.upper().max(page_width) - page_width) / 2.0;

        let margin = TILE_MARGIN / factor;
        let across = |value: f64, span: f64, origin: f64, extent: f64| {
            let from = (value - margin - origin) / extent;
            let to = (value + span + margin - origin) / extent;
            (from, to)
        };
        let (x0, x1) = across(hadj.value(), hadj.page_size(), left, page_width);
        let (y0, y1) = across(vadj.value(), vadj.page_size(), top, page_height.max(1.0));

        let mut pieces = vec![backing];
        for row in grid.rows_over(y0, y1) {
            for col in grid.cols_over(x0, x1) {
                pieces.push(TileKey { page, tile: Some((col, row)) });
            }
        }
        pieces
    }

    /// Drop textures far from the viewport, and further ones while the total
    /// is over budget, so a 1000-page book stays small and a deep zoom does
    /// not. Pages on screen are never dropped.
    fn evict(&self, first: usize, last: usize, ahead: usize) {
        let mut rendered = self.inner.rendered.borrow_mut();
        let window = first.saturating_sub(ahead)..=last.saturating_add(ahead);
        let tiles = &self.inner.tiles;
        rendered.retain(|&page, _| {
            let keep = window.contains(&page);
            if !keep {
                tiles[page].clear();
            }
            keep
        });

        let mut total: usize = rendered.values().sum();
        if total <= TEXTURE_BUDGET {
            return;
        }
        let middle = first.midpoint(last);
        let mut furthest: Vec<usize> = rendered.keys().copied().collect();
        furthest.sort_unstable_by_key(|&p| std::cmp::Reverse(p.abs_diff(middle)));
        for page in furthest {
            if total <= TEXTURE_BUDGET {
                break;
            }
            if (first..=last).contains(&page) {
                continue;
            }
            if let Some(bytes) = rendered.remove(&page) {
                total -= bytes;
                tiles[page].clear();
            }
        }
    }

    fn accept(&self, done: RenderedTile) {
        if done.generation != self.inner.renderer.borrow().generation() {
            return;
        }
        let page = done.key.page;
        self.inner.tiles[page].accept(done);
        self.inner.rendered.borrow_mut().insert(page, self.inner.tiles[page].bytes());
    }

    pub fn set_dark_pages(&self, dark: bool) {
        for p in &self.inner.pages {
            if dark { p.add_css_class("dark-page") } else { p.remove_css_class("dark-page") }
        }
    }

    // ── Text ────────────────────────────────────────────────────────────

    fn request_text(&self, page: usize) {
        if self.inner.texts.borrow().contains_key(&page) || !self.inner.asked.borrow_mut().insert(page) {
            return;
        }
        let layer = self.inner.text.borrow().clone();
        let epoch = self.inner.text_epoch.get();
        let weak = Rc::downgrade(&self.inner);
        glib::spawn_future_local(async move {
            let text = layer.page(page).await;
            let Some(inner) = weak.upgrade() else { return };
            if inner.text_epoch.get() != epoch {
                return;
            }
            inner.asked.borrow_mut().remove(&page);
            if let Some(text) = text {
                inner.texts.borrow_mut().insert(page, text);
                PdfView { inner }.paint_selection();
            }
        });
    }

    fn text_of(&self, page: usize) -> Option<Arc<PageText>> {
        let text = self.inner.texts.borrow().get(&page).cloned();
        if text.is_none() {
            self.request_text(page);
        }
        text
    }

    /// The page and the spot on it under a point in the column's
    /// coordinates. A point in the gap between pages belongs to the page
    /// above, clamped to its bottom edge.
    fn spot_at_column(&self, x: f64, y: f64) -> Option<Spot> {
        let scroller = &self.inner.scroller;
        let at = self.inner.column.compute_point(scroller, &gtk::graphene::Point::new(x as f32, y as f32))?;
        self.spot_at(at.x() as f64, at.y() as f64)
    }

    /// The same for a point in the scroller's coordinates.
    pub fn spot_at(&self, x: f64, y: f64) -> Option<Spot> {
        let scroller = &self.inner.scroller;
        let content_y = y + scroller.vadjustment().value();
        let page = self.page_at(content_y);
        let picture = self.inner.pages.get(page)?;
        let local = scroller.compute_point(picture, &gtk::graphene::Point::new(x as f32, y as f32))?;
        let (w, h) = (picture.width().max(1) as f32, picture.height().max(1) as f32);
        Some(Spot { page, x: (local.x() / w).clamp(0.0, 1.0), y: (local.y() / h).clamp(0.0, 1.0) })
    }

    fn caret_at(&self, spot: Spot) -> Option<Caret> {
        let text = self.text_of(spot.page)?;
        Some((spot.page, text.caret_at(spot.x, spot.y)?))
    }

    fn set_selection(&self, selection: Option<Selection>) {
        let before = self.inner.selection.get().is_some_and(|s| !s.is_empty());
        self.inner.selection.set(selection);
        self.paint_selection();
        let after = selection.is_some_and(|s| !s.is_empty());
        if before != after
            && let Some(cb) = self.inner.on_selection_changed.borrow().as_ref()
        {
            cb(after);
        }
    }

    fn paint_selection(&self) {
        let color = SELECTION_COLOR.with(Cell::get);
        let mut now: HashMap<usize, Vec<([f32; 4], gdk::RGBA)>> = HashMap::new();
        if let Some((start, end)) = self.inner.selection.get().filter(|s| !s.is_empty()).map(|s| s.ordered()) {
            let texts = self.inner.texts.borrow();
            for page in start.0..=end.0 {
                let Some(text) = texts.get(&page) else { continue };
                let from = if page == start.0 { start.1 } else { 0 };
                let to = if page == end.0 { end.1 } else { text.chars.len() };
                now.insert(page, text.boxes(from, to).into_iter().map(|b| (b, color)).collect());
            }
        }
        let mut marked = self.inner.marked.borrow_mut();
        for &page in marked.iter().filter(|p| !now.contains_key(p)) {
            self.inner.tiles[page].set_marks(Vec::new());
        }
        marked.clear();
        for (page, marks) in now {
            self.inner.tiles[page].set_marks(marks);
            marked.insert(page);
        }
    }

    pub fn has_selection(&self) -> bool {
        self.inner.selection.get().is_some_and(|s| !s.is_empty())
    }

    pub fn clear_selection(&self) {
        self.set_selection(None);
    }

    /// The selection page by page. Pages whose text has not been read yet
    /// (only possible for a selection flung across many pages) are skipped.
    pub fn selection(&self) -> Vec<PageSelection> {
        let Some((start, end)) = self.inner.selection.get().filter(|s| !s.is_empty()).map(|s| s.ordered()) else {
            return Vec::new();
        };
        let texts = self.inner.texts.borrow();
        (start.0..=end.0)
            .filter_map(|page| {
                let text = texts.get(&page)?;
                let from = if page == start.0 { start.1 } else { 0 };
                let to = if page == end.0 { end.1 } else { text.chars.len() };
                (from < to).then(|| PageSelection {
                    page,
                    text: text.text(from, to),
                    user_boxes: text.user_boxes(from, to),
                })
            })
            .collect()
    }

    pub fn selected_text(&self) -> Option<String> {
        let parts: Vec<String> = self.selection().into_iter().map(|s| s.text).collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// Put the selection on the clipboard. False when nothing is selected.
    pub fn copy(&self) -> bool {
        match self.selected_text() {
            Some(text) => {
                self.inner.scroller.clipboard().set_text(&text);
                true
            }
            None => false,
        }
    }

    /// Select all the text on the page being read.
    pub fn select_page(&self) {
        let page = self.current_page();
        if let Some(text) = self.text_of(page).filter(|t| !t.is_empty()) {
            self.set_selection(Some(Selection { anchor: (page, 0), focus: (page, text.chars.len()) }));
        }
    }

    /// The annotation under a spot, as an index into `info().annotations`.
    pub fn annotation_at(&self, spot: Spot) -> Option<usize> {
        let info = self.inner.info.borrow();
        // The last one drawn is the one on top.
        info.annotations.iter().rposition(|a| {
            a.page == spot.page
                && a.rect.is_some_and(|r| (r[0]..=r[2]).contains(&spot.x) && (r[1]..=r[3]).contains(&spot.y))
        })
    }

    /// The last box of the selection, in scroller coordinates.
    fn selection_end_rect(&self) -> Option<gdk::Rectangle> {
        let (_, end) = self.inner.selection.get().filter(|s| !s.is_empty())?.ordered();
        let (start, _) = self.inner.selection.get()?.ordered();
        let text = self.inner.texts.borrow().get(&end.0)?.clone();
        let from = if start.0 == end.0 { start.1 } else { 0 };
        let b = *text.boxes(from, end.1).last()?;
        let picture = &self.inner.pages[end.0];
        let (w, h) = (picture.width() as f32, picture.height() as f32);
        let a = picture.compute_point(&self.inner.scroller, &gtk::graphene::Point::new(b[0] * w, b[1] * h))?;
        let z = picture.compute_point(&self.inner.scroller, &gtk::graphene::Point::new(b[2] * w, b[3] * h))?;
        Some(gdk::Rectangle::new(
            a.x().min(z.x()) as i32,
            a.y().min(z.y()) as i32,
            (z.x() - a.x()).abs().max(1.0) as i32,
            (z.y() - a.y()).abs().max(1.0) as i32,
        ))
    }

    fn selection_done(&self) {
        if let Some(rect) = self.selection_end_rect()
            && let Some(cb) = self.inner.on_selection_done.borrow().as_ref()
        {
            cb(rect);
        }
    }

    fn connect_selection(&self) {
        let column = &self.inner.column;

        // Press, drag, release: select. Counting presses here rather than
        // with a separate click gesture keeps a double-click from being
        // undone by the drag that starts on its second press.
        let drag = gtk::GestureDrag::builder().button(gdk::BUTTON_PRIMARY).build();
        let weak = Rc::downgrade(&self.inner);
        drag.connect_drag_begin(move |gesture, x, y| {
            let Some(inner) = weak.upgrade() else { return };
            let view = PdfView { inner };
            // A finger scrolls; only a pointer selects.
            if gesture.device().is_some_and(|d| d.source() == gdk::InputSource::Touchscreen) {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            view.inner.column.grab_focus();
            let time = gesture.current_event_time();
            let prev = view.inner.press.get();
            let double = gtk::Settings::default().map_or(400, |s| s.gtk_double_click_time()) as u32;
            let near = (prev.x - x).abs() < 6.0 && (prev.y - y).abs() < 6.0;
            let count = if near && time.wrapping_sub(prev.time) <= double { prev.count % 3 + 1 } else { 1 };
            view.inner.press.set(Press { time, x, y, count });

            let Some(caret) = view.spot_at_column(x, y).and_then(|s| view.caret_at(s)) else {
                view.set_selection(None);
                return;
            };
            let shift = gesture.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            let text = view.inner.texts.borrow().get(&caret.0).cloned();
            let selection = match (count, text) {
                (2, Some(t)) => {
                    let (s, e) = t.word_at(caret.1);
                    Selection { anchor: (caret.0, s), focus: (caret.0, e) }
                }
                (3, Some(t)) => {
                    let (s, e) = t.line_around(caret.1);
                    Selection { anchor: (caret.0, s), focus: (caret.0, e) }
                }
                _ if shift => match view.inner.selection.get() {
                    Some(sel) => Selection { anchor: sel.anchor, focus: caret },
                    None => Selection { anchor: caret, focus: caret },
                },
                _ => Selection { anchor: caret, focus: caret },
            };
            view.set_selection(Some(selection));
            if count > 1 || shift {
                view.selection_done();
            }
        });
        let weak = Rc::downgrade(&self.inner);
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some(inner) = weak.upgrade() else { return };
            let view = PdfView { inner };
            if view.inner.press.get().count > 1 {
                return;
            }
            let Some((x, y)) = gesture.start_point() else { return };
            let at = view
                .inner
                .column
                .compute_point(&view.inner.scroller, &gtk::graphene::Point::new((x + dx) as f32, (y + dy) as f32));
            if let Some(at) = at {
                view.inner.drag_at.set(Some((at.x() as f64, at.y() as f64)));
                view.drag_to(at.x() as f64, at.y() as f64);
                view.autoscroll();
            }
        });
        let weak = Rc::downgrade(&self.inner);
        drag.connect_drag_end(move |_, _, _| {
            let Some(inner) = weak.upgrade() else { return };
            inner.drag_at.set(None);
            if let Some(id) = inner.autoscroll.borrow_mut().take() {
                id.remove();
            }
            let view = PdfView { inner };
            if view.inner.press.get().count == 1 && view.has_selection() {
                view.selection_done();
            }
        });
        column.add_controller(drag);

        let menu = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let weak = Rc::downgrade(&self.inner);
        menu.connect_pressed(move |gesture, _, x, y| {
            let Some(inner) = weak.upgrade() else { return };
            let view = PdfView { inner };
            view.inner.column.grab_focus();
            let at = view.inner.column.compute_point(&view.inner.scroller, &gtk::graphene::Point::new(x as f32, y as f32));
            let Some(at) = at else { return };
            let spot = view.spot_at(at.x() as f64, at.y() as f64);
            if let Some(cb) = view.inner.on_context_menu.borrow().as_ref() {
                cb((spot, at.x() as f64, at.y() as f64));
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        column.add_controller(menu);

        // An I-beam over text, and an annotation's words on hover.
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&self.inner);
        motion.connect_motion(move |_, x, y| {
            let Some(inner) = weak.upgrade() else { return };
            let view = PdfView { inner };
            let spot = view.spot_at_column(x, y);
            let over_text = spot.is_some_and(|s| {
                view.inner.texts.borrow().get(&s.page).is_some_and(|t| t.is_over_text(s.x, s.y))
            });
            view.inner.column.set_cursor_from_name(Some(if over_text { "text" } else { "default" }));
            let annotation = spot.and_then(|s| view.annotation_at(s));
            if view.inner.hovered_annotation.replace(annotation) != annotation {
                let tip = annotation.and_then(|i| {
                    let info = view.inner.info.borrow();
                    let a = info.annotations.get(i)?;
                    (!a.contents.is_empty()).then(|| a.contents.clone())
                });
                view.inner.column.set_tooltip_text(tip.as_deref());
            }
        });
        column.add_controller(motion);

        let keys = gtk::ShortcutController::new();
        let add = |trigger: &str, f: fn(&PdfView)| {
            let weak = Rc::downgrade(&self.inner);
            keys.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::CallbackAction::new(move |_, _| {
                    if let Some(inner) = weak.upgrade() {
                        f(&PdfView { inner });
                    }
                    glib::Propagation::Stop
                })),
            ));
        };
        add("<Control>c", |v| {
            v.copy();
        });
        add("<Control>a", |v| v.select_page());
        add("Escape", |v| v.clear_selection());
        self.inner.scroller.add_controller(keys);
    }

    fn drag_to(&self, x: f64, y: f64) {
        let Some(selection) = self.inner.selection.get() else { return };
        if let Some(focus) = self.spot_at(x, y).and_then(|s| self.caret_at(s)) {
            self.set_selection(Some(Selection { focus, ..selection }));
        }
    }

    /// While a selection drag is held near the top or bottom edge, keep
    /// scrolling and keep extending the selection, faster the closer to the
    /// edge — how a selection gets from one page to the next.
    fn autoscroll(&self) {
        if self.inner.autoscroll.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(&self.inner);
        let id = glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let Some(inner) = weak.upgrade() else { return glib::ControlFlow::Break };
            let view = PdfView { inner };
            let Some((x, y)) = view.inner.drag_at.get() else {
                view.inner.autoscroll.borrow_mut().take();
                return glib::ControlFlow::Break;
            };
            let adj = view.inner.scroller.vadjustment();
            let height = view.inner.scroller.height() as f64;
            let speed = if y < AUTOSCROLL_EDGE {
                -(AUTOSCROLL_EDGE - y)
            } else if y > height - AUTOSCROLL_EDGE {
                y - (height - AUTOSCROLL_EDGE)
            } else {
                0.0
            };
            if speed == 0.0 {
                view.inner.autoscroll.borrow_mut().take();
                return glib::ControlFlow::Break;
            }
            adj.set_value(adj.value() + speed * 0.6);
            view.drag_to(x, y.clamp(0.0, height));
            glib::ControlFlow::Continue
        });
        *self.inner.autoscroll.borrow_mut() = Some(id);
    }
}

thread_local! {
    /// The selection's colour: the desktop accent, see-through.
    static SELECTION_COLOR: Cell<gdk::RGBA> = const { Cell::new(gdk::RGBA::new(0.48, 0.64, 0.97, 0.32)) };
}

/// Page tops in scroller coordinates: `n + 1` entries, where `offsets[i]` is
/// the top of page `i` and `offsets[n]` the bottom of the column. Keeping the
/// running total means a page's position is a lookup rather than a sum over
/// everything above it, which is what made scrolling a long book quadratic.
fn page_offsets(sizes: &[(f32, f32)], zoom: f64) -> Vec<f64> {
    let mut offsets = Vec::with_capacity(sizes.len() + 1);
    let mut y = MARGIN as f64;
    for (_, height) in sizes {
        offsets.push(y);
        y += (*height as f64 * zoom).round() + PAGE_GAP as f64;
    }
    offsets.push(y);
    offsets
}

/// The page covering `y`, or the one just above it when `y` falls in a gap.
fn page_at(offsets: &[f64], y: f64) -> usize {
    let pages = offsets.len().saturating_sub(1);
    offsets.partition_point(|&top| top <= y).saturating_sub(1).min(pages.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LETTER: (f32, f32) = (612.0, 792.0);

    #[test]
    fn offsets_stack_pages_with_one_gap_between_them() {
        let offsets = page_offsets(&[LETTER; 3], 1.0);
        let gap = PAGE_GAP as f64;
        assert_eq!(offsets, vec![24.0, 24.0 + 792.0 + gap, 24.0 + 2.0 * (792.0 + gap), 24.0 + 3.0 * (792.0 + gap)]);
        // Height read back from the table excludes the gap.
        assert_eq!(offsets[2] - offsets[1] - gap, 792.0);
    }

    #[test]
    fn page_at_finds_the_page_under_a_point() {
        let sizes = [LETTER, (612.0, 200.0), LETTER];
        let offsets = page_offsets(&sizes, 1.0);
        assert_eq!(page_at(&offsets, 0.0), 0, "above the first page");
        assert_eq!(page_at(&offsets, 100.0), 0);
        assert_eq!(page_at(&offsets, offsets[1] + 10.0), 1);
        assert_eq!(page_at(&offsets, offsets[2] - 1.0), 1, "in the gap: the page above");
        assert_eq!(page_at(&offsets, offsets[2]), 2);
        assert_eq!(page_at(&offsets, 1.0e9), 2, "past the end clamps to the last page");
    }

    /// Every page of a long book must be reachable and in order — the bug
    /// this replaced summed the stack per page and drifted on rounding.
    #[test]
    fn every_page_of_a_long_book_is_locatable_at_any_zoom() {
        let sizes: Vec<(f32, f32)> = (0..700).map(|i| (612.0, 700.0 + (i % 7) as f32 * 20.0)).collect();
        for zoom in [0.1, 0.37, 1.0, 2.5, 8.0] {
            let offsets = page_offsets(&sizes, zoom);
            assert!(offsets.windows(2).all(|w| w[1] > w[0]), "tops must increase");
            for page in 0..sizes.len() {
                let top = offsets[page];
                let height = offsets[page + 1] - top - PAGE_GAP as f64;
                assert_eq!(page_at(&offsets, top), page);
                assert_eq!(page_at(&offsets, top + height * 0.5), page);
            }
        }
    }

    #[test]
    fn a_selection_orders_its_ends_whichever_way_it_was_dragged() {
        let backwards = Selection { anchor: (3, 10), focus: (1, 4) };
        assert_eq!(backwards.ordered(), ((1, 4), (3, 10)));
        let same_page = Selection { anchor: (2, 9), focus: (2, 3) };
        assert_eq!(same_page.ordered(), ((2, 3), (2, 9)));
        assert!(Selection { anchor: (0, 5), focus: (0, 5) }.is_empty());
    }
}
