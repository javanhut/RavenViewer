//! The PDF engine. hayro rasterizes on a dedicated thread that owns the
//! parsed document; lopdf reads what hayro does not expose (outline,
//! annotations, text for search). Nothing here touches GTK.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use anyhow::{Result, anyhow};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings};

/// One entry of the document outline ("chapters").
#[derive(Debug, Clone)]
pub struct OutlineEntry {
    pub level: usize,
    pub title: String,
    /// Zero-based page index.
    pub page: usize,
}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub page: usize,
    pub kind: String,
    pub contents: String,
    /// Distance of the annotation's top edge from the top of the page, 0..1.
    pub y_fraction: f64,
}

/// Everything the window needs before the first pixel is drawn.
#[derive(Debug, Clone, Default)]
pub struct DocumentInfo {
    pub title: Option<String>,
    /// Page size in PDF points, rotation applied.
    pub page_sizes: Vec<(f32, f32)>,
    pub outline: Vec<OutlineEntry>,
    pub annotations: Vec<Annotation>,
}

/// Takes the bytes by `Arc`: hayro borrows them instead of copying, so a
/// 300 MB book costs 300 MB and not a multiple of it.
pub fn load_info(bytes: &Arc<Vec<u8>>) -> Result<DocumentInfo> {
    let pdf = Pdf::new(bytes.clone()).map_err(|e| anyhow!("not a readable PDF ({e:?})"))?;
    let page_sizes: Vec<(f32, f32)> = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
    if page_sizes.is_empty() {
        return Err(anyhow!("the document has no pages"));
    }

    // The outline and annotations are a nicety: a file lopdf cannot parse
    // still opens, it just has an empty sidebar.
    let mut info = DocumentInfo { page_sizes, ..Default::default() };
    if let Ok(doc) = lopdf::Document::load_mem(bytes) {
        info.title = doc_title(&doc);
        info.outline = outline(&doc, info.page_sizes.len());
        info.annotations = annotations(&doc);
    }
    Ok(info)
}

fn doc_title(doc: &lopdf::Document) -> Option<String> {
    let info = doc.trailer.get_deref(b"Info", doc).ok()?.as_dict().ok()?;
    let title = lopdf::decode_text_string(info.get_deref(b"Title", doc).ok()?).ok()?;
    let title = title.trim();
    // Producers stamp placeholders; the filename says more than these.
    let placeholder = title.is_empty()
        || ["untitled", "untitled document", "microsoft word", "document"]
            .iter()
            .any(|p| title.eq_ignore_ascii_case(p) || title.to_ascii_lowercase().starts_with("microsoft word - "));
    (!placeholder).then(|| title.to_string())
}

fn outline(doc: &lopdf::Document, pages: usize) -> Vec<OutlineEntry> {
    let Ok(toc) = doc.get_toc() else { return Vec::new() };
    toc.toc
        .into_iter()
        .map(|t| OutlineEntry {
            level: t.level.saturating_sub(1),
            title: t.title.trim().to_string(),
            page: t.page.saturating_sub(1).min(pages - 1),
        })
        .collect()
}

