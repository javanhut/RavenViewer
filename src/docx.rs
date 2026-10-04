//! DOCX: a zip of XML whose body is `word/document.xml`. We keep what reading
//! and editing need — headings, paragraphs, emphasis, lists, tables — and,
//! crucially, the XML each of those came from.
//!
//! Saving writes back the original XML of everything that was not touched,
//! byte for byte, and regenerates only the paragraphs that were edited, keeping
//! their paragraph and run properties. Fields, tracked changes and the rest of
//! what we do not model therefore survive an edit elsewhere in the document;
//! paragraphs that carry such things are shown read-only rather than silently
//! losing them. Pictures are modelled: each one's drawing is kept as XML and
//! written back as it was wherever its paragraph ends up, and pictures added
//! here are written as new parts of the package. Every other part of the
//! package is copied untouched.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub highlight: bool,
    /// Stands in for something we do not show (an equation, a chart); drawn
    /// dimmed and never saved as text.
    pub placeholder: bool,
    /// A picture: the run's text is then a single U+FFFC.
    pub image: Option<Image>,
    /// The run's other properties (font, size, colour…) as XML, kept so an
    /// edited paragraph does not lose its typeface.
    pub props: String,
}

impl Run {
    fn same_format(&self, other: &Run) -> bool {
        self.image.is_none()
            && other.image.is_none()
            && (self.bold, self.italic, self.underline, self.highlight, self.placeholder)
                == (other.bold, other.italic, other.underline, other.highlight, other.placeholder)
    }

    pub fn picture(image: Image) -> Run {
        Run { text: OBJECT.into(), image: Some(image), ..Default::default() }
    }

    /// A page break, which shows as a placeholder.
    pub fn page_break() -> Run {
        placeholder(PAGE_BREAK)
    }

    pub fn is_page_break(&self) -> bool {
        self.placeholder && self.text == PAGE_BREAK
    }
}

/// What a picture is shown as in text: the object replacement character.
pub const OBJECT: &str = "\u{FFFC}";
const PAGE_BREAK: &str = "[page break]";

/// English Metric Units: what DrawingML measures in.
pub const EMU_PER_INCH: i64 = 914_400;
pub const EMU_PER_POINT: i64 = 12_700;

/// A picture in the text.
#[derive(Debug, Clone)]
pub struct Image {
    /// The picture file (PNG, JPEG…); empty when the document only links to
    /// it or the part is missing.
    pub data: Arc<Vec<u8>>,
    /// Its size on the page, in EMU.
    pub cx: i64,
    pub cy: i64,
    /// Where it was read from; `None` for a picture added here.
    pub origin: Option<Arc<Origin>>,
}

/// A picture as the document had it.
#[derive(Debug, PartialEq)]
pub struct Origin {
    /// Which loaded document it belongs to: copied into another one, it is a
    /// new picture there, since its relationship id means nothing there.
    pub doc: u64,
    /// The `<w:drawing>` (or VML `<w:pict>`) element, written back as it was.
    pub xml: String,
    /// The relationship naming the picture's part.
    pub rel: Option<String>,
}

impl PartialEq for Image {
    fn eq(&self, other: &Image) -> bool {
        (Arc::ptr_eq(&self.data, &other.data) || self.data == other.data)
            && (self.cx, self.cy) == (other.cx, other.cy)
            && self.origin == other.origin
    }
}

impl Image {
    /// A picture added to a document, sized from its pixels at 96 dpi and
    /// shrunk to fit `max_width` (EMU).
    pub fn new(data: Vec<u8>, pixels: (i32, i32), max_width: i64) -> Image {
        let (w, h) = (pixels.0.max(1) as i64, pixels.1.max(1) as i64);
        let (mut cx, mut cy) = (w * EMU_PER_INCH / 96, h * EMU_PER_INCH / 96);
        if max_width > 0 && cx > max_width {
            cy = cy * max_width / cx;
            cx = max_width;
        }
        Image { data: Arc::new(data), cx, cy, origin: None }
    }
}

/// What kind of picture file `data` is, as an extension and a media type —
/// for the kinds Word can show.
pub fn picture_type(data: &[u8]) -> Option<(&'static str, &'static str)> {
    Some(if data.starts_with(b"\x89PNG") {
        ("png", "image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ("jpeg", "image/jpeg")
    } else if data.starts_with(b"GIF8") {
        ("gif", "image/gif")
    } else if data.starts_with(b"BM") {
        ("bmp", "image/bmp")
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        ("tiff", "image/tiff")
    } else {
        return None;
    })
}

/// How a paragraph's lines sit between the margins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Justify,
}

impl Align {
    fn from_jc(v: &str) -> Align {
        match v {
            "center" => Align::Center,
            "right" | "end" => Align::End,
            "both" | "distribute" => Align::Justify,
            _ => Align::Start,
        }
    }

    fn jc(self) -> Option<&'static str> {
        match self {
            Align::Start => None,
            Align::Center => Some("center"),
            Align::End => Some("right"),
            Align::Justify => Some("both"),
        }
    }
}

/// The page and the text's defaults, for laying the document out on paper.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSetup {
    /// Paper size, in points.
    pub width: f64,
    pub height: f64,
    /// Top, right, bottom, left, in points.
    pub margins: [f64; 4],
    pub font: String,
    /// In points.
    pub size: f64,
    /// Space after a paragraph, in points.
    pub after: f64,
    /// Line height as a multiple of single spacing.
    pub line: f64,
}

impl Default for PageSetup {
    fn default() -> Self {
        PageSetup {
            width: 612.0,
            height: 792.0,
            margins: [72.0; 4],
            font: "Calibri".into(),
            size: 11.0,
            after: 8.0,
            line: 1.08,
        }
    }
}

