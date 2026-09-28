//! Lays a DOCX out as a reflowing text sheet — headings by weight and size,
//! emphasis as tags, lists with bullets, tables as grids — that can be read
//! or edited in place.
//!
//! Each paragraph of the file starts with an invisible mark naming it, so on
//! save every line of the buffer can be traced to the paragraph it came from:
//! unchanged ones are written back as they were, edited ones are rewritten
//! from that original, and lines with no origin are new. Tables and
//! paragraphs holding what cannot be edited as text (images, equations,
//! fields) are shown but not editable, so they cannot be damaged.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use libadwaita as adw;

use crate::docx::{Block, Docx, ItemKind, LINE_BREAK, Out, ParaStyle, Run, normalize};

const BULLET_TAG: &str = "bullet";
const LOCKED_TAG: &str = "locked";
const PLACEHOLDER_TAG: &str = "placeholder";

/// Character formatting the toolbar toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub highlight: bool,
}

impl Inline {
    fn of(tags: &[gtk::TextTag]) -> Self {
        let has = |n: &str| tags.iter().any(|t| t.name().as_deref() == Some(n));
        Inline { bold: has("bold"), italic: has("italic"), underline: has("underline"), highlight: has("highlight") }
    }

    fn names(self) -> impl Iterator<Item = &'static str> {
        [(self.bold, "bold"), (self.italic, "italic"), (self.underline, "underline"), (self.highlight, "highlight")]
            .into_iter()
            .filter_map(|(on, n)| on.then_some(n))
    }

    fn get(self, name: &str) -> bool {
        match name {
            "bold" => self.bold,
            "italic" => self.italic,
            "underline" => self.underline,
            _ => self.highlight,
        }
    }

    fn set(&mut self, name: &str, on: bool) {
        match name {
            "bold" => self.bold = on,
            "italic" => self.italic = on,
            "underline" => self.underline = on,
            _ => self.highlight = on,
        }
    }
}

struct Inner {
    doc: Docx,
    widget: gtk::ScrolledWindow,
    sheet: gtk::Box,
    view: gtk::TextView,
    buffer: gtk::TextBuffer,
    /// Set while the view changes its own text (building, bullets), so the
    /// edit handlers leave that alone.
    quiet: Cell<bool>,
    /// The paragraph style where text is being inserted, taken before the
    /// insertion lands.
    inserting_into: Cell<ParaStyle>,
    /// Formatting chosen with nothing selected, for the next thing typed.
    typing: Cell<Option<Inline>>,
    on_format: RefCell<Option<FormatListener>>,
}

type FormatListener = Box<dyn Fn(ParaStyle, Inline)>;

#[derive(Clone)]
pub struct DocxView {
    inner: Rc<Inner>,
}

impl DocxView {
    pub fn new(doc: Docx) -> Self {
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .pixels_below_lines(6)
            .build();
        let buffer = view.buffer();
        make_tags(&buffer);

        let sheet = gtk::Box::builder().css_classes(["docx-sheet"]).margin_top(28).margin_bottom(28).build();
        sheet.append(&view);
        view.set_hexpand(true);
        let clamp = adw::Clamp::builder().maximum_size(820).tightening_threshold(600).child(&sheet).build();
        let widget = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .css_classes(["canvas", "docx-view"])
            .child(&clamp)
            .build();

        let this = DocxView {
            inner: Rc::new(Inner {
                doc,
                widget,
                sheet,
                view,
                buffer,
                quiet: Cell::new(false),
                inserting_into: Cell::new(ParaStyle::Normal),
                typing: Cell::new(None),
                on_format: RefCell::new(None),
            }),
        };
        this.fill();
        this.connect_editing();
        this
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.widget
    }

    pub fn text_view(&self) -> &gtk::TextView {
        &self.inner.view
    }

    pub fn buffer(&self) -> &gtk::TextBuffer {
        &self.inner.buffer
    }

    pub fn set_editable(&self, on: bool) {
        self.inner.view.set_editable(on);
        self.inner.view.set_cursor_visible(on);
        if on {
            self.inner.sheet.add_css_class("editing");
            self.inner.view.grab_focus();
        } else {
            self.inner.sheet.remove_css_class("editing");
        }
    }

    pub fn is_modified(&self) -> bool {
        self.inner.buffer.is_modified()
    }

