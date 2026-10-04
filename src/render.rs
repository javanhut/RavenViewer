//! Documents set on paper, as PDF: the paragraphs, tables and pictures of a
//! DOCX (or of a `.doc` or text file read as one), laid out with Pango and
//! drawn with Cairo — the same text engine the rest of the desktop uses, so
//! the export needs nothing GTK does not already bring.
//!
//! Everything is drawn as the document's styles make it look (see `look`):
//! fonts, sizes, colours, spacing, indents, list numbers. Tables keep their
//! column widths, shading and merged cells, with formatted text and
//! pictures in them; equations are set as text; each page has its header
//! and footer, page numbers filled in; pictures placed on the page are put
//! where they float; EMF pictures are drawn as the vectors they are.
//! Headings become the PDF's bookmarks.

use std::borrow::Cow;

use anyhow::{Context, Result};
use gtk4::gdk_pixbuf;
use gtk4::gdk_pixbuf::prelude::*;
use pango::prelude::*;

use std::sync::Arc;

use crate::docx::{Align, Anchor, Block, Cell, EMU_PER_POINT, Field, Image, Note, PageSetup, ParaStyle, Run, Section, Shape, Table, TableWidth, VAlign};
use crate::emf;
use crate::look::{Line, Look, RunLook, TabAlign, Vert};

const SCALE: f64 = pango::SCALE as f64;

/// Something drawn rather than set as text: a picture or a shape.
#[derive(Clone)]
enum Graphic {
    Picture(Image),
    Shape(Arc<Shape>),
}

impl Graphic {
    fn of(run: &Run) -> Option<Graphic> {
        if let Some(i) = &run.image {
            return Some(Graphic::Picture(i.clone()));
        }
        run.shape.clone().map(Graphic::Shape)
    }

    fn anchor(&self) -> Option<&Anchor> {
        match self {
            Graphic::Picture(i) => i.anchor.as_ref(),
            Graphic::Shape(s) => s.anchor.as_ref(),
        }
    }

    fn size(&self) -> (f64, f64) {
        match self {
            Graphic::Picture(i) => picture_size(i),
            Graphic::Shape(s) => (s.cx as f64 / EMU_PER_POINT as f64, s.cy as f64 / EMU_PER_POINT as f64),
        }
    }
}

/// A graphic placed on the page, and where: drawn apart from the text.
type Floating = (Graphic, f64, f64, f64, f64);

/// `blocks` as a PDF, section by section: each on its own paper, with its
/// own headers and footers.
pub fn pdf(blocks: &[Block], sections: &[Section], title: &str) -> Result<Vec<u8>> {
    let fallback = [Section::default()];
    let sections = if sections.is_empty() { &fallback[..] } else { sections };
    let first = &sections[0].page;
    // "Page 3 of 9" needs the 9 first: lay it all out once to count.
    let total = if sections.iter().any(|s| s.decor.counts_pages()) {
        let surface = cairo::PdfSurface::for_stream(first.width, first.height, std::io::sink()).context("couldn’t start the PDF")?;
        let pages = set(&surface, false, blocks, sections, None)?;
        surface.finish();
        Some(pages)
    } else {
        None
    };
    let surface = cairo::PdfSurface::for_stream(first.width, first.height, Vec::<u8>::new()).context("couldn’t start the PDF")?;
    let _ = surface.set_metadata(cairo::PdfMetadata::Title, title);
    let _ = surface.set_metadata(cairo::PdfMetadata::Creator, "Raven Viewer");
    set(&surface, true, blocks, sections, total)?;
    surface.finish();
    let stream = surface.finish_output_stream().map_err(|e| anyhow::anyhow!("couldn’t write the PDF: {}", e.error))?;
    stream.downcast::<Vec<u8>>().map(|b| *b).map_err(|_| anyhow::anyhow!("couldn’t write the PDF"))
}

/// Lay the document out on `surface`, page by page; the number of pages.
fn set(surface: &cairo::PdfSurface, outline: bool, blocks: &[Block], sections: &[Section], total: Option<i32>) -> Result<i32> {
    let cr = cairo::Context::new(surface).context("couldn’t draw the PDF")?;
    let fonts = pangocairo::FontMap::new();
    crate::fonts::apply(fonts.upcast_ref());
    let ctx = fonts.create_context();
    pangocairo::functions::update_context(&cr, &ctx);
    // Points on the page are font points: no screen resolution in between,
    // and no hinting that would make widths depend on one.
    pangocairo::functions::context_set_resolution(&ctx, 72.0);
    if let Ok(mut options) = cairo::FontOptions::new() {
        options.set_hint_metrics(cairo::HintMetrics::Off);
        options.set_hint_style(cairo::HintStyle::None);
        pangocairo::functions::context_set_font_options(&ctx, Some(&options));
    }
    let page = sections[0].page.clone();
    let mut w = Writer {
        cr,
        surface: surface.clone(),
        outline,
        ctx,
        y: page.margins[0],
        top: page.margins[0],
        bottom: page.height - page.margins[2],
        page,
        sections,
        section: 0,
        section_start: 1,
        total,
        page_no: 1,
        shown_no: sections[0].restart.unwrap_or(1),
        blank: true,
        bookmarks: Vec::new(),
        float_bottom: 0.0,
        notes: Vec::new(),
    };
    w.decorate();
    w.flow(blocks);
    w.end_notes(blocks);
    w.foot_notes();
    Ok(w.page_no)
}

/// A font name with a generic family behind it, for when it is not
/// installed: fontconfig then picks a face of the same kind.
pub fn family(font: &str) -> String {
    let lower = font.to_ascii_lowercase();
    let generic = if ["mono", "courier", "consol", "code", "typewriter"].iter().any(|k| lower.contains(k)) {
        "monospace"
    } else if ["times", "georgia", "cambria", "garamond", "serif", "roman", "book", "palatino", "minion"]
        .iter()
        .any(|k| lower.contains(k) && !lower.contains("sans"))
    {
        "serif"
    } else {
        "sans-serif"
    };
    format!("{font},{generic}")
}