fn annotations(doc: &lopdf::Document) -> Vec<Annotation> {
    let mut out = Vec::new();
    for (number, page_id) in doc.get_pages() {
        let Ok(page) = doc.get_dictionary(page_id) else { continue };
        let Ok(annots) = page.get_deref(b"Annots", doc).and_then(|a| a.as_array()) else {
            continue;
        };
        let height = page_height(doc, page_id);
        for annot in annots {
            let dict = match annot {
                lopdf::Object::Reference(id) => doc.get_dictionary(*id).ok(),
                other => other.as_dict().ok(),
            };
            let Some(dict) = dict else { continue };
            let kind = dict
                .get(b"Subtype")
                .and_then(|s| s.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            // Links and form fields are the page's machinery, not notes.
            if matches!(kind.as_str(), "Link" | "Widget" | "Popup") {
                continue;
            }
            let contents = dict
                .get_deref(b"Contents", doc)
                .ok()
                .and_then(|c| lopdf::decode_text_string(c).ok())
                .unwrap_or_default();
            let top = dict
                .get_deref(b"Rect", doc)
                .and_then(|r| r.as_array())
                .ok()
                .and_then(|r| {
                    let ys: Vec<f32> = [1, 3].iter().filter_map(|&i| r.get(i)?.as_float().ok()).collect();
                    ys.into_iter().reduce(f32::max)
                })
                .unwrap_or(height);
            out.push(Annotation {
                page: number as usize - 1,
                kind,
                contents: contents.trim().to_string(),
                y_fraction: ((height - top) / height).clamp(0.0, 1.0) as f64,
            });
        }
    }
    out
}

fn page_height(doc: &lopdf::Document, page_id: lopdf::ObjectId) -> f32 {
    let mut id = page_id;
    // MediaBox is inheritable; walk up the page tree.
    for _ in 0..32 {
        let Ok(dict) = doc.get_dictionary(id) else { break };
        if let Ok(b) = dict.get_deref(b"MediaBox", doc).and_then(|b| b.as_array()) {
            let v: Vec<f32> = b.iter().filter_map(|n| n.as_float().ok()).collect();
            if v.len() == 4 {
                return (v[3] - v[1]).abs().max(1.0);
            }
        }
        match dict.get(b"Parent").and_then(|p| p.as_reference()) {
            Ok(parent) => id = parent,
            Err(_) => break,
        }
    }
    792.0
}

/// A page is cut into tiles once it is too big to rasterize in one go, so a
/// deep zoom draws the few million pixels actually on screen instead of the
/// hundred million the whole page would need.
pub const TILE: u32 = 1024;
/// Below this a page is drawn as a single texture — the ordinary reading
/// case, where tiling would only add seams and bookkeeping.
const WHOLE_PAGE_PIXELS: f64 = 24.0e6;
/// A tiled page also gets one cheap whole-page render to sit under the sharp
/// tiles, so panning never exposes blank paper.
const PREVIEW_PIXELS: f64 = 4.0e6;
/// hayro's pixmaps are `u16`-sized, and textures past this are more than GPUs
/// will accept.
const MAX_DIM: f64 = 16_384.0;

/// The largest scale at which a page of `size` points fits in `pixels` and
/// stays inside the texture limits.
fn fit_scale(size: (f32, f32), pixels: f64) -> f32 {
    let (w, h) = (size.0.max(1.0) as f64, size.1.max(1.0) as f64);
    let by_area = (pixels / (w * h)).sqrt();
    let by_edge = (MAX_DIM / w).min(MAX_DIM / h);
    by_area.min(by_edge).max(0.001) as f32
}

/// How a page is cut up at a given render scale. Both the view and the render
/// threads derive this from `(page size, scale)`, so they agree without
/// having to pass geometry around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    /// The whole page in device pixels at this scale.
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
    tile_w: u32,
    tile_h: u32,
}

impl Grid {
    pub fn new(size: (f32, f32), scale: f32) -> Self {
        let scale = scale.max(0.001) as f64;
        let width = (size.0.max(1.0) as f64 * scale).floor().clamp(1.0, u32::MAX as f64) as u32;
        let height = (size.1.max(1.0) as f64 * scale).floor().clamp(1.0, u32::MAX as f64) as u32;
        let fits = width as f64 * height as f64 <= WHOLE_PAGE_PIXELS
            && width as f64 <= MAX_DIM
            && height as f64 <= MAX_DIM;
        if fits {
            return Self { width, height, cols: 1, rows: 1, tile_w: width, tile_h: height };
        }
        Self { width, height, cols: width.div_ceil(TILE), rows: height.div_ceil(TILE), tile_w: TILE, tile_h: TILE }
    }

    pub fn tiled(&self) -> bool {
        self.cols > 1 || self.rows > 1
    }

    /// (x, y, width, height) of a tile in the page's device pixels.
    pub fn rect(&self, col: u32, row: u32) -> (u32, u32, u32, u32) {
        let (x, y) = (col.min(self.cols - 1) * self.tile_w, row.min(self.rows - 1) * self.tile_h);
        (x, y, self.tile_w.min(self.width - x), self.tile_h.min(self.height - y))
    }

