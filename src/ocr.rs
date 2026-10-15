//! Reading the words off a scan. A scanned page is a picture of text with no
//! text of its own, so search and selection have nothing to work with. ocrs —
//! pure Rust, like the PDF stack — finds the lines on a rendering of the page
//! and reads them. What it reads comes back in the shape the text layer
//! collects from a PDF's own glyphs, so everything built on that (search,
//! selection, copying, highlighting) works on a scan too.
//!
//! Its two models are data files, fetched by `make ocr-models` and installed
//! beside the app's other data; without them a scan simply has no text.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_interpret::{Image, InterpreterSettings};
use hayro::hayro_syntax::page::Page;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::kurbo::{Affine, Point, Rect, Vec2};
use hayro::{RenderCache, RenderSettings};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};

/// What a page says: each character, its box in user space and the way its
/// line runs (a vector along the baseline, also in user space).
pub type Read = Vec<(String, Rect, Vec2)>;

const MODELS: [&str; 2] = ["text-detection.rten", "text-recognition.rten"];

/// Pictures must cover this much of a page with no text for it to count as a
/// scan; a page with only a logo or a figure on it is not worth reading.
const SCAN_COVER: f64 = 0.3;

/// Pages are read at their scan's own resolution, kept within what reads
/// well: fax-coarse scans are drawn up, 600 dpi ones down.
const MIN_DPI: f64 = 200.0;
const MAX_DPI: f64 = 300.0;
const MAX_PIXELS: f64 = 40e6;

/// Where the models are: the user's data folder, the system's (where `make
/// install` puts them), beside the binary for an install outside the usual
/// prefixes, the checkout it was built from, and where ocrs's own
/// command-line tool keeps them.
pub fn model_dir() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("RAVEN_OCR_MODELS") {
        dirs.push(dir.into());
    }
    dirs.push(glib::user_data_dir().join("raven-viewer/ocr"));
    dirs.extend(glib::system_data_dirs().into_iter().map(|d| d.join("raven-viewer/ocr")));
    if let Some(prefix) = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.to_path_buf())) {
        dirs.push(prefix.join("share/raven-viewer/ocr"));
    }
    dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("data/ocr"));
    dirs.push(glib::user_cache_dir().join("ocrs"));
    dirs.into_iter().find(|d| MODELS.iter().all(|m| d.join(m).is_file()))
}

/// The engine, loaded the first time a scan is read.
fn engine() -> Option<&'static OcrEngine> {
    static ENGINE: OnceLock<Option<OcrEngine>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let dir = model_dir()?;
            let load = || -> anyhow::Result<OcrEngine> {
                OcrEngine::new(OcrEngineParams {
                    detection_model: Some(rten::Model::load_file(dir.join(MODELS[0]))?),
                    recognition_model: Some(rten::Model::load_file(dir.join(MODELS[1]))?),
                    ..Default::default()
                })
            };
            load().map_err(|e| eprintln!("raven-viewer: text recognition models in {} didn’t load: {e}", dir.display())).ok()
        })
        .as_ref()
}

/// The pictures a page draws, noted as it is interpreted: how much of it they
/// cover, how finely, and a fingerprint of what they are.
pub struct Pictures {
    /// Their boxes' area in user space, overlaps counted twice.
    area: f64,
    /// Pixels per point of the finest.
    density: f64,
    hasher: DefaultHasher,
    /// A stencil's pixels can't be got at to fingerprint it.
    hashable: bool,
}

impl Default for Pictures {
    fn default() -> Self {
        Self { area: 0.0, density: 0.0, hasher: DefaultHasher::new(), hashable: true }
    }
}