/// A paragraph's look, or — for one the document's styles were never
/// applied to (a text file's, a test's) — the page's defaults and the
/// paragraph kind's usual look.
pub fn effective<'a>(look: &'a Look, style: ParaStyle, page: &PageSetup) -> Cow<'a, Look> {
    if look.resolved {
        return Cow::Borrowed(look);
    }
    let mut l = Look {
        after: page.after,
        line: Line::Auto(page.line.max(1.0)),
        run: RunLook { font: page.font.clone(), size: page.size, ..Default::default() },
        resolved: true,
        ..Default::default()
    };
    if let ParaStyle::ListItem(level) = style {
        l.left = 18.0 * (level as f64 + 1.0);
        l.first = -14.0;
        l.label = Some((["•", "◦", "▪"][level as usize % 3].to_string(), l.run.clone()));
        l.after = 0.0;
    }
    crate::docx::builtin(&mut l, style);
    Cow::Owned(l)
}

/// A run's look: as worked out from the document's styles, or else from
/// the paragraph's and what the run sets itself.
pub fn run_look(run: &Run, para: &Look) -> RunLook {
    if let Some(l) = &run.look {
        return (**l).clone();
    }
    let mut l = para.run.clone();
    l.bold |= run.bold;
    l.italic |= run.italic;
    l.underline |= run.underline;
    if run.highlight {
        l.highlight = Some([255, 255, 0]);
    }
    if let Some(sz) = crate::docx::run_prop(&run.props, "sz").and_then(|v| v.parse::<f64>().ok()) {
        l.size = sz / 2.0;
    }
    if let Some(font) = crate::docx::run_font(&run.props) {
        l.font = font;
    }
    if let Some(c) = crate::docx::run_prop(&run.props, "color").and_then(|v| crate::look::color(&v)) {
        l.color = Some(c);
    }
    l.strike |= crate::docx::run_prop(&run.props, "strike").is_some();
    l
}

fn rgb16(c: [u8; 3]) -> (u16, u16, u16) {
    (c[0] as u16 * 257, c[1] as u16 * 257, c[2] as u16 * 257)
}

/// Whether a floating picture is drawn on its own, over (or under) the
/// text, rather than taking room in its flow.
pub fn floats(a: &Anchor) -> bool {
    a.behind || !a.wrap || matches!(a.v.from.as_str(), "page" | "margin" | "topMargin" | "bottomMargin" | "insideMargin" | "outsideMargin")
}

struct Writer<'d> {
    cr: cairo::Context,
    surface: cairo::PdfSurface,
    /// Bookmarks are made (not while only counting pages).
    outline: bool,
    ctx: pango::Context,
    /// The current section's paper.
    page: PageSetup,
    sections: &'d [Section],
    section: usize,
    /// The page the current section started on.
    section_start: i32,
    total: Option<i32>,
    /// The page number shown, as the sections number them.
    shown_no: i32,
    /// Where the next thing goes, from the top of the page, in points.
    y: f64,
    /// Where the body starts and ends on this page, past the header and
    /// before the footer.
    top: f64,
    bottom: f64,
    page_no: i32,
    /// Nothing has been drawn on this page yet.
    blank: bool,
    /// Open bookmarks: each heading's level and id, for nesting.
    bookmarks: Vec<(u8, i32)>,
    /// Where the lowest picture floating beside the text ends: empty lines
    /// sit beside it, and text goes below it.
    float_bottom: f64,
    /// The footnotes referred to on this page, set at its foot when it is
    /// done; `bottom` is above them.
    notes: Vec<Arc<Note>>,
}

/// The room between the text and the footnotes, the rule in the middle.
const NOTE_GAP: f64 = 12.0;

/// The endnotes `blocks` refer to, in order.
fn collect_notes(blocks: &[Block], out: &mut Vec<Arc<Note>>) {
    for block in blocks {
        match block {
            Block::Paragraph { runs, .. } => out.extend(runs.iter().filter_map(|r| r.note.clone()).filter(|n| !n.foot)),
            Block::Table(t) => t.rows.iter().flatten().for_each(|c| collect_notes(&c.blocks, out)),
        }
    }
}

