//! Where the text on a page is. hayro hands every glyph it draws to a
//! `Device` with its Unicode value and transform; collecting those instead of
//! painting them gives each character a box, which is what selecting, copying
//! and highlighting need. lopdf's text extraction has the words but not where
//! they are.
//!
//! Boxes are kept twice: as fractions of the page as displayed (top-left
//! origin, rotation applied), which is what the view hit-tests and draws in,
//! and in PDF user space, which is what an annotation's QuadPoints are in.

use std::sync::{Arc, mpsc};

use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, Paint,
    PathDrawMode, SoftMask, interpret_page,
};
use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::page::Page;
use hayro::vello_cpu::kurbo::{Affine, BezPath, Point, Rect};

/// One character on the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Char {
    /// Usually one character; a ligature glyph maps to several.
    pub text: String,
    /// (x0, y0, x1, y1) as fractions of the displayed page, y down.
    pub rect: [f32; 4],
    /// The same box in PDF user space, y up.
    pub user: [f32; 4],
    /// Which way the text runs on screen, in quarter turns clockwise from
    /// left-to-right.
    pub turn: u8,
}

/// A run of characters on one baseline, `start..end` into `PageText::chars`.
/// Its box is in the page's flow frame, not on screen: see `PageText::turn`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    pub start: usize,
    pub end: usize,
    rect: [f32; 4],
}

/// The text of a page in content-stream order, which is reading order for
/// nearly every producer (columns included) — sorting by position would
/// interleave the columns of a two-column paper.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageText {
    pub chars: Vec<Char>,
    pub lines: Vec<Line>,
    /// Lines are found and hit-tested in a frame turned so the page's text
    /// runs left to right, top to bottom. On a page shown turned (/Rotate, or
    /// a landscape table set sideways) "the next character" is further down
    /// the screen, not further right.
    turn: u8,
    /// Each character's box in that frame.
    flow: Vec<[f32; 4]>,
}

pub fn extract(page: &Page<'_>) -> PageText {
    let cache = InterpreterCache::new();
    let settings = InterpreterSettings { render_annotations: false, ..Default::default() };
    // Interpreting in user space keeps the boxes in the coordinates an
    // annotation needs; the display fractions are derived from them below.
    let crop = page.intersected_crop_box();
    let bbox = Rect::new(crop.x0, crop.y0, crop.x1, crop.y1);
    let mut context = Context::new(Affine::IDENTITY, bbox, &cache, page.xref(), settings);
    let mut device = Collector { chars: Vec::new() };
    interpret_page(page, &mut context, &mut device);

    let display = page.initial_transform(true).to_kurbo();
    let (width, height) = page.render_dimensions();
    let (width, height) = (width.max(1.0) as f64, height.max(1.0) as f64);
    let chars = device
        .chars
        .into_iter()
        .map(|(text, user, advance)| {
            let shown = display.transform_rect_bbox(user);
            let along = display * Point::new(advance.x, advance.y) - display * Point::ORIGIN;
            let turn = (along.y.atan2(along.x).to_degrees() / 90.0).round().rem_euclid(4.0) as u8;
            Char {
                turn,
                text,
                rect: [
                    (shown.x0 / width) as f32,
                    (shown.y0 / height) as f32,
                    (shown.x1 / width) as f32,
                    (shown.y1 / height) as f32,
                ],
                user: [user.x0 as f32, user.y0 as f32, user.x1 as f32, user.y1 as f32],
            }
        })
        .collect();
    PageText::new(chars)
}

struct Collector {
    /// Text, box and the direction of its advance, all in user space.
    chars: Vec<(String, Rect, hayro::vello_cpu::kurbo::Vec2)>,
}

impl<'a> Device<'a> for Collector {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}

    /// Invisible glyphs count: that is the text layer of an OCR'd scan.
    fn draw_glyph(&mut self, glyph: &Glyph<'a>, transform: Affine, glyph_transform: Affine, _: &Paint<'a>, _: &GlyphDrawMode) {
        let text = match glyph.as_unicode() {
            Some(BfString::Char(c)) => c.to_string(),
            Some(BfString::String(s)) => s,
            None => return,
        };
        if text.chars().all(char::is_control) {
            return;
        }
        // Copied text is pasted into editors and search boxes, where "ﬁt"
        // is not the word "fit".
        let text = match text.as_str() {
            "ﬀ" => "ff".into(),
            "ﬁ" => "fi".into(),
            "ﬂ" => "fl".into(),
            "ﬃ" => "ffi".into(),
            "ﬄ" => "ffl".into(),
            "ﬅ" | "ﬆ" => "st".into(),
            _ => text,
        };
        // Glyph space is 1000 units to the em. The box is the advance by the
        // em, not the ink: a full stop then is as easy to hit as an M, and
        // neighbouring boxes meet so a selection has no gaps.
        let advance = match glyph {
            Glyph::Outline(o) => o.advance_width().filter(|w| *w > 0.0).unwrap_or(500.0),
            Glyph::Type3(_) => 500.0,
        } as f64;
        let to_user = transform * glyph_transform;
        let corners = [(0.0, -200.0), (advance, -200.0), (0.0, 800.0), (advance, 800.0)].map(|(x, y)| to_user * Point::new(x, y));
        let rect = corners.iter().skip(1).fold(Rect::from_points(corners[0], corners[0]), |r, p| r.union_pt(*p));
        if rect.width().is_finite() && rect.height().is_finite() && rect.area() > 0.0 {
            let advance = to_user * Point::new(1000.0, 0.0) - to_user * Point::ORIGIN;
            self.chars.push((text, rect, advance));
        }
    }
}

