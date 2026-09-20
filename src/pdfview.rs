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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};

use crate::pagetiles::PageTiles;
use crate::pdf::{DocumentInfo, Grid, RenderedTile, Renderer, TILE, TileKey};

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

#[derive(Clone, Copy, PartialEq)]
enum Fit {
    Width,
    Manual,
}

struct Inner {
    info: DocumentInfo,
    renderer: Renderer,
    scroller: gtk::ScrolledWindow,
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
    on_page_changed: RefCell<Option<Box<dyn Fn(usize)>>>,
    on_zoom_changed: RefCell<Option<Box<dyn Fn(f64)>>>,
}

#[derive(Clone)]
pub struct PdfView {
    inner: Rc<Inner>,
}

impl PdfView {
    pub fn new(bytes: Arc<Vec<u8>>, info: DocumentInfo) -> anyhow::Result<Self> {
        let (tx, rx) = async_channel::unbounded::<RenderedTile>();
        let renderer = Renderer::spawn(bytes, tx)?;

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(PAGE_GAP)
            .halign(gtk::Align::Center)
            .margin_top(MARGIN)
            .margin_bottom(MARGIN)
            .margin_start(MARGIN)
            .margin_end(MARGIN)
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

        let inner = Rc::new(Inner {
            info,
            renderer,
            scroller,
            pages,
            tiles,
            offsets: Default::default(),
            zoom: Cell::new(1.0),
            fit: Cell::new(Fit::Width),
            rendered: Default::default(),
            current: Cell::new(0),
            on_page_changed: Default::default(),
            on_zoom_changed: Default::default(),
        });
        let view = PdfView { inner };
        view.apply_sizes();
        view.connect_signals();

        let weak = Rc::downgrade(&view.inner);
        glib::spawn_future_local(async move {
            while let Ok(done) = rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                PdfView { inner }.accept(done);
            }
        });
        Ok(view)
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.scroller
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
        let widest = self.inner.info.page_sizes.iter().map(|s| s.0).fold(1.0, f32::max) as f64;
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
        self.inner.renderer.bump();
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
        for (pic, (w, h)) in self.inner.pages.iter().zip(&self.inner.info.page_sizes) {
            pic.set_size_request((*w as f64 * zoom).round() as i32, (*h as f64 * zoom).round() as i32);
        }
        *self.inner.offsets.borrow_mut() = page_offsets(&self.inner.info.page_sizes, zoom);
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

        if current != self.inner.current.replace(current) {
            if let Some(cb) = self.inner.on_page_changed.borrow().as_ref() {
                cb(current);
            }
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
        let ahead = if Grid::new(self.inner.info.page_sizes[current], scale).tiled() { 1 } else { KEEP };

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

        let generation = self.inner.renderer.generation();
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
            let grid = Grid::new(self.inner.info.page_sizes[current], scale);
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
        self.inner.renderer.submit(scale, wanted);

        self.evict(first, last, ahead);
    }

    /// The pieces of a page worth having: always the whole-page render, plus
    /// — once the page is tiled — the tiles over the viewport. A page off
    /// screen gets only the whole-page render; drawing sharp tiles for pages
    /// nobody is looking at is what makes a zoomed-in book crawl.
    fn pieces_of(&self, page: usize, on_screen: bool, factor: f64) -> Vec<TileKey> {
        let size = self.inner.info.page_sizes[page];
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
        if done.generation != self.inner.renderer.generation() {
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
}