impl Writer<'_> {
    fn left(&self) -> f64 {
        self.page.margins[3]
    }

    fn width(&self) -> f64 {
        (self.page.width - self.page.margins[1] - self.page.margins[3]).max(72.0)
    }

    /// `blocks` set one after another down the pages.
    fn flow(&mut self, blocks: &[Block]) {
        for (i, block) in blocks.iter().enumerate() {
            match block {
                Block::Paragraph { style, runs, look, .. } => {
                    let next = blocks.get(i + 1).and_then(|b| match b {
                        Block::Paragraph { look, .. } => Some(&**look),
                        _ => None,
                    });
                    let look = effective(look, *style, &self.page);
                    self.paragraph(*style, runs, &look, next);
                    if let Some(n) = look.direct.section {
                        self.next_section(n + 1);
                    }
                }
                Block::Table(t) => self.table(t),
            }
        }
    }

    fn new_page(&mut self) {
        self.foot_notes();
        self.cr.show_page().ok();
        self.page_no += 1;
        self.shown_no += 1;
        self.decorate();
    }

    /// Section `n` starts: on a new page of its own paper, unless it
    /// carries on on this one.
    fn next_section(&mut self, n: usize) {
        let Some(section) = self.sections.get(n) else { return };
        self.section = n;
        if section.continuous {
            return;
        }
        self.foot_notes();
        self.cr.show_page().ok();
        self.page = section.page.clone();
        self.surface.set_size(self.page.width, self.page.height).ok();
        self.page_no += 1;
        self.shown_no = section.restart.unwrap_or(self.shown_no + 1);
        self.section_start = self.page_no;
        self.decorate();
    }

    /// The page's header and footer; the body goes between them.
    fn decorate(&mut self) {
        let sections = self.sections;
        let decor = &sections[self.section].decor;
        let (header, footer) = decor.of_page(self.page_no == self.section_start);
        let (x, width) = (self.left(), self.width());
        self.top = self.page.margins[0];
        self.bottom = self.page.height - self.page.margins[2];
        if !header.is_empty() {
            let h = self.boxed(header, x, self.page.header, width);
            self.top = self.top.max(self.page.header + h);
        }
        if !footer.is_empty() {
            let h = self.measure(footer, width);
            let y = self.page.height - self.page.footer - h;
            self.boxed(footer, x, y, width);
            self.bottom = self.bottom.min(y);
        }
        self.y = self.top;
        self.blank = true;
        self.float_bottom = 0.0;
    }

    /// Make room for `height` more points, on a new page if this one is full.
    fn room(&mut self, height: f64) {
        if !self.blank && self.y + height > self.bottom {
            self.new_page();
        }
    }

    // ── Text ────────────────────────────────────────────────────────────

    /// A paragraph's runs laid out `width` wide, as its look says.
    fn layout(&self, look: &Look, runs: &[&Run], width: f64) -> pango::Layout {
        let page_no = self.shown_no;
        let total = self.total.unwrap_or(self.page_no);
        layout_of(&self.ctx, look, runs, width, &|field| match field {
            Field::Page => page_no.to_string(),
            Field::Pages => total.to_string(),
            Field::NoteMark => String::new(),
        })
    }

    /// The notes a paragraph's runs refer to, each with where its number
    /// is in the paragraph's text.
    fn notes_in(&self, look: &Look, runs: &[&Run]) -> Vec<(usize, Arc<Note>)> {
        let field = |field| match field {
            Field::Page | Field::Pages => "0".to_string(),
            Field::NoteMark => String::new(),
        };
        (0..runs.len())
            .filter_map(|i| runs[i].note.as_ref().filter(|n| n.foot).map(|n| (attributed(look, &runs[..i], &field).0.len(), n.clone())))
            .collect()
    }

    /// The height a note takes at the foot of the page.
    fn note_height(&mut self, note: &Note) -> f64 {
        let width = self.width();
        self.measure(&note.blocks, width)
    }

    /// The footnotes on this page, under a short rule at the foot of it.
    fn foot_notes(&mut self) {
        let notes = std::mem::take(&mut self.notes);
        if notes.is_empty() {
            return;
        }
        let (x, width) = (self.left(), self.width());
        let mut y = self.bottom + NOTE_GAP / 2.0;
        self.cr.set_source_rgb(0.0, 0.0, 0.0);
        self.cr.set_line_width(0.5);
        self.cr.move_to(x, y);
        self.cr.line_to(x + 144.0f64.min(width), y);
        self.cr.stroke().ok();
        y += NOTE_GAP / 2.0;
        for note in notes {
            y += self.boxed(&note.blocks, x, y, width);
        }
    }

    /// The endnotes, after the text.
    fn end_notes(&mut self, blocks: &[Block]) {
        let mut notes = Vec::new();
        collect_notes(blocks, &mut notes);
        if notes.is_empty() {
            return;
        }
        let width = self.width();
        self.room(NOTE_GAP + 12.0);
        let (x, y) = (self.left(), self.y + NOTE_GAP / 2.0);
        self.cr.set_source_rgb(0.0, 0.0, 0.0);
        self.cr.set_line_width(0.5);
        self.cr.move_to(x, y);
        self.cr.line_to(x + 144.0f64.min(width), y);
        self.cr.stroke().ok();
        self.y += NOTE_GAP;
        for note in notes {
            self.flow(&note.blocks);
        }
    }

    /// A list number or bullet, in front of the line whose baseline is at `y`.
    fn label(&self, look: &Look, x: f64, baseline: f64) {
        let Some((text, rl)) = &look.label else { return };
        let layout = pango::Layout::new(&self.ctx);
        layout.set_font_description(Some(&description(rl)));
        layout.set_text(text);
        let (r, g, b) = rl.color.map_or((0.0, 0.0, 0.0), |c| (c[0] as f64 / 255.0, c[1] as f64 / 255.0, c[2] as f64 / 255.0));
        self.cr.set_source_rgb(r, g, b);
        self.cr.move_to(x, baseline - layout.baseline() as f64 / SCALE);
        pangocairo::functions::show_layout(&self.cr, &layout);
    }

    fn paragraph(&mut self, style: ParaStyle, runs: &[Run], look: &Look, next: Option<&Look>) {
        if look.page_break_before && !self.blank {
            self.new_page();
        }
        if !self.blank {
            self.y += look.before;
        }
        let top = self.y;
        // Pictures and page breaks split the paragraph; the text between
        // them is laid out as one.
        let mut text: Vec<&Run> = Vec::new();
        let mut first = true;
        let mut any = false;
        for run in runs {
            if let Some(g) = Graphic::of(run) {
                if !text.is_empty() {
                    self.text(style, &std::mem::take(&mut text), look, first);
                    first = false;
                    any = true;
                }
                any |= self.flow_graphic(&g, look, top);
            } else if run.is_page_break() {
                if !text.is_empty() {
                    self.text(style, &std::mem::take(&mut text), look, first);
                    first = false;
                }
                self.new_page();
                any = true;
            } else {
                text.push(run);
            }
        }
        let shows_text = text.iter().any(|r| !r.text.trim().is_empty() || r.field.is_some());
        if shows_text || (!any && self.y >= self.float_bottom) {
            self.text(style, &text, look, first);
        } else if !any {
            // An empty line beside a floating picture takes no room of its
            // own.
            return;
        }
        // Between paragraphs of one style that says so, no space.
        let contextual = look.contextual && next.is_some_and(|n| n.style_id == look.style_id);
        if !contextual {
            self.y += look.after;
        }
    }

    fn text(&mut self, style: ParaStyle, runs: &[&Run], look: &Look, first: bool) {
        // Text goes below a picture floating beside it.
        let shows = runs.iter().any(|r| !r.text.trim().is_empty() || r.field.is_some());
        if shows && self.y < self.float_bottom {
            self.y = self.float_bottom;
        }
        let x = self.left() + look.left;
        let layout = self.layout(look, runs, self.width() - look.left - look.right);
        let lines = placed(&self.ctx, look, &layout);
        let mut notes = self.notes_in(look, runs);
        // A paragraph that says so stays with what follows it.
        let height = lines.last().map_or(0.0, |l| l.top + l.height);
        if look.keep_next && height < self.bottom - self.top {
            self.room(height + 2.5 * look.run.size);
        }
        if matches!(style, ParaStyle::Heading(_) | ParaStyle::Title) {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            self.bookmark(style, &text);
        }
        let mut origin = self.y;
        for (i, placed) in lines.iter().enumerate() {
            // The footnotes this line refers to go at the foot of its page.
            let line = layout.line_readonly(i as i32);
            let end = match (&line, i + 1 < lines.len()) {
                (Some(line), true) => (line.start_index() + line.length()) as usize,
                _ => usize::MAX,
            };
            let here: Vec<Arc<Note>> = notes.iter().filter(|(at, _)| *at < end).map(|(_, n)| n.clone()).collect();
            notes.retain(|(at, _)| *at >= end);
            let mut foot = 0.0;
            for note in &here {
                foot += self.note_height(note);
            }
            // The first on a page brings the rule above them.
            let gap = |w: &Self| if !here.is_empty() && w.notes.is_empty() { NOTE_GAP } else { 0.0 };
            if !self.blank && origin + placed.top + placed.height > self.bottom - foot - gap(self) {
                self.new_page();
                origin = self.y - placed.top;
            }
            self.bottom -= foot + gap(self);
            self.notes.extend(here);
            if let Some(line) = line {
                if let Some([r, g, b]) = look.shading {
                    self.cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
                    self.cr.rectangle(self.left() + look.left, origin + placed.top, self.width() - look.left - look.right, placed.height);
                    self.cr.fill().ok();
                }
                if i == 0 && first {
                    self.label(look, x + look.first, origin + placed.baseline);
                }
                self.cr.move_to(x + placed.x, origin + placed.baseline);
                pangocairo::functions::show_layout_line(&self.cr, &line);
            }
            self.blank = false;
            self.y = origin + placed.top + placed.height;
        }
    }

    /// A heading in the PDF's bookmarks, under the last heading above it.
    fn bookmark(&mut self, style: ParaStyle, text: &str) {
        if !self.outline {
            return;
        }
        let surface = &self.surface;
        let level = match style {
            ParaStyle::Title => 1,
            ParaStyle::Heading(n) => n,
            _ => return,
        };
        let name: String = text.chars().filter(|c| !c.is_control() && *c != '\u{FFFC}').collect();
        if name.trim().is_empty() {
            return;
        }
        while self.bookmarks.last().is_some_and(|(l, _)| *l >= level) {
            self.bookmarks.pop();
        }
        let parent = self.bookmarks.last().map_or(0, |(_, id)| *id);
        let link = format!("page={} pos=[{:.1} {:.1}]", self.page_no, self.left(), self.y);
        if let Ok(id) = surface.add_outline(parent, name.trim(), &link, cairo::PdfOutline::empty()) {
            self.bookmarks.push((level, id));
        }
    }

    // ── Pictures ────────────────────────────────────────────────────────

    fn draw(&mut self, g: &Graphic, x: f64, y: f64, w: f64, h: f64) {
        match g {
            Graphic::Picture(image) => draw_picture(&self.cr, image, x, y, w, h),
            Graphic::Shape(shape) => self.draw_shape(shape, x, y, w, h),
        }
    }

    /// A shape: its outline and fill, and the text inside it.
    fn draw_shape(&mut self, shape: &Shape, x: f64, y: f64, w: f64, h: f64) {
        let cr = self.cr.clone();
        cr.new_path();
        match shape.geom.as_str() {
            "ellipse" => {
                cr.save().ok();
                cr.translate(x + w / 2.0, y + h / 2.0);
                cr.scale((w / 2.0).max(0.01), (h / 2.0).max(0.01));
                cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
                cr.restore().ok();
            }
            "roundRect" => {
                let r = w.min(h) * 0.1667;
                cr.new_sub_path();
                cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
                cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
                cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
                cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
                cr.close_path();
            }
            "line" | "straightConnector1" => {
                cr.move_to(x, y);
                cr.line_to(x + w, y + h);
            }
            _ => cr.rectangle(x, y, w, h),
        }
        let is_line = matches!(shape.geom.as_str(), "line" | "straightConnector1");
        if let (Some([r, g, b]), false) = (shape.fill, is_line) {
            cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
            cr.fill_preserve().ok();
        }
        if let Some(([r, g, b], width)) = shape.line {
            cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
            cr.set_line_width(width.max(0.25));
            cr.stroke().ok();
        }
        cr.new_path();
        if !shape.text.is_empty() {
            let [top, right, bottom, left] = shape.insets;
            let inner = (w - left - right).max(12.0);
            let content = self.measure(&shape.text, inner);
            let room = (h - top - bottom).max(0.0);
            let ty = y + top + match shape.valign {
                VAlign::Center => (room - content) / 2.0,
                VAlign::Bottom => room - content,
                VAlign::Top => 0.0,
            };
            self.boxed(&shape.text, x + left, ty, inner);
        }
    }

    /// Where a floating picture goes, from what it is placed against.
    fn anchored(&self, a: &Anchor, w: f64, h: f64, para_top: f64, column: (f64, f64)) -> (f64, f64) {
        let p = &self.page;
        let (hx, hw) = match a.h.from.as_str() {
            "page" => (0.0, p.width),
            "leftMargin" | "insideMargin" => (0.0, p.margins[3]),
            "rightMargin" | "outsideMargin" => (p.width - p.margins[1], p.margins[1]),
            "margin" => (p.margins[3], p.width - p.margins[1] - p.margins[3]),
            _ => column,
        };
        let x = hx + match a.h.align.as_deref() {
            Some("center") => (hw - w) / 2.0,
            Some("right" | "outside") => hw - w,
            Some(_) => 0.0,
            None => a.h.offset,
        };
        let (vy, vh) = match a.v.from.as_str() {
            "page" => (0.0, p.height),
            "topMargin" => (0.0, p.margins[0]),
            "bottomMargin" => (p.height - p.margins[2], p.margins[2]),
            "margin" => (p.margins[0], p.height - p.margins[0] - p.margins[2]),
            _ => (para_top, 0.0),
        };
        let y = vy + match a.v.align.as_deref() {
            Some("center") => (vh - h) / 2.0,
            Some("bottom" | "outside") => vh - h,
            Some(_) => 0.0,
            None => a.v.offset,
        };
        (x, y)
    }

    /// A picture or shape in the body's flow: in a line of its own, or
    /// floating. The result says whether it took up room.
    fn flow_graphic(&mut self, g: &Graphic, look: &Look, para_top: f64) -> bool {
        let (w, h) = g.size();
        let column = (self.left(), self.width());
        if let Some(a) = g.anchor() {
            // Floating, it keeps its size — up to the page's.
            let (w, h) = self.fit(w, h, self.page.width);
            let (x, y) = self.anchored(a, w, h, para_top, column);
            if floats(a) {
                self.draw(g, x, y, w, h);
            } else {
                // Text keeps clear of it: the empty lines after it sit
                // beside it, and text goes below it.
                self.draw(g, x, y, w, h);
                self.float_bottom = self.float_bottom.max(y + h);
                self.blank = false;
            }
            return false;
        }
        let (w, h) = self.fit(w, h, self.width());
        self.room(h);
        let x = aligned(look, w, column);
        let y = self.y;
        self.draw(g, x, y, w, h);
        self.y += h;
        self.blank = false;
        true
    }

    /// A picture's size, shrunk to fit `width` and the page.
    fn fit(&self, w: f64, h: f64, width: f64) -> (f64, f64) {
        let max_h = (self.page.height - self.page.margins[0] - self.page.margins[2]).max(72.0);
        let scale = (width / w.max(1e-3)).min(max_h / h.max(1e-3)).min(1.0);
        (w * scale, h * scale)
    }

    // ── Boxes: table cells, headers, footers ────────────────────────────

    /// How tall `blocks` are set `width` wide.
    fn measure(&mut self, blocks: &[Block], width: f64) -> f64 {
        self.cell_blocks(blocks, 0.0, 0.0, width, false, &mut Vec::new())
    }

    /// `blocks` drawn in a box at `x, y`, `width` wide, without breaking
    /// across pages: pictures behind the text first, the text, then
    /// pictures in front. Returns the height.
    fn boxed(&mut self, blocks: &[Block], x: f64, y: f64, width: f64) -> f64 {
        let mut floating = Vec::new();
        self.cell_blocks(blocks, x, y, width, false, &mut floating);
        let behind = |f: &&Floating| f.0.anchor().is_some_and(|a| a.behind);
        for (g, fx, fy, w, h) in floating.iter().filter(behind) {
            self.draw(g, *fx, *fy, *w, *h);
        }
        let height = self.cell_blocks(blocks, x, y, width, true, &mut Vec::new());
        for (g, fx, fy, w, h) in floating.iter().filter(|f| !behind(f)) {
            self.draw(g, *fx, *fy, *w, *h);
        }
        height
    }

    /// Lay out (and, `draw`, draw) blocks in a box; floating pictures are
    /// collected rather than drawn. Returns the height.
    fn cell_blocks(&mut self, blocks: &[Block], x: f64, y: f64, width: f64, draw: bool, floating: &mut Vec<Floating>) -> f64 {
        let mut cy = y;
        for (i, block) in blocks.iter().enumerate() {
            match block {
                Block::Paragraph { style, runs, look, .. } => {
                    let look = effective(look, *style, &self.page);
                    if i > 0 {
                        cy += look.before;
                    }
                    let para_top = cy;
                    let mut text: Vec<&Run> = Vec::new();
                    let mut first = true;
                    let mut any = false;
                    for run in runs {
                        if let Some(g) = Graphic::of(run) {
                            if !text.is_empty() {
                                cy += self.box_text(&look, &std::mem::take(&mut text), (x, cy, width), draw, first);
                                first = false;
                                any = true;
                            }
                            let (w, h) = g.size();
                            match g.anchor() {
                                Some(a) => {
                                    let (w, h) = self.fit(w, h, self.page.width);
                                    let (fx, fy) = self.anchored(a, w, h, para_top, (x, width));
                                    floating.push((g.clone(), fx, fy, w, h));
                                }
                                None => {
                                    let (w, h) = self.fit(w, h, width);
                                    if draw {
                                        let px = aligned(&look, w, (x, width));
                                        self.draw(&g, px, cy, w, h);
                                    }
                                    cy += h;
                                    any = true;
                                }
                            }
                        } else if !run.is_page_break() {
                            text.push(run);
                        }
                    }
                    if !text.is_empty() || !any {
                        cy += self.box_text(&look, &text, (x, cy, width), draw, first);
                    }
                    cy += look.after;
                }
                Block::Table(t) => cy += self.table_at(t, x, cy, width, draw),
            }
        }
        cy - y
    }

    fn box_text(&mut self, look: &Look, runs: &[&Run], (x, y, width): (f64, f64, f64), draw: bool, first: bool) -> f64 {
        let layout = self.layout(look, runs, width - look.left - look.right);
        let lines = placed(&self.ctx, look, &layout);
        let height = lines.last().map_or(0.0, |l| l.top + l.height);
        if draw {
            if let Some([r, g, b]) = look.shading {
                self.cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
                self.cr.rectangle(x + look.left, y, width - look.left - look.right, height);
                self.cr.fill().ok();
            }
            for (i, placed) in lines.iter().enumerate() {
                if i == 0 && first {
                    self.label(look, x + look.left + look.first, y + placed.baseline);
                }
                if let Some(line) = layout.line_readonly(i as i32) {
                    self.cr.move_to(x + look.left + placed.x, y + placed.baseline);
                    pangocairo::functions::show_layout_line(&self.cr, &line);
                }
            }
        }
        height
    }

    // ── Tables ──────────────────────────────────────────────────────────

    fn row_height(&mut self, t: &Table, r: usize, cells: &[(f64, f64)]) -> f64 {
        let mut h: f64 = 0.0;
        for (cell, (_, w)) in t.rows[r].iter().zip(cells) {
            if !cell.merged {
                let [top, right, bottom, left] = t.pad_of(cell);
                h = h.max(self.measure(&cell.blocks, w - left - right) + top + bottom);
            }
        }
        // As high as the row asks: at least so high, or exactly.
        match t.heights.get(r).copied().flatten() {
            Some((asked, true)) => asked,
            Some((asked, false)) => h.max(asked),
            None => h.max(12.0),
        }
    }

    fn draw_row(&mut self, t: &Table, r: usize, cells: &[(f64, f64)], y: f64, h: f64) {
        let row = &t.rows[r];
        let mut col = 0;
        for (cell, (cx, cw)) in row.iter().zip(cells) {
            let span = cell.span.max(1);
            let [top, right, bottom, left] = t.pad_of(cell);
            if let Some([red, g, b]) = cell.shade {
                self.cr.set_source_rgb(red as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
                self.cr.rectangle(*cx, y, *cw, h);
                self.cr.fill().ok();
            }
            if !cell.merged {
                let inner = cw - left - right;
                let offset = match cell.valign {
                    VAlign::Top => 0.0,
                    v => {
                        let content = self.measure(&cell.blocks, inner);
                        let room = (h - top - bottom - content).max(0.0);
                        if v == VAlign::Center { room / 2.0 } else { room }
                    }
                };
                self.boxed(&cell.blocks, cx + left, y + top + offset, inner);
            }
            // Each edge's line, as the cell or the table says; a cell merged
            // into the one above has none between them.
            for (side, (x0, y0, x1, y1)) in [
                ("top", (*cx, y, cx + cw, y)),
                ("bottom", (*cx, y + h, cx + cw, y + h)),
                ("left", (*cx, y, *cx, y + h)),
                ("right", (cx + cw, y, cx + cw, y + h)),
            ] {
                if side == "top" && cell.merged {
                    continue;
                }
                if let Some(edge) = t.edge(cell, side, r, col, span) {
                    let [red, g, b] = edge.color;
                    self.cr.set_source_rgb(red as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
                    self.cr.set_line_width(edge.width);
                    self.cr.move_to(x0, y0);
                    self.cr.line_to(x1, y1);
                    self.cr.stroke().ok();
                }
            }
            col += span;
        }
    }

    /// A table in the body: row by row, a row that does not fit going on
    /// the next page.
    fn table(&mut self, t: &Table) {
        if self.y < self.float_bottom {
            self.y = self.float_bottom;
        }
        let cols = columns(t, self.left(), self.width());
        for (r, row) in t.rows.iter().enumerate() {
            let cells = cells(row, &cols);
            let h = self.row_height(t, r, &cells);
            self.room(h);
            let y = self.y;
            self.draw_row(t, r, &cells, y, h);
            self.y += h;
            self.blank = false;
        }
        self.y += self.page.after.min(6.0);
    }

    /// A table inside a box (a cell, a header): no page breaks.
    fn table_at(&mut self, t: &Table, x: f64, y: f64, width: f64, draw: bool) -> f64 {
        let cols = columns(t, x, width);
        let mut cy = y;
        for (r, row) in t.rows.iter().enumerate() {
            let cells = cells(row, &cols);
            let h = self.row_height(t, r, &cells);
            if draw {
                self.draw_row(t, r, &cells, cy, h);
            }
            cy += h;
        }
        cy - y
    }
}

/// A look's font, as Pango describes it.
/// Word sets a document's lines by the metrics of the fonts it asks for.
/// Where one of those isn't installed its stand-in is often taller — Noto
/// Sans is 1.36 em a line to Arial's 1.15 — and every page runs long; so a
/// stand-in keeps the line height, in ems, of the font it stands in for.
const LINE_EMS: &[(&str, f64)] = &[
    ("arial", 1.150),
    ("helvetica", 1.150),
    ("helvetica neue", 1.193),
    ("times new roman", 1.150),
    ("calibri", 1.221),
    ("calibri light", 1.221),
    ("cambria", 1.172),
    ("georgia", 1.136),
    ("verdana", 1.215),
    ("tahoma", 1.207),
    ("trebuchet ms", 1.161),
    ("segoe ui", 1.330),
    ("courier new", 1.133),
    ("consolas", 1.171),
    ("garamond", 1.120),
    ("book antiqua", 1.170),
    ("century gothic", 1.226),
];

/// One line of a paragraph as it is set: its top, height and baseline from
/// the top of the paragraph, and its x from the paragraph's left.
struct Placed {
    top: f64,
    height: f64,
    baseline: f64,
    x: f64,
}

/// Where a layout's lines go, spaced as Word spaces them: a multiple of
/// the line height, with the room below the text; or exactly, or at least,
/// so many points, with the room above it.
fn placed(ctx: &pango::Context, look: &Look, layout: &pango::Layout) -> Vec<Placed> {
    let (actual, wanted) = line_ems(ctx, &look.run);
    let scale = wanted / actual;
    let mut lines = Vec::new();
    let mut y = 0.0;
    let mut iter = layout.iter();
    loop {
        let (_, logical) = iter.line_extents();
        let natural = logical.height() as f64 / SCALE * scale;
        let ascent = (iter.baseline() - logical.y()) as f64 / SCALE * scale;
        let height = match look.line {
            Line::Auto(m) => natural * m.max(0.5),
            Line::Exact(pt) => pt,
            Line::AtLeast(pt) => pt.max(natural),
        };
        let baseline = match look.line {
            Line::Auto(_) => ascent,
            _ => height - (natural - ascent),
        };
        lines.push(Placed { top: y, height, baseline: y + baseline, x: logical.x() as f64 / SCALE });
        y += height;
        if !iter.next_line() {
            break;
        }
    }
    lines
}

/// The height of a line set in `rl`'s font, in ems: what the installed font
/// gives, and what the document's font would have.
fn line_ems(ctx: &pango::Context, rl: &RunLook) -> (f64, f64) {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<(String, usize), (f64, f64)>> = Default::default();
    }
    let key = rl.font.to_ascii_lowercase();
    let generation = crate::fonts::generation();
    if let Some(ems) = CACHE.with(|c| c.borrow().get(&(key.clone(), generation)).copied()) {
        return ems;
    }
    let size = 100.0;
    let desc = description(&RunLook { font: rl.font.clone(), size, ..Default::default() });
    let metrics = ctx.metrics(Some(&desc), None);
    let actual = if metrics.height() > 0 {
        metrics.height() as f64 / SCALE / size
    } else {
        (metrics.ascent() + metrics.descent()) as f64 / SCALE / size
    };
    let actual = if actual > 0.0 { actual } else { 1.17 };
    let installed = ctx
        .load_font(&desc)
        .and_then(|f| f.describe().family().map(|f| f.to_ascii_lowercase()))
        .is_some_and(|f| f == key);
    let wanted = match LINE_EMS.iter().find(|(name, _)| *name == key) {
        Some(&(_, em)) if !installed => em,
        _ => actual,
    };
    CACHE.with(|c| c.borrow_mut().insert((key, generation), (actual, wanted)));
    (actual, wanted)
}

