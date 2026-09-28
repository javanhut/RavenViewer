//! Changing a PDF: marking up text, notes and text boxes, turning, removing
//! and moving pages, and combining files. All of it goes through lopdf, which
//! parses the whole file into objects we can rewrite and save.
//!
//! Every annotation is written with an appearance stream. The spec lets a
//! viewer draw a highlight from its QuadPoints alone, but many (hayro among
//! them) draw only what the appearance stream says, and one written here
//! looks the same in every viewer.

use std::collections::BTreeMap;
use std::sync::{Arc, mpsc};

use anyhow::{Context, Result, anyhow, bail};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Markup {
    Highlight,
    Underline,
    StrikeOut,
}

impl Markup {
    fn subtype(self) -> &'static str {
        match self {
            Markup::Highlight => "Highlight",
            Markup::Underline => "Underline",
            Markup::StrikeOut => "StrikeOut",
        }
    }
}

/// One change to a document. Pages are zero-based; boxes are in PDF user
/// space as (x0, y0, x1, y1).
#[derive(Debug, Clone)]
pub enum Edit {
    Markup { page: usize, kind: Markup, boxes: Vec<[f32; 4]>, color: [f32; 3], text: String, author: String },
    /// A sticky note whose icon's top-left corner is at `at`.
    Note { page: usize, at: (f32, f32), text: String, author: String },
    /// Text drawn straight onto the page, its top-left corner at `at`.
    TextBox { page: usize, at: (f32, f32), text: String, size: f32, author: String },
    SetContents { annotation: ObjectId, text: String },
    RemoveAnnotation { annotation: ObjectId },
    /// Turn a page by a multiple of 90°, clockwise.
    Rotate { page: usize, degrees: i64 },
    DeletePage { page: usize },
    /// Take a page out and put it back so it ends up at index `to`.
    MovePage { from: usize, to: usize },
}

pub fn apply(doc: &mut Document, edit: &Edit) -> Result<()> {
    match edit {
        Edit::Markup { page, kind, boxes, color, text, author } => {
            let annot = markup(doc, *kind, boxes, *color, text, author)?;
            add_annotation(doc, *page, annot)
        }
        Edit::Note { page, at, text, author } => {
            let annot = note(doc, *at, text, author);
            add_annotation(doc, *page, annot)
        }
        Edit::TextBox { page, at, text, size, author } => {
            let annot = text_box(doc, *at, text, *size, author)?;
            add_annotation(doc, *page, annot)
        }
        Edit::SetContents { annotation, text } => {
            let dict = doc.get_dictionary_mut(*annotation).context("the annotation is gone")?;
            dict.set("Contents", lopdf::text_string(text));
            dict.set("M", Object::string_literal(pdf_date()));
            Ok(())
        }
        Edit::RemoveAnnotation { annotation } => remove_annotation(doc, *annotation),
        Edit::Rotate { page, degrees } => {
            let id = page_id(doc, *page)?;
            let now = inherited(doc, id, b"Rotate").and_then(|r| r.as_i64().ok()).unwrap_or(0);
            doc.get_dictionary_mut(id)?.set("Rotate", (now + degrees).rem_euclid(360));
            Ok(())
        }
        Edit::DeletePage { page } => {
            let mut kids = flatten_pages(doc)?;
            if kids.len() <= 1 {
                bail!("a PDF must keep at least one page");
            }
            if *page >= kids.len() {
                bail!("there is no page {}", page + 1);
            }
            kids.remove(*page);
            set_kids(doc, &kids)
        }
        Edit::MovePage { from, to } => {
            let mut kids = flatten_pages(doc)?;
            if *from >= kids.len() || *to >= kids.len() {
                bail!("there is no such page");
            }
            let page = kids.remove(*from);
            kids.insert(*to, page);
            set_kids(doc, &kids)
        }
    }
}

pub fn save(doc: &mut Document) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).context("could not write the PDF")?;
    Ok(bytes)
}