    /// The tile columns covering a span given as fractions across the page.
    pub fn cols_over(&self, from: f64, to: f64) -> std::ops::RangeInclusive<u32> {
        Self::span(from, to, self.width, self.tile_w, self.cols)
    }

    pub fn rows_over(&self, from: f64, to: f64) -> std::ops::RangeInclusive<u32> {
        Self::span(from, to, self.height, self.tile_h, self.rows)
    }

    fn span(from: f64, to: f64, extent: u32, tile: u32, count: u32) -> std::ops::RangeInclusive<u32> {
        let at = |f: f64| ((f.clamp(0.0, 1.0) * extent as f64) as u32 / tile).min(count - 1);
        at(from.min(to))..=at(to.max(from))
    }
}

/// One piece of a page, premultiplied RGBA8.
pub struct RenderedTile {
    pub key: TileKey,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    /// Where it belongs on the page, as fractions of the page's width and
    /// height. Fractions rather than pixels so a texture drawn at one zoom
    /// still lands in the right place when stretched over another.
    pub area: (f32, f32, f32, f32),
    pub pixels: Vec<u8>,
}

/// A piece of a page to draw. `tile: None` is the whole page in one texture:
/// at an ordinary zoom that is the page at full quality, and on a tiled page
/// it is the cheap backing render under the sharp tiles.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileKey {
    pub page: usize,
    pub tile: Option<(u32, u32)>,
}

/// What the view currently wants on screen. The view *replaces* this on
/// every scroll rather than appending to it, which is the whole point: a
/// drag across a 700-page book used to queue hundreds of renders that were
/// all still "current" and had to be worked through before the page you
/// actually landed on got its turn.
struct Wishlist {
    /// Pieces still to rasterize, most important first.
    wanted: Vec<TileKey>,
    scale: f32,
    generation: u64,
    /// Pieces a worker is rasterizing right now, so two workers never take
    /// the same one and a re-submission does not queue it twice.
    active: Vec<TileKey>,
    closed: bool,
}

/// Owns the render threads. Work for pages that have scrolled past is
/// dropped rather than rendered late, and a generation bump (a zoom, a new
/// document) invalidates everything queued.
pub struct Renderer {
    shared: Arc<(Mutex<Wishlist>, Condvar)>,
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let (lock, idle) = &*self.shared;
        lock_wishlist(lock).closed = true;
        idle.notify_all();
    }
}

/// A worker panicking mid-page must not take the viewer down with it; the
/// wishlist is plain data and is still usable afterwards.
fn lock_wishlist(lock: &Mutex<Wishlist>) -> MutexGuard<'_, Wishlist> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
}