pub fn description(rl: &RunLook) -> pango::FontDescription {
    let mut desc = pango::FontDescription::from_string(&family(&rl.font));
    desc.set_size((rl.size * SCALE) as i32);
    desc.set_weight(if rl.bold { pango::Weight::Bold } else { pango::Weight::Normal });
    desc.set_style(if rl.italic { pango::Style::Italic } else { pango::Style::Normal });
    desc
}

/// The text and attributes of a paragraph's runs, as its look and theirs
/// say: what the editor shows and what is set on paper. `field` gives a
/// page number field its text.
pub fn attributed(look: &Look, runs: &[&Run], field: &dyn Fn(Field) -> String) -> (String, pango::AttrList) {
    let base = &look.run;
    let mut text = String::new();
    let attrs = pango::AttrList::new();
    for run in runs {
        let rl = run_look(run, look);
        if rl.hidden {
            continue;
        }
        let start = text.len() as u32;
        match (&run.math, run.field) {
            (Some(markup), _) => match pango::parse_markup(markup, '\0') {
                Ok((math, plain, _)) => {
                    text.push_str(&plain);
                    for mut a in math.attributes() {
                        let (s, e) = (a.start_index(), a.end_index());
                        a.set_start_index(start + s);
                        a.set_end_index(start.saturating_add(e).min(text.len() as u32));
                        attrs.insert(a);
                    }
                }
                Err(_) => text.push_str(&run.text),
            },
            (None, Some(f)) => text.push_str(&field(f)),
            _ if rl.caps => text.push_str(&run.text.to_uppercase()),
            _ => text.push_str(&run.text),
        }
        let end = text.len() as u32;
        let add = |mut a: pango::Attribute| {
            a.set_start_index(start);
            a.set_end_index(end);
            attrs.insert(a);
        };
        if rl.font != base.font {
            add(pango::AttrString::new_family(&family(&rl.font)).into());
        }
        let size = if rl.vert == Vert::Baseline { rl.size } else { rl.size * 0.65 };
        if size != base.size {
            add(pango::AttrSize::new((size * SCALE) as i32).into());
        }
        match rl.vert {
            Vert::Super => add(pango::AttrInt::new_rise((rl.size * 0.33 * SCALE) as i32).into()),
            Vert::Sub => add(pango::AttrInt::new_rise((-rl.size * 0.14 * SCALE) as i32).into()),
            Vert::Baseline => {}
        }
        if rl.bold != base.bold {
            add(pango::AttrInt::new_weight(if rl.bold { pango::Weight::Bold } else { pango::Weight::Normal }).into());
        }
        let placeholder = run.placeholder && run.math.is_none();
        if rl.italic != base.italic || placeholder {
            add(pango::AttrInt::new_style(if rl.italic || placeholder { pango::Style::Italic } else { pango::Style::Normal }).into());
        }
        if rl.underline {
            add(pango::AttrInt::new_underline(pango::Underline::Single).into());
        }
        if rl.strike {
            add(pango::AttrInt::new_strikethrough(true).into());
        }
        if rl.small_caps {
            add(pango::AttrInt::new_variant(pango::Variant::SmallCaps).into());
        }
        let color = if placeholder { Some([0x80, 0x80, 0x88]) } else { rl.color };
        let (r, g, b) = rgb16(color.unwrap_or([0, 0, 0]));
        add(pango::AttrColor::new_foreground(r, g, b).into());
        if let Some(h) = rl.highlight {
            let (r, g, b) = rgb16(h);
            add(pango::AttrColor::new_background(r, g, b).into());
        }
    }
    (text, attrs)
}