fn page_id(doc: &Document, page: usize) -> Result<ObjectId> {
    doc.get_pages().get(&(page as u32 + 1)).copied().ok_or_else(|| anyhow!("there is no page {}", page + 1))
}

/// An attribute a page may take from its parents (Resources, MediaBox,
/// CropBox, Rotate).
fn inherited<'a>(doc: &'a Document, page: ObjectId, key: &[u8]) -> Option<&'a Object> {
    let mut id = page;
    for _ in 0..64 {
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(value) = dict.get(key) {
            return match value {
                Object::Reference(r) => doc.get_object(*r).ok(),
                v => Some(v),
            };
        }
        id = dict.get(b"Parent").and_then(Object::as_reference).ok()?;
    }
    None
}

/// Make the page tree one flat list under the root, copying down anything a
/// page inherited from the nodes being dissolved. Reordering and removing
/// pages is then editing one array, whatever shape the producer left the tree
/// in. Returns the pages in order.
fn flatten_pages(doc: &mut Document) -> Result<Vec<ObjectId>> {
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for &id in &pages {
        for key in [&b"Resources"[..], b"MediaBox", b"CropBox", b"Rotate"] {
            let own = doc.get_dictionary(id).is_ok_and(|d| d.has(key));
            if !own && let Some(value) = inherited(doc, id, key).cloned() {
                doc.get_dictionary_mut(id)?.set(key, value);
            }
        }
    }
    Ok(pages)
}

fn set_kids(doc: &mut Document, kids: &[ObjectId]) -> Result<()> {
    let root = doc.catalog()?.get(b"Pages")?.as_reference()?;
    let root_dict = doc.get_dictionary_mut(root)?;
    root_dict.set("Kids", kids.iter().map(|&k| Object::Reference(k)).collect::<Vec<_>>());
    root_dict.set("Count", kids.len() as i64);
    for &kid in kids {
        doc.get_dictionary_mut(kid)?.set("Parent", root);
    }
    Ok(())
}

fn add_annotation(doc: &mut Document, page: usize, annot: Dictionary) -> Result<()> {
    let page = page_id(doc, page)?;
    let mut annot = annot;
    annot.set("P", page);
    let id = doc.add_object(annot);
    // /Annots may be inline or a reference to an array shared with nothing
    // else; either way it ends up an inline array on this page.
    let mut annots = match doc.get_dictionary(page)?.get(b"Annots") {
        Ok(Object::Array(a)) => a.clone(),
        Ok(Object::Reference(r)) => doc.get_object(*r).and_then(Object::as_array).cloned().unwrap_or_default(),
        _ => Vec::new(),
    };
    annots.push(Object::Reference(id));
    doc.get_dictionary_mut(page)?.set("Annots", annots);
    Ok(())
}

fn remove_annotation(doc: &mut Document, annotation: ObjectId) -> Result<()> {
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let mut found = false;
    for page in pages {
        let annots = match doc.get_dictionary(page)?.get(b"Annots") {
            Ok(Object::Array(a)) => a.clone(),
            Ok(Object::Reference(r)) => doc.get_object(*r).and_then(Object::as_array).cloned().unwrap_or_default(),
            _ => continue,
        };
        // A note's popup goes with it.
        let popup = doc.get_dictionary(annotation).ok().and_then(|d| d.get(b"Popup").and_then(Object::as_reference).ok());
        let kept: Vec<Object> = annots
            .iter()
            .filter(|a| !matches!(a, Object::Reference(r) if *r == annotation || Some(*r) == popup))
            .cloned()
            .collect();
        if kept.len() != annots.len() {
            found = true;
            doc.get_dictionary_mut(page)?.set("Annots", kept);
        }
    }
    if !found {
        bail!("the annotation is not on any page");
    }
    doc.objects.remove(&annotation);
    Ok(())
}

fn bounds(boxes: &[[f32; 4]]) -> [f32; 4] {
    boxes
        .iter()
        .copied()
        .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
        .unwrap_or_default()
}