impl Renderer {
    pub fn spawn(bytes: Arc<Vec<u8>>, results: async_channel::Sender<RenderedTile>) -> Result<Self> {
        let shared = Arc::new((
            Mutex::new(Wishlist {
                wanted: Vec::new(),
                scale: 1.0,
                generation: 0,
                active: Vec::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        // hayro rasterizes a page on one thread (vello_cpu's `multithreading`
        // feature is off upstream), so a couple of workers really do fill the
        // tiles around a jump while the first one is still being drawn. One
        // core is left for the UI.
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).clamp(1, 3);
        for i in 0..workers {
            let (bytes, results, threads) = (bytes.clone(), results.clone(), shared.clone());
            let started = std::thread::Builder::new()
                .name(format!("raven-render-{i}"))
                .spawn(move || render_loop(bytes, &threads, &results));
            if let Err(e) = started {
                // Whoever did start is parked on the condvar and would stay
                // there: nobody is left to build the `Renderer` that retires
                // them.
                let (lock, idle) = &*shared;
                lock_wishlist(lock).closed = true;
                idle.notify_all();
                return Err(anyhow::Error::new(e).context("could not start the render threads"));
            }
        }
        Ok(Self { shared })
    }

    /// Invalidate everything queued and already drawn; returns the new
    /// generation.
    pub fn bump(&self) -> u64 {
        let (lock, idle) = &*self.shared;
        let mut list = lock_wishlist(lock);
        list.generation += 1;
        list.wanted.clear();
        idle.notify_all();
        list.generation
    }

    pub fn generation(&self) -> u64 {
        lock_wishlist(&self.shared.0).generation
    }

    /// Replace the wish list: exactly these pieces, most important first.
    pub fn submit(&self, scale: f32, pieces: Vec<TileKey>) {
        let (lock, idle) = &*self.shared;
        let mut list = lock_wishlist(lock);
        list.wanted = pieces;
        list.scale = scale;
        drop(list);
        idle.notify_all();
    }
}

fn render_loop(bytes: Arc<Vec<u8>>, shared: &(Mutex<Wishlist>, Condvar), results: &async_channel::Sender<RenderedTile>) {
    // Each worker parses the file for itself — the bytes are shared, only
    // the lazily-built object tables and the render cache are per-thread.
    let Ok(pdf) = Pdf::new(bytes) else { return };
    let pages = pdf.pages();
    let cache = RenderCache::new();
    let settings = InterpreterSettings::default();
    let (lock, idle) = shared;

    loop {
        let (key, scale, generation) = {
            let mut list = lock_wishlist(lock);
            loop {
                if list.closed {
                    return;
                }
                let next = list.wanted.iter().position(|k| !list.active.contains(k));
                match next {
                    Some(at) => {
                        let key = list.wanted.remove(at);
                        list.active.push(key);
                        break (key, list.scale, list.generation);
                    }
                    None => list = idle.wait(list).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };

        let rendered = pages.get(key.page).map(|page| {
            let size = page.render_dimensions();
            let grid = Grid::new(size, scale);
            // The whole page in one texture: at full quality when it fits,
            // otherwise shrunk to something cheap to sit under the tiles.
            let (scale, x, y, width, height, area) = match key.tile {
                None if grid.tiled() => {
                    let preview = fit_scale(size, PREVIEW_PIXELS);
                    let cut = Grid::new(size, preview);
                    (preview, 0, 0, cut.width, cut.height, (0.0, 0.0, 1.0, 1.0))
                }
                None => (scale, 0, 0, grid.width, grid.height, (0.0, 0.0, 1.0, 1.0)),
                Some((col, row)) => {
                    let (x, y, w, h) = grid.rect(col, row);
                    let (pw, ph) = (grid.width as f32, grid.height as f32);
                    (scale, x, y, w, h, (x as f32 / pw, y as f32 / ph, w as f32 / pw, h as f32 / ph))
                }
            };
            let pixmap = hayro::render(
                page,
                &cache,
                &settings,
                &RenderSettings {
                    x_scale: scale,
                    y_scale: scale,
                    width: Some(width.min(u16::MAX as u32) as u16),
                    height: Some(height.min(u16::MAX as u32) as u16),
                    x_offset: x as f32,
                    y_offset: y as f32,
                    bg_color: WHITE,
                },
            );
            RenderedTile {
                key,
                generation,
                width: pixmap.width() as u32,
                height: pixmap.height() as u32,
                area,
                pixels: pixmap.data_as_u8_slice().to_vec(),
            }
        });

        {
            let mut list = lock_wishlist(lock);
            list.active.retain(|k| *k != key);
            // The view only re-asks for a piece after it hears back, so drop
            // any duplicate that arrived while this one was being drawn.
            if generation == list.generation {
                list.wanted.retain(|k| *k != key);
            }
        }

        if let Some(done) = rendered
            && results.send_blocking(done).is_err()
        {
            return; // the window is gone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_survives_lost_spaces_and_ligatures() {
        let page = normalize("14.4Self-OrganizingMaps529 Noticethatdistanceisdeﬁnedinthespace");
        assert!(page.contains(&normalize("Self-Organizing Maps")));
        assert!(page.contains(&normalize("distance is defined")));
        assert_eq!(normalize("Abu‑Mostafa’s “hints”"), "abu-mostafa's\"hints\"");
    }

    /// An ordinary page at an ordinary zoom is one texture, exactly as it
    /// was before tiling existed.
    #[test]
    fn a_readable_zoom_draws_the_page_in_one_piece() {
        let letter = (612.0, 792.0);
        for scale in [0.5, 1.0, 2.0, 4.0] {
            let grid = Grid::new(letter, scale);
            assert!(!grid.tiled(), "a letter page at {scale}x should not be tiled");
            assert_eq!(grid.rect(0, 0), (0, 0, grid.width, grid.height));
        }
    }

    /// Deep zoom cuts the page up, the tiles tile the page exactly — no gap,
    /// no overlap — and none is bigger than hayro's u16 pixmap will take.
    #[test]
    fn tiles_cover_a_deeply_zoomed_page_exactly_once() {
        for (size, scale) in [((612.0, 792.0), 16.0), ((2384.0, 3370.0), 9.0), ((72.0, 72.0), 400.0)] {
            let grid = Grid::new(size, scale);
            assert!(grid.tiled(), "{size:?} at {scale}x should be tiled");
            let mut covered = 0u64;
            for row in 0..grid.rows {
                for col in 0..grid.cols {
                    let (x, y, w, h) = grid.rect(col, row);
                    assert!(w <= TILE && h <= TILE && w > 0 && h > 0);
                    assert!(x + w <= grid.width && y + h <= grid.height, "tile runs off the page");
                    // Tiles start where the previous one ended.
                    assert_eq!((x, y), (col * TILE, row * TILE));
                    covered += w as u64 * h as u64;
                }
            }
            assert_eq!(covered, grid.width as u64 * grid.height as u64, "tiles must cover the page once");
        }
    }

    /// Only the tiles under the viewport are asked for: the point of the
    /// exercise is that a deep zoom costs a screenful, not a whole page.
    #[test]
    fn only_the_viewport_is_asked_for_at_deep_zoom() {
        let grid = Grid::new((612.0, 792.0), 16.0);
        let total = grid.cols as usize * grid.rows as usize;
        // A viewport over the middle tenth of the page.
        let cols = grid.cols_over(0.45, 0.55);
        let rows = grid.rows_over(0.45, 0.55);
        let asked = (cols.end() - cols.start() + 1) as usize * (rows.end() - rows.start() + 1) as usize;
        assert!(asked * 8 < total, "asked for {asked} of {total} tiles, expected far fewer");
        assert!(*cols.end() < grid.cols && *rows.end() < grid.rows);
        // The whole page still asks for every tile, and clamps at the edges.
        let all = grid.rows_over(-0.5, 1.5);
        assert_eq!((*all.start(), *all.end()), (0, grid.rows - 1));
    }

    /// The vendored offset patch (vendor/hayro) is what makes tiling possible
    /// at all — stock hayro can only rasterize a page from its top-left
    /// corner. If a re-sync ever drops the patch, this is what notices.
    #[test]
    fn rendering_a_tile_picks_out_that_part_of_the_page() {
        use lopdf::{Document, Object, Stream, dictionary};
        // Red square in the page's top-left, blue in its bottom-right.
        let ops = b"1 0 0 rg 0 692 100 100 re f  0 0 1 rg 512 0 100 100 re f".to_vec();
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, ops));
        let page = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Count" => 1, "Kids" => vec![page.into()],
                "MediaBox" => vec![Object::Integer(0), Object::Integer(0), Object::Integer(612), Object::Integer(792)],
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let pdf = Pdf::new(bytes).unwrap();
        let pages = pdf.pages();
        let (cache, settings) = (RenderCache::new(), InterpreterSettings::default());
        let middle_of = |x: f32, y: f32| {
            let pixmap = hayro::render(
                &pages[0],
                &cache,
                &settings,
                &RenderSettings {
                    x_scale: 1.0,
                    y_scale: 1.0,
                    width: Some(100),
                    height: Some(100),
                    x_offset: x,
                    y_offset: y,
                    bg_color: WHITE,
                },
            );
            let data = pixmap.data_as_u8_slice();
            let at = ((50 * pixmap.width() as u32 + 50) * 4) as usize;
            ((pixmap.width(), pixmap.height()), (data[at], data[at + 1], data[at + 2]))
        };
        assert_eq!(middle_of(0.0, 0.0), ((100, 100), (255, 0, 0)), "the corner tile is the red square");
        assert_eq!(middle_of(512.0, 692.0), ((100, 100), (0, 0, 255)), "a far tile is the blue square");
        assert_eq!(middle_of(250.0, 350.0), ((100, 100), (255, 255, 255)), "a middle tile is blank page");
    }

    /// A one-page PDF with a sticky note and a link: the note is listed,
    /// the link is not, and its position is measured from the page top.
    #[test]
    fn lists_notes_but_not_links() {
        use lopdf::{Document, Object, Stream, dictionary};
        let n = |v: i64| Object::Integer(v);
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let note = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Text",
            "Rect" => vec![n(100), n(700), n(120), n(720)],
            "Contents" => Object::string_literal("Check this derivation"),
        });
        let link = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Link", "Rect" => vec![n(0), n(0), n(10), n(10)],
        });
        let content = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content,
            "Annots" => vec![note.into(), link.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
                "MediaBox" => vec![n(0), n(0), n(612), n(792)],
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let info = load_info(&Arc::new(bytes)).unwrap();
        assert_eq!(info.page_sizes, vec![(612.0, 792.0)]);
        assert_eq!(info.annotations.len(), 1);
        let a = &info.annotations[0];
        assert_eq!((a.page, a.kind.as_str(), a.contents.as_str()), (0, "Text", "Check this derivation"));
        assert!((a.y_fraction - 72.0 / 792.0).abs() < 1e-6);
    }

    /// End to end through the render threads: a deeply zoomed page is cut
    /// into tiles, each tile is rasterized on its own, and every one shows
    /// the part of the page it claims to. This is what catches an offset
    /// that is out by a tile or a PDF y-axis that never got flipped.
    #[test]
    fn each_tile_shows_its_own_part_of_the_page() {
        const CELLS: u32 = 8;
        const PAGE: f32 = 640.0;
        const SCALE: f32 = 8.0;
        let cell = PAGE / CELLS as f32;
        // A checkerboard of distinct colours, so a tile's centre pixel says
        // exactly which part of the page was drawn.
        let colour = |col: u32, row: u32| (col as u8 * 30 + 5, row as u8 * 30 + 5, 128u8);

        use lopdf::{Document, Object, Stream, dictionary};
        let mut ops = String::new();
        for row in 0..CELLS {
            for col in 0..CELLS {
                let (r, g, b) = colour(col, row);
                // PDF space counts y from the bottom; `row` counts from the top.
                let y = PAGE - (row + 1) as f32 * cell;
                ops.push_str(&format!(
                    "{} {} {} rg {} {} {} {} re f\n",
                    r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0,
                    col as f32 * cell, y, cell, cell
                ));
            }
        }
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
        let page = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Count" => 1, "Kids" => vec![page.into()],
                "MediaBox" => vec![Object::Integer(0), Object::Integer(0),
                                   Object::Integer(PAGE as i64), Object::Integer(PAGE as i64)],
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let grid = Grid::new((PAGE, PAGE), SCALE);
        assert!(grid.tiled(), "this page at {SCALE}x should be tiled");

        let (tx, rx) = async_channel::unbounded();
        let renderer = Renderer::spawn(Arc::new(bytes), tx).unwrap();
        let every: Vec<TileKey> = (0..grid.rows)
            .flat_map(|row| (0..grid.cols).map(move |col| TileKey { page: 0, tile: Some((col, row)) }))
            .collect();
        renderer.submit(SCALE, every.clone());

        for _ in 0..every.len() {
            let tile = rx.recv_blocking().expect("the render threads hung up");
            let (col, row) = tile.key.tile.unwrap();
            let (x, y, w, h) = grid.rect(col, row);
            assert_eq!((tile.width, tile.height), (w, h), "tile {col},{row} is the wrong size");
            // The area it claims must be exactly where the grid puts it.
            let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
            assert!(
                near(tile.area.0, x as f32 / grid.width as f32) && near(tile.area.1, y as f32 / grid.height as f32),
                "tile {col},{row} claims {:?}, grid says {x},{y}", tile.area
            );
            // …and the pixel in the middle of it must be the checkerboard
            // cell that part of the page is painted with.
            let (cx, cy) = (x + w / 2, y + h / 2);
            let at = ((h / 2 * tile.width + w / 2) * 4) as usize;
            let got = (tile.pixels[at], tile.pixels[at + 1], tile.pixels[at + 2]);
            let want = colour(
                ((cx as f32 / SCALE / cell) as u32).min(CELLS - 1),
                ((cy as f32 / SCALE / cell) as u32).min(CELLS - 1),
            );
            assert_eq!(got, want, "tile {col},{row} (page pixel {cx},{cy}) shows the wrong part of the page");
        }
    }

    /// A book whose pages all share one fat content stream, so every page
    /// costs real time to rasterize.
    fn slow_book(pages: usize) -> Arc<Vec<u8>> {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut ops = String::new();
        for i in 0..1500 {
            ops.push_str(&format!("{} 0.3 0.6 rg {} {} 40 30 re f\n", (i % 97) as f32 / 97.0, i % 560, (i * 7) % 750));
        }
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
        let kids: Vec<Object> = (0..pages)
            .map(|_| {
                doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content })
                    .into()
            })
            .collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Count" => pages as i64, "Kids" => kids,
                "MediaBox" => vec![Object::Integer(0), Object::Integer(0), Object::Integer(612), Object::Integer(792)],
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        Arc::new(bytes)
    }

    /// The bug this guards: dragging the scrollbar across a long book used to
    /// append a render request per page, and the page you actually landed on
    /// waited behind every page you had scrolled past. The wish list is
    /// replaced, not extended, so a jump is served almost immediately.
    #[test]
    fn a_jump_does_not_queue_behind_the_pages_scrolled_past() {
        const PAGES: usize = 200;
        const TARGET: usize = PAGES - 1;
        let (tx, rx) = async_channel::unbounded();
        let renderer = Renderer::spawn(slow_book(PAGES), tx).unwrap();

        renderer.submit(1.0, (0..PAGES).map(|page| TileKey { page, tile: None }).collect());
        // Let the workers get their teeth into the old wish list first.
        std::thread::sleep(std::time::Duration::from_millis(120));
        // Pages finished before the jump are already drawn and cost nothing
        // more; what matters is the work still to come.
        let done_before = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert!(done_before < PAGES / 2, "the book rendered too fast for this test to mean anything");
        renderer.submit(1.0, vec![TileKey { page: TARGET, tile: None }]);

        let mut before = 0;
        loop {
            let page = rx.recv_blocking().expect("the render threads hung up").key.page;
            if page == TARGET {
                break;
            }
            before += 1;
            assert!(before < 40, "the jump waited behind {before} abandoned pages");
        }
        // Only what is still being drawn can land ahead of the jump: one page
        // per worker, and at most a handful of workers.
        assert!(before <= 4, "expected the jump to be served at once, {before} pages came first");
    }

    /// `RAVEN_TEST_PDF=… RAVEN_TEST_PAGE=2 cargo test dump_text -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_text() {
        let bytes = std::fs::read(std::env::var("RAVEN_TEST_PDF").unwrap()).unwrap();
        let page: u32 = std::env::var("RAVEN_TEST_PAGE").ok().and_then(|p| p.parse().ok()).unwrap_or(1);
        let doc = lopdf::Document::load_mem(&bytes).unwrap();
        let text = doc.extract_text(&[page]);
        eprintln!("--- lopdf page {page} ---\n{:?}", text.map(|t| t.chars().take(600).collect::<String>()));
    }

    /// `RAVEN_TEST_PDF=/path/file.pdf cargo test smoke -- --ignored --nocapture`
    /// renders page 1 to target/smoke.png and prints timings.
    #[test]
    #[ignore]
    fn smoke() {
        let path = std::env::var("RAVEN_TEST_PDF").expect("set RAVEN_TEST_PDF");
        let bytes = Arc::new(std::fs::read(&path).unwrap());
        let t = std::time::Instant::now();
        let info = load_info(&bytes).unwrap();
        eprintln!(
            "load_info: {:?}, {} pages, {} outline, {} annotations, title {:?}",
            t.elapsed(),
            info.page_sizes.len(),
            info.outline.len(),
            info.annotations.len(),
            info.title
        );

        let (tx, rx) = async_channel::unbounded();
        let renderer = Renderer::spawn(bytes.clone(), tx).unwrap();
        let t = std::time::Instant::now();
        renderer.submit(1.5, vec![TileKey { page: 0, tile: None }]);
        let page = rx.recv_blocking().unwrap();
        eprintln!("render page 1 @1.5x: {:?} ({}x{})", t.elapsed(), page.width, page.height);

        let pixmap = hayro::vello_cpu::Pixmap::from_parts(
            page.pixels
                .chunks_exact(4)
                .map(|p| hayro::vello_cpu::color::PremulRgba8 { r: p[0], g: p[1], b: p[2], a: p[3] })
                .collect(),
            page.width as u16,
            page.height as u16,
        );
        std::fs::write("target/smoke.png", pixmap.into_png().unwrap()).unwrap();

        let query = std::env::var("RAVEN_TEST_QUERY").unwrap_or_else(|_| "the".into());
        let searcher = Searcher::spawn(bytes, info.page_sizes.len());
        let t = std::time::Instant::now();
        let hit = gtk4::glib::MainContext::default().block_on(searcher.find(&query, 0, false));
        eprintln!("search {query:?} from page 1: {hit:?} in {:?}", t.elapsed());
    }
}

