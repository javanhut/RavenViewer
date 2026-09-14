//! The continuous page view. Every page is a placeholder sized from the PDF
//! up front, so scrolling and the scrollbar are right before anything is
//! rasterized; only pages near the viewport are rendered, and a zoom keeps
//! showing the old pixels (stretched) until the sharp ones arrive.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};

use crate::pdf::{DocumentInfo, RenderedPage, Renderer};

const PAGE_GAP: i32 = 18;
const MARGIN: i32 = 24;
/// Pages kept rasterized on each side of the viewport.
const KEEP: usize = 4;
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
    zoom: Cell<f64>,
    fit: Cell<Fit>,
    /// page -> generation its texture was rendered at.
    rendered: RefCell<HashMap<usize, u64>>,
    requested: RefCell<HashMap<usize, u64>>,
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
        let (tx, rx) = async_channel::unbounded::<RenderedPage>();
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
        let pages: Vec<gtk::Picture> = info
            .page_sizes
            .iter()
            .map(|_| {
                let p = gtk::Picture::builder()
                    .can_shrink(true)
                    .content_fit(gtk::ContentFit::Fill)
                    .css_classes(["page"])
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
            zoom: Cell::new(1.0),
            fit: Cell::new(Fit::Width),
            rendered: Default::default(),
            requested: Default::default(),
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
        let adj = self.inner.scroller.vadjustment();
        adj.set_value(top + height * y_fraction - if y_fraction > 0.0 { 60.0 } else { 0.0 });
    }

    pub fn next_page(&self) {
        self.go_to(self.current_page() + 1, 0.0);
    }

    pub fn prev_page(&self) {
        self.go_to(self.current_page().saturating_sub(1), 0.0);
    }

    fn fit_width_zoom(&self) -> f64 {
        let avail = self.inner.scroller.width() as f64 - 2.0 * MARGIN as f64 - 16.0;
        let widest = self.inner.info.page_sizes.iter().map(|s| s.0).fold(1.0, f32::max) as f64;
        if avail <= 0.0 { 1.0 } else { (avail / widest).clamp(MIN_ZOOM, MAX_ZOOM) }
    }

    fn zoom_keeping_position(&self, zoom: f64) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - self.zoom()).abs() < 1e-3 {
            return;
        }
        let page = self.current_page();
        let (top, height) = self.page_extent(page);
        let frac = ((self.inner.scroller.vadjustment().value() - top) / height).clamp(0.0, 1.0);

        self.inner.zoom.set(zoom);
        if let Some(cb) = self.inner.on_zoom_changed.borrow().as_ref() {
            cb(zoom);
        }
        self.inner.renderer.bump();
        self.inner.requested.borrow_mut().clear();
        self.apply_sizes();
        // Sizes land on the next layout pass; restore the position after it.
        let view = self.clone();
        glib::idle_add_local_once(move || {
            let (top, height) = view.page_extent(page);
            view.inner.scroller.vadjustment().set_value(top + height * frac);
            view.update_visible();
        });
    }

    fn apply_sizes(&self) {
        let zoom = self.zoom();
        for (pic, (w, h)) in self.inner.pages.iter().zip(&self.inner.info.page_sizes) {
            pic.set_size_request((*w as f64 * zoom).round() as i32, (*h as f64 * zoom).round() as i32);
        }
    }

    /// (top offset, height) of a page in scroller coordinates, from the model
    /// rather than widget allocation so it is right before layout happens.
    fn page_extent(&self, page: usize) -> (f64, f64) {
        let zoom = self.zoom();
        let sizes = &self.inner.info.page_sizes;
        let top: f64 = MARGIN as f64
            + sizes[..page].iter().map(|s| (s.1 as f64 * zoom).round() + PAGE_GAP as f64).sum::<f64>();
        (top, (sizes[page].1 as f64 * zoom).round())
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
        let adj = self.inner.scroller.vadjustment();
        let (view_top, view_bottom) = (adj.value(), adj.value() + adj.page_size().max(1.0));
        let n = self.page_count();

        let mut first = None;
        let mut last = 0;
        let mut current = 0;
        let probe = view_top + adj.page_size() * 0.35;
        for page in 0..n {
            let (top, height) = self.page_extent(page);
            if top > view_bottom {
                break;
            }
            if top + height >= view_top {
                first.get_or_insert(page);
                last = page;
            }
            if top <= probe {
                current = page;
            }
        }
        let first = first.unwrap_or(0);

        if current != self.inner.current.replace(current) {
            if let Some(cb) = self.inner.on_page_changed.borrow().as_ref() {
                cb(current);
            }
        }

        let generation = self.inner.renderer.generation();
        let scale = (self.zoom() * self.inner.scroller.scale_factor() as f64) as f32;
        // Visible pages first, then one on each side for smooth scrolling.
        let wanted = (first..=last).chain([last + 1, first.wrapping_sub(1)]).filter(|&p| p < n);
        for page in wanted {
            let done = self.inner.rendered.borrow().get(&page) == Some(&generation);
            let queued = self.inner.requested.borrow().get(&page) == Some(&generation);
            if !done && !queued {
                self.inner.requested.borrow_mut().insert(page, generation);
                self.inner.renderer.request(page, scale);
            }
        }

        // Drop pixels far from the viewport so a 1000-page book stays small.
        let keep = first.saturating_sub(KEEP)..=(last + KEEP);
        self.inner.rendered.borrow_mut().retain(|&page, _| {
            let kept = keep.contains(&page);
            if !kept {
                self.inner.pages[page].set_paintable(None::<&gdk::Paintable>);
            }
            kept
        });
    }

    fn accept(&self, done: RenderedPage) {
        if done.generation != self.inner.renderer.generation() {
            return;
        }
        let bytes = glib::Bytes::from_owned(done.pixels);
        let texture = gdk::MemoryTexture::new(
            done.width as i32,
            done.height as i32,
            gdk::MemoryFormat::R8g8b8a8Premultiplied,
            &bytes,
            done.width as usize * 4,
        );
        self.inner.pages[done.page].set_paintable(Some(&texture));
        self.inner.rendered.borrow_mut().insert(done.page, done.generation);
    }

    pub fn set_dark_pages(&self, dark: bool) {
        for p in &self.inner.pages {
            if dark { p.add_css_class("dark-page") } else { p.remove_css_class("dark-page") }
        }
    }
}