fn rect_object(r: [f32; 4]) -> Object {
    Object::Array(r.iter().map(|&v| Object::Real(v)).collect())
}

fn common(subtype: &str, rect: [f32; 4], author: &str) -> Dictionary {
    let now = pdf_date();
    dictionary! {
        "Type" => "Annot",
        "Subtype" => subtype,
        "Rect" => rect_object(rect),
        // Print: annotations meant to be read are meant to be printed too.
        "F" => 4,
        "T" => lopdf::text_string(author),
        "CreationDate" => Object::string_literal(now.clone()),
        "M" => Object::string_literal(now),
        "NM" => Object::string_literal(format!("raven-{:x}", unique())),
    }
}

/// A form XObject in the annotation's own coordinates: `Rect` and `BBox` are
/// the same box, so the appearance maps onto the page unscaled. Streams may
/// only be indirect objects — written inline, readers reject the whole file.
fn appearance(doc: &mut Document, rect: [f32; 4], ops: String, resources: Dictionary) -> Object {
    let form = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => rect_object(rect),
            "Resources" => resources,
        },
        ops.into_bytes(),
    );
    Object::Dictionary(dictionary! { "N" => doc.add_object(form) })
}

fn markup(doc: &mut Document, kind: Markup, boxes: &[[f32; 4]], color: [f32; 3], text: &str, author: &str) -> Result<Dictionary> {
    if boxes.is_empty() {
        bail!("nothing is selected");
    }
    let rect = bounds(boxes);
    let [r, g, b] = color;
    // Quads go top-left, top-right, bottom-left, bottom-right, the order
    // Acrobat writes and every reader accepts.
    let quads: Vec<Object> = boxes
        .iter()
        .flat_map(|q| [q[0], q[3], q[2], q[3], q[0], q[1], q[2], q[1]])
        .map(Object::Real)
        .collect();
    let mut ops = String::new();
    let resources = match kind {
        Markup::Highlight => {
            // Translucent colour rather than the Multiply blend Acrobat
            // writes: hayro draws a page as an isolated group, so Multiply
            // there blends against nothing and paints the text over black.
            // Translucent looks the same in every reader and leaves the
            // words under it readable.
            ops.push_str(&format!("/GS0 gs {r} {g} {b} rg\n"));
            for q in boxes {
                ops.push_str(&format!("{} {} {} {} re f\n", q[0], q[1], q[2] - q[0], q[3] - q[1]));
            }
            dictionary! { "ExtGState" => dictionary! { "GS0" => dictionary! {
                "Type" => "ExtGState", "ca" => HIGHLIGHT_ALPHA, "CA" => HIGHLIGHT_ALPHA,
            } } }
        }
        Markup::Underline | Markup::StrikeOut => {
            for q in boxes {
                // The box is the em box (descent to ascent), so the baseline
                // sits a fifth of the way up and the x-height's middle at about
                // half of it.
                let height = q[3] - q[1];
                let width = (height / 14.0).max(0.5);
                let y = match kind {
                    Markup::Underline => q[1] + height * 0.2 - width,
                    _ => q[1] + height * 0.43,
                };
                ops.push_str(&format!("{r} {g} {b} RG {width} w {} {y} m {} {y} l S\n", q[0], q[2]));
            }
            Dictionary::new()
        }
    };
    let mut dict = common(kind.subtype(), rect, author);
    dict.set("QuadPoints", quads);
    dict.set("C", vec![Object::Real(r), Object::Real(g), Object::Real(b)]);
    dict.set("CA", 1.0);
    if !text.is_empty() {
        dict.set("Contents", lopdf::text_string(text));
    }
    dict.set("AP", appearance(doc, rect, ops, resources));
    Ok(dict)
}

const NOTE_SIZE: f32 = 20.0;
/// Strong enough to see at a glance, faint enough to read through.
const HIGHLIGHT_ALPHA: f32 = 0.38;