    pub fn set_unmodified(&self) {
        self.inner.buffer.set_modified(false);
    }

    /// Fired when the formatting under the cursor changes, for the toolbar.
    pub fn connect_format_changed(&self, f: impl Fn(ParaStyle, Inline) + 'static) {
        *self.inner.on_format.borrow_mut() = Some(Box::new(f));
        self.report_format();
    }

    fn fill(&self) {
        let (view, buffer) = (&self.inner.view, &self.inner.buffer);
        self.inner.quiet.set(true);
        buffer.begin_irreversible_action();
        for (i, item) in self.inner.doc.items.iter().enumerate() {
            if !item.visible() {
                continue;
            }
            let start = buffer.end_iter();
            buffer.create_mark(Some(&mark_name(i)), &start, true);
            let from = buffer.create_mark(None, &start, true);
            for block in &item.blocks {
                match block {
                    Block::Paragraph { style, runs } => append_paragraph(buffer, *style, runs),
                    Block::Table { rows } => {
                        let mut end = buffer.end_iter();
                        let anchor = buffer.create_child_anchor(&mut end);
                        view.add_child_at_anchor(&table(rows), &anchor);
                        buffer.insert(&mut buffer.end_iter(), "\n");
                    }
                }
            }
            if item.kind != ItemKind::Paragraph {
                // Locked text cannot be edited, so it cannot lose this tag
                // either: it names the item wherever the text ends up.
                let own = gtk::TextTag::new(Some(&lock_name(i)));
                buffer.tag_table().add(&own);
                let (s, e) = (buffer.iter_at_mark(&from), buffer.end_iter());
                buffer.apply_tag(&own, &s, &e);
                buffer.apply_tag_by_name(LOCKED_TAG, &s, &e);
            }
            buffer.delete_mark(&from);
        }
        buffer.end_irreversible_action();
        buffer.set_modified(false);
        buffer.place_cursor(&buffer.start_iter());
        self.inner.quiet.set(false);
    }

    // ── Reading the buffer back ─────────────────────────────────────────

    /// The document as the buffer now has it, paragraph by paragraph.
    pub fn to_out(&self) -> Vec<Out> {
        let buffer = &self.inner.buffer;
        let doc = &self.inner.doc;
        let locked = buffer.tag_table().lookup(LOCKED_TAG);
        struct Line {
            origins: Vec<usize>,
            /// The locked item this line shows, if it shows one.
            locked_item: Option<usize>,
            locked: bool,
            style: ParaStyle,
            runs: Vec<Run>,
        }
        let mut lines: Vec<Line> = Vec::new();
        let count = buffer.line_count();
        for n in 0..count {
            let Some(start) = buffer.iter_at_line(n) else { continue };
            let mut end = start;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            // The buffer ends with the last paragraph's newline; the empty
            // "line" after it is not a paragraph.
            if n == count - 1 && start == end && n > 0 {
                break;
            }
            let origins: Vec<usize> = start.marks().iter().filter_map(|m| item_of(m.name()?.as_str())).collect();
            let is_locked = locked.as_ref().is_some_and(|t| start.has_tag(t));
            let locked_item = start.tags().iter().find_map(|t| item_of_lock(t.name()?.as_str()));
            let (style, runs) = if is_locked { (ParaStyle::Normal, Vec::new()) } else { read_line(buffer, start, end) };
            lines.push(Line { origins, locked_item, locked: is_locked, style, runs });
        }

        let original = |i: usize| match doc.items[i].blocks.first() {
            Some(Block::Paragraph { style, runs }) => Some((*style, normalize(runs))),
            _ => None,
        };
        let same = |line: &Line, i: usize| {
            doc.items[i].kind == ItemKind::Paragraph
                && original(i).is_some_and(|(s, r)| s == line.style && r == normalize(&line.runs))
        };

        let mut out = Vec::new();
        let mut claimed: Option<usize> = None;
        let mut last_locked: Option<usize> = None;
        for (at, line) in lines.iter().enumerate() {
            if line.locked {
                // A locked item may span several lines; it is written once.
                if let Some(i) = line.locked_item
                    && last_locked != Some(i)
                {
                    out.push(Out::Keep(i));
                    last_locked = Some(i);
                }
                continue;
            }
            last_locked = None;
            if let Some(i) = claimed.take() {
                out.push(Out::Keep(i));
                continue;
            }
            let editable: Vec<usize> =
                line.origins.iter().copied().filter(|&i| doc.items[i].kind == ItemKind::Paragraph).collect();
            if let Some(&i) = editable.iter().find(|&&i| same(line, i)) {
                out.push(Out::Keep(i));
                continue;
            }
            // Enter at the start of a paragraph leaves its mark on the new
            // empty line above; the paragraph itself is the next line.
            if let Some(&i) = editable.last()
                && let Some(next) = lines.get(at + 1)
                && next.origins.is_empty()
                && !next.locked
                && same(next, i)
            {
                claimed = Some(i);
                out.push(Out::Para { style: line.style, runs: line.runs.clone(), base: None });
                continue;
            }
            let base = editable.last().copied();
            let runs = with_props(&line.runs, base.and_then(|b| match doc.items[b].blocks.first() {
                Some(Block::Paragraph { runs, .. }) => Some(runs.as_slice()),
                _ => None,
            }));
            out.push(Out::Para { style: line.style, runs, base });
        }
        out
    }