impl PageText {
    pub fn new(chars: Vec<Char>) -> Self {
        let mut votes = [0usize; 4];
        for c in &chars {
            votes[c.turn as usize % 4] += 1;
        }
        let turn = (0..4u8).max_by_key(|&t| (votes[t as usize], std::cmp::Reverse(t))).unwrap_or(0);
        let flow: Vec<[f32; 4]> = chars.iter().map(|c| to_flow(turn, c.rect)).collect();
        let lines = lines(&flow);
        Self { chars, lines, turn, flow }
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// The caret position nearest a point given as page fractions: `i` is
    /// just before character `i`, `chars.len()` after the last. `None` when
    /// the page has no text.
    pub fn caret_at(&self, x: f32, y: f32) -> Option<usize> {
        let (x, y) = flow_point(self.turn, x, y);
        let line = self.line_at(x, y)?;
        let boxes = &self.flow[line.start..line.end];
        let within = boxes.iter().position(|r| x < (r[0] + r[2]) / 2.0).unwrap_or(boxes.len());
        Some(line.start + within)
    }

    /// Whether the point is over text, for the I-beam cursor.
    pub fn is_over_text(&self, x: f32, y: f32) -> bool {
        let (x, y) = flow_point(self.turn, x, y);
        self.lines.iter().any(|l| contains(l.rect, x, y))
    }

    /// The line under a point in the flow frame, or the nearest one. Of lines at the same
    /// height — the columns of a paper — the horizontally nearest wins.
    fn line_at(&self, x: f32, y: f32) -> Option<&Line> {
        let distance = |l: &Line| {
            let dy = (l.rect[1] - y).max(y - l.rect[3]).max(0.0);
            let dx = (l.rect[0] - x).max(x - l.rect[2]).max(0.0);
            (dy, dx)
        };
        self.lines.iter().min_by(|a, b| distance(a).partial_cmp(&distance(b)).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// The word around a caret, for a double-click.
    pub fn word_at(&self, caret: usize) -> (usize, usize) {
        let is_word = |i: usize| self.chars.get(i).is_some_and(|c| c.text.chars().any(char::is_alphanumeric));
        let Some(line) = self.lines.iter().find(|l| l.start <= caret && caret < l.end.max(l.start + 1)) else {
            return (caret, caret);
        };
        let at = caret.min(line.end - 1);
        if !is_word(at) {
            return (at, at + 1);
        }
        let joined = |a: usize, b: usize| !self.gap_between(a, b);
        let mut start = at;
        while start > line.start && is_word(start - 1) && joined(start - 1, start) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < line.end && is_word(end) && joined(end - 1, end) {
            end += 1;
        }
        (start, end)
    }

    /// The whole line around a caret, for a triple-click.
    pub fn line_around(&self, caret: usize) -> (usize, usize) {
        self.lines
            .iter()
            .find(|l| l.start <= caret && caret < l.end)
            .or(self.lines.last().filter(|l| caret >= l.end))
            .map_or((caret, caret), |l| (l.start, l.end))
    }

    /// The text from caret `from` to caret `to`, with the spaces and line
    /// breaks the PDF only implies put back: most producers position words
    /// rather than drawing a space between them.
    pub fn text(&self, from: usize, to: usize) -> String {
        let (from, to) = (from.min(to), to.max(from).min(self.chars.len()));
        let mut out = String::new();
        for i in from..to {
            if i > from {
                if self.line_break_before(i) {
                    out.truncate(out.trim_end_matches(' ').len());
                    out.push('\n');
                } else if self.gap_between(i - 1, i) && !out.ends_with(' ') && self.chars[i].text != " " {
                    out.push(' ');
                }
            }
            out.push_str(&self.chars[i].text);
        }
        out.truncate(out.trim_end_matches(' ').len());
        out
    }

    /// One box per line piece covering `from..to`, as page fractions — what
    /// the view paints for a selection.
    pub fn boxes(&self, from: usize, to: usize) -> Vec<[f32; 4]> {
        self.spans(from, to).map(|(s, e)| union(self.chars[s..e].iter().map(|c| c.rect))).collect()
    }

    /// The same pieces in PDF user space — an annotation's QuadPoints.
    pub fn user_boxes(&self, from: usize, to: usize) -> Vec<[f32; 4]> {
        self.spans(from, to).map(|(s, e)| union(self.chars[s..e].iter().map(|c| c.user))).collect()
    }

    /// The part of each line inside `from..to`, trimmed of spaces at either
    /// end — a line that is only spaces (producers draw plenty) would
    /// otherwise be marked as a stub of colour in the margin.
    fn spans(&self, from: usize, to: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
        let (from, to) = (from.min(to), to.max(from).min(self.chars.len()));
        let blank = |i: usize| self.chars[i].text.trim().is_empty();
        self.lines.iter().filter_map(move |l| {
            let (mut s, mut e) = (l.start.max(from), l.end.min(to));
            while s < e && blank(s) {
                s += 1;
            }
            while e > s && blank(e - 1) {
                e -= 1;
            }
            (s < e).then_some((s, e))
        })
    }

    fn line_break_before(&self, i: usize) -> bool {
        self.lines.iter().any(|l| l.start == i)
    }

    /// Whether two neighbouring characters on a line have a word space
    /// between them that was drawn as a gap rather than a space glyph.
    fn gap_between(&self, a: usize, b: usize) -> bool {
        let (ra, rb) = (self.flow[a], self.flow[b]);
        let height = (ra[3] - ra[1]).max(rb[3] - rb[1]);
        rb[0] - ra[2] > height * 0.12 || self.chars[a].text == " " || self.chars[b].text == " "
    }
}

/// Characters follow on one line while they sit at the same height and keep
/// moving right. A drop to the next line, or a jump back left, starts
/// another.
fn lines(boxes: &[[f32; 4]]) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    for (i, &r) in boxes.iter().enumerate() {
        let same = lines.last().is_some_and(|l| {
            // Half the glyph inside the line's band keeps sub- and
            // superscripts on their line — even a subscript straight after a
            // superscript, which barely overlap each other.
            let height = r[3] - r[1];
            let overlap = l.rect[3].min(r[3]) - l.rect[1].max(r[1]);
            overlap > height * 0.5 && r[0] > l.rect[0] - height * 0.5
        });
        match lines.last_mut() {
            Some(line) if same => {
                line.end = i + 1;
                line.rect = union([line.rect, r]);
            }
            _ => lines.push(Line { start: i, end: i + 1, rect: r }),
        }
    }
    lines
}

/// A point on the displayed page, in the frame where text flowing `turn`
/// quarter turns clockwise runs left to right and its lines stack downwards.
fn flow_point(turn: u8, x: f32, y: f32) -> (f32, f32) {
    match turn % 4 {
        0 => (x, y),
        1 => (y, 1.0 - x),
        2 => (1.0 - x, 1.0 - y),
        _ => (1.0 - y, x),
    }
}

fn to_flow(turn: u8, r: [f32; 4]) -> [f32; 4] {
    let (ax, ay) = flow_point(turn, r[0], r[1]);
    let (bx, by) = flow_point(turn, r[2], r[3]);
    [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)]
}

fn union(rects: impl IntoIterator<Item = [f32; 4]>) -> [f32; 4] {
    rects
        .into_iter()
        .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
        .unwrap_or_default()
}

fn contains(r: [f32; 4], x: f32, y: f32) -> bool {
    (r[0]..=r[2]).contains(&x) && (r[1]..=r[3]).contains(&y)
}

struct Request {
    page: usize,
    reply: async_channel::Sender<Arc<PageText>>,
}

/// Text layout on its own thread: interpreting a page costs about what
/// drawing it does, and must not stall scrolling. Pages are asked for one at
/// a time as they come on screen.
pub struct TextLayer {
    tx: mpsc::Sender<Request>,
}

impl TextLayer {
    pub fn spawn(bytes: Arc<Vec<u8>>) -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        let _ = std::thread::Builder::new().name("raven-text".into()).spawn(move || {
            let Ok(pdf) = Pdf::new(bytes) else { return };
            let pages = pdf.pages();
            while let Ok(req) = rx.recv() {
                let text = pages.get(req.page).map(extract).unwrap_or_default();
                if req.reply.send_blocking(Arc::new(text)).is_err() {
                    continue; // the asker went away; keep serving the rest
                }
            }
        });
        Self { tx }
    }