fn note(doc: &mut Document, at: (f32, f32), text: &str, author: &str) -> Dictionary {
    let (x, y) = at;
    let rect = [x, y - NOTE_SIZE, x + NOTE_SIZE, y];
    // A folded yellow sheet with three lines on it.
    let (x0, y0, s) = (rect[0], rect[1], NOTE_SIZE);
    let ops = format!(
        "0.99 0.83 0.25 rg 0.55 0.42 0.05 RG 1 w \
         {a} {b} m {c} {b} l {c} {d} l {e} {f} l {a} {f} l h B \
         0.55 0.42 0.05 RG 1.2 w {g} {h1} m {i} {h1} l S {g} {h2} m {i} {h2} l S {g} {h3} m {j} {h3} l S",
        a = x0 + 1.0,
        b = y0 + 1.0,
        c = x0 + s - 1.0,
        d = y0 + s - 6.0,
        e = x0 + s - 6.0,
        f = y0 + s - 1.0,
        g = x0 + 5.0,
        i = x0 + s - 5.0,
        j = x0 + s - 9.0,
        h1 = y0 + s - 8.0,
        h2 = y0 + s - 12.0,
        h3 = y0 + s - 16.0,
    );
    let mut dict = common("Text", rect, author);
    dict.set("Contents", lopdf::text_string(text));
    dict.set("Name", "Comment");
    dict.set("C", vec![Object::Real(0.99), Object::Real(0.83), Object::Real(0.25)]);
    dict.set("AP", appearance(doc, rect, ops, Dictionary::new()));
    dict
}

/// Helvetica's advance widths for printable ASCII, in 1000ths of an em. A
/// text box is drawn in one of the standard 14 fonts, which every reader
/// carries, so these are the widths it will be drawn with.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667,
    556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556,
    556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722,
    500, 500, 500, 334, 260, 334, 584,
];

fn text_width(line: &str, size: f32) -> f32 {
    line.chars()
        .map(|c| match c as u32 {
            32..=126 => HELVETICA[c as usize - 32] as f32,
            _ => 556.0,
        })
        .sum::<f32>()
        * size
        / 1000.0
}

/// WinAnsi, which is what a standard font without an embedded encoding
/// draws: Latin-1 plus a few typographic marks. Anything else would come out
/// as the wrong glyph, so it is drawn as "?" — the annotation's /Contents
/// still has the real text.
fn win_ansi(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match c {
            '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => c as u8,
            '€' => 0x80,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            '–' => 0x96,
            '—' => 0x97,
            '…' => 0x85,
            _ => b'?',
        })
        .collect()
}

fn pdf_string(bytes: &[u8]) -> String {
    let mut out = String::from("(");
    for &b in bytes {
        match b {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            32..=126 => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push(')');
    out
}

fn text_box(doc: &mut Document, at: (f32, f32), text: &str, size: f32, author: &str) -> Result<Dictionary> {
    let text = text.trim_end();
    if text.is_empty() {
        bail!("the text box is empty");
    }
    let lines: Vec<&str> = text.lines().collect();
    let leading = size * 1.2;
    let pad = 2.0;
    let width = lines.iter().map(|l| text_width(l, size)).fold(0.0, f32::max) + 2.0 * pad;
    let height = leading * lines.len() as f32 + 2.0 * pad;
    let (x, y) = at;
    let rect = [x, y - height, x + width, y];
    let mut ops = format!("BT /Helv {size} Tf 0 g {leading} TL {} {} Td\n", x + pad, y - pad - size);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            ops.push_str("T*\n");
        }
        ops.push_str(&format!("{} Tj\n", pdf_string(&win_ansi(line))));
    }
    ops.push_str("ET");
    let font = dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
    };
    let mut dict = common("FreeText", rect, author);
    dict.set("Contents", lopdf::text_string(text));
    dict.set("DA", Object::string_literal(format!("/Helv {size} Tf 0 g")));
    dict.set("AP", appearance(doc, rect, ops, dictionary! { "Font" => dictionary! { "Helv" => font } }));
    Ok(dict)
}