    pub fn save(&self) -> anyhow::Result<Vec<u8>> {
        self.inner.doc.save(&self.to_out())
    }

    // ── Editing ─────────────────────────────────────────────────────────

    fn connect_editing(&self) {
        let buffer = &self.inner.buffer;

        let weak = Rc::downgrade(&self.inner);
        buffer.connect_insert_text(move |buffer, at, _| {
            if let Some(inner) = weak.upgrade() {
                inner.inserting_into.set(style_at_line(buffer, at.line()));
            }
        });
        let weak = Rc::downgrade(&self.inner);
        buffer.connect_closure(
            "insert-text",
            true,
            glib::closure_local!(move |buffer: gtk::TextBuffer, end: gtk::TextIter, text: &str, _len: i32| {
                let Some(inner) = weak.upgrade() else { return };
                if inner.quiet.get() {
                    return;
                }
                let this = DocxView { inner };
                this.after_insert(&buffer, end, text);
            }),
        );
        let weak = Rc::downgrade(&self.inner);
        buffer.connect_closure(
            "delete-range",
            true,
            glib::closure_local!(move |buffer: gtk::TextBuffer, at: gtk::TextIter, _end: gtk::TextIter| {
                let Some(inner) = weak.upgrade() else { return };
                if inner.quiet.get() {
                    return;
                }
                let this = DocxView { inner };
                // Deleting the line break before locked text would pull it
                // into the paragraph above, which would then be rewritten
                // with it — image and all. Put the break back.
                if !at.starts_line()
                    && let Some(locked) = buffer.tag_table().lookup(LOCKED_TAG)
                    && at.has_tag(&locked)
                {
                    let offset = at.offset();
                    this.inner.quiet.set(true);
                    let mut at = at;
                    buffer.insert(&mut at, "\n");
                    this.inner.quiet.set(false);
                    buffer.place_cursor(&buffer.iter_at_offset(offset));
                }
                let line = buffer.iter_at_mark(&buffer.get_insert()).line();
                let style = style_at_line(&buffer, line);
                this.restyle_lines(line, line, style);
            }),
        );

        let weak = Rc::downgrade(&self.inner);
        buffer.connect_mark_set(move |buffer, _, mark| {
            if mark == &buffer.get_insert()
                && let Some(inner) = weak.upgrade()
            {
                inner.typing.set(None);
                DocxView { inner }.report_format();
            }
        });

        // Backspace just after a bullet ends the list item, as does Enter on
        // an empty one: the bullet itself cannot be deleted as text.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&self.inner);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(inner) = weak.upgrade() else { return glib::Propagation::Proceed };
            if !inner.view.is_editable() || state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK) {
                return glib::Propagation::Proceed;
            }
            let buffer = &inner.buffer;
            if buffer.has_selection() {
                return glib::Propagation::Proceed;
            }
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            let ParaStyle::ListItem(_) = style_at_line(buffer, cursor.line()) else {
                return glib::Propagation::Proceed;
            };
            let this = DocxView { inner: inner.clone() };
            let after_bullet = cursor.offset() == text_start(buffer, cursor.line()).offset();
            let empty_item = after_bullet && cursor.ends_line();
            match key {
                gdk::Key::BackSpace if after_bullet => {
                    this.set_style(ParaStyle::Normal);
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter if empty_item => {
                    this.set_style(ParaStyle::Normal);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.inner.view.add_controller(keys);
    }

    fn after_insert(&self, buffer: &gtk::TextBuffer, end: gtk::TextIter, text: &str) {
        let mut start = end;
        start.backward_chars(text.chars().count() as i32);
        // Typed text takes the formatting of what it follows (or of what it
        // precedes, at the start of a line) unless the toolbar said otherwise.
        let inline = self.inner.typing.get().unwrap_or_else(|| {
            let mut probe = start;
            if !start.starts_line() && probe.backward_char() {
                Inline::of(&probe.tags())
            } else {
                Inline::of(&end.tags())
            }
        });
        for name in ["bold", "italic", "underline", "highlight"] {
            buffer.remove_tag_by_name(name, &start, &end);
        }
        for name in inline.names() {
            buffer.apply_tag_by_name(name, &start, &end);
        }
        for name in [LOCKED_TAG, PLACEHOLDER_TAG, BULLET_TAG] {
            buffer.remove_tag_by_name(name, &start, &end);
        }
        // The paragraph it went into keeps its style, and so does the text
        // that followed the insertion point on to its new line. Lines the
        // insertion created are new paragraphs of the same kind — except
        // after a heading, where the next paragraph is ordinary text.
        // Text typed at the start of locked text becomes a paragraph of its
        // own in front of it, rather than part of what cannot be edited.
        let mut end = end;
        if !text.ends_with('\n')
            && let Some(locked) = buffer.tag_table().lookup(LOCKED_TAG)
            && end.has_tag(&locked)
        {
            let offset = end.offset();
            self.inner.quiet.set(true);
            buffer.insert(&mut end, "\n");
            self.inner.quiet.set(false);
            buffer.place_cursor(&buffer.iter_at_offset(offset));
            end = buffer.iter_at_offset(offset);
            start = buffer.iter_at_offset(offset - text.chars().count() as i32);
            self.inner.inserting_into.set(ParaStyle::Normal);
        }
        let style = self.inner.inserting_into.get();
        // Taken before restyling, which may insert bullets and so move
        // every iterator.
        let (first, last) = (start.line(), end.line());
        let carried = !end.ends_line();
        self.restyle_lines(first, first, style);
        if last > first {
            let next = match style {
                ParaStyle::Title | ParaStyle::Heading(_) => ParaStyle::Normal,
                s => s,
            };
            if last > first + 1 {
                self.restyle_lines(first + 1, last - 1, next);
            }
            // Enter in the middle or at the start of a paragraph carries its
            // text on to the new line, and the text keeps its style.
            self.restyle_lines(last, last, if carried { style } else { next });
        }
    }

    /// Give lines `from..=to` one paragraph style, bullets and all.
    fn restyle_lines(&self, from: i32, to: i32, style: ParaStyle) {
        let buffer = &self.inner.buffer;
        let was_quiet = self.inner.quiet.replace(true);
        for line in from..=to {
            let Some(start) = buffer.iter_at_line(line) else { continue };
            // Stray bullets — a list item joined onto the line above — go.
            let mut end = start;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            let bullet = buffer.tag_table().lookup(BULLET_TAG);
            if let Some(bullet) = &bullet {
                let mut at = start;
                let mut stray = Vec::new();
                while at < end {
                    let mut next = at;
                    if !next.forward_to_tag_toggle(Some(bullet)) || next > end {
                        next = end;
                    }
                    if at.has_tag(bullet) && (at != start || !matches!(style, ParaStyle::ListItem(_))) {
                        stray.push((buffer.create_mark(None, &at, true), buffer.create_mark(None, &next, false)));
                    }
                    at = next;
                }
                for (a, b) in stray {
                    let (mut s, mut e) = (buffer.iter_at_mark(&a), buffer.iter_at_mark(&b));
                    buffer.delete(&mut s, &mut e);
                    buffer.delete_mark(&a);
                    buffer.delete_mark(&b);
                }
            }
            let start = buffer.iter_at_line(line).unwrap_or(buffer.end_iter());
            if let ParaStyle::ListItem(level) = style
                && !bullet.as_ref().is_some_and(|b| start.has_tag(b))
            {
                let mut at = start;
                buffer.insert_with_tags_by_name(&mut at, bullet_text(level), &[BULLET_TAG]);
            }
            let start = buffer.iter_at_line(line).unwrap_or(buffer.end_iter());
            let mut end = start;
            end.forward_line();
            for name in BLOCK_TAGS.iter().copied().chain(LIST_TAGS.iter().copied()) {
                buffer.remove_tag_by_name(name, &start, &end);
            }
            buffer.apply_tag_by_name(&block_tag(style), &start, &end);
        }
        self.inner.quiet.set(was_quiet);
        self.report_format();
    }

    /// The paragraph style of every line the selection touches.
    pub fn set_style(&self, style: ParaStyle) {
        let buffer = &self.inner.buffer;
        let (a, b) = buffer.selection_bounds().unwrap_or_else(|| {
            let at = buffer.iter_at_mark(&buffer.get_insert());
            (at, at)
        });
        let locked = buffer.tag_table().lookup(LOCKED_TAG);
        buffer.begin_user_action();
        for line in a.line()..=b.line() {
            if let (Some(tag), Some(it)) = (&locked, buffer.iter_at_line(line))
                && it.has_tag(tag)
            {
                continue;
            }
            self.restyle_lines(line, line, style);
        }
        buffer.end_user_action();
        buffer.set_modified(true);
    }

    /// Turn a character format on or off: over the selection if there is
    /// one, otherwise for what is typed next.
    pub fn toggle(&self, name: &str) {
        let buffer = &self.inner.buffer;
        match buffer.selection_bounds() {
            Some((start, end)) => {
                let tag = buffer.tag_table().lookup(name);
                let all = tag.as_ref().is_some_and(|t| covers(start, end, t));
                let locked = buffer.tag_table().lookup(LOCKED_TAG);
                buffer.begin_user_action();
                // Only over editable text: a table or an image paragraph is
                // kept exactly as it was.
                for (s, e) in editable_spans(start, end, locked.as_ref()) {
                    if all {
                        buffer.remove_tag_by_name(name, &s, &e);
                    } else {
                        buffer.apply_tag_by_name(name, &s, &e);
                    }
                }
                buffer.end_user_action();
                buffer.set_modified(true);
            }
            None => {
                let at = buffer.iter_at_mark(&buffer.get_insert());
                let mut current = self.inner.typing.get().unwrap_or_else(|| {
                    let mut probe = at;
                    if probe.backward_char() { Inline::of(&probe.tags()) } else { Inline::of(&at.tags()) }
                });
                current.set(name, !current.get(name));
                self.inner.typing.set(Some(current));
            }
        }
        self.report_format();
    }

    fn report_format(&self) {
        if self.inner.on_format.borrow().is_none() {
            return;
        }
        let buffer = &self.inner.buffer;
        let at = buffer.iter_at_mark(&buffer.get_insert());
        let inline = self.inner.typing.get().unwrap_or_else(|| match buffer.selection_bounds() {
            Some((s, e)) => {
                let table = buffer.tag_table();
                let all = |n: &str| table.lookup(n).is_some_and(|t| covers(s, e, &t));
                Inline { bold: all("bold"), italic: all("italic"), underline: all("underline"), highlight: all("highlight") }
            }
            None => {
                let mut probe = at;
                if !at.starts_line() && probe.backward_char() { Inline::of(&probe.tags()) } else { Inline::of(&at.tags()) }
            }
        });
        if let Some(cb) = self.inner.on_format.borrow().as_ref() {
            cb(style_at_line(buffer, at.line()), inline);
        }
    }
}

fn mark_name(item: usize) -> String {
    format!("item-{item}")
}

fn item_of(mark: &str) -> Option<usize> {
    mark.strip_prefix("item-")?.parse().ok()
}

fn lock_name(item: usize) -> String {
    format!("lock-{item}")
}

fn item_of_lock(tag: &str) -> Option<usize> {
    tag.strip_prefix("lock-")?.parse().ok()
}

const BLOCK_TAGS: [&str; 9] = ["title", "h1", "h2", "h3", "h4", "h5", "h6", "body", "quote"];
const LIST_TAGS: [&str; 6] = ["list0", "list1", "list2", "list3", "list4", "list5"];

fn block_tag(style: ParaStyle) -> String {
    match style {
        ParaStyle::Normal => "body".to_string(),
        ParaStyle::Title => "title".to_string(),
        ParaStyle::Heading(n) => format!("h{}", n.clamp(1, 6)),
        ParaStyle::ListItem(n) => format!("list{}", n.min(5)),
        ParaStyle::Quote => "quote".to_string(),
    }
}

fn style_of_tag(name: &str) -> Option<ParaStyle> {
    Some(match name {
        "body" => ParaStyle::Normal,
        "title" => ParaStyle::Title,
        "quote" => ParaStyle::Quote,
        n if n.starts_with('h') => ParaStyle::Heading(n[1..].parse().ok()?),
        n if n.starts_with("list") => ParaStyle::ListItem(n[4..].parse().ok()?),
        _ => return None,
    })
}

fn style_at_line(buffer: &gtk::TextBuffer, line: i32) -> ParaStyle {
    let Some(start) = buffer.iter_at_line(line) else { return ParaStyle::Normal };
    let mut probe = start;
    // An empty last line has no characters of its own; it takes the style
    // of the paragraph above.
    if probe.is_end() && line > 0 {
        probe.backward_char();
    }
    probe.tags().iter().find_map(|t| style_of_tag(t.name()?.as_str())).unwrap_or(ParaStyle::Normal)
}

fn bullet_text(level: u8) -> &'static str {
    if level.is_multiple_of(2) { "•  " } else { "◦  " }
}

/// Where a line's text starts, past its bullet.
fn text_start(buffer: &gtk::TextBuffer, line: i32) -> gtk::TextIter {
    let mut at = buffer.iter_at_line(line).unwrap_or(buffer.end_iter());
    if let Some(bullet) = buffer.tag_table().lookup(BULLET_TAG)
        && at.has_tag(&bullet)
    {
        at.forward_to_tag_toggle(Some(&bullet));
    }
    at
}

/// A line's style and its text as runs, bullets and placeholders left out.
fn read_line(buffer: &gtk::TextBuffer, start: gtk::TextIter, end: gtk::TextIter) -> (ParaStyle, Vec<Run>) {
    let style = style_at_line(buffer, start.line());
    let skip: Vec<gtk::TextTag> =
        [BULLET_TAG, PLACEHOLDER_TAG].iter().filter_map(|n| buffer.tag_table().lookup(n)).collect();
    let mut runs = Vec::new();
    let mut at = start;
    while at < end {
        let mut next = at;
        if !next.forward_to_tag_toggle(None::<&gtk::TextTag>) || next > end {
            next = end;
        }
        if next == at {
            break;
        }
        if !skip.iter().any(|t| at.has_tag(t)) {
            let flags = Inline::of(&at.tags());
            let text: String = buffer.text(&at, &next, false).chars().filter(|&c| c != '\u{FFFC}').collect();
            runs.push(Run {
                text,
                bold: flags.bold,
                italic: flags.italic,
                underline: flags.underline,
                highlight: flags.highlight,
                ..Default::default()
            });
        }
        at = next;
    }
    (style, normalize(&runs))
}

/// Give rewritten runs the fonts and sizes of the paragraph they were
/// edited from: each takes the properties of an original run formatted the
/// same way, or of the first one.
fn with_props(runs: &[Run], original: Option<&[Run]>) -> Vec<Run> {
    let Some(original) = original.filter(|o| !o.is_empty()) else { return runs.to_vec() };
    runs.iter()
        .map(|r| {
            let like = original
                .iter()
                .find(|o| (o.bold, o.italic, o.underline, o.highlight) == (r.bold, r.italic, r.underline, r.highlight))
                .unwrap_or(&original[0]);
            Run { props: like.props.clone(), ..r.clone() }
        })
        .collect()
}

fn covers(start: gtk::TextIter, end: gtk::TextIter, tag: &gtk::TextTag) -> bool {
    let mut at = start;
    while at < end {
        if !at.has_tag(tag) && !at.ends_line() {
            return false;
        }
        if !at.forward_char() {
            break;
        }
    }
    true
}

/// The parts of `start..end` not under `locked`.
fn editable_spans(start: gtk::TextIter, end: gtk::TextIter, locked: Option<&gtk::TextTag>) -> Vec<(gtk::TextIter, gtk::TextIter)> {
    let Some(locked) = locked else { return vec![(start, end)] };
    let mut spans = Vec::new();
    let mut at = start;
    while at < end {
        let mut next = at;
        if !next.forward_to_tag_toggle(Some(locked)) || next > end {
            next = end;
        }
        if !at.has_tag(locked) {
            spans.push((at, next));
        }
        if next == at {
            break;
        }
        at = next;
    }
    spans
}

fn make_tags(buffer: &gtk::TextBuffer) {
    let tags = buffer.tag_table();
    let tag = |name: &str, f: &dyn Fn(&gtk::TextTag)| {
        let t = gtk::TextTag::new(Some(name));
        f(&t);
        tags.add(&t);
    };
    tag("title", &|t| {
        t.set_scale(2.0);
        t.set_weight(800);
        t.set_pixels_below_lines(18);
    });
    for level in 1..=6u8 {
        let scale = [1.6, 1.35, 1.18, 1.08, 1.0, 1.0][level as usize - 1];
        tag(&format!("h{level}"), &|t| {
            t.set_scale(scale);
            t.set_weight(700);
            t.set_pixels_above_lines(14);
            t.set_pixels_below_lines(6);
        });
    }
    tag("body", &|t| t.set_scale(1.05));
    tag("bold", &|t| t.set_weight(700));
    tag("italic", &|t| t.set_style(gtk::pango::Style::Italic));
    tag("underline", &|t| t.set_underline(gtk::pango::Underline::Single));
    tag("highlight", &|t| {
        t.set_background_rgba(Some(&gdk::RGBA::new(1.0, 0.86, 0.2, 0.55)));
    });
    tag("quote", &|t| {
        t.set_style(gtk::pango::Style::Italic);
        t.set_left_margin(28);
        t.set_foreground_rgba(Some(&gdk::RGBA::new(0.6, 0.62, 0.7, 1.0)));
    });
    for level in 0..6 {
        tag(&format!("list{level}"), &|t| {
            t.set_left_margin(18 + 22 * level);
            t.set_indent(-16);
        });
    }
    tag(BULLET_TAG, &|t| t.set_editable(false));
    tag(LOCKED_TAG, &|t| t.set_editable(false));
    tag(PLACEHOLDER_TAG, &|t| {
        t.set_style(gtk::pango::Style::Italic);
        t.set_foreground_rgba(Some(&gdk::RGBA::new(0.55, 0.57, 0.65, 1.0)));
    });
}

fn append_paragraph(buffer: &gtk::TextBuffer, style: ParaStyle, runs: &[Run]) {
    let start = buffer.create_mark(None, &buffer.end_iter(), true);
    if let ParaStyle::ListItem(level) = style {
        buffer.insert_with_tags_by_name(&mut buffer.end_iter(), bullet_text(level), &[BULLET_TAG]);
    }
    for run in runs {
        let mut names: Vec<&str> = Vec::new();
        if run.bold {
            names.push("bold");
        }
        if run.italic {
            names.push("italic");
        }
        if run.underline {
            names.push("underline");
        }
        if run.highlight {
            names.push("highlight");
        }
        if run.placeholder {
            names.push(PLACEHOLDER_TAG);
        }
        let text = run.text.replace('\n', &LINE_BREAK.to_string());
        buffer.insert_with_tags_by_name(&mut buffer.end_iter(), &text, &names);
    }
    buffer.insert(&mut buffer.end_iter(), "\n");
    let (s, e) = (buffer.iter_at_mark(&start), buffer.end_iter());
    buffer.apply_tag_by_name(&block_tag(style), &s, &e);
    buffer.delete_mark(&start);
}

fn table(rows: &[Vec<String>]) -> gtk::Widget {
    let grid = gtk::Grid::builder()
        .column_spacing(0)
        .row_spacing(0)
        .css_classes(["card"])
        .margin_top(6)
        .margin_bottom(6)
        .build();
    for (r, row) in rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let label = gtk::Label::builder()
                .label(cell)
                .wrap(true)
                .xalign(0.0)
                .selectable(true)
                .margin_start(8)
                .margin_end(8)
                .margin_top(4)
                .margin_bottom(4)
                .build();
            if r == 0 {
                label.add_css_class("heading");
            }
            grid.attach(&label, c as i32, r as i32, 1, 1);
        }
    }
    grid.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::tests::package;

    #[test]
    fn edits_in_the_buffer_come_back_as_the_right_paragraphs() {
        crate::gtk_test::run(editing_round_trip);
    }

    fn editing_round_trip() {
        let body = r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title here</w:t></w:r></w:p><w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>First para</w:t></w:r></w:p><w:p><w:r><w:drawing/></w:r><w:r><w:t>With image</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p><w:p><w:r><w:t>Last</w:t></w:r></w:p>"#;
        let doc = crate::docx::load(&package(body, &[])).unwrap();
        let view = DocxView::new(doc);
        let buffer = view.buffer().clone();

        // Untouched: every paragraph kept as it was.
        assert_eq!(view.to_out(), vec![Out::Keep(0), Out::Keep(1), Out::Keep(2), Out::Keep(3), Out::Keep(4)]);
        assert!(buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).contains("•  Item"));

        view.set_editable(true);
        // Type at the end of "First para".
        let mut at = buffer.iter_at_line(1).unwrap();
        at.forward_to_line_end();
        buffer.insert_interactive(&mut at, " more", true);
        // Enter at the end of "Item" makes a new bullet.
        let mut at = buffer.iter_at_line(3).unwrap();
        at.forward_to_line_end();
        buffer.insert_interactive(&mut at, "\nSecond item", true);
        // Bold the word "Last".
        let s = buffer.iter_at_line(5).unwrap();
        let mut e = s;
        e.forward_to_line_end();
        buffer.select_range(&s, &e);
        view.toggle("bold");
        // The image paragraph cannot be typed into: typing at its start
        // makes a paragraph of its own in front of it, and backspacing into
        // it from its start is undone.
        let mut at = buffer.iter_at_line(2).unwrap();
        at.forward_char();
        assert!(!buffer.insert_interactive(&mut at, "nope", true), "a locked paragraph took text");
        let mut at = buffer.iter_at_line(2).unwrap();
        buffer.insert_interactive(&mut at, "Before", true);
        let mut s = buffer.iter_at_line(3).unwrap();
        let mut e = s;
        s.backward_char();
        buffer.delete_interactive(&mut s, &mut e, true);

        let out = view.to_out();
        assert_eq!(out[0], Out::Keep(0));
        let Out::Para { style, runs, base } = &out[1] else { panic!("{:?}", out[1]) };
        assert_eq!((*style, runs[0].text.as_str(), *base), (ParaStyle::Normal, "First para more", Some(1)));
        let Out::Para { runs, base: None, .. } = &out[2] else { panic!("{:?}", out[2]) };
        assert_eq!(runs[0].text, "Before");
        assert_eq!(out[3], Out::Keep(2), "the image paragraph is kept verbatim");
        assert_eq!(out[4], Out::Keep(3));
        let Out::Para { style, runs, base } = &out[5] else { panic!("{:?}", out[5]) };
        assert_eq!((*style, runs[0].text.as_str(), *base), (ParaStyle::ListItem(0), "Second item", None));
        let Out::Para { runs, .. } = &out[6] else { panic!("{:?}", out[6]) };
        assert!(runs[0].bold && runs[0].text == "Last");

        // Enter at the very start of "Title here" pushes it down a line; the
        // heading still comes back as itself rather than as a rewrite.
        let mut at = buffer.start_iter();
        buffer.insert_interactive(&mut at, "\n", true);
        let out = view.to_out();
        assert!(matches!(&out[0], Out::Para { base: None, runs, .. } if runs.is_empty()), "{:?}", out[0]);
        assert_eq!(out[1], Out::Keep(0));

        // Headings change style from the toolbar, and the result saves.
        buffer.place_cursor(&buffer.iter_at_line(2).unwrap());
        assert_eq!(buffer.iter_at_line(2).unwrap().line(), 2);
        view.set_style(ParaStyle::Heading(2));
        let out = view.to_out();
        assert!(matches!(&out[2], Out::Para { style: ParaStyle::Heading(2), base: Some(1), .. }), "{:?}", out[2]);
        let saved = view.save().unwrap();
        let back = crate::docx::load(&saved).unwrap();
        assert!(back.blocks().iter().any(|b| matches!(b, Block::Paragraph { style: ParaStyle::ListItem(0), runs } if runs.iter().any(|r| r.text == "Second item"))));
    }
}