/// Search compares normalized text: lower-case, no whitespace, ligatures
/// expanded, typographic punctuation flattened. Extracted PDF text often
/// loses word spaces ("Self-OrganizingMaps") and keeps ligatures ("deﬁned"),
/// so a literal comparison misses matches a reader can plainly see.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            c if c.is_whitespace() => {}
            '\u{00AD}' => {} // soft hyphen
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            '‘' | '’' => out.push('\''),
            '“' | '”' => out.push('"'),
            '‐' | '‑' | '–' | '—' => out.push('-'),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

struct SearchRequest {
    query: String,
    start: usize,
    backwards: bool,
    reply: async_channel::Sender<Option<usize>>,
}

/// Text search on its own thread. Extracting text is ~20ms a page, so a
/// whole book up front is many seconds; instead pages are read lazily in
/// search order, cached, and a search stops at the first hit. A new query
/// arriving mid-scan replaces the old one immediately.
pub struct Searcher {
    tx: mpsc::Sender<SearchRequest>,
}

impl Searcher {
    pub fn spawn(bytes: Arc<Vec<u8>>, pages: usize) -> Self {
        let (tx, rx) = mpsc::channel::<SearchRequest>();
        let _ = std::thread::Builder::new()
            .name("raven-search".into())
            .spawn(move || search_loop(&bytes, pages, rx));
        Self { tx }
    }

