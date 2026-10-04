//! Documents set on paper, as PDF: the paragraphs, tables and pictures of a
//! DOCX (or of a `.doc` or text file read as one), laid out with Pango and
//! drawn with Cairo — the same text engine the rest of the desktop uses, so
//! the export needs nothing GTK does not already bring.
//!
//! Pages are the document's own size and margins. Text is set in the
//! document's font with the sizes of Word's standard styles; headings become
//! the PDF's bookmarks, so the exported file has a table of contents.
//! Headers, footers and floating shapes are not drawn.

use anyhow::{Context, Result};
use gtk4::gdk_pixbuf;
use gtk4::gdk_pixbuf::prelude::*;
use pango::prelude::*;

use crate::docx::{self, Align, Block, EMU_PER_POINT, Image, PageSetup, ParaStyle, Run};

const SCALE: f64 = pango::SCALE as f64;

/// `blocks` as a PDF on `page`'s paper.
pub fn pdf(blocks: &[Block], page: &PageSetup, title: &str) -> Result<Vec<u8>> {
    let surface = cairo::PdfSurface::for_stream(page.width, page.height, Vec::<u8>::new())
        .context("couldn’t start the PDF")?;
    let _ = surface.set_metadata(cairo::PdfMetadata::Title, title);
    let _ = surface.set_metadata(cairo::PdfMetadata::Creator, "Raven Viewer");
    let cr = cairo::Context::new(&surface).context("couldn’t draw the PDF")?;

    let fonts = pangocairo::FontMap::new();
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

    let mut w = Writer {
        cr,
        surface: surface.clone(),
        ctx,
        page: page.clone(),
        family: family(&page.font),
        y: page.margins[0],
        page_no: 1,
        blank: true,
        outline: Vec::new(),
    };
    for (i, block) in blocks.iter().enumerate() {
        match block {
            Block::Paragraph { style, runs, align } => {
                let next_is_list = matches!(blocks.get(i + 1), Some(Block::Paragraph { style: ParaStyle::ListItem(_), .. }));
                w.paragraph(*style, runs, *align, next_is_list);
            }
            Block::Table { rows } => w.table(rows),
        }
    }
    drop(w);
    surface.finish();
    let stream = surface.finish_output_stream().map_err(|e| anyhow::anyhow!("couldn’t write the PDF: {}", e.error))?;
    stream.downcast::<Vec<u8>>().map(|b| *b).map_err(|_| anyhow::anyhow!("couldn’t write the PDF"))
}

