//! The PDF engine. hayro rasterizes on a dedicated thread that owns the
//! parsed document; lopdf reads what hayro does not expose (outline,
//! annotations, text for search). Nothing here touches GTK.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use anyhow::{Context, Result, anyhow};
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

pub fn load_info(bytes: &[u8]) -> Result<DocumentInfo> {
    let pdf = Pdf::new(bytes.to_vec()).map_err(|e| anyhow!("not a readable PDF ({e:?})"))?;
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

/// A rendered page, premultiplied RGBA8.
pub struct RenderedPage {
    pub page: usize,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

struct Request {
    page: usize,
    scale: f32,
    generation: u64,
}

/// Owns the render thread. Requests older than the current generation
/// (a zoom changed, a new document) are dropped before they cost anything.
pub struct Renderer {
    tx: mpsc::Sender<Request>,
    generation: Arc<AtomicU64>,
}

impl Renderer {
    pub fn spawn(bytes: Arc<Vec<u8>>, results: async_channel::Sender<RenderedPage>) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Request>();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        std::thread::Builder::new()
            .name("raven-render".into())
            .spawn(move || render_loop(&bytes, rx, results, current))
            .context("could not start the render thread")?;
        Ok(Self { tx, generation })
    }

    /// Invalidate every queued request; returns the new generation.
    pub fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn request(&self, page: usize, scale: f32) {
        let _ = self.tx.send(Request { page, scale, generation: self.generation() });
    }
}

fn render_loop(
    bytes: &[u8],
    rx: mpsc::Receiver<Request>,
    results: async_channel::Sender<RenderedPage>,
    current: Arc<AtomicU64>,
) {
    let Ok(pdf) = Pdf::new(bytes.to_vec()) else { return };
    let pages = pdf.pages();
    let cache = RenderCache::new();
    let settings = InterpreterSettings::default();

    while let Ok(req) = rx.recv() {
        if req.generation != current.load(Ordering::SeqCst) {
            continue;
        }
        let Some(page) = pages.get(req.page) else { continue };
        let pixmap = hayro::render(
            page,
            &cache,
            &settings,
            &RenderSettings { x_scale: req.scale, y_scale: req.scale, bg_color: WHITE, ..Default::default() },
        );
        let done = RenderedPage {
            page: req.page,
            generation: req.generation,
            width: pixmap.width() as u32,
            height: pixmap.height() as u32,
            pixels: pixmap.data_as_u8_slice().to_vec(),
        };
        if results.send_blocking(done).is_err() {
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

        let info = load_info(&bytes).unwrap();
        assert_eq!(info.page_sizes, vec![(612.0, 792.0)]);
        assert_eq!(info.annotations.len(), 1);
        let a = &info.annotations[0];
        assert_eq!((a.page, a.kind.as_str(), a.contents.as_str()), (0, "Text", "Check this derivation"));
        assert!((a.y_fraction - 72.0 / 792.0).abs() < 1e-6);
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
        let bytes = std::fs::read(&path).unwrap();
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
        let renderer = Renderer::spawn(Arc::new(bytes.clone()), tx).unwrap();
        let t = std::time::Instant::now();
        renderer.request(0, 1.5);
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
        let searcher = Searcher::spawn(Arc::new(bytes), info.page_sizes.len());
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