/// `D:YYYYMMDDHHmmSSZ`, the PDF date format, in UTC.
fn pdf_date() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("D:{year:04}{month:02}{day:02}{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn unique() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    nanos ^ ((COUNTER.fetch_add(1, Ordering::Relaxed) as u128) << 64)
}

/// Pages of every file one after another, in order. Each file's outline
/// comes along under an entry named after it, pointing at its first page, so
/// the combined file can still be navigated by chapter.
pub fn merge(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    if files.is_empty() {
        bail!("nothing to combine");
    }
    let mut out = Document::with_version("1.7");
    let mut all_pages: Vec<ObjectId> = Vec::new();
    // (title, first page, the file's own outline First/Last)
    type Section = (String, ObjectId, Option<(ObjectId, ObjectId)>);
    let mut sections: Vec<Section> = Vec::new();
    let mut next_id = 1;

    for (name, bytes) in files {
        let mut doc = Document::load_mem(bytes).with_context(|| format!("{name} is not a readable PDF"))?;
        if doc.is_encrypted() {
            bail!("{name} is password-protected");
        }
        doc.renumber_objects_with(next_id);
        next_id = doc.max_id + 1;
        let pages = flatten_pages(&mut doc)?;
        let Some(&first) = pages.first() else { bail!("{name} has no pages") };
        let outline = doc
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Outlines").ok())
            .and_then(|o| match o {
                Object::Reference(r) => doc.get_dictionary(*r).ok(),
                Object::Dictionary(d) => Some(d),
                _ => None,
            })
            .and_then(|o| Some((o.get(b"First").ok()?.as_reference().ok()?, o.get(b"Last").ok()?.as_reference().ok()?)));
        sections.push((name.clone(), first, outline));
        all_pages.extend(pages);
        out.objects.extend(std::mem::take(&mut doc.objects));
    }
    out.max_id = next_id;

    let pages_root = out.new_object_id();
    let outline_root = out.new_object_id();
    let mut entries: Vec<ObjectId> = Vec::new();
    for (title, first, own) in &sections {
        let id = out.new_object_id();
        let mut entry = dictionary! {
            "Title" => lopdf::text_string(title),
            "Parent" => outline_root,
            "Dest" => vec![Object::Reference(*first), Object::Name(b"Fit".to_vec())],
        };
        if let Some((a, z)) = *own {
            // Re-parent the file's top-level entries and fold them away.
            let mut children = 0i64;
            let mut at = Some(a);
            while let Some(child) = at.filter(|_| children < 100_000) {
                children += 1;
                let Ok(dict) = out.get_dictionary_mut(child) else { break };
                dict.set("Parent", id);
                at = dict.get(b"Next").and_then(Object::as_reference).ok();
            }
            entry.set("First", a);
            entry.set("Last", z);
            entry.set("Count", -children);
        }
        out.objects.insert(id, Object::Dictionary(entry));
        entries.push(id);
    }
    for (i, &id) in entries.iter().enumerate() {
        let dict = out.get_dictionary_mut(id)?;
        if i > 0 {
            dict.set("Prev", entries[i - 1]);
        }
        if let Some(&next) = entries.get(i + 1) {
            dict.set("Next", next);
        }
    }
    out.objects.insert(
        outline_root,
        Object::Dictionary(dictionary! {
            "Type" => "Outlines",
            "First" => entries[0],
            "Last" => *entries.last().unwrap_or(&entries[0]),
            "Count" => entries.len() as i64,
        }),
    );
    out.objects.insert(
        pages_root,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => all_pages.iter().map(|&p| Object::Reference(p)).collect::<Vec<_>>(),
            "Count" => all_pages.len() as i64,
        }),
    );
    for &page in &all_pages {
        out.get_dictionary_mut(page)?.set("Parent", pages_root);
    }
    let catalog = out.add_object(dictionary! {
        "Type" => "Catalog", "Pages" => pages_root, "Outlines" => outline_root, "PageMode" => "UseOutlines",
    });
    out.trailer.set("Root", catalog);
    // The old catalogs and page-tree nodes are now unreachable.
    out.prune_objects();
    out.renumber_objects();
    save(&mut out)
}