    pub async fn page(&self, page: usize) -> Option<Arc<PageText>> {
        let (reply, answer) = async_channel::bounded(1);
        self.tx.send(Request { page, reply }).ok()?;
        answer.recv().await.ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One page, Helvetica 12pt: two lines, the second built TeX-style from
    /// positioned words with no space glyphs between them.
    pub(crate) fn sample_pdf(rotate: i64) -> Vec<u8> {
        use lopdf::{Document, Object, Stream, dictionary};
        let ops = b"BT /F1 12 Tf 72 700 Td (Hello world) Tj ET \
                    BT /F1 12 Tf 72 680 Td [(Second) -900 (line)] TJ ET"
            .to_vec();
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let content = doc.add_object(Stream::new(dictionary! {}, ops));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content, "Rotate" => rotate,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        });
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
        bytes
    }

    fn page_text(bytes: Vec<u8>) -> PageText {
        let pdf = Pdf::new(bytes).unwrap();
        extract(&pdf.pages()[0])
    }

    #[test]
    fn finds_the_text_and_where_it_is() {
        let text = page_text(sample_pdf(0));
        assert_eq!(text.text(0, text.chars.len()), "Hello world\nSecond line");
        assert_eq!(text.lines.len(), 2);
        // "H" starts at x = 72pt on a 612pt page, its baseline 92pt from the top.
        let h = &text.chars[0];
        assert_eq!(h.text, "H");
        assert!((h.rect[0] - 72.0 / 612.0).abs() < 1e-3, "H starts at {:?}", h.rect);
        assert!(h.rect[1] < 92.0 / 792.0 && h.rect[3] > 92.0 / 792.0, "the box straddles the baseline");
        assert!((h.user[0] - 72.0).abs() < 0.01 && h.user[1] < 700.0 && h.user[3] > 700.0);
    }

    #[test]
    fn hit_testing_finds_carets_words_and_lines() {
        let text = page_text(sample_pdf(0));
        let (w, _) = (612.0, 792.0);
        // Just left of the first glyph's middle: the caret before it.
        let first = &text.chars[0];
        let mid_y = (first.rect[1] + first.rect[3]) / 2.0;
        assert_eq!(text.caret_at(first.rect[0] + 0.001, mid_y), Some(0));
        // Far right of the first line: after its last character.
        assert_eq!(text.caret_at(500.0 / w, mid_y), Some(text.lines[0].end));
        // Double-clicking inside "world" takes the word and nothing else.
        let (s, e) = text.word_at(8);
        assert_eq!(text.text(s, e), "world");
        // The TJ gap counts as a word boundary even with no space glyph.
        let second = text.lines[1];
        let (s, e) = text.word_at(second.start + 1);
        assert_eq!(text.text(s, e), "Second");
        let (s, e) = text.line_around(second.start + 2);
        assert_eq!(text.text(s, e), "Second line");
        // One selection box per line it crosses.
        assert_eq!(text.boxes(3, second.start + 3).len(), 2);
        // The space alone between "Hello" and "world" gets no box.
        assert!(text.boxes(5, 6).is_empty());
    }

    /// A page turned by /Rotate still reads left to right on screen, and the
    /// boxes in user space are where an annotation must go.
    #[test]
    fn rotated_pages_hit_test_as_displayed() {
        let text = page_text(sample_pdf(90));
        assert_eq!(text.text(0, text.chars.len()), "Hello world\nSecond line");
        let h = &text.chars[0];
        assert_eq!(h.turn, 1, "turned clockwise, the text runs down the screen");
        // Turned clockwise, the page's left edge is its top: "H", 72pt in
        // from the left, sits 72pt down a page now 612pt tall.
        assert!((h.rect[1] - 72.0 / 612.0).abs() < 1e-3, "{:?}", h.rect);
        assert!((h.user[0] - 72.0).abs() < 0.01, "user space is unrotated: {:?}", h.user);
        // Hit-testing follows the text round: a point just above the first
        // glyph's middle on screen is the caret before it.
        let (x, y) = ((h.rect[0] + h.rect[2]) / 2.0, h.rect[1] + 0.001);
        assert_eq!(text.caret_at(x, y), Some(0));
        assert_eq!(text.boxes(0, 5).len(), 1, "\"Hello\" is one box, however it is turned");
    }

    /// `RAVEN_TEST_PDF=… RAVEN_TEST_PAGE=1 cargo test text_layer -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn text_layer() {
        let bytes = std::fs::read(std::env::var("RAVEN_TEST_PDF").unwrap()).unwrap();
        let page: usize = std::env::var("RAVEN_TEST_PAGE").ok().and_then(|p| p.parse().ok()).unwrap_or(1);
        let pdf = Pdf::new(bytes).unwrap();
        let t = std::time::Instant::now();
        let text = extract(&pdf.pages()[page - 1]);
        eprintln!("{} chars, {} lines in {:?}", text.chars.len(), text.lines.len(), t.elapsed());
        eprintln!("{}", text.text(0, text.chars.len()).chars().take(1500).collect::<String>());
    }
}