impl PageSetup {
    /// The width text is set in, in EMU.
    pub fn text_width_emu(&self) -> i64 {
        ((self.width - self.margins[1] - self.margins[3]).max(72.0) * EMU_PER_POINT as f64) as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParaStyle {
    #[default]
    Normal,
    Title,
    Heading(u8),
    ListItem(u8),
    Quote,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph { style: ParaStyle, runs: Vec<Run>, align: Align },
    Table { rows: Vec<Vec<String>> },
}

impl Block {
    pub fn paragraph(style: ParaStyle, runs: Vec<Run>) -> Block {
        Block::Paragraph { style, runs, align: Align::Start }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// A paragraph that can be edited as text.
    Paragraph,
    /// A paragraph holding something we cannot rewrite (an image, a field,
    /// an equation): shown, kept verbatim, not editable.
    Locked,
    Table,
    /// Anything else in the body — content controls, bookmarks, the section
    /// properties. Shown if it has text, always kept.
    Other,
}

/// One child of `<w:body>`.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: ItemKind,
    /// What to show for it; empty for things with nothing to show.
    pub blocks: Vec<Block>,
    span: Range<usize>,
}

impl Item {
    pub fn visible(&self) -> bool {
        !self.blocks.is_empty() || matches!(self.kind, ItemKind::Paragraph | ItemKind::Locked | ItemKind::Table)
    }
}

pub struct Docx {
    /// Tells this document's pictures from another's.
    id: u64,
    package: Vec<u8>,
    xml: String,
    /// Where the body's children start and end in `xml`.
    body: Range<usize>,
    pub items: Vec<Item>,
    /// Style ids the document defines; a heading written with a style it does
    /// not have is given direct formatting instead.
    styles: HashSet<String>,
    /// A list paragraph's numbering, reused for new bullets.
    bullet: Option<String>,
    pub page: PageSetup,
}

/// What to write for one paragraph of the edited document.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Item `i` as it was.
    Keep(usize),
    /// A paragraph with this content, taking its properties from item
    /// `base` if it was edited from one.
    Para { style: ParaStyle, runs: Vec<Run>, base: Option<usize> },
    /// A paragraph or table written from scratch, as it says.
    Block(Block),
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn load(bytes: &[u8]) -> Result<Docx> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("not a DOCX (zip) file")?;
    let xml = read_part(&mut zip, "word/document.xml")?.context("the file has no word/document.xml")?;
    let styles_xml = read_part(&mut zip, "word/styles.xml")?;
    let styles = styles_xml.as_deref().map(style_ids).unwrap_or_default();
    let (body, mut items) = split_body(&xml)?;
    let bullet = items
        .iter()
        .filter(|i| i.kind == ItemKind::Paragraph)
        .find_map(|i| inner_of(&xml[i.span.clone()], b"numPr").map(|n| format!("<w:numPr>{n}</w:numPr>")))
        .or_else(|| read_part(&mut zip, "word/numbering.xml").ok().flatten().and_then(|n| bullet_numbering(&n)));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    // Pictures: their parts, read once however often they are shown.
    let rels = read_part(&mut zip, &rels_path("word/document.xml"))?.map(|x| rels_of(&x)).unwrap_or_default();
    let mut parts: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    let empty = Arc::new(Vec::new());
    for run in items.iter_mut().flat_map(|i| i.blocks.iter_mut()).flat_map(|b| match b {
        Block::Paragraph { runs, .. } => runs.iter_mut(),
        Block::Table { .. } => [].iter_mut(),
    }) {
        let Some(image) = run.image.as_mut() else { continue };
        let Some(origin) = image.origin.as_ref() else { continue };
        let part = origin
            .rel
            .as_ref()
            .and_then(|r| rels.iter().find(|x| &x.id == r && !x.external))
            .map(|r| resolve("word", &r.target));
        image.data = match part {
            Some(part) => match parts.get(&part) {
                Some(data) => data.clone(),
                None => {
                    let mut data = Vec::new();
                    if let Ok(mut f) = zip.by_name(&part) {
                        let _ = f.read_to_end(&mut data);
                    }
                    let data = Arc::new(data);
                    parts.insert(part, data.clone());
                    data
                }
            },
            None => empty.clone(),
        };
        image.origin = Some(Arc::new(Origin { doc: id, xml: origin.xml.clone(), rel: origin.rel.clone() }));
    }

    let page = page_setup(&xml, styles_xml.as_deref());
    Ok(Docx { id, package: bytes.to_vec(), xml, body, items, styles, bullet, page })
}

/// A bulleted list the numbering part defines, as the `numPr` to use it.
fn bullet_numbering(numbering: &str) -> Option<String> {
    let (abstracts, nums) = numbering_defs(numbering);
    let bulleted: HashSet<u32> = abstracts
        .iter()
        .filter(|(_, x)| {
            children(inner_of(x, b"abstractNum").as_deref().unwrap_or_default())
                .iter()
                .find(|(n, _)| n == "lvl")
                .is_some_and(|(_, lvl)| lvl.contains(r#"w:numFmt w:val="bullet""#))
        })
        .map(|(id, _)| *id)
        .collect();
    nums.iter()
        .find(|(_, x)| {
            children(inner_of(x, b"num").as_deref().unwrap_or_default())
                .iter()
                .any(|(n, a)| n == "abstractNumId" && attr_value(a, "val").and_then(|v| v.parse().ok()).is_some_and(|v| bulleted.contains(&v)))
        })
        .map(|(id, _)| format!(r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="{id}"/></w:numPr>"#))
}

/// The paper and margins of the last section, and the text defaults the
/// styles set.
fn page_setup(document: &str, styles: Option<&str>) -> PageSetup {
    let mut page = PageSetup::default();
    let twips = |v: Option<String>| v.and_then(|v| v.parse::<f64>().ok()).map(|t| t / 20.0);
    if let Some(at) = document.rfind("<w:sectPr")
        && let sect = &document[at..]
        && let Some(open_end) = sect.find('>')
        && !sect[..open_end].ends_with('/')
    {
        let inner = &sect[open_end + 1..sect.find("</w:sectPr>").unwrap_or(sect.len())];
        for (name, x) in children(inner) {
            match name.as_str() {
                "pgSz" => {
                    if let (Some(w), Some(h)) = (twips(attr_value(&x, "w")), twips(attr_value(&x, "h"))) {
                        (page.width, page.height) = (w, h);
                    }
                }
                "pgMar" => {
                    let side = |names: &[&str], old: f64| {
                        names.iter().find_map(|n| twips(attr_value(&x, n))).map_or(old, |v| v.abs())
                    };
                    page.margins = [
                        side(&["top"], page.margins[0]),
                        side(&["right", "end"], page.margins[1]),
                        side(&["bottom"], page.margins[2]),
                        side(&["left", "start"], page.margins[3]),
                    ];
                }
                _ => {}
            }
        }
    }
    let Some(styles) = styles else { return page };
    // The document defaults, then what the Normal style says on top.
    let normal = children(inner_of(styles, b"styles").as_deref().unwrap_or_default())
        .into_iter()
        .find(|(n, x)| n == "style" && attr_value(x, "styleId").as_deref() == Some("Normal"))
        .map(|(_, x)| x);
    for scope in [inner_of(styles, b"docDefaults"), normal] {
        let Some(scope) = scope else { continue };
        let mut reader = Reader::from_str(&scope);
        loop {
            match reader.read_event() {
                Ok(Event::Start(e) | Event::Empty(e)) => match local(e.name().as_ref()) {
                    b"rFonts" => {
                        if let Some(f) = attr(&e, b"ascii").or_else(|| attr(&e, b"hAnsi")) {
                            page.font = f;
                        }
                    }
                    b"sz" => {
                        if let Some(s) = val(&e).and_then(|v| v.parse::<f64>().ok()) {
                            page.size = s / 2.0;
                        }
                    }
                    b"spacing" => {
                        if let Some(a) = attr(&e, b"after").and_then(|v| v.parse::<f64>().ok()) {
                            page.after = a / 20.0;
                        }
                        if attr(&e, b"lineRule").as_deref().is_none_or(|r| r == "auto")
                            && let Some(l) = attr(&e, b"line").and_then(|v| v.parse::<f64>().ok())
                        {
                            page.line = l / 240.0;
                        }
                    }
                    _ => {}
                },
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }
    page
}

fn read_part<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Option<String>> {
    let Ok(mut file) = zip.by_name(name) else { return Ok(None) };
    let mut text = String::new();
    file.read_to_string(&mut text).with_context(|| format!("{name} is not readable"))?;
    Ok(Some(text))
}

impl Docx {
    /// Everything there is to show, in order.
    #[allow(dead_code)]
    pub fn blocks(&self) -> Vec<Block> {
        self.items.iter().flat_map(|i| i.blocks.iter().cloned()).collect()
    }

    /// The package with `out` as its body.
    pub fn save(&self, out: &[Out]) -> Result<Vec<u8>> {
        let mut media = Media::new(self);
        let body = self.write_body(out, &mut media);
        let head = &self.xml[..self.body.start];
        let head = if media.wrote_drawing { with_namespaces(head) } else { head.to_string() };
        let mut xml = String::with_capacity(self.xml.len() + body.len());
        xml.push_str(&head);
        xml.push_str(&body);
        xml.push_str(&self.xml[self.body.end..]);
        let mut replace: Vec<(String, Vec<u8>)> = vec![("word/document.xml".into(), xml.into_bytes())];
        if !media.added.is_empty() {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&self.package[..]))?;
            let ct_xml = read_part(&mut zip, "[Content_Types].xml")?.context("the file has no content types")?;
            let mut ct = ContentTypes::parse(&ct_xml);
            let rels_name = rels_path("word/document.xml");
            let mut rels = read_part(&mut zip, &rels_name)?.map(|x| rels_of(&x)).unwrap_or_default();
            for (data, id, part) in &media.added {
                let ext = part.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default();
                if ct.of(part).is_none() {
                    let mime = picture_type(data).map_or("application/octet-stream", |(_, m)| m);
                    ct.defaults.insert(ext, mime.into());
                }
                rels.push(Rel {
                    id: id.clone(),
                    kind: "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image".into(),
                    target: part.trim_start_matches("word/").into(),
                    external: false,
                });
                replace.push((part.clone(), data.to_vec()));
            }
            replace.push(("[Content_Types].xml".into(), ct.xml().into_bytes()));
            replace.push((rels_name, rels_xml(&rels).into_bytes()));
        }
        repackage(&self.package, &replace, &[])
    }

    /// The text width of the page, for sizing pictures added to it.
    pub fn text_width_emu(&self) -> i64 {
        self.page.text_width_emu()
    }

    /// What `out` shows, paragraph by paragraph: what the document holds where
    /// it is kept, and the edits where it is not.
    pub fn blocks_of(&self, out: &[Out]) -> Vec<Block> {
        let mut blocks = Vec::new();
        for o in out {
            match o {
                Out::Keep(i) => blocks.extend(self.items[*i].blocks.iter().cloned()),
                Out::Para { style, runs, base } => {
                    let align = base
                        .and_then(|b| match self.items[b].blocks.first() {
                            Some(Block::Paragraph { align, .. }) => Some(*align),
                            _ => None,
                        })
                        .unwrap_or_default();
                    blocks.push(Block::Paragraph { style: *style, runs: runs.clone(), align });
                }
                Out::Block(b) => blocks.push(b.clone()),
            }
        }
        blocks
    }

    fn write_body(&self, out: &[Out], media: &mut Media) -> String {
        // Things with nothing to show (bookmarks, the section properties) are
        // kept in front of the next thing that is shown, so they travel with
        // it; the last of them — the section properties, which must end the
        // body — stay at the end.
        let mut owner: Vec<Option<usize>> = vec![None; self.items.len()];
        let mut pending: Vec<usize> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            if item.visible() {
                for h in pending.drain(..) {
                    owner[h] = Some(i);
                }
            } else {
                pending.push(i);
            }
        }
        let trailing = pending;
        let mut emitted = vec![false; self.items.len()];
        let mut body = String::new();
        let emit = |i: usize, body: &mut String, emitted: &mut Vec<bool>| {
            if !emitted[i] {
                emitted[i] = true;
                body.push_str(&self.xml[self.items[i].span.clone()]);
            }
        };
        for o in out {
            let base = match o {
                Out::Keep(i) => Some(*i),
                Out::Para { base, .. } => *base,
                Out::Block(_) => None,
            };
            if let Some(b) = base {
                for h in (0..self.items.len()).filter(|&h| owner[h] == Some(b)) {
                    emit(h, &mut body, &mut emitted);
                }
            }
            match o {
                Out::Keep(i) => emit(*i, &mut body, &mut emitted),
                Out::Para { style, runs, base } => body.push_str(&self.paragraph(*style, runs, *base, None, media)),
                Out::Block(Block::Paragraph { style, runs, align }) => {
                    body.push_str(&self.paragraph(*style, runs, None, Some(*align), media))
                }
                Out::Block(Block::Table { rows }) => body.push_str(&self.table(rows)),
            }
        }
        // Whatever travelled with a deleted paragraph is still kept.
        for h in (0..self.items.len()).filter(|h| !self.items[*h].visible() && !trailing.contains(h)) {
            emit(h, &mut body, &mut emitted);
        }
        for h in trailing {
            emit(h, &mut body, &mut emitted);
        }
        body
    }

    fn paragraph(&self, style: ParaStyle, runs: &[Run], base: Option<usize>, align: Option<Align>, media: &mut Media) -> String {
        let base_xml = base.map(|b| &self.xml[self.items[b].span.clone()]);
        let base_style = base.and_then(|b| match self.items[b].blocks.first() {
            Some(Block::Paragraph { style, .. }) => Some(*style),
            _ => None,
        });
        let old_ppr = base_xml.and_then(|x| inner_of(x, b"pPr")).unwrap_or_default();
        let ppr = if base_style == Some(style) {
            old_ppr
        } else {
            let mut parts = children(&old_ppr);
            parts.retain(|(name, _)| name != "pStyle" && name != "numPr");
            // Only styles the document defines: a paragraph naming a missing
            // style loses its list numbering in LibreOffice. Without the style,
            // what it stands for is written out directly.
            parts.retain(|(name, _)| name != "outlineLvl");
            match style_id(style) {
                Some(id) if self.styles.contains(id) => {
                    parts.push(("pStyle".into(), format!(r#"<w:pStyle w:val="{id}"/>"#)));
                }
                _ => match style {
                    ParaStyle::Heading(n) => {
                        parts.push(("outlineLvl".into(), format!(r#"<w:outlineLvl w:val="{}"/>"#, n - 1)))
                    }
                    ParaStyle::Quote => {
                        parts.retain(|(name, _)| name != "ind");
                        parts.push(("ind".into(), r#"<w:ind w:left="720"/>"#.into()));
                    }
                    _ => {}
                },
            }
            if let ParaStyle::ListItem(level) = style {
                let numpr = base_xml
                    .and_then(|x| inner_of(x, b"numPr"))
                    .map(|n| format!("<w:numPr>{n}</w:numPr>"))
                    .or_else(|| self.bullet.clone());
                if let Some(numpr) = numpr {
                    let mut kids = children(&numpr[9..numpr.len() - 10]);
                    kids.retain(|(n, _)| n != "ilvl");
                    kids.insert(0, ("ilvl".into(), format!(r#"<w:ilvl w:val="{level}"/>"#)));
                    let inner: String = kids.into_iter().map(|(_, x)| x).collect();
                    parts.push(("numPr".into(), format!("<w:numPr>{inner}</w:numPr>")));
                }
            }
            if let Some(jc) = align.and_then(Align::jc) {
                parts.retain(|(name, _)| name != "jc");
                parts.push(("jc".into(), format!(r#"<w:jc w:val="{jc}"/>"#)));
            }
            ordered(parts, PPR_ORDER)
        };
        // Headings and quotes in a document without those styles still look
        // like headings and quotes.
        let styled = style_id(style).is_some_and(|id| self.styles.contains(id));
        let direct: &[(&str, String)] = &match style {
            ParaStyle::Title | ParaStyle::Heading(_) if !styled => {
                let size = match style {
                    ParaStyle::Title => 56,
                    ParaStyle::Heading(1) => 32,
                    ParaStyle::Heading(2) => 26,
                    _ => 24,
                };
                vec![("b", "<w:b/>".into()), ("sz", format!(r#"<w:sz w:val="{size}"/>"#))]
            }
            ParaStyle::Quote if !styled => vec![("i", "<w:i/>".into())],
            _ => vec![],
        };
        let listed_without_numbering = matches!(style, ParaStyle::ListItem(_)) && !ppr.contains("numPr");

        let mut xml = String::from("<w:p>");
        if !ppr.is_empty() {
            xml.push_str(&format!("<w:pPr>{ppr}</w:pPr>"));
        }
        if listed_without_numbering {
            // No list definition to point at: write the bullet as text.
            xml.push_str(r#"<w:r><w:t xml:space="preserve">• </w:t></w:r>"#);
        }
        for run in runs {
            if let Some(image) = &run.image {
                xml.push_str(&media.picture(image, &run.props));
            } else if run.is_page_break() {
                xml.push_str(r#"<w:r><w:br w:type="page"/></w:r>"#);
            } else if !run.placeholder && !run.text.is_empty() {
                xml.push_str(&write_run(run, direct));
            }
        }
        xml.push_str("</w:p>");
        xml
    }

    /// A plain bordered table, its columns sharing the text width.
    fn table(&self, rows: &[Vec<String>]) -> String {
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
        let width = ((self.page.width - self.page.margins[1] - self.page.margins[3]) * 20.0) as i64;
        let col = width / cols as i64;
        let border = |side: &str| format!(r#"<w:{side} w:val="single" w:sz="4" w:space="0" w:color="auto"/>"#);
        let mut xml = format!(
            r#"<w:tbl><w:tblPr><w:tblW w:w="{width}" w:type="dxa"/><w:tblBorders>{}{}{}{}{}{}</w:tblBorders><w:tblLayout w:type="fixed"/></w:tblPr><w:tblGrid>"#,
            border("top"), border("left"), border("bottom"), border("right"), border("insideH"), border("insideV")
        );
        for _ in 0..cols {
            xml.push_str(&format!(r#"<w:gridCol w:w="{col}"/>"#));
        }
        xml.push_str("</w:tblGrid>");
        for row in rows {
            xml.push_str("<w:tr>");
            for c in 0..cols {
                xml.push_str(&format!(r#"<w:tc><w:tcPr><w:tcW w:w="{col}" w:type="dxa"/></w:tcPr>"#));
                for line in row.get(c).map_or("", String::as_str).split('\n') {
                    xml.push_str("<w:p>");
                    if !line.is_empty() {
                        xml.push_str(&write_run(&Run { text: line.into(), ..Default::default() }, &[]));
                    }
                    xml.push_str("</w:p>");
                }
                xml.push_str("</w:tc>");
            }
            xml.push_str("</w:tr>");
        }
        xml.push_str("</w:tbl>");
        xml
    }
}

/// Pictures being written: new ones become parts of the package, and every
/// drawing gets an id of its own.
struct Media<'a> {
    doc: &'a Docx,
    /// New picture parts: their bytes, relationship id and part name.
    added: Vec<(Arc<Vec<u8>>, String, String)>,
    taken_ids: HashSet<String>,
    taken_parts: HashSet<String>,
    next_drawing: u64,
    wrote_drawing: bool,
}

impl<'a> Media<'a> {
    fn new(doc: &'a Docx) -> Self {
        let mut taken_parts = HashSet::new();
        if let Ok(zip) = zip::ZipArchive::new(std::io::Cursor::new(&doc.package[..])) {
            taken_parts.extend(zip.file_names().map(str::to_string));
        }
        let taken_ids = zip::ZipArchive::new(std::io::Cursor::new(&doc.package[..]))
            .ok()
            .and_then(|mut z| read_part(&mut z, &rels_path("word/document.xml")).ok().flatten())
            .map(|x| rels_of(&x).into_iter().map(|r| r.id).collect())
            .unwrap_or_default();
        // Drawing ids must be unique across the document; ours start past
        // every number the document already uses for one.
        let next_drawing = drawing_ids(&doc.xml).max().unwrap_or(0) + 1;
        Media { doc, added: Vec::new(), taken_ids, taken_parts, next_drawing, wrote_drawing: false }
    }

    /// A run showing `image`.
    fn picture(&mut self, image: &Image, props: &str) -> String {
        let props = if props.is_empty() { String::new() } else { format!("<w:rPr>{props}</w:rPr>") };
        if let Some(origin) = image.origin.as_ref().filter(|o| o.doc == self.doc.id) {
            // Its own drawing, renumbered: the paragraph it came from may
            // still hold it too.
            let id = self.next_drawing;
            self.next_drawing += 1;
            return format!("<w:r>{props}{}</w:r>", renumber_drawing(&origin.xml, id));
        }
        let Some((ext, _)) = picture_type(&image.data) else { return String::new() };
        let rel = match self.added.iter().find(|(d, _, _)| Arc::ptr_eq(d, &image.data) || **d == *image.data) {
            Some((_, rel, _)) => rel.clone(),
            None => {
                let n = (1..).find(|n| {
                    !self.taken_ids.contains(&format!("rIdRaven{n}"))
                        && !self.taken_parts.contains(&format!("word/media/raven-image{n}.{ext}"))
                });
                let n = n.unwrap_or(0);
                let (rel, part) = (format!("rIdRaven{n}"), format!("word/media/raven-image{n}.{ext}"));
                self.taken_ids.insert(rel.clone());
                self.taken_parts.insert(part.clone());
                self.added.push((image.data.clone(), rel.clone(), part));
                rel
            }
        };
        let id = self.next_drawing;
        self.next_drawing += 1;
        self.wrote_drawing = true;
        let (cx, cy) = (image.cx.max(1), image.cy.max(1));
        format!(
            r#"<w:r>{props}<w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:docPr id="{id}" name="Picture {id}"/><wp:cNvGraphicFramePr><a:graphicFrameLocks xmlns:a="{A_NS}" noChangeAspect="1"/></wp:cNvGraphicFramePr><a:graphic xmlns:a="{A_NS}"><a:graphicData uri="{PIC_NS}"><pic:pic xmlns:pic="{PIC_NS}"><pic:nvPicPr><pic:cNvPr id="{id}" name="Picture {id}"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="{rel}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
        )
    }
}

const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const PIC_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
const WP_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// The numbers of the drawings (`wp:docPr id="…"`) in `xml`.
fn drawing_ids(xml: &str) -> impl Iterator<Item = u64> + '_ {
    xml.match_indices(":docPr ").filter_map(move |(at, _)| {
        let tag = &xml[at..at + xml[at..].find('>').unwrap_or(0)];
        let v = &tag[tag.find(" id=\"")? + 5..];
        v[..v.find('"')?].parse().ok()
    })
}

/// A drawing with its `docPr` (and the picture's own `cNvPr`) numbered `id`.
fn renumber_drawing(xml: &str, id: u64) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    for name in [":docPr ", ":cNvPr "] {
        let Some(at) = rest.find(name) else { continue };
        let tag_end = at + rest[at..].find('>').unwrap_or(0);
        let Some(v0) = rest[at..tag_end].find(" id=\"").map(|v| at + v + 5) else { continue };
        let Some(v1) = rest[v0..].find('"').map(|e| v0 + e) else { continue };
        out.push_str(&rest[..v0]);
        out.push_str(&id.to_string());
        rest = &rest[v1..];
    }
    out.push_str(rest);
    out
}

/// The document's head with the namespaces our drawings use declared on its
/// root element, where Word looks for them.
fn with_namespaces(head: &str) -> String {
    let mut root = 0;
    while let Some(at) = head[root..].find('<').map(|a| root + a) {
        if !matches!(head.as_bytes().get(at + 1), Some(b'?' | b'!')) {
            root = at;
            break;
        }
        root = at + 1;
    }
    let Some(end) = head[root..].find('>').map(|e| root + e) else { return head.to_string() };
    let tag = &head[root..end];
    let mut extra = String::new();
    for (prefix, uri) in [("wp", WP_NS), ("r", R_NS)] {
        if !tag.contains(&format!("xmlns:{prefix}=")) {
            extra.push_str(&format!(r#" xmlns:{prefix}="{uri}""#));
        }
    }
    format!("{}{extra}{}", &head[..end], &head[end..])
}

fn style_id(style: ParaStyle) -> Option<&'static str> {
    Some(match style {
        ParaStyle::Normal => return None,
        ParaStyle::Title => "Title",
        ParaStyle::Heading(1) => "Heading1",
        ParaStyle::Heading(2) => "Heading2",
        ParaStyle::Heading(3) => "Heading3",
        ParaStyle::Heading(4) => "Heading4",
        ParaStyle::Heading(5) => "Heading5",
        ParaStyle::Heading(_) => "Heading6",
        ParaStyle::ListItem(_) => "ListParagraph",
        ParaStyle::Quote => "Quote",
    })
}

/// The schema's order for a paragraph's and a run's properties. Word rejects
/// a document whose properties are out of order, so ours are slotted in where
/// they belong rather than appended.
const PPR_ORDER: &[&str] = &[
    "pStyle", "keepNext", "keepLines", "pageBreakBefore", "framePr", "widowControl", "numPr",
    "suppressLineNumbers", "pBdr", "shd", "tabs", "suppressAutoHyphens", "kinsoku", "wordWrap",
    "overflowPunct", "topLinePunct", "autoSpaceDE", "autoSpaceDN", "bidi", "adjustRightInd", "snapToGrid",
    "spacing", "ind", "contextualSpacing", "mirrorIndents", "suppressOverlap", "jc", "textDirection",
    "textAlignment", "textboxTightWrap", "outlineLvl", "divId", "cnfStyle", "rPr", "sectPr", "pPrChange",
];
const RPR_ORDER: &[&str] = &[
    "rStyle", "rFonts", "b", "bCs", "i", "iCs", "caps", "smallCaps", "strike", "dstrike", "outline", "shadow",
    "emboss", "imprint", "noProof", "snapToGrid", "vanish", "webHidden", "color", "spacing", "w", "kern",
    "position", "sz", "szCs", "highlight", "u", "effect", "bdr", "shd", "fitText", "vertAlign", "rtl", "cs", "em",
    "lang", "eastAsianLayout", "specVanish", "oMath",
];

fn ordered(mut parts: Vec<(String, String)>, order: &[&str]) -> String {
    parts.sort_by_key(|(name, _)| order.iter().position(|o| o == name).unwrap_or(order.len()));
    parts.into_iter().map(|(_, xml)| xml).collect()
}

fn write_run(run: &Run, direct: &[(&str, String)]) -> String {
    let mut props = children(&run.props);
    let ours = ["b", "bCs", "i", "iCs", "u", "highlight"];
    props.retain(|(n, _)| !ours.contains(&n.as_str()) && !direct.iter().any(|(d, _)| d == n));
    let mut add = |on: bool, name: &str, xml: &str| {
        if on {
            props.push((name.into(), xml.into()));
        }
    };
    let forced = |name: &str| direct.iter().any(|(d, _)| *d == name);
    add(run.bold && !forced("b"), "b", "<w:b/>");
    add(run.italic && !forced("i"), "i", "<w:i/>");
    add(run.underline, "u", r#"<w:u w:val="single"/>"#);
    add(run.highlight, "highlight", r#"<w:highlight w:val="yellow"/>"#);
    props.extend(direct.iter().map(|(n, x)| (n.to_string(), x.clone())));
    let props = ordered(props, RPR_ORDER);

    let mut xml = String::from("<w:r>");
    if !props.is_empty() {
        xml.push_str(&format!("<w:rPr>{props}</w:rPr>"));
    }
    let mut text = String::new();
    let flush = |text: &mut String, xml: &mut String| {
        if !text.is_empty() {
            xml.push_str(&format!(r#"<w:t xml:space="preserve">{}</w:t>"#, escape(text)));
            text.clear();
        }
    };
    for c in run.text.chars() {
        match c {
            '\t' => {
                flush(&mut text, &mut xml);
                xml.push_str("<w:tab/>");
            }
            LINE_BREAK | '\n' => {
                flush(&mut text, &mut xml);
                xml.push_str("<w:br/>");
            }
            c if (c as u32) < 0x20 => {}
            c => text.push(c),
        }
    }
    flush(&mut text, &mut xml);
    xml.push_str("</w:r>");
    xml
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A line break inside a paragraph (`<w:br/>`). U+2028 rather than a newline,
/// so the text view keeps it inside the paragraph it belongs to.
pub const LINE_BREAK: char = '\u{2028}';

fn local(name: &[u8]) -> &[u8] {
    name.rsplit(|&b| b == b':').next().unwrap_or(name)
}

/// The byte range of `<w:body>`'s content, and its children.
fn split_body(xml: &str) -> Result<(Range<usize>, Vec<Item>)> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut body_depth = None;
    let mut body_start = 0;
    let mut child_start = 0;
    let mut items = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader.read_event().context("word/document.xml is not well-formed")?;
        let after = reader.buffer_position() as usize;
        match event {
            Event::Start(e) => {
                depth += 1;
                if body_depth.is_none() && local(e.name().as_ref()) == b"body" {
                    body_depth = Some(depth);
                    body_start = after;
                } else if Some(depth - 1) == body_depth {
                    child_start = before;
                }
            }
            Event::Empty(e) if body_depth == Some(depth) => {
                items.push(item(xml, local(e.name().as_ref()), before..after));
            }
            Event::End(e) => {
                if Some(depth) == body_depth {
                    return Ok((body_start..before, items));
                }
                if body_depth.is_some_and(|b| depth == b + 1) {
                    items.push(item(xml, local(e.name().as_ref()), child_start..after));
                }
                depth -= 1;
            }
            Event::Eof => bail!("word/document.xml has no body"),
            _ => {}
        }
    }
}

fn item(xml: &str, name: &[u8], span: Range<usize>) -> Item {
    let text = &xml[span.clone()];
    let blocks = parse(text);
    let kind = match name {
        b"p" if locked(text) => ItemKind::Locked,
        b"p" => ItemKind::Paragraph,
        b"tbl" => ItemKind::Table,
        _ => ItemKind::Other,
    };
    let blocks = match kind {
        // An empty paragraph is still a line of the document.
        ItemKind::Paragraph | ItemKind::Locked if blocks.is_empty() => {
            vec![Block::paragraph(ParaStyle::Normal, vec![])]
        }
        _ => blocks,
    };
    Item { kind, blocks, span }
}

/// Whether a paragraph holds something we would destroy by rewriting it.
/// A picture set in the text does not count: it is written back as it was.
fn locked(xml: &str) -> bool {
    let mut reader = Reader::from_str(xml);
    let mut parents: Vec<Vec<u8>> = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader.read_event();
        match event {
            Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                let name = local(e.name().as_ref()).to_vec();
                // A page break would come back as a line break.
                if name == b"br" && matches!(attr(e, b"type").as_deref(), Some("page" | "column")) {
                    return true;
                }
                if name == b"drawing" && matches!(event, Ok(Event::Start(_))) && parents.last().is_some_and(|p| p == b"r") {
                    let end = e.name().as_ref().to_vec();
                    if reader.read_to_end(quick_xml::name::QName(&end)).is_err() {
                        return true;
                    }
                    if !is_picture(&xml[before..reader.buffer_position() as usize]) {
                        return true;
                    }
                    continue;
                }
                if matches!(
                    name.as_slice(),
                    b"drawing" | b"pict" | b"object" | b"fldChar" | b"fldSimple" | b"oMath" | b"oMathPara"
                        | b"footnoteReference" | b"endnoteReference" | b"commentReference" | b"ins" | b"del"
                        | b"moveFrom" | b"moveTo" | b"sdt" | b"ruby" | b"sym" | b"AlternateContent"
                ) {
                    return true;
                }
                if matches!(event, Ok(Event::Start(_))) {
                    parents.push(name);
                }
            }
            Ok(Event::End(_)) => {
                parents.pop();
            }
            Ok(Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}

/// Whether a drawing is just a picture from the package — not a chart, a
/// shape, a text box or a picture linked from outside.
fn is_picture(drawing: &str) -> bool {
    drawing.contains(&format!("\"{PIC_NS}\"")) && drawing.contains(":embed=") && !drawing.contains("txbxContent")
}

fn val(e: &BytesStart) -> Option<String> {
    attr(e, b"val")
}

fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// `w:b`, `w:i` are on unless `w:val` says false/0.
fn toggle(e: &BytesStart) -> bool {
    !matches!(val(e).as_deref(), Some("false" | "0" | "none"))
}

fn style_ids(styles: &str) -> HashSet<String> {
    let mut reader = Reader::from_str(styles);
    let mut ids = HashSet::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if local(e.name().as_ref()) == b"style" => {
                if let Some(id) = e.attributes().flatten().find(|a| a.key.local_name().as_ref() == b"styleId") {
                    ids.insert(String::from_utf8_lossy(&id.value).into_owned());
                }
            }
            Ok(Event::Eof) | Err(_) => return ids,
            _ => {}
        }
    }
}

/// The XML inside the first `<…:name>` element, if there is one.
fn inner_of(xml: &str, name: &[u8]) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut start = None;
    let mut depth = 0usize;
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader.read_event().ok()?;
        let after = reader.buffer_position() as usize;
        match event {
            Event::Start(e) => {
                if start.is_none() && local(e.name().as_ref()) == name {
                    start = Some(after);
                    depth = 0;
                } else if start.is_some() {
                    depth += 1;
                }
            }
            Event::Empty(e) if start.is_none() && local(e.name().as_ref()) == name => return Some(String::new()),
            Event::End(_) if start.is_some() => {
                if depth == 0 {
                    return Some(xml[start?..before].to_string());
                }
                depth -= 1;
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// The top-level elements of an XML fragment, by local name.
fn children(xml: &str) -> Vec<(String, String)> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let mut name = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        let Ok(event) = reader.read_event() else { return out };
        let after = reader.buffer_position() as usize;
        match event {
            Event::Start(e) => {
                if depth == 0 {
                    start = before;
                    name = local(e.name().as_ref()).to_vec();
                }
                depth += 1;
            }
            Event::Empty(e) if depth == 0 => {
                out.push((String::from_utf8_lossy(local(e.name().as_ref())).into_owned(), xml[before..after].to_string()));
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push((String::from_utf8_lossy(&name).into_owned(), xml[start..after].to_string()));
                }
            }
            Event::Eof => return out,
            _ => {}
        }
    }
}

pub fn parse(xml: &str) -> Vec<Block> {
    let mut reader = Reader::from_str(xml);
    let mut blocks = Vec::new();

    let mut style = ParaStyle::Normal;
    let mut align = Align::Start;
    let mut runs: Vec<Run> = Vec::new();
    let mut run = Run::default();
    let mut in_text = false;
    let mut list_level: Option<u8> = None;
    // Where the run's properties start, so they can be kept as XML.
    let mut rpr_start: Option<usize> = None;
    // Text boxes and the fallback copies of drawings hold paragraphs of
    // their own; they are not part of this paragraph's text.
    let mut skip = 0usize;
    // A picture being read: where its element starts, at what depth.
    let mut depth = 0usize;
    let mut drawing: Option<Drawing> = None;

    // Tables: collect cell text; nested paragraphs inside a cell become lines.
    let mut table_depth = 0usize;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cell = String::new();

    loop {
        let before = reader.buffer_position() as usize;
        let event = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(e) => e,
        };
        let after = reader.buffer_position() as usize;
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                if !empty {
                    depth += 1;
                }
                // Inside a picture only the picture is of interest.
                if let Some(d) = drawing.as_mut() {
                    d.read(e);
                    continue;
                }
                match local(e.name().as_ref()) {
                    b"txbxContent" | b"Fallback" if !empty => {
                        skip = 1;
                        depth -= 1;
                    }
                    b"drawing" | b"pict" | b"object" => {
                        if empty {
                            runs.push(placeholder("[image]"));
                        } else {
                            drawing = Some(Drawing { start: before, depth, ..Default::default() });
                        }
                    }
                    b"oMath" => {
                        runs.push(placeholder("[equation]"));
                        if !empty {
                            skip = 1;
                            depth -= 1;
                        }
                    }
                    b"p" => {
                        style = ParaStyle::Normal;
                        align = Align::Start;
                        runs.clear();
                        list_level = None;
                    }
                    b"jc" => align = Align::from_jc(&val(e).unwrap_or_default()),
                    b"pStyle" => {
                        let name = val(e).unwrap_or_default().to_ascii_lowercase();
                        style = if name == "title" {
                            ParaStyle::Title
                        } else if let Some(n) = name.strip_prefix("heading") {
                            ParaStyle::Heading(n.trim().parse().unwrap_or(1).clamp(1, 6))
                        } else if name.contains("quote") {
                            ParaStyle::Quote
                        } else if name.contains("list") {
                            ParaStyle::ListItem(0)
                        } else {
                            style
                        };
                    }
                    b"outlineLvl" if style == ParaStyle::Normal => {
                        if let Some(level) = val(e).and_then(|v| v.parse::<u8>().ok()).filter(|l| *l < 9) {
                            style = ParaStyle::Heading((level + 1).min(6));
                        }
                    }
                    b"ilvl" => list_level = Some(val(e).and_then(|v| v.parse().ok()).unwrap_or(0)),
                    b"numPr" => list_level = list_level.or(Some(0)),
                    b"r" => run = Run::default(),
                    b"rPr" if !empty => rpr_start = Some(after),
                    b"b" => run.bold = toggle(e),
                    b"i" => run.italic = toggle(e),
                    b"u" => run.underline = toggle(e),
                    b"highlight" => run.highlight = val(e).is_some_and(|v| v != "none"),
                    b"t" if !empty => in_text = true,
                    b"tab" => run.text.push('\t'),
                    b"br" if matches!(attr(e, b"type").as_deref(), Some("page" | "column")) => {
                        runs.push(Run::page_break())
                    }
                    b"br" | b"cr" => run.text.push(LINE_BREAK),
                    b"tbl" => {
                        table_depth += 1;
                        if table_depth == 1 {
                            rows.clear();
                        }
                    }
                    b"tr" if table_depth == 1 => rows.push(Vec::new()),
                    b"tc" if table_depth == 1 => cell.clear(),
                    _ => {}
                }
            }
            Event::Text(t) if in_text => {
                if let Ok(text) = t.unescape() {
                    run.text.push_str(&text);
                }
            }
            Event::End(_) if drawing.is_some() => {
                if drawing.as_ref().is_some_and(|d| d.depth == depth)
                    && let Some(d) = drawing.take()
                {
                    let start = d.start;
                    runs.push(d.finish(&xml[start..after], &run.props));
                }
                depth = depth.saturating_sub(1);
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                match local(e.name().as_ref()) {
                    b"t" => in_text = false,
                    b"rPr" => {
                        // Paragraph-mark properties (pPr/rPr) are dropped with the
                        // run they are never attached to.
                        if let Some(start) = rpr_start.take() {
                            run.props = xml[start..before].to_string();
                        }
                    }
                    b"r" => {
                        if !run.text.is_empty() {
                            runs.push(std::mem::take(&mut run));
                        }
                    }
                    b"p" => {
                        if table_depth > 0 {
                            let line: String =
                                runs.iter().map(|r| r.text.replace(LINE_BREAK, "\n")).collect::<Vec<_>>().concat();
                            if !cell.is_empty() {
                                cell.push('\n');
                            }
                            cell.push_str(&line);
                        } else {
                            if let (Some(level), ParaStyle::Normal | ParaStyle::ListItem(_)) = (list_level, style) {
                                style = ParaStyle::ListItem(level);
                            }
                            blocks.push(Block::Paragraph { style, runs: std::mem::take(&mut runs), align });
                        }
                    }
                    b"tc" if table_depth == 1 => {
                        if let Some(row) = rows.last_mut() {
                            row.push(std::mem::take(&mut cell));
                        }
                    }
                    b"tbl" => {
                        table_depth = table_depth.saturating_sub(1);
                        if table_depth == 0 {
                            blocks.push(Block::Table { rows: std::mem::take(&mut rows) });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    blocks
}

fn placeholder(text: &str) -> Run {
    Run { text: text.into(), placeholder: true, ..Default::default() }
}

/// What a `<w:drawing>`, `<w:pict>` or `<w:object>` being read holds.
#[derive(Default)]
struct Drawing {
    start: usize,
    depth: usize,
    rel: Option<String>,
    size: Option<(i64, i64)>,
    /// A chart, shape or text box rather than a picture.
    other: bool,
}

impl Drawing {
    fn read(&mut self, e: &BytesStart) {
        match local(e.name().as_ref()) {
            b"extent" if self.size.is_none() => {
                let n = |k: &[u8]| attr(e, k).and_then(|v| v.parse::<i64>().ok());
                if let (Some(cx), Some(cy)) = (n(b"cx"), n(b"cy")) {
                    self.size = Some((cx, cy));
                }
            }
            b"blip" => self.rel = attr(e, b"embed").or_else(|| self.rel.take()),
            // VML: the picture's part, and its size in the shape's style.
            b"imagedata" => self.rel = attr(e, b"id").or_else(|| self.rel.take()),
            b"shape" if self.size.is_none() => {
                let style = attr(e, b"style").unwrap_or_default();
                let get = |key: &str| {
                    style.split(';').find_map(|p| {
                        let (k, v) = p.split_once(':')?;
                        (k.trim() == key).then(|| css_length_emu(v.trim())).flatten()
                    })
                };
                if let (Some(w), Some(h)) = (get("width"), get("height")) {
                    self.size = Some((w, h));
                }
            }
            b"chart" | b"wsp" | b"wgp" | b"relIds" | b"txbx" => self.other = true,
            _ => {}
        }
    }

    fn finish(self, xml: &str, props: &str) -> Run {
        match (self.rel, self.other) {
            (Some(rel), false) => {
                let (cx, cy) = self.size.unwrap_or((0, 0));
                let origin = Origin { doc: 0, xml: xml.to_string(), rel: Some(rel) };
                Run { props: props.to_string(), ..Run::picture(Image { data: Arc::default(), cx, cy, origin: Some(Arc::new(origin)) }) }
            }
            _ if xml.contains("/chart\"") => placeholder("[chart]"),
            _ => placeholder("[image]"),
        }
    }
}

/// A CSS length (`72pt`, `1in`, `2.5cm`, `96px`) in EMU.
fn css_length_emu(v: &str) -> Option<i64> {
    let split = v.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(v.len());
    let n: f64 = v[..split].trim().parse().ok()?;
    let per = match &v[split..] {
        "pt" | "" => EMU_PER_POINT as f64,
        "in" => EMU_PER_INCH as f64,
        "cm" => 360_000.0,
        "mm" => 36_000.0,
        "px" => EMU_PER_INCH as f64 / 96.0,
        _ => return None,
    };
    Some((n * per) as i64)
}

/// Merge neighbouring runs formatted alike and drop empty ones, so two
/// renderings of the same paragraph compare equal however their runs were cut.
pub fn normalize(runs: &[Run]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for run in runs.iter().filter(|r| !r.text.is_empty()) {
        match out.last_mut() {
            Some(last) if last.same_format(run) => last.text.push_str(&run.text),
            _ => out.push(Run { props: String::new(), ..run.clone() }),
        }
    }
    out
}

/// A copy of `package` with some parts replaced or added, and some removed.
/// Parts not mentioned are copied as they are, still compressed.
fn repackage<S: AsRef<str>>(package: &[u8], replace: &[(S, Vec<u8>)], remove: &[&str]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(package)).context("not a DOCX (zip) file")?;
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for i in 0..zip.len() {
        let file = zip.by_index_raw(i)?;
        let name = file.name().to_string();
        if replace.iter().any(|(n, _)| n.as_ref() == name) || remove.contains(&name.as_str()) {
            continue;
        }
        out.raw_copy_file(file)?;
    }
    for (name, bytes) in replace {
        out.start_file(name.as_ref(), options)?;
        out.write_all(bytes)?;
    }
    Ok(out.finish()?.into_inner())
}

/// The value of a run property (`<w:sz w:val="24"/>` → `24`), or an empty
/// string for one that is just on (`<w:strike/>`); `None` if absent or
/// switched off.
pub fn run_prop(props: &str, name: &str) -> Option<String> {
    let at = props.find(&format!("<w:{name}"))?;
    let rest = &props[at + 3 + name.len()..];
    if !matches!(rest.chars().next(), Some(' ' | '/' | '>')) {
        return None;
    }
    let tag = &rest[..rest.find('>').unwrap_or(rest.len())];
    match tag.find("w:val=\"") {
        Some(v) => {
            let v = &tag[v + 7..];
            let v = v[..v.find('"').unwrap_or(v.len())].to_string();
            (!matches!(v.as_str(), "false" | "0" | "none" | "auto")).then_some(v)
        }
        None => Some(String::new()),
    }
}

pub fn run_font(props: &str) -> Option<String> {
    let at = props.find("<w:rFonts")?;
    let tag = &props[at..at + props[at..].find('>').unwrap_or(0)];
    let v = &tag[tag.find("w:ascii=\"").or_else(|| tag.find("w:hAnsi=\""))? + 9..];
    Some(v[..v.find('"')?].to_string())
}

pub fn hex_color(v: &str) -> Option<[u16; 3]> {
    if v.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(v, 16).ok()?;
    let c = |shift: u32| ((n >> shift) & 0xFF) as u16 * 257;
    Some([c(16), c(8), c(0)])
}

// ── New documents ────────────────────────────────────────────────────────

/// The paper of a new document and its margins, in twips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paper {
    pub width: u32,
    pub height: u32,
    /// Top, right, bottom, left.
    pub margins: [u32; 4],
}

impl Paper {
    pub const LETTER: Paper = Paper { width: 12240, height: 15840, margins: [1440; 4] };
    pub const A4: Paper = Paper { width: 11906, height: 16838, margins: [1440; 4] };
}

/// How the text of a new document is set.
#[derive(Debug, Clone)]
pub struct TextDefaults {
    pub font: String,
    /// In half-points.
    pub size: u32,
    /// Space after a paragraph, in twips.
    pub after: u32,
}

impl TextDefaults {
    pub fn document() -> Self {
        TextDefaults { font: "Calibri".into(), size: 22, after: 160 }
    }

    /// For plain text: a typewriter face, one line after another.
    pub fn plain_text() -> Self {
        TextDefaults { font: "Courier New".into(), size: 20, after: 0 }
    }
}

const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// A complete, empty document: the parts Word itself writes for one, with
/// the styles our formatting bar names and a bulleted list to use.
pub fn blank(paper: Paper, text: &TextDefaults) -> Vec<u8> {
    let (w, h) = (paper.width, paper.height);
    let [top, right, bottom, left] = paper.margins;
    let ns = format!(r#"xmlns:w="{W_NS}" xmlns:r="{R_NS}" xmlns:wp="{WP_NS}""#);
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {ns}><w:body><w:p/><w:sectPr><w:pgSz w:w="{w}" w:h="{h}"/><w:pgMar w:top="{top}" w:right="{right}" w:bottom="{bottom}" w:left="{left}" w:header="720" w:footer="720" w:gutter="0"/><w:cols w:space="720"/></w:sectPr></w:body></w:document>"#
    );
    let font = attr_escape(&text.font);
    let (size, after) = (text.size, text.after);
    let line = if after == 0 { 240 } else { 259 };
    let heading = |id: &str, name: &str, level: u32, size: u32, before: u32| {
        format!(
            r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="{name}"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="9"/><w:qFormat/><w:pPr><w:keepNext/><w:keepLines/><w:spacing w:before="{before}" w:after="80"/><w:outlineLvl w:val="{level}"/></w:pPr><w:rPr><w:b/><w:color w:val="1F3864"/><w:sz w:val="{size}"/><w:szCs w:val="{size}"/></w:rPr></w:style>"#
        )
    };
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="{W_NS}"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="{font}" w:hAnsi="{font}" w:eastAsia="{font}" w:cs="{font}"/><w:sz w:val="{size}"/><w:szCs w:val="{size}"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="{after}" w:line="{line}" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style>{}{}{}{}<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="10"/><w:qFormat/><w:pPr><w:spacing w:after="160" w:line="240" w:lineRule="auto"/><w:contextualSpacing/></w:pPr><w:rPr><w:spacing w:val="-10"/><w:kern w:val="28"/><w:sz w:val="56"/><w:szCs w:val="56"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="29"/><w:qFormat/><w:pPr><w:spacing w:before="160"/><w:ind w:left="864" w:right="864"/></w:pPr><w:rPr><w:i/><w:iCs/><w:color w:val="404040"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:uiPriority w:val="34"/><w:qFormat/><w:pPr><w:ind w:left="720"/><w:contextualSpacing/></w:pPr></w:style><w:style w:type="character" w:default="1" w:styleId="DefaultParagraphFont"><w:name w:val="Default Paragraph Font"/><w:uiPriority w:val="1"/><w:semiHidden/><w:unhideWhenUsed/></w:style><w:style w:type="table" w:default="1" w:styleId="TableNormal"><w:name w:val="Normal Table"/><w:uiPriority w:val="99"/><w:semiHidden/><w:unhideWhenUsed/><w:tblPr><w:tblInd w:w="0" w:type="dxa"/><w:tblCellMar><w:top w:w="0" w:type="dxa"/><w:left w:w="108" w:type="dxa"/><w:bottom w:w="0" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar></w:tblPr></w:style></w:styles>"#,
        heading("Heading1", "heading 1", 0, 32, 360),
        heading("Heading2", "heading 2", 1, 26, 160),
        heading("Heading3", "heading 3", 2, 24, 160),
        heading("Heading4", "heading 4", 3, 22, 80),
    );
    let mut levels = String::new();
    for (i, bullet) in ["•", "◦", "▪"].iter().cycle().take(9).enumerate() {
        let indent = 720 * (i + 1);
        levels.push_str(&format!(
            r#"<w:lvl w:ilvl="{i}"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="{bullet}"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="{indent}" w:hanging="360"/></w:pPr></w:lvl>"#
        ));
    }
    let numbering = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:numbering xmlns:w="{W_NS}"><w:abstractNum w:abstractNumId="0"><w:multiLevelType w:val="hybridMultilevel"/>{levels}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
    );
    let settings = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:settings xmlns:w="{W_NS}"><w:defaultTabStop w:val="720"/><w:characterSpacingControl w:val="doNotCompress"/><w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/></w:compat></w:settings>"#
    );
    let wml = "application/vnd.openxmlformats-officedocument.wordprocessingml";
    let parts: [(&str, String); 9] = [
        ("[Content_Types].xml", format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="jpeg" ContentType="image/jpeg"/><Override PartName="/word/document.xml" ContentType="{wml}.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="{wml}.styles+xml"/><Override PartName="/word/numbering.xml" ContentType="{wml}.numbering+xml"/><Override PartName="/word/settings.xml" ContentType="{wml}.settings+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/><Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/></Types>"#
        )),
        ("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>"#.into()),
        ("word/_rels/document.xml.rels", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/></Relationships>"#.into()),
        ("word/document.xml", document),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
        ("word/settings.xml", settings),
        ("docProps/core.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><dc:title></dc:title></cp:coreProperties>"#.into()),
        ("docProps/app.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Raven Viewer</Application></Properties>"#.into()),
    ];
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, xml) in parts {
        // Writing to memory cannot fail.
        let _ = out.start_file(name, options);
        let _ = out.write_all(xml.as_bytes());
    }
    out.finish().map(|c| c.into_inner()).unwrap_or_default()
}

/// A document holding `blocks`, on a blank package.
pub fn build(blocks: &[Block], paper: Paper, text: &TextDefaults) -> Result<Vec<u8>> {
    let doc = load(&blank(paper, text))?;
    // The blank body's own empty paragraph gives way to the content.
    let mut out: Vec<Out> = blocks.iter().cloned().map(Out::Block).collect();
    if out.is_empty() {
        out.push(Out::Keep(0));
    }
    doc.save(&out)
}

/// Plain text as paragraphs, one per line.
pub fn from_text(text: &str) -> Vec<Block> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let runs = if line.is_empty() { vec![] } else { vec![Run { text: line.into(), ..Default::default() }] };
            Block::paragraph(ParaStyle::Normal, runs)
        })
        .collect()
}

/// Bytes of a text file as text, whatever it was saved as: UTF-8 or UTF-16
/// with a byte order mark, UTF-8, or else Windows-1252.
pub fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, false);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    }
}

/// A Windows-1252 byte as the character it stands for.
pub fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    if (0x80..0xA0).contains(&b) { HIGH[(b - 0x80) as usize] } else { b as char }
}

/// The text of `blocks`, a line per paragraph: bullets as bullets, table
/// cells separated by tabs, pictures and placeholders left out.
pub fn text_of(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            Block::Paragraph { style, runs, .. } => {
                if let ParaStyle::ListItem(level) = style {
                    out.push_str(&"    ".repeat(*level as usize));
                    out.push_str("• ");
                }
                for run in runs.iter().filter(|r| !r.placeholder && r.image.is_none()) {
                    out.push_str(&run.text.replace(LINE_BREAK, "\n"));
                }
                out.push('\n');
            }
            Block::Table { rows } => {
                for row in rows {
                    let cells: Vec<String> = row.iter().map(|c| c.replace('\n', " ")).collect();
                    out.push_str(&cells.join("\t"));
                    out.push('\n');
                }
            }
        }
    }
    out
}

// ── Combining ────────────────────────────────────────────────────────────

/// A relationship of one part to another (an image, a hyperlink…).
struct Rel {
    id: String,
    kind: String,
    target: String,
    external: bool,
}

fn rels_of(xml: &str) -> Vec<Rel> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if local(e.name().as_ref()) == b"Relationship" => {
                let attr = |k: &[u8]| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == k)
                        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
                        .unwrap_or_default()
                };
                out.push(Rel {
                    id: attr(b"Id"),
                    kind: attr(b"Type"),
                    target: attr(b"Target"),
                    external: attr(b"TargetMode") == "External",
                });
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

fn rels_xml(rels: &[Rel]) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for r in rels {
        let mode = if r.external { r#" TargetMode="External""# } else { "" };
        xml.push_str(&format!(
            r#"<Relationship Id="{}" Type="{}" Target="{}"{mode}/>"#,
            attr_escape(&r.id),
            attr_escape(&r.kind),
            attr_escape(&r.target)
        ));
    }
    xml.push_str("</Relationships>");
    xml
}

fn attr_escape(s: &str) -> String {
    escape(s).replace('"', "&quot;")
}

/// Resolve a relationship target relative to the part that holds it.
fn resolve(from_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = from_dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn rels_path(part: &str) -> String {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") }
}

/// Content types: defaults by extension, overrides by part.
struct ContentTypes {
    defaults: BTreeMap<String, String>,
    overrides: BTreeMap<String, String>,
}

impl ContentTypes {
    fn parse(xml: &str) -> Self {
        let mut reader = Reader::from_str(xml);
        let mut ct = ContentTypes { defaults: BTreeMap::new(), overrides: BTreeMap::new() };
        loop {
            match reader.read_event() {
                Ok(Event::Start(e) | Event::Empty(e)) => {
                    let attr = |k: &[u8]| {
                        e.attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == k)
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
                    };
                    match local(e.name().as_ref()) {
                        b"Default" => {
                            if let (Some(ext), Some(ty)) = (attr(b"Extension"), attr(b"ContentType")) {
                                ct.defaults.insert(ext.to_ascii_lowercase(), ty);
                            }
                        }
                        b"Override" => {
                            if let (Some(part), Some(ty)) = (attr(b"PartName"), attr(b"ContentType")) {
                                ct.overrides.insert(part.trim_start_matches('/').to_string(), ty);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) | Err(_) => return ct,
                _ => {}
            }
        }
    }

    fn of(&self, part: &str) -> Option<&String> {
        self.overrides.get(part).or_else(|| {
            let ext = part.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
            self.defaults.get(&ext)
        })
    }

    fn xml(&self) -> String {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
        );
        for (ext, ty) in &self.defaults {
            xml.push_str(&format!(r#"<Default Extension="{}" ContentType="{}"/>"#, attr_escape(ext), attr_escape(ty)));
        }
        for (part, ty) in &self.overrides {
            xml.push_str(&format!(r#"<Override PartName="/{}" ContentType="{}"/>"#, attr_escape(part), attr_escape(ty)));
        }
        xml.push_str("</Types>");
        xml
    }
}

/// Parts being combined into the first document's package.
struct Package {
    parts: BTreeMap<String, Vec<u8>>,
}

impl Package {
    fn read(bytes: &[u8]) -> Result<Self> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("not a DOCX (zip) file")?;
        let mut parts = BTreeMap::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i)?;
            if f.is_dir() {
                continue;
            }
            let mut data = Vec::new();
            f.read_to_end(&mut data)?;
            parts.insert(f.name().to_string(), data);
        }
        Ok(Self { parts })
    }

    fn text(&self, name: &str) -> Option<String> {
        self.parts.get(name).map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

/// Elements by their numeric id, as XML.
type ById = Vec<(u32, String)>;

/// Numbering definitions (`abstractNum`, `num`) of a numbering part.
fn numbering_defs(xml: &str) -> (ById, ById) {
    let mut abstracts = Vec::new();
    let mut nums = Vec::new();
    for (name, x) in children(inner_of(xml, b"numbering").as_deref().unwrap_or_default()) {
        let id_attr = if name == "abstractNum" { "abstractNumId" } else { "numId" };
        let id = attr_value(&x, id_attr).and_then(|v| v.parse().ok());
        match (name.as_str(), id) {
            ("abstractNum", Some(id)) => abstracts.push((id, x)),
            ("num", Some(id)) => nums.push((id, x)),
            _ => {}
        }
    }
    (abstracts, nums)
}

/// The value of attribute `name` (any prefix) on the first element of `xml`.
fn attr_value(xml: &str, name: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                return e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.local_name().as_ref() == name.as_bytes())
                    .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Rewrite every `w:<attr>="n"` on elements named `element` through `map`.
fn renumber(xml: &str, element: &str, attr: &str, map: &HashMap<u32, u32>) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    let open = format!(":{element} ");
    let key = format!(":{attr}=\"");
    while let Some(at) = rest.find(&open) {
        let tag_end = rest[at..].find('>').map_or(rest.len(), |e| at + e);
        let (head, tag) = (&rest[..at], &rest[at..tag_end]);
        out.push_str(head);
        match tag.find(&key) {
            Some(k) => {
                let v0 = k + key.len();
                let v1 = tag[v0..].find('"').map_or(tag.len(), |e| v0 + e);
                let new = tag[v0..v1].parse::<u32>().ok().and_then(|n| map.get(&n)).map(u32::to_string);
                out.push_str(&tag[..v0]);
                out.push_str(new.as_deref().unwrap_or(&tag[v0..v1]));
                out.push_str(&tag[v1..]);
            }
            None => out.push_str(tag),
        }
        rest = &rest[tag_end..];
    }
    out.push_str(rest);
    out
}

/// Every attribute in the relationships namespace (`r:id`, `r:embed`,
/// `r:link`, …) mapped through `map`.
fn rewrite_rel_ids(xml: &str, prefix: &str, map: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    let needle = format!(" {prefix}:");
    while let Some(at) = rest.find(&needle) {
        let eq = rest[at..].find("=\"").map(|e| at + e + 2);
        let Some(v0) = eq.filter(|&e| rest[at + needle.len()..e - 2].chars().all(|c| c.is_ascii_alphanumeric())) else {
            out.push_str(&rest[..at + needle.len()]);
            rest = &rest[at + needle.len()..];
            continue;
        };
        let v1 = rest[v0..].find('"').map_or(rest.len(), |e| v0 + e);
        out.push_str(&rest[..v0]);
        let id = &rest[v0..v1];
        out.push_str(map.get(id).map_or(id, String::as_str));
        rest = &rest[v1..];
    }
    out.push_str(rest);
    out
}

/// The prefix a document binds to the relationships namespace (almost always
/// `r`).
fn rel_prefix(xml: &str) -> String {
    let ns = "=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"";
    xml.find(ns)
        .and_then(|at| xml[..at].rfind("xmlns:").map(|s| xml[s + 6..at].to_string()))
        .unwrap_or_else(|| "r".into())
}

/// The body of each file after the first, appended to the first, each
/// starting on a new page. What the body refers to comes along: images and
/// other parts (renamed so nothing collides), hyperlinks, list definitions
/// (renumbered) and styles the first document does not already define.
/// Notes, comments and each file's own headers are left behind — they belong
/// to the section and file they came from.
pub fn merge(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let Some((_, first)) = files.first() else { bail!("nothing to combine") };
    let mut pkg = Package::read(first)?;
    let base = load(first)?;
    let mut ct = ContentTypes::parse(&pkg.text("[Content_Types].xml").context("the first file has no content types")?);
    let doc_rels_path = rels_path("word/document.xml");
    let mut doc_rels = pkg.text(&doc_rels_path).map(|x| rels_of(&x)).unwrap_or_default();
    let mut styles = pkg.text("word/styles.xml");
    let mut numbering = pkg.text("word/numbering.xml");
    let prefix = rel_prefix(&base.xml);

    let mut appended = String::new();
    for (n, (name, bytes)) in files.iter().enumerate().skip(1) {
        let other = load(bytes).with_context(|| format!("{name} is not a readable DOCX"))?;
        let theirs = Package::read(bytes)?;
        let their_ct = ContentTypes::parse(&theirs.text("[Content_Types].xml").unwrap_or_default());
        let their_prefix = rel_prefix(&other.xml);

        // The body's paragraphs and tables — not its section properties,
        // and without references to notes and comments left behind.
        let mut body = String::new();
        for item in &other.items {
            let xml = &other.xml[item.span.clone()];
            if item.kind == ItemKind::Other && xml.trim_start().starts_with(&format!("<{}", "w:sectPr")) {
                continue;
            }
            body.push_str(&strip_elements(xml, &[
                "footnoteReference", "endnoteReference", "commentReference", "commentRangeStart", "commentRangeEnd",
            ]));
        }

        // Relationships: parts are copied under a fresh name; external
        // links are just re-declared.
        let their_rels = theirs.text(&doc_rels_path).map(|x| rels_of(&x)).unwrap_or_default();
        let mut id_map = HashMap::new();
        let mut copied: HashMap<String, String> = HashMap::new();
        for rel in &their_rels {
            if !body.contains(&format!("\"{}\"", rel.id)) {
                continue;
            }
            let new_id = format!("rMerged{n}_{}", rel.id);
            let target = if rel.external {
                rel.target.clone()
            } else {
                let part = resolve("word", &rel.target);
                let new_part = copy_part(&theirs, &their_ct, &part, n, &mut pkg, &mut ct, &mut copied);
                new_part.strip_prefix("word/").map_or(format!("/{new_part}"), str::to_string)
            };
            doc_rels.push(Rel { id: new_id.clone(), kind: rel.kind.clone(), target, external: rel.external });
            id_map.insert(rel.id.clone(), new_id);
        }
        let mut body = rewrite_rel_ids(&body, &their_prefix, &id_map);
        if their_prefix != prefix {
            body = body.replace(&format!(" {their_prefix}:"), &format!(" {prefix}:"));
        }

        // Lists: their definitions are appended with ids past ours.
        if let Some(their_numbering) = theirs.text("word/numbering.xml") {
            let (abs, nums) = numbering_defs(&their_numbering);
            let ours = numbering.clone().unwrap_or_else(|| {
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"></w:numbering>"#.into()
            });
            let (our_abs, our_nums) = numbering_defs(&ours);
            let abs_base = our_abs.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
            let num_base = our_nums.iter().map(|(i, _)| i + 1).max().unwrap_or(1);
            let abs_map: HashMap<u32, u32> = abs.iter().map(|(i, _)| (*i, i + abs_base)).collect();
            let num_map: HashMap<u32, u32> = nums.iter().map(|(i, _)| (*i, i + num_base)).collect();
            // abstractNums must all come before the nums.
            let new_abs: String = abs.iter().map(|(_, x)| renumber(x, "abstractNum", "abstractNumId", &abs_map)).collect();
            let new_nums: String = nums
                .iter()
                .map(|(_, x)| renumber(&renumber(x, "num", "numId", &num_map), "abstractNumId", "val", &abs_map))
                .collect();
            let mut merged = ours;
            let first_num = merged.find("<w:num ").or_else(|| merged.find("<w:num>"));
            let insert_abs = first_num.or_else(|| merged.rfind("</w:numbering>")).unwrap_or(merged.len());
            merged.insert_str(insert_abs, &new_abs);
            let end = merged.rfind("</w:numbering>").unwrap_or(merged.len());
            merged.insert_str(end, &new_nums);
            if numbering.is_none() {
                ct.overrides.insert(
                    "word/numbering.xml".into(),
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml".into(),
                );
                doc_rels.push(Rel {
                    id: "rMergedNumbering".into(),
                    kind: "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering".into(),
                    target: "numbering.xml".into(),
                    external: false,
                });
            }
            numbering = Some(merged);
            body = renumber(&body, "numId", "val", &num_map);
        } else {
            body = strip_elements(&body, &["numPr"]);
        }

        // Styles: theirs that we lack, by id.
        if let (Some(ours), Some(their_styles)) = (styles.as_mut(), theirs.text("word/styles.xml")) {
            let have = style_ids(ours);
            let extra: String = children(inner_of(&their_styles, b"styles").as_deref().unwrap_or_default())
                .into_iter()
                .filter(|(name, x)| {
                    name == "style" && attr_value(x, "styleId").is_some_and(|id| !have.contains(&id))
                })
                .map(|(_, x)| x)
                .collect();
            if let Some(end) = ours.rfind("</w:styles>") {
                ours.insert_str(end, &extra);
            }
        }

        appended.push_str(r#"<w:p><w:r><w:br w:type="page"/></w:r></w:p>"#);
        appended.push_str(&body);
    }

    // Before the first document's own section properties, which end its body.
    let items_end = base
        .items
        .iter()
        .rev()
        .find(|i| i.kind != ItemKind::Other || !base.xml[i.span.clone()].trim_start().starts_with("<w:sectPr"))
        .map_or(base.body.start, |i| i.span.end);
    let mut xml = base.xml.clone();
    xml.insert_str(items_end, &appended);

    let mut replace: Vec<(&str, Vec<u8>)> = vec![
        ("word/document.xml", xml.into_bytes()),
        ("[Content_Types].xml", ct.xml().into_bytes()),
        (doc_rels_path.as_str(), rels_xml(&doc_rels).into_bytes()),
    ];
    if let Some(s) = styles {
        replace.push(("word/styles.xml", s.into_bytes()));
    }
    if let Some(n) = numbering {
        replace.push(("word/numbering.xml", n.into_bytes()));
    }
    let added: Vec<(String, Vec<u8>)> =
        pkg.parts.iter().filter(|(k, _)| !first_has(first, k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in &added {
        replace.push((k.as_str(), v.clone()));
    }
    repackage(first, &replace, &[])
}

fn first_has(package: &[u8], name: &str) -> bool {
    zip::ZipArchive::new(std::io::Cursor::new(package)).is_ok_and(|mut z| z.by_name(name).is_ok())
}

/// Copy a part (and whatever it refers to in turn) from `theirs` into
/// `pkg` under a name that cannot collide, returning the new name.
fn copy_part(
    theirs: &Package,
    their_ct: &ContentTypes,
    part: &str,
    n: usize,
    pkg: &mut Package,
    ct: &mut ContentTypes,
    copied: &mut HashMap<String, String>,
) -> String {
    if let Some(done) = copied.get(part) {
        return done.clone();
    }
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let new_part = if dir.is_empty() { format!("merged{n}_{file}") } else { format!("{dir}/merged{n}_{file}") };
    copied.insert(part.to_string(), new_part.clone());
    let Some(data) = theirs.parts.get(part) else { return new_part };
    pkg.parts.insert(new_part.clone(), data.clone());
    if let Some(ty) = their_ct.of(part) {
        let ext = new_part.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        if ct.defaults.get(&ext) != Some(ty) {
            ct.overrides.insert(new_part.clone(), ty.clone());
        }
    }
    if let Some(rels) = theirs.text(&rels_path(part)) {
        let mut rels = rels_of(&rels);
        for rel in rels.iter_mut().filter(|r| !r.external) {
            let target = resolve(dir, &rel.target);
            let new_target = copy_part(theirs, their_ct, &target, n, pkg, ct, copied);
            rel.target = format!("/{new_target}");
        }
        pkg.parts.insert(rels_path(&new_part), rels_xml(&rels).into_bytes());
    }
    new_part
}

/// `xml` without any elements (and their content) of the given local names.
fn strip_elements(xml: &str, names: &[&str]) -> String {
    let mut reader = Reader::from_str(xml);
    let mut out = String::with_capacity(xml.len());
    let mut copied_to = 0;
    let mut skipping: Option<(usize, usize)> = None; // (start, depth)
    let mut depth = 0usize;
    loop {
        let before = reader.buffer_position() as usize;
        let Ok(event) = reader.read_event() else { break };
        let after = reader.buffer_position() as usize;
        match event {
            Event::Start(e) => {
                depth += 1;
                if skipping.is_none() && names.iter().any(|n| local(e.name().as_ref()) == n.as_bytes()) {
                    skipping = Some((before, depth));
                }
            }
            Event::Empty(e) if skipping.is_none() && names.iter().any(|n| local(e.name().as_ref()) == n.as_bytes()) => {
                out.push_str(&xml[copied_to..before]);
                copied_to = after;
            }
            Event::End(_) => {
                if let Some((start, d)) = skipping
                    && d == depth
                {
                    out.push_str(&xml[copied_to..start]);
                    copied_to = after;
                    skipping = None;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out.push_str(&xml[copied_to..]);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const DOC: &str = r#"<w:document xmlns:w="w"><w:body>
      <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Intro</w:t></w:r></w:p>
      <w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Bold</w:t></w:r><w:r><w:t xml:space="preserve"> plain &amp; simple</w:t></w:r></w:p>
      <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr><w:r><w:t>item</w:t></w:r></w:p>
      <w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
    </w:body></w:document>"#;

    #[test]
    fn parses_headings_runs_lists_tables() {
        let blocks = parse(DOC);
        assert_eq!(blocks.len(), 4);
        assert!(matches!(&blocks[0], Block::Paragraph { style: ParaStyle::Heading(1), runs, .. } if runs[0].text == "Intro"));
        let Block::Paragraph { runs, .. } = &blocks[1] else { panic!() };
        assert!(runs[0].bold && !runs[1].bold);
        assert_eq!(runs[1].text, " plain & simple");
        assert!(matches!(&blocks[2], Block::Paragraph { style: ParaStyle::ListItem(1), .. }));
        assert_eq!(blocks[3], Block::Table { rows: vec![vec!["a".into(), "b".into()]] });
    }

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

    /// A minimal but complete package: what Word itself would accept.
    pub(crate) fn package(body: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let document = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {NS}><w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr></w:body></w:document>"#
        );
        let mut parts: Vec<(String, String)> = vec![
            ("[Content_Types].xml".into(), r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.into()),
            ("_rels/.rels".into(), r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.into()),
            ("word/document.xml".into(), document),
            ("word/styles.xml".into(), format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles {NS}><w:style w:type="paragraph" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:rPr><w:b/><w:sz w:val="32"/></w:rPr></w:style></w:styles>"#)),
        ];
        for (k, v) in extra {
            parts.retain(|(n, _)| n != k);
            parts.push((k.to_string(), v.to_string()));
        }
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (k, v) in parts {
            out.start_file(k, o).unwrap();
            out.write_all(v.as_bytes()).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    fn document_xml(package: &[u8]) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(package)).unwrap();
        read_part(&mut zip, "word/document.xml").unwrap().unwrap()
    }

    /// Keeping everything writes the very same body back.
    #[test]
    fn saving_unchanged_keeps_every_byte() {
        let body = r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:rPr><w:rFonts w:ascii="Georgia"/></w:rPr><w:t>Hi</w:t></w:r></w:p><w:bookmarkStart w:id="0" w:name="x"/><w:p><w:r><w:drawing><wp:inline xmlns:wp="wp"/></w:drawing></w:r></w:p><w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl>"#;
        let pkg = package(body, &[]);
        let doc = load(&pkg).unwrap();
        let kinds: Vec<_> = doc.items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [ItemKind::Paragraph, ItemKind::Other, ItemKind::Locked, ItemKind::Table, ItemKind::Other]);
        let keep: Vec<Out> = (0..doc.items.len()).filter(|&i| doc.items[i].visible()).map(Out::Keep).collect();
        let saved = doc.save(&keep).unwrap();
        assert_eq!(document_xml(&saved), document_xml(&pkg));
    }

    /// An edited paragraph keeps its alignment and its font; what was not
    /// touched stays exactly as it was; new paragraphs slot in before the
    /// section properties.
    #[test]
    fn an_edit_rewrites_only_that_paragraph() {
        let body = r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:rPr><w:rFonts w:ascii="Georgia"/><w:sz w:val="28"/></w:rPr><w:t>Hello</w:t></w:r></w:p><w:p><w:r><w:t>Untouched &amp; kept</w:t></w:r></w:p>"#;
        let pkg = package(body, &[]);
        let doc = load(&pkg).unwrap();
        let Block::Paragraph { runs, .. } = &doc.items[0].blocks[0] else { panic!() };
        let mut edited = runs.clone();
        edited[0].text = "Hello there".into();
        edited.push(Run { text: " <bold>".into(), bold: true, props: runs[0].props.clone(), ..Default::default() });
        let out = vec![
            Out::Para { style: ParaStyle::Normal, runs: edited, base: Some(0) },
            Out::Keep(1),
            Out::Para { style: ParaStyle::Heading(1), runs: vec![Run { text: "New".into(), ..Default::default() }], base: None },
        ];
        let xml = document_xml(&doc.save(&out).unwrap());
        assert!(xml.contains(r#"<w:pPr><w:jc w:val="center"/></w:pPr>"#), "{xml}");
        assert!(xml.contains(r#"<w:rFonts w:ascii="Georgia"/><w:b/><w:sz w:val="28"/>"#), "bold slots in before the size: {xml}");
        assert!(xml.contains("&lt;bold&gt;"));
        assert!(xml.contains(r#"<w:p><w:r><w:t>Untouched &amp; kept</w:t></w:r></w:p>"#));
        let heading = xml.find(r#"<w:pStyle w:val="Heading1"/>"#).unwrap();
        assert!(heading < xml.find("<w:sectPr").unwrap(), "the section properties must stay last");
        // And it reads back as what was written.
        let back = load(&doc.save(&out).unwrap()).unwrap();
        let texts: Vec<String> = back.blocks().iter().map(|b| match b {
            Block::Paragraph { runs, .. } => runs.iter().map(|r| r.text.as_str()).collect(),
            _ => String::new(),
        }).collect();
        assert_eq!(texts, ["Hello there <bold>", "Untouched & kept", "New"]);
    }

    #[test]
    fn a_heading_without_a_heading_style_is_formatted_directly() {
        let pkg = package("<w:p/>", &[]);
        let doc = load(&pkg).unwrap();
        let out = [Out::Para { style: ParaStyle::Heading(2), runs: vec![Run { text: "Sub".into(), ..Default::default() }], base: None }];
        let xml = document_xml(&doc.save(&out).unwrap());
        assert!(xml.contains(r#"<w:rPr><w:b/><w:sz w:val="26"/></w:rPr>"#), "{xml}");
        assert!(xml.contains(r#"<w:outlineLvl w:val="1"/>"#) && !xml.contains("Heading2"), "{xml}");
        // …and still reads back as a heading.
        let back = load(&doc.save(&out).unwrap()).unwrap();
        assert!(matches!(back.items[0].blocks[0], Block::Paragraph { style: ParaStyle::Heading(2), .. }));
    }

    #[test]
    fn line_breaks_tabs_and_highlights_round_trip() {
        let pkg = package("<w:p/>", &[]);
        let doc = load(&pkg).unwrap();
        let run = Run { text: format!("a\tb{LINE_BREAK}c"), highlight: true, ..Default::default() };
        let saved = doc.save(&[Out::Para { style: ParaStyle::Normal, runs: vec![run.clone()], base: None }]).unwrap();
        let back = load(&saved).unwrap();
        let Block::Paragraph { runs, .. } = &back.items[0].blocks[0] else { panic!() };
        assert_eq!(normalize(runs), normalize(&[run]));
    }

    const PIC_PARAGRAPH: &str = r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t xml:space="preserve">Look: </w:t></w:r><w:r><w:drawing><wp:inline xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><wp:extent cx="914400" cy="457200"/><wp:docPr id="7" name="Picture 7"/><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:nvPicPr><pic:cNvPr id="0" name="a.png"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rId9"/></pic:blipFill><pic:spPr/></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#;

    fn with_picture() -> Vec<u8> {
        package(PIC_PARAGRAPH, &[
            ("word/_rels/document.xml.rels", r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/a.png"/></Relationships>"#),
            ("word/media/a.png", "\u{89}PNG picture bytes"),
        ])
    }

    /// A paragraph with a picture in it can be edited: the picture is read
    /// with its size and bytes, and written back as the drawing it was.
    #[test]
    fn a_paragraph_with_a_picture_is_editable() {
        let doc = load(&with_picture()).unwrap();
        assert_eq!(doc.items[0].kind, ItemKind::Paragraph);
        let Block::Paragraph { runs, align, .. } = &doc.items[0].blocks[0] else { panic!() };
        assert_eq!(*align, Align::Center);
        let image = runs[1].image.as_ref().expect("the picture is a run");
        assert_eq!((image.cx, image.cy, image.data.as_slice()), (914_400, 457_200, "\u{89}PNG picture bytes".as_bytes()));

        let mut edited = runs.clone();
        edited.insert(0, Run { text: "Now ".into(), ..Default::default() });
        // The picture twice: once where it was and once copied.
        edited.push(runs[1].clone());
        let xml = document_xml(&doc.save(&[Out::Para { style: ParaStyle::Normal, runs: edited, base: Some(0) }]).unwrap());
        assert!(xml.contains(r#"<w:jc w:val="center"/>"#), "{xml}");
        assert_eq!(xml.matches(r#"r:embed="rId9""#).count(), 2, "{xml}");
        let ids: Vec<u64> = drawing_ids(&xml).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "each drawing has an id of its own: {xml}");
        assert!(xml.find("Now ").unwrap() < xml.find("<w:drawing>").unwrap());
    }

    /// A picture added to a document becomes a part of its package, with a
    /// relationship and a content type, and reads back as itself.
    #[test]
    fn a_new_picture_becomes_part_of_the_package() {
        let png = b"\x89PNG\r\n\x1a\n new picture".to_vec();
        let doc = load(&package("<w:p/>", &[])).unwrap();
        let picture = Image { data: Arc::new(png.clone()), cx: 100_000, cy: 50_000, origin: None };
        let out = [
            Out::Para { style: ParaStyle::Normal, runs: vec![Run::picture(picture.clone())], base: Some(0) },
            Out::Block(Block::paragraph(ParaStyle::Normal, vec![Run::picture(picture)])),
        ];
        let saved = doc.save(&out).unwrap();
        let pkg = Package::read(&saved).unwrap();
        let rels = rels_of(&pkg.text("word/_rels/document.xml.rels").unwrap());
        let rel = rels.iter().find(|r| r.kind.ends_with("/image")).expect("an image relationship");
        assert_eq!(rels.iter().filter(|r| r.kind.ends_with("/image")).count(), 1, "the same picture is stored once");
        assert_eq!(pkg.parts.get(&resolve("word", &rel.target)), Some(&png));
        assert!(pkg.text("[Content_Types].xml").unwrap().contains(r#"Extension="png""#));
        let xml = pkg.text("word/document.xml").unwrap();
        assert!(xml.contains(r#"xmlns:wp="#), "the drawing namespace is declared: {xml}");
        let back = load(&saved).unwrap();
        let images: Vec<Image> = back.blocks().iter().flat_map(|b| match b {
            Block::Paragraph { runs, .. } => runs.iter().filter_map(|r| r.image.clone()).collect(),
            _ => vec![],
        }).collect();
        assert_eq!(images.len(), 2);
        assert!(images.iter().all(|i| i.data.as_slice() == png.as_slice() && (i.cx, i.cy) == (100_000, 50_000)));
    }

    #[test]
    fn a_blank_document_has_the_styles_it_offers() {
        let doc = load(&blank(Paper::A4, &TextDefaults::document())).unwrap();
        for id in ["Normal", "Title", "Heading1", "Heading2", "Heading3", "Quote", "ListParagraph"] {
            assert!(doc.styles.contains(id), "{id}");
        }
        assert!(doc.bullet.is_some(), "bullets have a list to use");
        assert_eq!((doc.page.width.round(), doc.page.height.round()), (595.0, 842.0));
        let built = build(&[
            Block::paragraph(ParaStyle::Heading(1), vec![Run { text: "Head".into(), ..Default::default() }]),
            Block::paragraph(ParaStyle::ListItem(0), vec![Run { text: "Item".into(), ..Default::default() }]),
            Block::Table { rows: vec![vec!["a".into(), "b".into()]] },
        ], Paper::LETTER, &TextDefaults::document()).unwrap();
        let back = load(&built).unwrap();
        let xml = document_xml(&built);
        assert!(xml.contains(r#"<w:numId w:val="1"/>"#), "a real list, not a typed bullet: {xml}");
        assert_eq!(back.blocks().len(), 3);
        assert!(matches!(back.blocks()[2], Block::Table { .. }));
    }

    #[test]
    fn documents_combine_with_their_images_lists_and_styles() {
        let png = "\u{89}PNG fake";
        let first = package(r#"<w:p><w:r><w:t>One</w:t></w:r></w:p>"#, &[]);
        let second = package(
            r#"<w:p><w:pPr><w:pStyle w:val="Fancy"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Two</w:t></w:r><w:r><w:drawing><a:blip xmlns:a="a" r:embed="rId7"/></w:drawing></w:r><w:r><w:footnoteReference w:id="1"/></w:r></w:p>"#,
            &[
                ("word/_rels/document.xml.rels", r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/></Relationships>"#),
                ("word/media/image1.png", png),
                ("word/numbering.xml", &format!(r#"<?xml version="1.0"?><w:numbering {NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#)),
                ("word/styles.xml", &format!(r#"<?xml version="1.0"?><w:styles {NS}><w:style w:type="paragraph" w:styleId="Normal"/><w:style w:type="paragraph" w:styleId="Fancy"><w:name w:val="Fancy"/></w:style></w:styles>"#)),
            ],
        );
        let merged = merge(&[("a.docx".into(), first), ("b.docx".into(), second)]).unwrap();
        let back = load(&merged).unwrap();
        let text: Vec<String> = back.blocks().iter().filter_map(|b| match b {
            Block::Paragraph { runs, .. } => Some(runs.iter().filter(|r| !r.placeholder && r.image.is_none()).map(|r| r.text.as_str()).collect()),
            _ => None,
        }).collect();
        assert_eq!(text, ["One", "", "Two"], "the second file follows a page break");
        let picture = back.blocks().iter().find_map(|b| match b {
            Block::Paragraph { runs, .. } => runs.iter().find_map(|r| r.image.clone()),
            _ => None,
        });
        assert_eq!(picture.map(|p| p.data.to_vec()), Some(png.as_bytes().to_vec()), "the picture reads back from its new part");
        let pkg = Package::read(&merged).unwrap();
        let xml = pkg.text("word/document.xml").unwrap();
        assert!(!xml.contains("footnoteReference"), "notes are left behind");
        assert!(xml.find("<w:sectPr").unwrap() > xml.find("Two").unwrap(), "section properties stay last");
        // The image came along under a new name, reachable from the body.
        let rels = rels_of(&pkg.text("word/_rels/document.xml.rels").unwrap());
        let rel = rels.iter().find(|r| xml.contains(&format!("r:embed=\"{}\"", r.id))).expect("the image's rel");
        assert_eq!(pkg.parts.get(&resolve("word", &rel.target)).map(Vec::as_slice), Some(png.as_bytes()));
        // Its list and style were carried over.
        assert!(pkg.text("word/numbering.xml").unwrap().contains("bullet"));
        assert!(pkg.text("[Content_Types].xml").unwrap().contains("numbering+xml"));
        assert!(pkg.text("word/styles.xml").unwrap().contains(r#"w:styleId="Fancy""#));
    }
}

/// `RAVEN_TEST_DOCX=a.docx RAVEN_TEST_DOCX2=b.docx cargo test docx_real -- --ignored --nocapture`
/// writes an edited copy and a combined copy next to the inputs.
#[cfg(test)]
#[test]
#[ignore]
fn docx_real() {
    let a = std::env::var("RAVEN_TEST_DOCX").unwrap();
    let bytes = std::fs::read(&a).unwrap();
    let doc = load(&bytes).unwrap();
    for (i, item) in doc.items.iter().enumerate() {
        eprintln!("{i:2} {:?} {:?}", item.kind, item.blocks.iter().map(|b| match b {
            Block::Paragraph { style, runs, .. } => format!("{style:?} {:?}", runs.iter().map(|r| r.text.as_str()).collect::<String>()),
            Block::Table { rows } => format!("table {rows:?}"),
        }).collect::<Vec<_>>());
    }
    // Edit every editable paragraph's text, turn the first into a quote.
    let mut out = Vec::new();
    let mut first = true;
    for (i, item) in doc.items.iter().enumerate() {
        match (item.kind, item.blocks.first()) {
            (ItemKind::Paragraph, Some(Block::Paragraph { style, runs, .. })) => {
                let mut runs = runs.clone();
                runs.push(Run { text: " [edited]".into(), bold: true, ..Default::default() });
                let style = if std::mem::take(&mut first) { ParaStyle::Quote } else { *style };
                out.push(Out::Para { style, runs, base: Some(i) });
            }
            _ if item.visible() => out.push(Out::Keep(i)),
            _ => {}
        }
    }
    out.push(Out::Para { style: ParaStyle::ListItem(0), runs: vec![Run { text: "A brand new bullet".into(), ..Default::default() }], base: None });
    if let Ok(png) = std::env::var("RAVEN_TEST_PNG") {
        let picture = Image::new(std::fs::read(png).unwrap(), (240, 120), doc.text_width_emu());
        out.push(Out::Para { style: ParaStyle::Normal, runs: vec![Run { text: "A new picture: ".into(), ..Default::default() }, Run::picture(picture)], base: None });
    }
    std::fs::write(a.replace(".docx", "-edited.docx"), doc.save(&out).unwrap()).unwrap();
    if let Ok(b) = std::env::var("RAVEN_TEST_DOCX2") {
        let merged = merge(&[(a.clone(), bytes), (b.clone(), std::fs::read(&b).unwrap())]).unwrap();
        std::fs::write(a.replace(".docx", "-combined.docx"), merged).unwrap();
    }
}