struct Job {
    edit: Option<Edit>,
    /// Start over from these bytes instead (undo).
    reset: Option<Arc<Vec<u8>>>,
    reply: async_channel::Sender<Result<Arc<Vec<u8>>, String>>,
}

/// Holds the parsed document on its own thread, so a book is parsed once
/// rather than on every highlight, and neither parsing nor saving blocks the
/// window.
pub struct Editor {
    tx: mpsc::Sender<Job>,
}

impl Editor {
    pub fn spawn(bytes: Arc<Vec<u8>>) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let _ = std::thread::Builder::new().name("raven-edit".into()).spawn(move || {
            let load = |b: &[u8]| Document::load_mem(b).map_err(|e| format!("this PDF can’t be edited ({e})"));
            let mut doc = load(&bytes);
            while let Ok(job) = rx.recv() {
                if let Some(bytes) = job.reset {
                    doc = load(&bytes);
                    let _ = job.reply.send_blocking(Ok(bytes));
                    continue;
                }
                let result = match (&mut doc, &job.edit) {
                    (Err(e), _) => Err(e.clone()),
                    (Ok(_), None) => Err("nothing to do".into()),
                    (Ok(d), Some(edit)) => {
                        // A failed edit must not leave half of itself behind.
                        let before = d.clone();
                        match apply(d, edit).and_then(|()| save(d)) {
                            Ok(bytes) => Ok(Arc::new(bytes)),
                            Err(e) => {
                                *d = before;
                                Err(e.to_string())
                            }
                        }
                    }
                };
                let _ = job.reply.send_blocking(result);
            }
        });
        Self { tx }
    }

    /// Apply an edit; the answer is the whole new file.
    pub async fn apply(&self, edit: Edit) -> Result<Arc<Vec<u8>>, String> {
        self.send(Some(edit), None).await
    }

    /// Go back to an earlier version of the file.
    pub async fn reset(&self, bytes: Arc<Vec<u8>>) -> Result<Arc<Vec<u8>>, String> {
        self.send(None, Some(bytes)).await
    }

    async fn send(&self, edit: Option<Edit>, reset: Option<Arc<Vec<u8>>>) -> Result<Arc<Vec<u8>>, String> {
        let (reply, answer) = async_channel::bounded(1);
        self.tx.send(Job { edit, reset, reply }).map_err(|_| "the editor stopped".to_string())?;
        answer.recv().await.map_err(|_| "the editor stopped".to_string())?
    }
}

