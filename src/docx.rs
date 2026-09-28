//! DOCX: a zip of XML whose body is `word/document.xml`. We keep what reading
//! and editing need — headings, paragraphs, emphasis, lists, tables — and,
//! crucially, the XML each of those came from.
//!
//! Saving writes back the original XML of everything that was not touched,
//! byte for byte, and regenerates only the paragraphs that were edited, keeping
//! their paragraph and run properties. Images, fields, tracked changes and the
//! rest of what we do not model therefore survive an edit elsewhere in the
//! document; paragraphs that carry such things are shown read-only rather than
//! silently losing them. Every other part of the package is copied untouched.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::ops::Range;

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
    /// Stands in for something we do not show (an image, an equation); drawn
    /// dimmed and never saved as text.
    pub placeholder: bool,
    /// The run's other properties (font, size, colour…) as XML, kept so an
    /// edited paragraph does not lose its typeface.
    pub props: String,
}

impl Run {
    fn same_format(&self, other: &Run) -> bool {
        (self.bold, self.italic, self.underline, self.highlight, self.placeholder)
            == (other.bold, other.italic, other.underline, other.highlight, other.placeholder)
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
    Paragraph { style: ParaStyle, runs: Vec<Run> },
    Table { rows: Vec<Vec<String>> },
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
}

/// What to write for one paragraph of the edited document.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Item `i` as it was.
    Keep(usize),
    /// A paragraph with this content, taking its properties from item
    /// `base` if it was edited from one.
    Para { style: ParaStyle, runs: Vec<Run>, base: Option<usize> },
}

pub fn load(bytes: &[u8]) -> Result<Docx> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("not a DOCX (zip) file")?;
    let xml = read_part(&mut zip, "word/document.xml")?.context("the file has no word/document.xml")?;
    let styles = read_part(&mut zip, "word/styles.xml")?.map(|s| style_ids(&s)).unwrap_or_default();
    let (body, items) = split_body(&xml)?;
    let bullet = items
        .iter()
        .filter(|i| i.kind == ItemKind::Paragraph)
        .find_map(|i| inner_of(&xml[i.span.clone()], b"numPr").map(|n| format!("<w:numPr>{n}</w:numPr>")));
    Ok(Docx { package: bytes.to_vec(), xml, body, items, styles, bullet })
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
        let body = self.write_body(out);
        let mut xml = String::with_capacity(self.xml.len() + body.len());
        xml.push_str(&self.xml[..self.body.start]);
        xml.push_str(&body);
        xml.push_str(&self.xml[self.body.end..]);
        repackage(&self.package, &[("word/document.xml", xml.into_bytes())], &[])
    }

    fn write_body(&self, out: &[Out]) -> String {
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
            };
            if let Some(b) = base {
                for h in (0..self.items.len()).filter(|&h| owner[h] == Some(b)) {
                    emit(h, &mut body, &mut emitted);
                }
            }
            match o {
                Out::Keep(i) => emit(*i, &mut body, &mut emitted),
                Out::Para { style, runs, base } => body.push_str(&self.paragraph(*style, runs, *base)),
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

    fn paragraph(&self, style: ParaStyle, runs: &[Run], base: Option<usize>) -> String {
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
        for run in runs.iter().filter(|r| !r.placeholder && !r.text.is_empty()) {
            xml.push_str(&write_run(run, direct));
        }
        xml.push_str("</w:p>");
        xml
    }
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
            vec![Block::Paragraph { style: ParaStyle::Normal, runs: vec![] }]
        }
        _ => blocks,
    };
    Item { kind, blocks, span }
}

/// Whether a paragraph holds something we would destroy by rewriting it.
fn locked(xml: &str) -> bool {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                // A page break would come back as a line break.
                if local(e.name().as_ref()) == b"br" && matches!(attr(&e, b"type").as_deref(), Some("page" | "column")) {
                    return true;
                }
                if matches!(
                    local(e.name().as_ref()),
                    b"drawing" | b"pict" | b"object" | b"fldChar" | b"fldSimple" | b"oMath" | b"oMathPara"
                        | b"footnoteReference" | b"endnoteReference" | b"commentReference" | b"ins" | b"del"
                        | b"moveFrom" | b"moveTo" | b"sdt" | b"ruby" | b"sym"
                ) {
                    return true;
                }
            }
            Ok(Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
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
    let mut runs: Vec<Run> = Vec::new();
    let mut run = Run::default();
    let mut in_text = false;
    let mut list_level: Option<u8> = None;
    // Where the run's properties start, so they can be kept as XML.
    let mut rpr_start: Option<usize> = None;
    // Text boxes and the fallback copies of drawings hold paragraphs of
    // their own; they are not part of this paragraph's text.
    let mut skip = 0usize;

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
                match local(e.name().as_ref()) {
                    b"txbxContent" | b"Fallback" if !empty => skip = 1,
                    b"drawing" | b"pict" | b"object" => runs.push(placeholder("[image]")),
                    b"oMath" => {
                        runs.push(placeholder("[equation]"));
                        if !empty {
                            skip = 1;
                        }
                    }
                    b"p" => {
                        style = ParaStyle::Normal;
                        runs.clear();
                        list_level = None;
                    }
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
                        runs.push(placeholder("[page break]"))
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
            Event::End(e) => match local(e.name().as_ref()) {
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
                        blocks.push(Block::Paragraph { style, runs: std::mem::take(&mut runs) });
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
            },
            _ => {}
        }
    }
    blocks
}

fn placeholder(text: &str) -> Run {
    Run { text: text.into(), placeholder: true, ..Default::default() }
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
fn repackage(package: &[u8], replace: &[(&str, Vec<u8>)], remove: &[&str]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(package)).context("not a DOCX (zip) file")?;
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for i in 0..zip.len() {
        let file = zip.by_index_raw(i)?;
        let name = file.name().to_string();
        if replace.iter().any(|(n, _)| *n == name) || remove.contains(&name.as_str()) {
            continue;
        }
        out.raw_copy_file(file)?;
    }
    for (name, bytes) in replace {
        out.start_file(*name, options)?;
        out.write_all(bytes)?;
    }
    Ok(out.finish()?.into_inner())
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
        assert!(matches!(&blocks[0], Block::Paragraph { style: ParaStyle::Heading(1), runs } if runs[0].text == "Intro"));
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
            Block::Paragraph { runs, .. } => Some(runs.iter().filter(|r| !r.placeholder).map(|r| r.text.as_str()).collect()),
            _ => None,
        }).collect();
        assert_eq!(text, ["One", "", "Two"], "the second file follows a page break");
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
            Block::Paragraph { style, runs } => format!("{style:?} {:?}", runs.iter().map(|r| r.text.as_str()).collect::<String>()),
            Block::Table { rows } => format!("table {rows:?}"),
        }).collect::<Vec<_>>());
    }
    // Edit every editable paragraph's text, turn the first into a quote.
    let mut out = Vec::new();
    let mut first = true;
    for (i, item) in doc.items.iter().enumerate() {
        match (item.kind, item.blocks.first()) {
            (ItemKind::Paragraph, Some(Block::Paragraph { style, runs })) => {
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
    std::fs::write(a.replace(".docx", "-edited.docx"), doc.save(&out).unwrap()).unwrap();
    if let Ok(b) = std::env::var("RAVEN_TEST_DOCX2") {
        let merged = merge(&[(a.clone(), bytes), (b.clone(), std::fs::read(&b).unwrap())]).unwrap();
        std::fs::write(a.replace(".docx", "-combined.docx"), merged).unwrap();
    }
}