fn layout_of(ctx: &pango::Context, look: &Look, runs: &[&Run], width: f64, field: &dyn Fn(Field) -> String) -> pango::Layout {
    let layout = pango::Layout::new(ctx);
    let base = &look.run;
    layout.set_font_description(Some(&description(base)));
    layout.set_width((width.max(12.0) * SCALE) as i32);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_alignment(match look.align {
        Align::Center => pango::Alignment::Center,
        Align::End => pango::Alignment::Right,
        _ => pango::Alignment::Left,
    });
    layout.set_justify(look.align == Align::Justify);
    // With a list number the first line starts where the others do — the
    // number hangs out in front; without, the first line is indented (or
    // hangs) as the paragraph says.
    if look.label.is_none() && look.first != 0.0 {
        layout.set_indent((look.first * SCALE) as i32);
    }
    // Tab stops are measured from the margin; the layout starts at the
    // paragraph's indent.
    if !look.tabs.is_empty() {
        let tabs = pango::TabArray::new(look.tabs.len() as i32, false);
        let mut tabs = tabs;
        for (i, tab) in look.tabs.iter().enumerate() {
            let align = match tab.align {
                TabAlign::Left => pango::TabAlign::Left,
                TabAlign::Center => pango::TabAlign::Center,
                TabAlign::Right => pango::TabAlign::Right,
                TabAlign::Decimal => pango::TabAlign::Decimal,
            };
            tabs.set_tab(i as i32, align, ((tab.pos - look.left).max(0.0) * SCALE) as i32);
            if tab.align == TabAlign::Decimal {
                tabs.set_decimal_point(i as i32, '.');
            }
        }
        layout.set_tabs(Some(&tabs));
    }
    let (text, attrs) = attributed(look, runs, field);
    layout.set_text(&text);
    layout.set_attributes(Some(&attrs));
    layout
}