/// A font name with a generic family behind it, for when it is not
/// installed: fontconfig then picks a face of the same kind.
fn family(font: &str) -> String {
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

struct Writer {
    cr: cairo::Context,
    surface: cairo::PdfSurface,
    ctx: pango::Context,
    page: PageSetup,
    family: String,
    /// Where the next thing goes, from the top of the page, in points.
    y: f64,
    page_no: i32,
    /// Nothing has been drawn on this page yet.
    blank: bool,
    /// Open bookmarks: each heading's level and id, for nesting.
    outline: Vec<(u8, i32)>,
}

impl Writer {
    fn left(&self) -> f64 {
        self.page.margins[3]
    }

    fn width(&self) -> f64 {
        (self.page.width - self.page.margins[1] - self.page.margins[3]).max(72.0)
    }

    fn bottom(&self) -> f64 {
        self.page.height - self.page.margins[2]
    }

    fn new_page(&mut self) {
        self.cr.show_page().ok();
        self.page_no += 1;
        self.y = self.page.margins[0];
        self.blank = true;
    }

    /// Make room for `height` more points, on a new page if this one is full.
    fn room(&mut self, height: f64) {
        if !self.blank && self.y + height > self.bottom() {
            self.new_page();
        }
    }

    /// Font size, weight and colour for a paragraph style.
    fn look(&self, style: ParaStyle) -> (f64, bool, Option<[f64; 3]>, f64, f64) {
        let s = self.page.size;
        let heading = Some([0.12, 0.22, 0.39]);
        // Size, bold, colour, space before, space after.
        match style {
            ParaStyle::Title => (s * 28.0 / 11.0, false, None, 0.0, 12.0),
            ParaStyle::Heading(1) => (s * 16.0 / 11.0, true, heading, 18.0, 4.0),
            ParaStyle::Heading(2) => (s * 13.0 / 11.0, true, heading, 10.0, 4.0),
            ParaStyle::Heading(3) => (s * 12.0 / 11.0, true, heading, 8.0, 4.0),
            ParaStyle::Heading(_) => (s, true, heading, 6.0, 2.0),
            _ => (s, false, None, 0.0, self.page.after),
        }
    }

    fn paragraph(&mut self, style: ParaStyle, runs: &[Run], align: Align, next_is_list: bool) {
        let (size, bold, color, before, mut after) = self.look(style);
        if matches!(style, ParaStyle::ListItem(_)) && next_is_list {
            after = 0.0;
        }
        if !self.blank {
            self.y += before;
        }
        // Pictures and page breaks split the paragraph; the text between
        // them is laid out as one.
        let mut text: Vec<&Run> = Vec::new();
        let mut first = true;
        for run in runs {
            if let Some(image) = &run.image {
                if !text.is_empty() {
                    self.text(style, &std::mem::take(&mut text), align, size, bold, color, first);
                    first = false;
                }
                self.image(image, align);
            } else if run.is_page_break() {
                if !text.is_empty() {
                    self.text(style, &std::mem::take(&mut text), align, size, bold, color, first);
                    first = false;
                }
                self.new_page();
            } else {
                text.push(run);
            }
        }
        let only_pictures = !runs.is_empty() && runs.iter().all(|r| r.image.is_some() || r.is_page_break());
        if !text.is_empty() || !only_pictures {
            self.text(style, &text, align, size, bold, color, first);
        }
        self.y += after;
    }

    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, style: ParaStyle, runs: &[&Run], align: Align, size: f64, bold: bool, color: Option<[f64; 3]>, first: bool) {
        let (indent, right) = match style {
            ParaStyle::ListItem(level) => (18.0 * (level as f64 + 1.0), 0.0),
            ParaStyle::Quote => (36.0, 36.0),
            _ => (0.0, 0.0),
        };
        let layout = pango::Layout::new(&self.ctx);
        let mut desc = pango::FontDescription::from_string(&self.family);
        desc.set_size((size * SCALE) as i32);
        if bold {
            desc.set_weight(pango::Weight::Bold);
        }
        if style == ParaStyle::Quote {
            desc.set_style(pango::Style::Italic);
        }
        layout.set_font_description(Some(&desc));
        layout.set_width(((self.width() - indent - right) * SCALE) as i32);
        layout.set_wrap(pango::WrapMode::WordChar);
        layout.set_line_spacing(self.page.line.max(1.0) as f32);
        layout.set_alignment(match align {
            Align::Center => pango::Alignment::Center,
            Align::End => pango::Alignment::Right,
            _ => pango::Alignment::Left,
        });
        layout.set_justify(align == Align::Justify);

        let mut text = String::new();
        let attrs = pango::AttrList::new();
        for run in runs {
            let start = text.len() as u32;
            text.push_str(&run.text);
            let end = text.len() as u32;
            let add = |mut a: pango::Attribute| {
                a.set_start_index(start);
                a.set_end_index(end);
                attrs.insert(a);
            };
            if run.bold {
                add(pango::AttrInt::new_weight(pango::Weight::Bold).into());
            }
            if run.italic || run.placeholder {
                add(pango::AttrInt::new_style(pango::Style::Italic).into());
            }
            if run.underline {
                add(pango::AttrInt::new_underline(pango::Underline::Single).into());
            }
            if run.highlight {
                add(pango::AttrColor::new_background(0xFFFF, 0xFFFF, 0).into());
            }
            if run.placeholder {
                add(pango::AttrColor::new_foreground(0x8000, 0x8000, 0x8800).into());
            }
            if docx::run_prop(&run.props, "strike").is_some() {
                add(pango::AttrInt::new_strikethrough(true).into());
            }
            if let Some(sz) = docx::run_prop(&run.props, "sz").and_then(|v| v.parse::<f64>().ok()) {
                add(pango::AttrSize::new((sz / 2.0 * SCALE) as i32).into());
            }
            if let Some(font) = docx::run_font(&run.props) {
                add(pango::AttrString::new_family(&family(&font)).into());
            }
            if let Some([r, g, b]) = docx::run_prop(&run.props, "color").and_then(|v| docx::hex_color(&v)) {
                add(pango::AttrColor::new_foreground(r, g, b).into());
            }
        }
        layout.set_text(&text);
        layout.set_attributes(Some(&attrs));

        // A heading stays with what follows it.
        let height = layout.size().1 as f64 / SCALE;
        if matches!(style, ParaStyle::Heading(_) | ParaStyle::Title) {
            self.room(height + 3.0 * self.page.size);
            self.bookmark(style, &text);
        }

        let (r, g, b) = color.map_or((0.0, 0.0, 0.0), |c| (c[0], c[1], c[2]));
        let x = self.left() + indent;
        let mut iter = layout.iter();
        let mut origin = self.y;
        let mut first_line = true;
        loop {
            let (_, logical) = iter.line_extents();
            let top = logical.y() as f64 / SCALE;
            let line_height = logical.height() as f64 / SCALE;
            if !self.blank && origin + top + line_height > self.bottom() {
                self.new_page();
                origin = self.y - top;
            }
            let baseline = iter.baseline() as f64 / SCALE;
            if let Some(line) = iter.line_readonly() {
                self.cr.set_source_rgb(r, g, b);
                if first_line && first && let ParaStyle::ListItem(level) = style {
                    self.bullet(level, x - 14.0, origin + baseline, size);
                    self.cr.set_source_rgb(r, g, b);
                }
                self.cr.move_to(x + logical.x() as f64 / SCALE, origin + baseline);
                pangocairo::functions::show_layout_line(&self.cr, &line);
            }
            self.blank = false;
            self.y = origin + top + line_height;
            first_line = false;
            if !iter.next_line() {
                break;
            }
        }
    }

    fn bullet(&self, level: u8, x: f64, baseline: f64, size: f64) {
        let layout = pango::Layout::new(&self.ctx);
        let mut desc = pango::FontDescription::from_string(&self.family);
        desc.set_size((size * SCALE) as i32);
        layout.set_font_description(Some(&desc));
        layout.set_text(["•", "◦", "▪"][level as usize % 3]);
        let baseline_of = layout.baseline() as f64 / SCALE;
        self.cr.move_to(x, baseline - baseline_of);
        pangocairo::functions::show_layout(&self.cr, &layout);
    }

    /// A heading in the PDF's bookmarks, under the last heading above it.
    fn bookmark(&mut self, style: ParaStyle, text: &str) {
        let level = match style {
            ParaStyle::Title => 1,
            ParaStyle::Heading(n) => n,
            _ => return,
        };
        let name: String = text.chars().filter(|c| !c.is_control() && *c != '\u{FFFC}').collect();
        if name.trim().is_empty() {
            return;
        }
        while self.outline.last().is_some_and(|(l, _)| *l >= level) {
            self.outline.pop();
        }
        let parent = self.outline.last().map_or(0, |(_, id)| *id);
        let link = format!("page={} pos=[{:.1} {:.1}]", self.page_no, self.left(), self.y);
        if let Ok(id) = self.surface.add_outline(parent, name.trim(), &link, cairo::PdfOutline::empty()) {
            self.outline.push((level, id));
        }
    }

    fn image(&mut self, image: &Image, align: Align) {
        let Some(picture) = surface_of(&image.data) else {
            // A metafile or a missing picture: its frame, so the layout holds.
            let (w, h) = (image.cx as f64 / EMU_PER_POINT as f64, image.cy as f64 / EMU_PER_POINT as f64);
            if w > 0.0 && h > 0.0 {
                let (w, h) = self.fit(w, h);
                self.room(h);
                let x = self.aligned(align, w);
                self.cr.set_source_rgb(0.85, 0.85, 0.88);
                self.cr.set_line_width(0.75);
                self.cr.rectangle(x, self.y, w, h);
                self.cr.stroke().ok();
                self.y += h;
                self.blank = false;
            }
            return;
        };
        let (pw, ph) = (picture.width() as f64, picture.height() as f64);
        let (w, h) = if image.cx > 0 && image.cy > 0 {
            (image.cx as f64 / EMU_PER_POINT as f64, image.cy as f64 / EMU_PER_POINT as f64)
        } else {
            (pw * 0.75, ph * 0.75)
        };
        let (w, h) = self.fit(w, h);
        self.room(h);
        let x = self.aligned(align, w);
        self.cr.save().ok();
        self.cr.translate(x, self.y);
        self.cr.scale(w / pw, h / ph);
        self.cr.set_source_surface(&picture, 0.0, 0.0).ok();
        self.cr.source().set_filter(cairo::Filter::Good);
        self.cr.paint().ok();
        self.cr.restore().ok();
        self.y += h;
        self.blank = false;
    }

    /// A picture's size, shrunk to fit the text width and the page.
    fn fit(&self, w: f64, h: f64) -> (f64, f64) {
        let max_h = self.bottom() - self.page.margins[0];
        let scale = (self.width() / w).min(max_h / h).min(1.0);
        (w * scale, h * scale)
    }

    fn aligned(&self, align: Align, w: f64) -> f64 {
        self.left()
            + match align {
                Align::Center => (self.width() - w) / 2.0,
                Align::End => self.width() - w,
                _ => 0.0,
            }
    }

    fn table(&mut self, rows: &[Vec<String>]) {
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let pad = 4.0;
        let col = self.width() / cols as f64;
        let mut desc = pango::FontDescription::from_string(&self.family);
        desc.set_size((self.page.size * SCALE) as i32);
        for row in rows {
            let layouts: Vec<pango::Layout> = (0..cols)
                .map(|c| {
                    let l = pango::Layout::new(&self.ctx);
                    l.set_font_description(Some(&desc));
                    l.set_width(((col - 2.0 * pad) * SCALE) as i32);
                    l.set_wrap(pango::WrapMode::WordChar);
                    l.set_text(row.get(c).map_or("", String::as_str));
                    l
                })
                .collect();
            let height = layouts.iter().map(|l| l.size().1 as f64 / SCALE).fold(0.0, f64::max) + 2.0 * pad;
            self.room(height);
            for (c, layout) in layouts.iter().enumerate() {
                let x = self.left() + col * c as f64;
                self.cr.set_source_rgb(0.0, 0.0, 0.0);
                self.cr.move_to(x + pad, self.y + pad);
                pangocairo::functions::show_layout(&self.cr, layout);
                self.cr.set_line_width(0.5);
                self.cr.rectangle(x, self.y, col, height);
                self.cr.stroke().ok();
            }
            self.y += height;
            self.blank = false;
        }
        self.y += self.page.after.max(6.0);
    }
}

/// A picture file decoded for drawing. A JPEG keeps its original bytes for
/// the PDF to embed as they are.
fn surface_of(data: &[u8]) -> Option<cairo::ImageSurface> {
    if data.is_empty() {
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
    fn a_document_becomes_a_pdf_with_its_text_and_bookmarks() {
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
        blocks.push(Block::Table { rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]] });
        let pdf = pdf(&blocks, &PageSetup::default(), "Report").unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        let info = crate::pdf::load_info(&std::sync::Arc::new(pdf)).unwrap();
        assert!(info.page_sizes.len() >= 2, "120 paragraphs spill onto a second page");
        assert_eq!((info.page_sizes[0].0.round(), info.page_sizes[0].1.round()), (612.0, 792.0));
        assert!(info.outline.iter().any(|o| o.title == "Findings"), "{:?}", info.outline.iter().map(|o| &o.title).collect::<Vec<_>>());
    }
}