impl Pictures {
    pub fn add(&mut self, image: &Image<'_, '_>, transform: Affine) {
        // `transform` takes the image's pixels to user space.
        let (w, h) = (image.width() as f64, image.height() as f64);
        let bbox = transform.transform_rect_bbox(Rect::new(0.0, 0.0, w, h));
        if !bbox.area().is_finite() {
            return;
        }
        self.area += bbox.area();
        let pixel = vector(transform, 1.0, 0.0).hypot().min(vector(transform, 0.0, 1.0).hypot());
        if pixel > 1e-6 {
            self.density = self.density.max(1.0 / pixel);
        }
        match image {
            Image::Raster(raster) => raster.stream().raw_data().hash(&mut self.hasher),
            Image::Stencil(_) => self.hashable = false,
        }
        transform.as_coeffs().map(f64::to_bits).hash(&mut self.hasher);
    }

    /// Whether this is a scan rather than a page with a picture on it.
    fn is_scan(&self, page_area: f64) -> bool {
        page_area > 0.0 && self.area >= SCAN_COVER * page_area
    }
}

/// How reading a scan went, for whoever is waiting on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scan {
    /// It is being read now, which takes a second or two.
    Reading,
    /// It is a scan, but there is nothing to read it with.
    Unreadable,
}

/// Pages already read, by fingerprint, so a page is read once however many
/// times its text is asked for — by the view and the search at once, or
/// after an edit rewrote the file around it — and the ones being read now.
#[derive(Default)]
struct Shelf {
    done: HashMap<u64, Arc<Read>>,
    reading: HashSet<u64>,
}

fn shelf() -> &'static (Mutex<Shelf>, Condvar) {
    static SHELF: OnceLock<(Mutex<Shelf>, Condvar)> = OnceLock::new();
    SHELF.get_or_init(Default::default)
}

/// A page's text read from how it looks, when it is a scan with none of its
/// own; `None` otherwise, or when text recognition is not installed.
/// `told` hears of reading that will keep the caller waiting.
pub fn read(page: &Page<'_>, pictures: &Pictures, told: &mut dyn FnMut(Scan)) -> Option<Arc<Read>> {
    let crop = page.intersected_crop_box();
    if !pictures.is_scan((crop.x1 - crop.x0).abs() * (crop.y1 - crop.y0).abs()) {
        return None;
    }
    let Some(engine) = engine() else {
        told(Scan::Unreadable);
        return None;
    };
    let key = pictures.hashable.then(|| pictures.hasher.finish());
    let (lock, ready) = shelf();
    if let Some(key) = key {
        let mut shelf = lock.lock().unwrap_or_else(|e| e.into_inner());
        while shelf.reading.contains(&key) {
            shelf = ready.wait(shelf).unwrap_or_else(|e| e.into_inner());
        }
        if let Some(read) = shelf.done.get(&key) {
            return Some(read.clone());
        }
        shelf.reading.insert(key);
    }
    // Whatever happens below, the page stops being read: nobody waiting on
    // it waits forever.
    let _reading = key.map(Reading);
    told(Scan::Reading);
    // A model that trips over a page costs that page its text, not the
    // thread asking for it.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| recognize(engine, page, pictures.density)))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the recognizer failed")));
    let read = Arc::new(result.unwrap_or_else(|e| {
        eprintln!("raven-viewer: couldn’t read the text of a scanned page: {e}");
        Vec::new()
    }));
    if let Some(key) = key {
        let mut shelf = lock.lock().unwrap_or_else(|e| e.into_inner());
        // A page's text is a few kilobytes; a few thousand pages is plenty.
        if shelf.done.len() >= 4000 {
            shelf.done.clear();
        }
        shelf.done.insert(key, read.clone());
    }
    Some(read)
}

/// A page being read, by fingerprint; dropping it says it is done.
struct Reading(u64);

impl Drop for Reading {
    fn drop(&mut self) {
        let (lock, ready) = shelf();
        lock.lock().unwrap_or_else(|e| e.into_inner()).reading.remove(&self.0);
        ready.notify_all();
    }
}