/// Where a picture goes across a box, as its paragraph is aligned.
fn aligned(look: &Look, w: f64, (x, width): (f64, f64)) -> f64 {
    let (x, width) = (x + look.left, (width - look.left - look.right).max(w));
    x + match look.align {
        Align::Center => (width - w) / 2.0,
        Align::End => width - w,
        _ => 0.0,
    }
}

/// Where the table's columns start, and how wide each is, set within
/// `avail`: the table as wide as it says (no wider than there is room
/// for), its columns sharing that as its grid does — or equally.
pub fn columns(t: &Table, x: f64, avail: f64) -> Vec<(f64, f64)> {
    let cols = t.columns().max(1);
    let grid: f64 = t.widths.iter().sum();
    let total = match t.width {
        Some(TableWidth::Share(k)) => avail * k,
        Some(TableWidth::Points(w)) => w,
        None if grid > 1.0 => grid,
        None => avail,
    }
    .min(avail)
    .max(12.0);
    let widths: Vec<f64> = if t.widths.len() == cols && grid > 1.0 {
        t.widths.iter().map(|w| w * total / grid).collect()
    } else {
        vec![total / cols as f64; cols]
    };
    let mut at = x;
    widths
        .into_iter()
        .map(|w| {
            let c = (at, w);
            at += w;
            c
        })
        .collect()
}