    /// First page after `start` (wrapping, `start` itself last) whose text
    /// contains `query`, compared case-insensitively.
    pub async fn find(&self, query: &str, start: usize, backwards: bool) -> Option<usize> {
        let (reply, answer) = async_channel::bounded(1);
        let req = SearchRequest { query: normalize(query), start, backwards, reply };
        self.tx.send(req).ok()?;
        answer.recv().await.ok().flatten()
    }
}

fn search_loop(bytes: &[u8], pages: usize, rx: mpsc::Receiver<SearchRequest>) {
    let doc = lopdf::Document::load_mem(bytes).ok();
    let mut cache: Vec<Option<String>> = vec![None; pages];
    let mut pending: Option<SearchRequest> = None;

    'requests: loop {
        let req = match pending.take() {
            Some(r) => r,
            None => match rx.recv() {
                Ok(r) => r,
                Err(_) => return,
            },
        };
        if pages == 0 || req.query.is_empty() {
            let _ = req.reply.send_blocking(None);
            continue;
        }
        for i in 1..=pages {
            if let Ok(newer) = rx.try_recv() {
                pending = Some(newer);
                continue 'requests; // the old caller's reply channel just closes
            }
            let page = if req.backwards { (req.start + pages - i % pages) % pages } else { (req.start + i) % pages };
            let text = cache[page].get_or_insert_with(|| {
                normalize(&doc.as_ref().and_then(|d| d.extract_text(&[page as u32 + 1]).ok()).unwrap_or_default())
            });
            if text.contains(&req.query) {
                let _ = req.reply.send_blocking(Some(page));
                continue 'requests;
            }
        }
        let _ = req.reply.send_blocking(None);
    }
}