/// Every annotation id by page, for tests and the sidebar.
#[allow(dead_code)]
pub fn annotation_ids(doc: &Document) -> BTreeMap<u32, Vec<ObjectId>> {
    doc.get_pages()
        .into_iter()
        .map(|(n, id)| {
            let ids = doc
                .get_dictionary(id)
                .ok()
                .and_then(|p| p.get_deref(b"Annots", doc).ok())
                .and_then(|a| a.as_array().ok())
                .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
                .unwrap_or_default();
            (n, ids)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdftext::tests::sample_pdf;
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{RenderCache, RenderSettings};

    fn edited(bytes: Vec<u8>, edits: &[Edit]) -> Vec<u8> {
        let mut doc = Document::load_mem(&bytes).unwrap();
        for e in edits {
            apply(&mut doc, e).unwrap();
        }
        save(&mut doc).unwrap()
    }

    /// Page 1 at 1pt per pixel, RGB of the pixel at (x, y) from the top-left.
    fn pixel(bytes: &[u8], page: usize, x: u32, y: u32) -> (u8, u8, u8) {
        let pdf = Pdf::new(bytes.to_vec()).unwrap();
        let pages = pdf.pages();
        let pixmap = hayro::render(
            &pages[page],
            &RenderCache::new(),
            &InterpreterSettings::default(),
            &RenderSettings { bg_color: WHITE, ..Default::default() },
        );
        let at = ((y * pixmap.width() as u32 + x) * 4) as usize;
        let d = pixmap.data_as_u8_slice();
        (d[at], d[at + 1], d[at + 2])
    }

    fn text_of(bytes: &[u8]) -> crate::pdftext::PageText {
        let pdf = Pdf::new(bytes.to_vec()).unwrap();
        crate::pdftext::extract(&pdf.pages()[0])
    }

    /// A highlight shows as colour on the page, and the text under it stays
    /// dark: the appearance multiplies rather than paints over.
    #[test]
    fn a_highlight_is_drawn_and_leaves_the_text_readable() {
        let bytes = sample_pdf(0);
        let text = text_of(&bytes);
        let boxes = text.user_boxes(0, 5); // "Hello"
        let out = edited(
            bytes,
            &[Edit::Markup {
                page: 0,
                kind: Markup::Highlight,
                boxes: boxes.clone(),
                color: [1.0, 0.9, 0.2],
                text: "Hello".into(),
                author: "Tester".into(),
            }],
        );
        // Paper inside the highlight — between the stems of the "H", above
        // its crossbar — comes out yellow.
        let b = boxes[0];
        let (x, y) = (b[0] as u32 + 4, 792 - 706);
        let (r, g, bl) = pixel(&out, 0, x, y);
        assert!(r > 220 && g > 200 && bl < 200 && bl + 30 < g, "expected yellow paper, got {:?}", (r, g, bl));
        // …and ink under it stays ink: find the darkest pixel across "H".
        let row = 792 - 700 - 4; // just above the baseline, through the stems
        let darkest = (b[0] as u32..b[0] as u32 + 9).map(|x| pixel(&out, 0, x, row)).min().unwrap();
        assert!(darkest.0 < 110 && darkest.1 < 110, "the text went under the colour: {darkest:?}");
        // The text is still there to select and still says the same thing.
        assert_eq!(text_of(&out).text(0, 11), "Hello world");
        // And it is listed as a note on the page, with what it covers.
        let info = crate::pdf::load_info(&Arc::new(out)).unwrap();
        assert_eq!(info.annotations.len(), 1);
        assert_eq!((info.annotations[0].kind.as_str(), info.annotations[0].contents.as_str()), ("Highlight", "Hello"));
    }

    #[test]
    fn notes_and_text_boxes_are_listed_and_can_be_removed() {
        let out = edited(
            sample_pdf(0),
            &[
                Edit::Note { page: 0, at: (300.0, 500.0), text: "Check this".into(), author: "T".into() },
                Edit::TextBox { page: 0, at: (100.0, 400.0), text: "Typed on".into(), size: 14.0, author: "T".into() },
            ],
        );
        let info = crate::pdf::load_info(&Arc::new(out.clone())).unwrap();
        let kinds: Vec<_> = info.annotations.iter().map(|a| (a.kind.as_str(), a.contents.as_str())).collect();
        assert_eq!(kinds, vec![("Text", "Check this"), ("FreeText", "Typed on")]);
        // The note's icon is drawn where it was put.
        let (r, g, b) = pixel(&out, 0, 310, 792 - 490);
        assert!(r > 200 && g > 150 && b < 120, "expected the note's yellow, got {:?}", (r, g, b));
        // The text box's words are really on the page.
        let (r, _, _) = (100..200).map(|x| pixel(&out, 0, x, 792 - 400 + 10)).min().unwrap();
        assert!(r < 100, "the text box drew nothing dark");

        let id = info.annotations[0].id.unwrap();
        let out = edited(out, &[Edit::RemoveAnnotation { annotation: id }]);
        let info = crate::pdf::load_info(&Arc::new(out)).unwrap();
        assert_eq!(info.annotations.len(), 1);
        assert_eq!(info.annotations[0].kind, "FreeText");
    }

    fn book(pages: usize) -> Vec<u8> {
        use lopdf::Stream;
        let mut doc = Document::with_version("1.5");
        let root = doc.new_object_id();
        // Two levels of page tree, with the MediaBox and resources on the
        // intermediate node: what flattening must carry down.
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let mid = doc.new_object_id();
        let kids: Vec<Object> = (0..pages)
            .map(|i| {
                let ops = format!("BT /F1 24 Tf 72 700 Td (Page {}) Tj ET", i + 1).into_bytes();
                let content = doc.add_object(Stream::new(dictionary! {}, ops));
                doc.add_object(dictionary! { "Type" => "Page", "Parent" => mid, "Contents" => content }).into()
            })
            .collect();
        doc.objects.insert(
            mid,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Parent" => root, "Kids" => kids, "Count" => pages as i64,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            }),
        );
        doc.objects.insert(
            root,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![mid.into()], "Count" => pages as i64 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", catalog);
        save(&mut doc).unwrap()
    }

    fn page_titles(bytes: &[u8]) -> Vec<String> {
        let pdf = Pdf::new(bytes.to_vec()).unwrap();
        pdf.pages()
            .iter()
            .map(|p| {
                let t = crate::pdftext::extract(p);
                t.text(0, t.chars.len())
            })
            .collect()
    }

    #[test]
    fn pages_move_rotate_and_go_and_keep_their_inherited_resources() {
        let out = edited(
            book(4),
            &[
                Edit::MovePage { from: 3, to: 0 },
                Edit::DeletePage { page: 2 },
                Edit::Rotate { page: 1, degrees: 90 },
                Edit::Rotate { page: 1, degrees: 90 },
                Edit::Rotate { page: 1, degrees: -270 },
            ],
        );
        assert_eq!(page_titles(&out), ["Page 4", "Page 1", "Page 3"]);
        let info = crate::pdf::load_info(&Arc::new(out)).unwrap();
        // 90 + 90 − 270 = −90 → 270: the page is sideways.
        assert_eq!(info.page_sizes, vec![(612.0, 792.0), (792.0, 612.0), (612.0, 792.0)]);
    }

    #[test]
    fn the_last_page_cannot_be_deleted() {
        let mut doc = Document::load_mem(&book(1)).unwrap();
        assert!(apply(&mut doc, &Edit::DeletePage { page: 0 }).is_err());
    }

    #[test]
    fn merging_keeps_every_page_in_order_with_an_entry_per_file() {
        let merged = merge(&[("a.pdf".into(), book(2)), ("b.pdf".into(), book(3)), ("c.pdf".into(), sample_pdf(0))]).unwrap();
        assert_eq!(
            page_titles(&merged),
            ["Page 1", "Page 2", "Page 1", "Page 2", "Page 3", "Hello world\nSecond line"]
        );
        let info = crate::pdf::load_info(&Arc::new(merged)).unwrap();
        let outline: Vec<_> = info.outline.iter().map(|e| (e.title.as_str(), e.page)).collect();
        assert_eq!(outline, [("a.pdf", 0), ("b.pdf", 2), ("c.pdf", 5)]);
    }

    #[test]
    fn a_merged_files_own_outline_comes_along_under_its_entry() {
        let chaptered = merge(&[("one.pdf".into(), book(2)), ("two.pdf".into(), book(2))]).unwrap();
        let twice = merge(&[("both.pdf".into(), chaptered), ("more.pdf".into(), book(1))]).unwrap();
        let info = crate::pdf::load_info(&Arc::new(twice)).unwrap();
        let outline: Vec<_> = info.outline.iter().map(|e| (e.level, e.title.as_str(), e.page)).collect();
        assert_eq!(outline, [(0, "both.pdf", 0), (1, "one.pdf", 0), (1, "two.pdf", 2), (0, "more.pdf", 4)]);
    }

    #[test]
    fn dates_are_pdf_dates() {
        let d = pdf_date();
        assert!(d.starts_with("D:20") && d.ends_with('Z') && d.len() == 17, "{d}");
    }
}
