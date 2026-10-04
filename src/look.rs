//! What a Word document looks like: its styles, its theme's fonts and its
//! numbered lists, worked out into the font, size, colour, spacing and
//! indents of each paragraph and each run of text — and the "1." or "a)" in
//! front of each list item.
//!
//! A document says little directly. Most of its look is in `styles.xml`:
//! document defaults, then a chain of styles each based on another, then
//! character styles on runs, and only then what a paragraph or run sets
//! itself. Fonts may be named through the theme ("the body font"), and list
//! numbers come from `numbering.xml`, counted through the document.
//!
//! The editor's view, PDF export and Word 97–2003 export all draw from what
//! is worked out here, so they agree with each other and with Word.

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::docx::Align;

/// Paragraph properties as a style or a paragraph states them: each one set
/// or left to what is underneath.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParaProps {
    /// Points.
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub line: Option<Line>,
    pub left: Option<f64>,
    pub right: Option<f64>,
    /// First-line indent; negative for a hanging one.
    pub first: Option<f64>,
    pub align: Option<Align>,
    pub keep_next: Option<bool>,
    pub page_break_before: Option<bool>,
    /// List and level; a list of 0 says "not a list item", over a style
    /// that would make it one.
    pub num: Option<u32>,
    pub level: Option<u8>,
    pub shading: Option<[u8; 3]>,
    /// Space between paragraphs of the same style is dropped.
    pub contextual: Option<bool>,
    /// Tab stops: where (points from the margin) and how text lines up on
    /// them.
    pub tabs: Option<Vec<Tab>>,
    /// The paragraph ends section `n` of the document.
    pub section: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tab {
    pub pos: f64,
    pub align: TabAlign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAlign {
    Left,
    Center,
    Right,
    Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Line {
    /// A multiple of single spacing.
    Auto(f64),
    /// Points, at least or exactly.
    AtLeast(f64),
    Exact(f64),
}

impl Default for Line {
    fn default() -> Self {
        Line::Auto(1.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FontRef {
    Name(String),
    /// The theme's heading font (major) or body font (minor).
    Theme(bool),
}

/// Run properties as a style or a run states them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunProps {
    pub font: Option<FontRef>,
    pub size: Option<f64>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    pub caps: Option<bool>,
    pub small_caps: Option<bool>,
    /// `Some(None)` is "automatic": black on white.
    pub color: Option<Option<[u8; 3]>>,
    pub highlight: Option<Option<[u8; 3]>>,
    pub vert: Option<Vert>,
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Vert {
    #[default]
    Baseline,
    Super,
    Sub,
}

impl RunProps {
    /// These over `base`: what these set wins.
    pub fn over(&self, base: &RunProps) -> RunProps {
        RunProps {
            font: self.font.clone().or_else(|| base.font.clone()),
            size: self.size.or(base.size),
            bold: self.bold.or(base.bold),
            italic: self.italic.or(base.italic),
            underline: self.underline.or(base.underline),
            strike: self.strike.or(base.strike),
            caps: self.caps.or(base.caps),
            small_caps: self.small_caps.or(base.small_caps),
            color: self.color.or(base.color),
            highlight: self.highlight.or(base.highlight),
            vert: self.vert.or(base.vert),
            hidden: self.hidden.or(base.hidden),
        }
    }
}

impl ParaProps {
    pub fn over(&self, base: &ParaProps) -> ParaProps {
        ParaProps {
            before: self.before.or(base.before),
            after: self.after.or(base.after),
            line: self.line.or(base.line),
            left: self.left.or(base.left),
            right: self.right.or(base.right),
            first: self.first.or(base.first),
            align: self.align.or(base.align),
            keep_next: self.keep_next.or(base.keep_next),
            page_break_before: self.page_break_before.or(base.page_break_before),
            num: self.num.or(base.num),
            level: self.level.or(base.level),
            shading: self.shading.or(base.shading),
            contextual: self.contextual.or(base.contextual),
            tabs: self.tabs.clone().or_else(|| base.tabs.clone()),
            section: self.section.or(base.section),
        }
    }
}

/// A run's look with everything decided.
#[derive(Debug, Clone, PartialEq)]
pub struct RunLook {
    pub font: String,
    /// Points.
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub caps: bool,
    pub small_caps: bool,
    /// `None` is automatic: the text colour of the page.
    pub color: Option<[u8; 3]>,
    pub highlight: Option<[u8; 3]>,
    pub vert: Vert,
    pub hidden: bool,
}

impl Default for RunLook {
    fn default() -> Self {
        RunLook {
            font: "Calibri".into(),
            size: 11.0,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            caps: false,
            small_caps: false,
            color: None,
            highlight: None,
            vert: Vert::Baseline,
            hidden: false,
        }
    }
}

/// A paragraph's look with everything decided.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Look {
    /// The style it names, kept for re-deciding the look after an edit.
    pub style_id: Option<String>,
    /// What the paragraph itself says, kept for the same.
    pub direct: ParaProps,
    /// Points.
    pub before: f64,
    pub after: f64,
    pub line: Line,
    pub left: f64,
    pub right: f64,
    pub first: f64,
    pub align: Align,
    pub keep_next: bool,
    pub page_break_before: bool,
    pub contextual: bool,
    pub shading: Option<[u8; 3]>,
    pub tabs: Vec<Tab>,
    /// The paragraph's text, unless a run says otherwise.
    pub run: RunLook,
    /// The number or bullet in front of a list item, and how it is set.
    pub label: Option<(String, RunLook)>,
    /// Whether the look has been worked out (rather than defaulted).
    pub resolved: bool,
}

/// A line drawn along an edge of a table or cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edge {
    pub color: [u8; 3],
    /// Points.
    pub width: f64,
}

/// A table's lines: round it, and between its rows and columns. `None` is
/// no line.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Borders {
    pub top: Option<Edge>,
    pub left: Option<Edge>,
    pub bottom: Option<Edge>,
    pub right: Option<Edge>,
    pub inside_h: Option<Edge>,
    pub inside_v: Option<Edge>,
}

impl Borders {
    pub fn any(&self) -> bool {
        [self.top, self.left, self.bottom, self.right, self.inside_h, self.inside_v].iter().any(Option::is_some)
    }
}

/// The edges named in a `tblBorders` or `tcBorders`: for each, the line or
/// "none" — or nothing said, left out.
pub fn edges(xml: &str) -> HashMap<String, Option<Edge>> {
    let mut out = HashMap::new();
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                let name = String::from_utf8_lossy(local(e.name().as_ref())).into_owned();
                let name = match name.as_str() {
                    "start" => "left".to_string(),
                    "end" => "right".to_string(),
                    n => n.to_string(),
                };
                if !matches!(name.as_str(), "top" | "left" | "bottom" | "right" | "insideH" | "insideV") {
                    continue;
                }
                let kind = attr(&e, b"val").unwrap_or_default();
                let edge = (!matches!(kind.as_str(), "nil" | "none" | "")).then(|| Edge {
                    color: attr(&e, b"color").and_then(|c| color(&c)).unwrap_or([0, 0, 0]),
                    width: (attr(&e, b"sz").and_then(|v| v.parse::<f64>().ok()).unwrap_or(4.0) / 8.0).max(0.25),
                });
                out.insert(name, edge);
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// A cell margin set (`tblCellMar`, `tcMar`): top, right, bottom, left, in
/// points, for those it sets.
pub fn margins(xml: &str) -> [Option<f64>; 4] {
    let mut out = [None; 4];
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                let i = match local(e.name().as_ref()) {
                    b"top" => 0,
                    b"right" | b"end" => 1,
                    b"bottom" => 2,
                    b"left" | b"start" => 3,
                    _ => continue,
                };
                out[i] = attr(&e, b"w").and_then(|v| v.parse::<f64>().ok()).map(|v| v / 20.0);
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Paragraph,
    Character,
    Table,
    Other,
}

#[derive(Debug, Clone)]
struct Style {
    kind: Kind,
    based_on: Option<String>,
    ppr: ParaProps,
    rpr: RunProps,
    /// A table style's table properties, as XML.
    tblpr: String,
}

#[derive(Debug, Clone, Default)]
struct Level {
    format: String,
    text: String,
    start: u32,
    ppr: ParaProps,
    rpr: RunProps,
}

/// Everything about how a document looks that is not in its paragraphs.
#[derive(Debug, Clone, Default)]
pub struct Sheet {
    styles: HashMap<String, Style>,
    default_para: Option<String>,
    default_char: Option<String>,
    defaults_ppr: ParaProps,
    defaults_rpr: RunProps,
    major: Option<String>,
    minor: Option<String>,
    /// List id → its abstract list, and the levels whose start it changes.
    nums: HashMap<u32, (u32, HashMap<u8, u32>)>,
    abstracts: HashMap<u32, Vec<Level>>,
}

/// How far each list has counted, through the document.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    lists: HashMap<u32, [u32; 9]>,
    /// The style the last paragraph had, for contextual spacing.
    pub last_style: Option<String>,
}

fn local(name: &[u8]) -> &[u8] {
    name.rsplit(|&b| b == b':').next().unwrap_or(name)
}

fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn on(e: &BytesStart) -> bool {
    !matches!(attr(e, b"val").as_deref(), Some("false" | "0" | "none" | "off"))
}

fn twips(v: Option<String>) -> Option<f64> {
    v.and_then(|v| v.parse::<f64>().ok()).map(|t| t / 20.0)
}

/// A colour as `RRGGBB`, or `None` for "auto".
pub fn color(v: &str) -> Option<[u8; 3]> {
    if v.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(v, 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

/// The colours a highlight can be.
fn highlight(v: &str) -> Option<[u8; 3]> {
    Some(match v {
        "yellow" => [255, 255, 0],
        "green" => [0, 255, 0],
        "cyan" => [0, 255, 255],
        "magenta" => [255, 0, 255],
        "blue" => [0, 0, 255],
        "red" => [255, 0, 0],
        "darkBlue" => [0, 0, 128],
        "darkCyan" => [0, 128, 128],
        "darkGreen" => [0, 128, 0],
        "darkMagenta" => [128, 0, 128],
        "darkRed" => [128, 0, 0],
        "darkYellow" => [128, 128, 0],
        "darkGray" => [128, 128, 128],
        "lightGray" => [192, 192, 192],
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        _ => return None,
    })
}

/// Run properties in an `rPr` (or anything holding run properties).
pub fn run_props(xml: &str) -> RunProps {
    let mut p = RunProps::default();
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            // What a tracked change replaced is not how the run looks now.
            Ok(Event::Start(e)) if local(e.name().as_ref()) == b"rPrChange" => {
                let end = e.name().as_ref().to_vec();
                let _ = reader.read_to_end(quick_xml::name::QName(&end));
            }
            Ok(Event::Start(e) | Event::Empty(e)) => match local(e.name().as_ref()) {
                b"rFonts" => {
                    if let Some(name) = attr(&e, b"ascii").or_else(|| attr(&e, b"hAnsi")) {
                        p.font = Some(FontRef::Name(name));
                    } else if let Some(theme) = attr(&e, b"asciiTheme").or_else(|| attr(&e, b"hAnsiTheme")) {
                        p.font = Some(FontRef::Theme(theme.starts_with("major")));
                    }
                }
                b"sz" => p.size = attr(&e, b"val").and_then(|v| v.parse::<f64>().ok()).map(|v| v / 2.0),
                b"b" => p.bold = Some(on(&e)),
                b"i" => p.italic = Some(on(&e)),
                // An underline with no type (just a colour) is none.
                b"u" => p.underline = Some(attr(&e, b"val").is_some_and(|v| v != "none")),
                b"strike" | b"dstrike" => p.strike = Some(on(&e)),
                b"caps" => p.caps = Some(on(&e)),
                b"smallCaps" => p.small_caps = Some(on(&e)),
                b"vanish" => p.hidden = Some(on(&e)),
                b"color" => p.color = attr(&e, b"val").map(|v| color(&v)),
                b"highlight" => p.highlight = attr(&e, b"val").map(|v| highlight(&v)),
                b"vertAlign" => {
                    p.vert = Some(match attr(&e, b"val").as_deref() {
                        Some("superscript") => Vert::Super,
                        Some("subscript") => Vert::Sub,
                        _ => Vert::Baseline,
                    })
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => return p,
            _ => {}
        }
    }
}

/// Paragraph properties in a `pPr` — not the run properties inside it.
pub fn para_props(xml: &str) -> ParaProps {
    let mut p = ParaProps::default();
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            // The paragraph mark's run properties, tracked changes and the
            // like are not the paragraph's.
            Ok(Event::Start(e)) if local(e.name().as_ref()) == b"tabs" => {
                let end = e.name().as_ref().to_vec();
                let Ok(span) = reader.read_to_end(quick_xml::name::QName(&end)) else { return p };
                let inner = &xml[span.start as usize..span.end as usize];
                let mut tabs = Vec::new();
                for tab in elements(inner, b"tab") {
                    let kind = first_attr(&tab, b"val").unwrap_or_default();
                    let Some(pos) = twips(first_attr(&tab, b"pos")) else { continue };
                    let align = match kind.as_str() {
                        "clear" | "bar" => continue,
                        "center" => TabAlign::Center,
                        "right" | "end" => TabAlign::Right,
                        "decimal" => TabAlign::Decimal,
                        _ => TabAlign::Left,
                    };
                    tabs.push(Tab { pos, align });
                }
                tabs.sort_by(|a, b| a.pos.total_cmp(&b.pos));
                p.tabs = Some(tabs);
            }
            Ok(Event::Start(e)) if matches!(local(e.name().as_ref()), b"rPr" | b"pPrChange" | b"sectPr") => {
                let end = e.name().as_ref().to_vec();
                let _ = reader.read_to_end(quick_xml::name::QName(&end));
            }
            Ok(Event::Start(e) | Event::Empty(e)) => match local(e.name().as_ref()) {
                b"spacing" => {
                    p.before = twips(attr(&e, b"before")).or(p.before);
                    p.after = twips(attr(&e, b"after")).or(p.after);
                    if let Some(line) = attr(&e, b"line").and_then(|v| v.parse::<f64>().ok()) {
                        p.line = Some(match attr(&e, b"lineRule").as_deref() {
                            Some("exact") => Line::Exact(line / 20.0),
                            Some("atLeast") => Line::AtLeast(line / 20.0),
                            _ => Line::Auto(line / 240.0),
                        });
                    }
                    // "Auto" spacing before and after is about 14 points.
                    if attr(&e, b"beforeAutospacing").is_some_and(|v| v == "1" || v == "true") {
                        p.before = Some(14.0);
                    }
                    if attr(&e, b"afterAutospacing").is_some_and(|v| v == "1" || v == "true") {
                        p.after = Some(14.0);
                    }
                }
                b"ind" => {
                    p.left = twips(attr(&e, b"left").or_else(|| attr(&e, b"start"))).or(p.left);
                    p.right = twips(attr(&e, b"right").or_else(|| attr(&e, b"end"))).or(p.right);
                    if let Some(h) = twips(attr(&e, b"hanging")) {
                        p.first = Some(-h);
                    } else if let Some(f) = twips(attr(&e, b"firstLine")) {
                        p.first = Some(f);
                    }
                }
                b"jc" => {
                    p.align = attr(&e, b"val").map(|v| match v.as_str() {
                        "center" => Align::Center,
                        "right" | "end" => Align::End,
                        "both" | "distribute" => Align::Justify,
                        _ => Align::Start,
                    })
                }
                b"keepNext" => p.keep_next = Some(on(&e)),
                b"pageBreakBefore" => p.page_break_before = Some(on(&e)),
                b"contextualSpacing" => p.contextual = Some(on(&e)),
                b"numId" => p.num = attr(&e, b"val").and_then(|v| v.parse().ok()),
                b"ilvl" => p.level = attr(&e, b"val").and_then(|v| v.parse().ok()),
                b"shd" => {
                    if let Some(fill) = attr(&e, b"fill").and_then(|f| color(&f)) {
                        p.shading = Some(fill);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => return p,
            _ => {}
        }
    }
}

/// The XML inside the first element named `name` in `xml`.
fn inner(xml: &str, name: &[u8]) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        let event = reader.read_event().ok()?;
        match event {
            Event::Start(e) if local(e.name().as_ref()) == name => {
                let end_name = e.name().as_ref().to_vec();
                let span = reader.read_to_end(quick_xml::name::QName(&end_name)).ok()?;
                return xml.get(span.start as usize..span.end as usize).map(str::to_string);
            }
            Event::Empty(e) if local(e.name().as_ref()) == name => return Some(String::new()),
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// The top-level elements of `xml` named `name`, each as its whole XML.
fn elements(xml: &str, name: &[u8]) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        let before = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(e)) if local(e.name().as_ref()) == name => {
                let end_name = e.name().as_ref().to_vec();
                if reader.read_to_end(quick_xml::name::QName(&end_name)).is_err() {
                    return out;
                }
                out.push(xml[before..reader.buffer_position() as usize].to_string());
            }
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == name => {
                out.push(xml[before..reader.buffer_position() as usize].to_string());
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

fn first_attr(xml: &str, name: &[u8]) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => return attr(&e, name),
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// The `w:val` of the first element named `name` inside `xml`.
fn val_of(xml: &str, name: &[u8]) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if local(e.name().as_ref()) == name => return attr(&e, b"val"),
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

impl Sheet {
    pub fn load(styles: Option<&str>, numbering: Option<&str>, theme: Option<&str>) -> Sheet {
        let mut sheet = Sheet::default();
        if let Some(styles) = styles {
            if let Some(defaults) = inner(styles, b"docDefaults") {
                sheet.defaults_rpr = inner(&defaults, b"rPrDefault").map(|x| run_props(&x)).unwrap_or_default();
                sheet.defaults_ppr = inner(&defaults, b"pPrDefault").map(|x| para_props(&x)).unwrap_or_default();
            }
            let body = inner(styles, b"styles").unwrap_or_default();
            for style in elements(&body, b"style") {
                let Some(id) = first_attr(&style, b"styleId") else { continue };
                let kind = match first_attr(&style, b"type").as_deref() {
                    Some("paragraph") => Kind::Paragraph,
                    Some("character") => Kind::Character,
                    Some("table") => Kind::Table,
                    _ => Kind::Other,
                };
                let content = inner(&style, b"style").unwrap_or_default();
                let default = first_attr(&style, b"default").is_some_and(|v| v == "1" || v == "true");
                if default && kind == Kind::Paragraph {
                    sheet.default_para = Some(id.clone());
                }
                if default && kind == Kind::Character {
                    sheet.default_char = Some(id.clone());
                }
                let ppr = elements(&content, b"pPr").first().map(|x| para_props(x)).unwrap_or_default();
                let rpr = elements(&content, b"rPr").first().map(|x| run_props(x)).unwrap_or_default();
                let based_on = val_of(&content, b"basedOn").filter(|b| *b != id);
                let tblpr = elements(&content, b"tblPr").first().and_then(|x| inner(x, b"tblPr")).unwrap_or_default();
                sheet.styles.insert(id, Style { kind, based_on, ppr, rpr, tblpr });
            }
        }
        if let Some(theme) = theme {
            let font = |which: &[u8]| inner(theme, which).and_then(|x| elements(&x, b"latin").first().and_then(|l| first_attr(l, b"typeface")));
            sheet.major = font(b"majorFont").filter(|f| !f.is_empty());
            sheet.minor = font(b"minorFont").filter(|f| !f.is_empty());
        }
        if let Some(numbering) = numbering {
            let body = inner(numbering, b"numbering").unwrap_or_default();
            for abs in elements(&body, b"abstractNum") {
                let Some(id) = first_attr(&abs, b"abstractNumId").and_then(|v| v.parse().ok()) else { continue };
                let mut levels = vec![Level::default(); 9];
                for lvl in elements(&inner(&abs, b"abstractNum").unwrap_or_default(), b"lvl") {
                    let Some(i) = first_attr(&lvl, b"ilvl").and_then(|v| v.parse::<usize>().ok()).filter(|i| *i < 9) else { continue };
                    let content = inner(&lvl, b"lvl").unwrap_or_default();
                    levels[i] = Level {
                        format: val_of(&content, b"numFmt").unwrap_or_else(|| "decimal".into()),
                        text: val_of(&content, b"lvlText").unwrap_or_default(),
                        start: val_of(&content, b"start").and_then(|v| v.parse().ok()).unwrap_or(1),
                        ppr: elements(&content, b"pPr").first().map(|x| para_props(x)).unwrap_or_default(),
                        rpr: elements(&content, b"rPr").first().map(|x| run_props(x)).unwrap_or_default(),
                    };
                }
                sheet.abstracts.insert(id, levels);
            }
            for num in elements(&body, b"num") {
                let Some(id) = first_attr(&num, b"numId").and_then(|v| v.parse().ok()) else { continue };
                let content = inner(&num, b"num").unwrap_or_default();
                let Some(abs) = val_of(&content, b"abstractNumId").and_then(|v| v.parse().ok()) else { continue };
                let mut starts = HashMap::new();
                for o in elements(&content, b"lvlOverride") {
                    if let (Some(l), Some(s)) = (
                        first_attr(&o, b"ilvl").and_then(|v| v.parse().ok()),
                        val_of(&o, b"startOverride").and_then(|v| v.parse().ok()),
                    ) {
                        starts.insert(l, s);
                    }
                }
                sheet.nums.insert(id, (abs, starts));
            }
        }
        sheet
    }

    /// A style and those it is based on, nearest first.
    fn chain(&self, id: Option<&str>) -> Vec<&Style> {
        let mut out = Vec::new();
        let mut next = id.map(str::to_string);
        while let Some(id) = next {
            let Some(style) = self.styles.get(&id) else { break };
            if out.len() > 16 {
                break;
            }
            out.push(style);
            next = style.based_on.clone();
        }
        out
    }

    /// What a paragraph style (or the default one) sets, all the way down
    /// to the document defaults.
    fn paragraph_style(&self, id: Option<&str>) -> (ParaProps, RunProps) {
        let id = id.or(self.default_para.as_deref());
        let mut ppr = ParaProps::default();
        let mut rpr = RunProps::default();
        for style in self.chain(id) {
            ppr = ppr.over(&style.ppr);
            rpr = rpr.over(&style.rpr);
        }
        (ppr.over(&self.defaults_ppr), rpr.over(&self.defaults_rpr))
    }

    fn character_style(&self, id: &str) -> RunProps {
        let mut rpr = RunProps::default();
        for style in self.chain(Some(id)).into_iter().filter(|s| s.kind == Kind::Character) {
            rpr = rpr.over(&style.rpr);
        }
        rpr
    }

    /// What a table style (and those it is based on) says of a table's
    /// lines and its cells' margins.
    pub fn table_style(&self, id: Option<&str>) -> (HashMap<String, Option<Edge>>, [Option<f64>; 4]) {
        let mut lines = HashMap::new();
        let mut pad = [None; 4];
        // Nearest last, so it wins.
        for style in self.chain(id).into_iter().rev() {
            if let Some(b) = inner(&style.tblpr, b"tblBorders") {
                lines.extend(edges(&b));
            }
            if let Some(m) = inner(&style.tblpr, b"tblCellMar") {
                for (i, v) in margins(&m).into_iter().enumerate() {
                    pad[i] = v.or(pad[i]);
                }
            }
        }
        (lines, pad)
    }

    /// Whether a paragraph style puts its paragraphs in a list.
    pub fn numbered(&self, id: Option<&str>) -> bool {
        self.paragraph_style(id).0.num.is_some_and(|n| n != 0)
    }

    fn font(&self, font: &Option<FontRef>) -> String {
        match font {
            Some(FontRef::Name(n)) => n.clone(),
            Some(FontRef::Theme(true)) => self.major.clone().unwrap_or_else(|| "Calibri Light".into()),
            Some(FontRef::Theme(false)) | None => self.minor.clone().unwrap_or_else(|| "Calibri".into()),
        }
    }

    fn decide(&self, p: &RunProps) -> RunLook {
        RunLook {
            font: self.font(&p.font),
            // Word's own default, where nothing says.
            size: p.size.unwrap_or(10.0),
            bold: p.bold.unwrap_or(false),
            italic: p.italic.unwrap_or(false),
            underline: p.underline.unwrap_or(false),
            strike: p.strike.unwrap_or(false),
            caps: p.caps.unwrap_or(false),
            small_caps: p.small_caps.unwrap_or(false),
            color: p.color.flatten(),
            highlight: p.highlight.flatten(),
            vert: p.vert.unwrap_or_default(),
            hidden: p.hidden.unwrap_or(false),
        }
    }

    /// A paragraph's look: its style under what it sets itself, its list
    /// number counted on from the paragraphs before it.
    pub fn look(&self, style_id: Option<&str>, direct: &ParaProps, counters: &mut Counters) -> Look {
        let (style_ppr, style_rpr) = self.paragraph_style(style_id);
        let num = direct.num.or(style_ppr.num).filter(|n| *n != 0);
        let level = direct.level.or(style_ppr.level).unwrap_or(0).min(8);
        // The list's own indents come between the style's and the
        // paragraph's.
        let mut ppr = style_ppr.clone();
        let mut label = None;
        if let Some(num) = num
            && let Some((abs, starts)) = self.nums.get(&num)
            && let Some(levels) = self.abstracts.get(abs)
        {
            let lvl = &levels[level as usize];
            ppr = lvl.ppr.over(&ppr);
            let counts = counters.lists.entry(num).or_insert_with(|| {
                let mut c = [0u32; 9];
                for (i, l) in levels.iter().enumerate() {
                    c[i] = starts.get(&(i as u8)).copied().unwrap_or(l.start).saturating_sub(1);
                }
                c
            });
            counts[level as usize] += 1;
            for deeper in level as usize + 1..9 {
                counts[deeper] = starts.get(&(deeper as u8)).copied().unwrap_or(levels[deeper].start).saturating_sub(1);
            }
            let text = label_text(lvl, levels, counts);
            if !text.is_empty() {
                let label_rpr = lvl.rpr.over(&style_rpr);
                let mut look = self.decide(&label_rpr);
                // Symbol-font bullets are written as private-use characters.
                if lvl.format == "bullet" {
                    look.font = self.font(&style_rpr.font);
                    look.bold = false;
                    look.italic = false;
                }
                label = Some((text, look));
            }
        }
        let ppr = direct.over(&ppr);
        let contextual = ppr.contextual.unwrap_or(false);
        let same_style = counters.last_style.as_deref() == style_id.or(self.default_para.as_deref());
        counters.last_style = style_id.or(self.default_para.as_deref()).map(str::to_string);
        Look {
            style_id: style_id.map(str::to_string),
            direct: direct.clone(),
            before: if contextual && same_style { 0.0 } else { ppr.before.unwrap_or(0.0) },
            after: ppr.after.unwrap_or(0.0),
            line: ppr.line.unwrap_or_default(),
            left: ppr.left.unwrap_or(0.0),
            right: ppr.right.unwrap_or(0.0),
            first: ppr.first.unwrap_or(0.0),
            align: ppr.align.unwrap_or_default(),
            keep_next: ppr.keep_next.unwrap_or(false),
            page_break_before: ppr.page_break_before.unwrap_or(false),
            contextual,
            shading: ppr.shading,
            tabs: ppr.tabs.clone().unwrap_or_default(),
            run: self.decide(&style_rpr),
            label,
            resolved: true,
        }
    }

    /// A run's look in its paragraph: the paragraph's style, then the run's
    /// character style, then what the run sets itself.
    pub fn run_look(&self, para: &Look, props: &str) -> RunLook {
        let (_, style_rpr) = self.paragraph_style(para.style_id.as_deref());
        let direct = run_props(props);
        let char_style = val_of(props, b"rStyle").or_else(|| self.default_char.clone());
        let char_rpr = char_style.map(|id| self.character_style(&id)).unwrap_or_default();
        self.decide(&direct.over(&char_rpr.over(&style_rpr)))
    }
}

/// A level's number text, `%1.%2` filled in from the counts.
fn label_text(lvl: &Level, levels: &[Level], counts: &[u32; 9]) -> String {
    if lvl.format == "bullet" {
        return lvl.text.chars().map(bullet).collect();
    }
    if lvl.format == "none" {
        return String::new();
    }
    let mut out = String::new();
    let mut chars = lvl.text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%'
            && let Some(d) = chars.peek().and_then(|d| d.to_digit(10))
        {
            chars.next();
            let i = (d as usize).saturating_sub(1).min(8);
            out.push_str(&number(counts[i], &levels[i].format));
        } else {
            out.push(c);
        }
    }
    out
}

/// A list bullet as a character any font has: Symbol and Wingdings
/// bullets are written in Unicode's private use area.
fn bullet(c: char) -> char {
    match c as u32 {
        0xF0B7 | 0xF0A8 | 0xF06C | 0xF09F => '•',
        0xF0A7 | 0xF06E | 0xF0FA | 0xF0A2 => '▪',
        0xF0D8 | 0xF0E0 | 0xF0E8 => '➢',
        0xF0FC | 0xF0FE => '✓',
        0xF076 => '❖',
        0xF0B0 => '°',
        0xE000..=0xF8FF => '•',
        _ if c == 'o' => '◦',
        _ => c,
    }
}

/// A count as a list numbers it.
pub fn number(n: u32, format: &str) -> String {
    match format {
        "lowerLetter" => letters(n, b'a'),
        "upperLetter" => letters(n, b'A'),
        "lowerRoman" => roman(n).to_lowercase(),
        "upperRoman" => roman(n),
        "decimalZero" => format!("{n:02}"),
        "ordinal" => format!("{n}{}", match (n % 10, n % 100) {
            (1, x) if x != 11 => "st",
            (2, x) if x != 12 => "nd",
            (3, x) if x != 13 => "rd",
            _ => "th",
        }),
        "none" | "bullet" => String::new(),
        _ => n.to_string(),
    }
}

/// a, b, … z, aa, bb, … as Word counts in letters.
fn letters(n: u32, base: u8) -> String {
    if n == 0 {
        return String::new();
    }
    let c = (base + ((n - 1) % 26) as u8) as char;
    c.to_string().repeat(((n - 1) / 26 + 1) as usize)
}

fn roman(mut n: u32) -> String {
    const NUMERALS: [(u32, &str); 13] = [
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"), (40, "XL"),
        (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
    ];
    let mut out = String::new();
    for (v, s) in NUMERALS {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn sheet() -> Sheet {
        let styles = format!(
            r#"<w:styles {W}><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/><w:sz w:val="24"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120"/></w:pPr></w:pPrDefault></w:docDefaults>
            <w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:rPr><w:rFonts w:ascii="Tahoma" w:hAnsi="Tahoma"/></w:rPr></w:style>
            <w:style w:type="paragraph" w:styleId="Title"><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="180"/></w:pPr><w:rPr><w:rFonts w:asciiTheme="majorHAnsi" w:hAnsiTheme="majorHAnsi"/><w:b/><w:color w:val="002D72"/><w:sz w:val="36"/></w:rPr></w:style>
            <w:style w:type="paragraph" w:styleId="NumberedList"><w:basedOn w:val="Normal"/><w:pPr><w:numPr><w:numId w:val="15"/></w:numPr></w:pPr></w:style>
            <w:style w:type="character" w:styleId="Strong"><w:rPr><w:b/><w:color w:val="FF0000"/></w:rPr></w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {W}><w:abstractNum w:abstractNumId="3"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%1.%2)"/></w:lvl></w:abstractNum>
            <w:abstractNum w:abstractNumId="4"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#xF0B7;"/><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr></w:lvl></w:abstractNum>
            <w:num w:numId="15"><w:abstractNumId w:val="3"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="4"/></w:num></w:numbering>"#
        );
        let theme = r#"<a:theme xmlns:a="a"><a:themeElements><a:fontScheme><a:majorFont><a:latin typeface="Georgia"/></a:majorFont><a:minorFont><a:latin typeface="Verdana"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#;
        Sheet::load(Some(&styles), Some(&numbering), Some(theme))
    }

    #[test]
    fn styles_cascade_into_a_look() {
        let s = sheet();
        let mut c = Counters::default();
        let title = s.look(Some("Title"), &ParaProps::default(), &mut c);
        assert_eq!((title.run.font.as_str(), title.run.size, title.run.bold), ("Georgia", 18.0, true));
        assert_eq!(title.run.color, Some([0x00, 0x2D, 0x72]));
        assert_eq!(title.after, 9.0);
        let normal = s.look(None, &ParaProps::default(), &mut c);
        assert_eq!((normal.run.font.as_str(), normal.run.size, normal.after), ("Tahoma", 12.0, 6.0));
        // A character style, then the run's own.
        let strong = s.run_look(&normal, r#"<w:rStyle w:val="Strong"/><w:i/>"#);
        assert!(strong.bold && strong.italic && strong.color == Some([255, 0, 0]));
    }

    #[test]
    fn lists_are_counted_through_the_document() {
        let s = sheet();
        let mut c = Counters::default();
        let labels: Vec<Option<String>> = [
            (Some("NumberedList"), ParaProps::default()),
            (Some("NumberedList"), ParaProps { level: Some(1), ..Default::default() }),
            (Some("NumberedList"), ParaProps { level: Some(1), ..Default::default() }),
            // "Not a list item", over the style's list.
            (Some("NumberedList"), ParaProps { num: Some(0), ..Default::default() }),
            (Some("NumberedList"), ParaProps::default()),
            (None, ParaProps { num: Some(2), ..Default::default() }),
        ]
        .iter()
        .map(|(style, direct)| s.look(*style, direct, &mut c).label.map(|l| l.0))
        .collect();
        assert_eq!(labels, [Some("1.".into()), Some("1.a)".into()), Some("1.b)".into()), None, Some("2.".into()), Some("•".into())]);
        let look = s.look(Some("NumberedList"), &ParaProps::default(), &mut c);
        assert_eq!((look.left, look.first), (36.0, -18.0), "the list's indents");
    }

    #[test]
    fn numbers_in_every_format() {
        assert_eq!(number(4, "lowerRoman"), "iv");
        assert_eq!(number(28, "lowerLetter"), "bb");
        assert_eq!(number(3, "ordinal"), "3rd");
        assert_eq!(number(12, "ordinal"), "12th");
    }
}