/// Where `t` takes the vector (x, y): its linear part alone.
fn vector(t: Affine, x: f64, y: f64) -> Vec2 {
    t * Point::new(x, y) - t * Point::ORIGIN
}

/// Draw the page the way it is shown, read it, and put what was read back in
/// user space.
fn recognize(engine: &OcrEngine, page: &Page<'_>, density: f64) -> anyhow::Result<Read> {
    let (width, height) = page.render_dimensions();
    let (width, height) = (width.max(1.0) as f64, height.max(1.0) as f64);
    let mut scale = density.clamp(MIN_DPI / 72.0, MAX_DPI / 72.0);
    scale = scale.min((MAX_PIXELS / (width * height)).sqrt()).min(u16::MAX as f64 / width.max(height));
    let settings = InterpreterSettings { render_annotations: false, ..Default::default() };
    let pixmap = hayro::render(
        page,
        &RenderCache::new(),
        &settings,
        &RenderSettings { x_scale: scale as f32, y_scale: scale as f32, bg_color: WHITE, ..Default::default() },
    );
    let (w, h) = (pixmap.width() as u32, pixmap.height() as u32);
    // Grey is all the recognizer looks at; the background is opaque white.
    let grey: Vec<u8> = pixmap
        .data_as_u8_slice()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|[r, g, b, _]| ((*r as u32 * 77 + *g as u32 * 150 + *b as u32 * 29) >> 8) as u8)
        .collect();
    let input = engine.prepare_input(ImageSource::from_bytes(&grey, (w, h))?)?;
    let words = engine.detect_words(&input)?;
    let lines = engine.find_text_lines(&input, &words);
    let lines = engine.recognize_text(&input, &lines)?;

    // Pixels to the page as shown, then back through the page's own
    // transform to user space.
    let to_user = page.initial_transform(true).to_kurbo().inverse();
    let (sx, sy) = (width / w as f64, height / h as f64);
    let along = vector(to_user, 1000.0, 0.0);
    let mut read = Read::new();
    // Lines need nothing between them: the text layer breaks lines by where
    // the characters sit. A lone character is nearly always a speck taken
    // for one.
    for line in lines.iter().flatten().filter(|l| l.chars().iter().filter(|c| c.char != ' ').count() > 1) {
        for c in line.chars() {
            let r = c.rect;
            let shown = Rect::new(r.left() as f64 * sx, r.top() as f64 * sy, r.right() as f64 * sx, r.bottom() as f64 * sy);
            let corners = [(shown.x0, shown.y0), (shown.x1, shown.y0), (shown.x0, shown.y1), (shown.x1, shown.y1)]
                .map(|(x, y)| to_user * Point::new(x, y));
            let user = corners.iter().skip(1).fold(Rect::from_points(corners[0], corners[0]), |r, p| r.union_pt(*p));
            if user.area() > 0.0 {
                read.push((c.char.to_string(), user, along));
            }
        }
    }
    Ok(read)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A page of a real PDF as a scanner would give it: drawn at 300 dpi in
    /// grey, with a little noise, saved as a TIFF.
    fn scan_of(page: &Page<'_>) -> Vec<u8> {
        tiff_of(&[scan_pixels(page)])
    }

    /// Scans as the pages of one TIFF.
    fn tiff_of(scans: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        use tiff::encoder::{TiffEncoder, colortype};
        let mut out = std::io::Cursor::new(Vec::new());
        let mut tiff = TiffEncoder::new(&mut out).unwrap();
        for (w, h, grey) in scans {
            let mut image = tiff.new_image::<colortype::Gray8>(*w, *h).unwrap();
            image.resolution(tiff::tags::ResolutionUnit::Inch, tiff::encoder::Rational { n: 300, d: 1 });
            image.write_data(grey).unwrap();
        }
        out.into_inner()
    }

    fn scan_pixels(page: &Page<'_>) -> (u32, u32, Vec<u8>) {
        let scale = 300.0 / 72.0;
        let pixmap = hayro::render(
            page,
            &RenderCache::new(),
            &InterpreterSettings::default(),
            &RenderSettings { x_scale: scale, y_scale: scale, bg_color: WHITE, ..Default::default() },
        );
        let mut seed = 0x2545_f491_u32;
        let grey: Vec<u8> = pixmap
            .data_as_u8_slice()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|[r, g, b, _]| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let v = (*r as i32 * 77 + *g as i32 * 150 + *b as i32 * 29) >> 8;
                (v - 12 + (seed % 25) as i32).clamp(0, 255) as u8
            })
            .collect();
        (pixmap.width() as u32, pixmap.height() as u32, grey)
    }

    fn pdf(bytes: Vec<u8>) -> hayro::hayro_syntax::Pdf {
        hayro::hayro_syntax::Pdf::new(Arc::new(bytes)).unwrap()
    }

    /// A scan of a page of plain text, as a PDF of the scan.
    pub(crate) fn scanned(text: &str) -> Vec<u8> {
        let original = pdf(crate::convert::convert(text.as_bytes(), crate::convert::Format::Pdf, "t").unwrap());
        crate::tiffpdf::to_pdf(&scan_of(&original.pages()[0])).unwrap()
    }

    /// The models are fetched, not built in; without them there is nothing
    /// to test.
    pub(crate) fn have_models() -> bool {
        let have = model_dir().is_some();
        if !have {
            eprintln!("skipped: no text recognition models (make ocr-models)");
        }
        have
    }

    /// A scan is read, its words where they are on the page; reading it
    /// again — as after an edit rewrote the file — takes the words already
    /// read. A page with text of its own is never read.
    #[test]
    fn a_scan_is_read_where_its_words_are() {
        if !have_models() {
            return;
        }
        let text = "Raven Viewer reads scanned pages\n\nso their words can be searched\n";
        let original = pdf(crate::convert::convert(text.as_bytes(), crate::convert::Format::Pdf, "t").unwrap());
        let mut told = Vec::new();
        let own = crate::pdftext::extract_with(&original.pages()[0], &mut |s| told.push(s));
        assert!(told.is_empty(), "a page with text of its own was read");

        let scan = scanned(text);
        let read = crate::pdftext::extract_with(&pdf(scan.clone()).pages()[0], &mut |s| told.push(s));
        assert_eq!(told, [Scan::Reading]);
        // Read as lines; a monospaced "so" may come back "s0", as a reader
        // without a dictionary sees it.
        let lines: Vec<String> = read.text(0, read.chars.len()).to_lowercase().lines().map(String::from).collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "raven viewer reads scanned pages");
        assert!(lines[1].ends_with("their words can be searched"), "{lines:?}");
        for word in ["scanned", "searched"] {
            let (a, b) = (own.find(word)[0], read.find(word)[0]);
            let (a, b) = (own.boxes(a.0, a.1)[0], read.boxes(b.0, b.1)[0]);
            for (x, y) in a.iter().zip(b) {
                assert!((x - y).abs() < 0.01, "“{word}” is at {b:?} on the scan, {a:?} on the page");
            }
        }

        told.clear();
        let again = crate::pdftext::extract_with(&pdf(scan).pages()[0], &mut |s| told.push(s));
        assert!(told.is_empty(), "the same scan was read twice");
        assert_eq!(again, read);
    }

    /// A small picture on an otherwise empty page — a logo, a figure — is
    /// not a scan, and nobody waits for it to be read.
    #[test]
    fn a_picture_on_a_page_is_not_a_scan() {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let image = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 8, "Height" => 8,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
            vec![40u8; 64],
        ));
        let content = doc.add_object(Stream::new(dictionary! {}, b"q 100 0 0 100 72 600 cm /Im0 Do Q".to_vec()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image } },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let mut told = Vec::new();
        let text = crate::pdftext::extract_with(&pdf(bytes).pages()[0], &mut |s| told.push(s));
        assert!(told.is_empty() && text.is_empty());
    }

    fn words(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(str::to_lowercase).collect()
    }

    /// Scans made from the pages of a real PDF (`RAVEN_OCR_PDF`, pages
    /// `RAVEN_OCR_PAGES`, 1-based and comma-separated) are read back and
    /// held up against the PDF's own text. `RAVEN_OCR_OUT` keeps the scans,
    /// each on its own and all together as `scans.tif`.
    #[test]
    #[ignore = "needs the OCR models and a PDF"]
    fn reads_scanned_pages() {
        let path = std::env::var("RAVEN_OCR_PDF").expect("RAVEN_OCR_PDF");
        let original = hayro::hayro_syntax::Pdf::new(Arc::new(std::fs::read(&path).unwrap())).unwrap();
        let pages: Vec<usize> = std::env::var("RAVEN_OCR_PAGES")
            .unwrap_or("1".into())
            .split(',')
            .map(|p| p.trim().parse::<usize>().unwrap() - 1)
            .collect();
        if let Ok(dir) = std::env::var("RAVEN_OCR_OUT") {
            let all: Vec<_> = pages.iter().map(|&n| scan_pixels(&original.pages()[n])).collect();
            std::fs::write(Path::new(&dir).join("scans.tif"), tiff_of(&all)).unwrap();
        }
        for n in pages {
            let page = &original.pages()[n];
            let truth = crate::pdftext::extract(page);
            let scan = scan_of(page);
            if let Ok(dir) = std::env::var("RAVEN_OCR_OUT") {
                std::fs::write(Path::new(&dir).join(format!("page{}.tif", n + 1)), &scan).unwrap();
            }
            let scanned = hayro::hayro_syntax::Pdf::new(Arc::new(crate::tiffpdf::to_pdf(&scan).unwrap())).unwrap();
            let started = std::time::Instant::now();
            let mut told = Vec::new();
            let read = crate::pdftext::extract_with(&scanned.pages()[0], &mut |s| told.push(s));
            let took = started.elapsed();
            assert_eq!(told, [Scan::Reading]);
            let expected = words(&truth.text(0, truth.chars.len()));
            let got = words(&read.text(0, read.chars.len()));
            let found = expected.iter().filter(|w| got.contains(w)).count();
            eprintln!(
                "page {}: {found} of {} words read, {} characters, in {took:?}",
                n + 1,
                expected.len(),
                read.chars.len()
            );
            if std::env::var_os("RAVEN_OCR_SHOW").is_some() {
                eprintln!("{}\n", read.text(0, read.chars.len()));
            }
            // Read where the words are: a word searched for on the scan is
            // close to where it is on the original.
            for word in ["the", "and"] {
                if let (Some(a), Some(b)) = (truth.find(word).first(), read.find(word).first()) {
                    let (ra, rb) = (truth.boxes(a.0, a.1)[0], read.boxes(b.0, b.1)[0]);
                    eprintln!("  first “{word}”: {ra:.3?} vs {rb:.3?}");
                }
            }
            assert!(found * 10 >= expected.len() * 8, "too few words read on page {}", n + 1);
        }
    }

    /// A photo is not text: nothing is read off it, quickly.
    #[test]
    #[ignore = "needs the OCR models and a scan"]
    fn reads_a_scan() {
        let path = std::env::var("RAVEN_OCR_SCAN").expect("RAVEN_OCR_SCAN");
        let pdf = crate::tiffpdf::to_pdf(&std::fs::read(&path).unwrap()).unwrap();
        let pdf = hayro::hayro_syntax::Pdf::new(Arc::new(pdf)).unwrap();
        let started = std::time::Instant::now();
        let text = crate::pdftext::extract(&pdf.pages()[0]);
        eprintln!("{} characters in {:?}", text.chars.len(), started.elapsed());
        eprintln!("{}", text.text(0, text.chars.len()));
    }
}