/// A row's cells: where each starts and how wide it is, spans counted.
fn cells(row: &[Cell], cols: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut at = 0;
    row.iter()
        .map(|cell| {
            let span = cell.span.max(1);
            let start = cols.get(at).map_or(cols.last().map_or(0.0, |c| c.0 + c.1), |c| c.0);
            let width: f64 = cols.iter().skip(at).take(span).map(|c| c.1).sum();
            at += span;
            (start, width.max(12.0))
        })
        .collect()
}

/// A picture's size on the page, in points: as the document says, else its
/// own.
pub fn picture_size(image: &Image) -> (f64, f64) {
    if image.cx > 0 && image.cy > 0 {
        return (image.cx as f64 / EMU_PER_POINT as f64, image.cy as f64 / EMU_PER_POINT as f64);
    }
    if let Some(size) = emf::size(&image.data) {
        return size;
    }
    surface_of(&image.data).map_or((72.0, 72.0), |s| (s.width() as f64 * 0.75, s.height() as f64 * 0.75))
}

/// A picture drawn into `x, y, w, h`: a metafile as its vectors, anything
/// else from its pixels, and one that cannot be read as its frame.
pub fn draw_picture(cr: &cairo::Context, image: &Image, x: f64, y: f64, w: f64, h: f64) {
    if emf::draw(&image.data, cr, x, y, w, h) {
        return;
    }
    let Some(picture) = surface_of(&image.data) else {
        cr.set_source_rgb(0.85, 0.85, 0.88);
        cr.set_line_width(0.75);
        cr.rectangle(x, y, w, h);
        cr.stroke().ok();
        return;
    };
    let (pw, ph) = (picture.width() as f64, picture.height() as f64);
    cr.save().ok();
    cr.translate(x, y);
    cr.scale(w / pw, h / ph);
    cr.set_source_surface(&picture, 0.0, 0.0).ok();
    cr.source().set_filter(cairo::Filter::Good);
    cr.paint().ok();
    cr.restore().ok();
}

/// A picture file decoded for drawing. A JPEG keeps its original bytes for
/// the PDF to embed as they are.
pub fn surface_of(data: &[u8]) -> Option<cairo::ImageSurface> {
    if data.is_empty() || emf::is_emf(data) {
        return None;
    }
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(data).ok()?;
    loader.close().ok()?;
    let raw = loader.pixbuf()?;
    let rotated = raw.option("orientation").is_some_and(|o| o != "1");
    let pixbuf = raw.apply_embedded_orientation().unwrap_or(raw);
    let (w, h) = (pixbuf.width(), pixbuf.height());
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
    {
        let stride = surface.stride() as usize;
        let mut out = surface.data().ok()?;
        let px = pixbuf.read_pixel_bytes();
        let (n, rs) = (pixbuf.n_channels() as usize, pixbuf.rowstride() as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = y * rs + x * n;
                let (r, g, b) = (px[s] as u32, px[s + 1] as u32, px[s + 2] as u32);
                let a = if n == 4 { px[s + 3] as u32 } else { 255 };
                let pre = |c: u32| (c * a + 127) / 255;
                let v = (a << 24) | (pre(r) << 16) | (pre(g) << 8) | pre(b);
                out[y * stride + 4 * x..y * stride + 4 * x + 4].copy_from_slice(&v.to_ne_bytes());
            }
        }
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) && !rotated {
        // The surface keeps the bytes, so it is given its own copy.
        let _ = surface.set_mime_data("image/jpeg", Vec::from(data));
    }
    Some(surface)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::Run;

    #[test]
    fn a_document_becomes_a_pdf_with_its_text_bookmarks_and_page_numbers() {
        let mut blocks = vec![
            Block::paragraph(ParaStyle::Title, vec![Run { text: "Report".into(), ..Default::default() }]),
            Block::paragraph(ParaStyle::Heading(1), vec![Run { text: "Findings".into(), ..Default::default() }]),
        ];
        for i in 0..120 {
            blocks.push(Block::paragraph(ParaStyle::Normal, vec![
                Run { text: format!("Paragraph {i} with "), ..Default::default() },
                Run { text: "bold".into(), bold: true, ..Default::default() },
                Run { text: " text — ünïcödé.".into(), ..Default::default() },
            ]));
        }
        blocks.push(Block::Table(Box::new(Table::of_text(vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]]))));
        let r = |text: &str, field: Option<Field>| Run { text: text.into(), field, ..Default::default() };
        let footer = vec![Block::paragraph(ParaStyle::Normal, vec![r("Page ", None), r("1", Some(Field::Page)), r(" of ", None), r("9", Some(Field::Pages))])];
        let sections = [Section { decor: crate::docx::Decor { footer, ..Default::default() }, ..Default::default() }];
        let pdf = pdf(&blocks, &sections, "Report").unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        let bytes = std::sync::Arc::new(pdf);
        let info = crate::pdf::load_info(&bytes).unwrap();
        let pages = info.page_sizes.len();
        assert!(pages >= 2, "120 paragraphs spill onto a second page");
        assert_eq!((info.page_sizes[0].0.round(), info.page_sizes[0].1.round()), (612.0, 792.0));
        assert!(info.outline.iter().any(|o| o.title == "Findings"), "{:?}", info.outline.iter().map(|o| &o.title).collect::<Vec<_>>());
        let layer = crate::pdftext::TextLayer::spawn(bytes);
        let last = gtk4::glib::MainContext::new().block_on(layer.page(pages - 1)).map(|p| p.text(0, p.chars.len())).unwrap_or_default();
        assert!(last.contains(&format!("Page {pages} of {pages}")), "the footer numbers the pages: {last:?}");
    }

    /// A footnote is set on the page that refers to it, below the text;
    /// endnotes come after the text.
    #[test]
    fn notes_go_at_the_foot_of_the_page_and_the_end() {
        let doc = crate::docx::load(&crate::docx::tests::with_notes()).unwrap();
        let blocks: Vec<Block> = doc.items.iter().flat_map(|i| i.blocks.clone()).collect();
        let pdf = pdf(&blocks, &doc.sections, "Notes").unwrap();
        let bytes = std::sync::Arc::new(pdf);
        let layer = crate::pdftext::TextLayer::spawn(bytes);
        let page = gtk4::glib::MainContext::new().block_on(layer.page(0)).unwrap();
        let text = page.text(0, page.chars.len());
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s:?} in {text:?}"));
        assert!(at("Claim") < at("At the end.") && at("At the end.") < at("First note."), "{text:?}");
        assert!(at("First note.") < at("Second note."), "{text:?}");
    }
}
