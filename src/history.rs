//! Undo and redo for a document being edited.
//!
//! GTK's own history keeps text only. A picture in the text is a paintable,
//! which it does not see: undoing past one restores a placeholder character
//! where the picture was, and every offset it recorded after the picture
//! went in is one out, so later undos change the wrong characters. It does
//! not see formatting either. This history records every change to the
//! buffer — text, pictures, tags, and the marks that tie paragraphs to the
//! file — as it happens, and plays changes back exactly.
//!
//! Changes are grouped by user action (a key press, a paste, a toolbar
//! click), so one undo takes back one thing the reader did; typing is
//! grouped a word at a time.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::gdk;

/// How many steps are kept.
const LIMIT: usize = 500;

/// Text, pictures and the marks among them, as removed from the buffer.
#[derive(Clone)]
struct Content {
    pieces: Vec<Piece>,
    /// Named marks inside what was removed, by offset from its start.
    marks: Vec<(String, i32)>,
    len: i32,
}

#[derive(Clone)]
enum Piece {
    Text(String, Vec<gtk::TextTag>),
    Paintable(gdk::Paintable),
}

#[derive(Clone)]
enum Op {
    Insert { at: i32, content: Content },
    Delete { at: i32, content: Content },
    /// A tag put on (or taken off) these ranges, where it was not (or was).
    Tag { tag: gtk::TextTag, ranges: Vec<(i32, i32)>, applied: bool },
}

#[derive(Default)]
struct Step {
    ops: Vec<Op>,
    /// One character typed: it may join the step before it.
    typed: Option<(i32, char)>,
}

#[derive(Default)]
struct State {
    undo: RefCell<Vec<Step>>,
    redo: RefCell<Vec<Step>>,
    open: RefCell<Option<Step>>,
    depth: Cell<u32>,
    replaying: Cell<bool>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
}

#[derive(Clone)]
pub struct History {
    buffer: gtk::TextBuffer,
    state: Rc<State>,
}

impl History {
    /// Take over undo for `buffer`, from its content as it now is.
    pub fn attach(buffer: &gtk::TextBuffer) -> History {
        buffer.set_enable_undo(false);
        let h = History { buffer: buffer.clone(), state: Rc::default() };

        let s = h.clone();
        buffer.connect_begin_user_action(move |_| {
            if s.state.depth.get() == 0 && !s.state.replaying.get() {
                *s.state.open.borrow_mut() = Some(Step::default());
            }
            s.state.depth.set(s.state.depth.get() + 1);
        });
        let s = h.clone();
        buffer.connect_end_user_action(move |_| {
            let depth = s.state.depth.get().saturating_sub(1);
            s.state.depth.set(depth);
            if depth == 0
                && let Some(step) = s.state.open.borrow_mut().take()
            {
                s.close(step);
            }
        });

        let s = h.clone();
        buffer.connect_insert_text(move |_, at, text| {
            let content = Content { pieces: vec![Piece::Text(text.to_string(), Vec::new())], marks: Vec::new(), len: text.chars().count() as i32 };
            let mut chars = text.chars();
            let typed = match (chars.next(), chars.next()) {
                (Some(c), None) => Some((at.offset(), c)),
                _ => None,
            };
            s.record(Op::Insert { at: at.offset(), content }, typed);
        });
        let s = h.clone();
        buffer.connect_insert_paintable(move |_, at, paintable| {
            let content = Content { pieces: vec![Piece::Paintable(paintable.clone())], marks: Vec::new(), len: 1 };
            s.record(Op::Insert { at: at.offset(), content }, None);
        });
        let s = h.clone();
        buffer.connect_delete_range(move |buffer, start, end| {
            if s.state.replaying.get() {
                return;
            }
            let (start, end) = if start.offset() <= end.offset() { (*start, *end) } else { (*end, *start) };
            if start == end {
                return;
            }
            let content = capture(buffer, &start, &end);
            s.record(Op::Delete { at: start.offset(), content }, None);
        });
        for applied in [true, false] {
            let s = h.clone();
            let handler = move |_: &gtk::TextBuffer, tag: &gtk::TextTag, start: &gtk::TextIter, end: &gtk::TextIter| {
                if s.state.replaying.get() {
                    return;
                }
                let ranges = changing(tag, start, end, applied);
                if !ranges.is_empty() {
                    s.record(Op::Tag { tag: tag.clone(), ranges, applied }, None);
                }
            };
            if applied {
                buffer.connect_apply_tag(handler);
            } else {
                buffer.connect_remove_tag(handler);
            }
        }
        h
    }

    pub fn can_undo(&self) -> bool {
        !self.state.undo.borrow().is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.state.redo.borrow().is_empty()
    }

    /// Called whenever what can be undone or redone changes.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.state.on_change.borrow_mut() = Some(Box::new(f));
    }

    fn changed(&self) {
        if let Some(f) = self.state.on_change.borrow().as_ref() {
            f();
        }
    }

    fn record(&self, op: Op, typed: Option<(i32, char)>) {
        if self.state.replaying.get() {
            return;
        }
        let mut open = self.state.open.borrow_mut();
        match open.as_mut() {
            Some(step) => {
                // Only a step that is one typed character (and the
                // formatting given to it) counts as typing.
                step.typed = if step.ops.is_empty() { typed } else if matches!(op, Op::Tag { .. }) { step.typed } else { None };
                step.ops.push(op);
            }
            None => {
                drop(open);
                self.close(Step { ops: vec![op], typed: None });
            }
        }
    }

    /// A finished step joins the history — or the step before it, when
    /// both are typing within one word.
    fn close(&self, step: Step) {
        if step.ops.is_empty() {
            return;
        }
        {
            let mut undo = self.state.undo.borrow_mut();
            // A letter typed right after the last one continues its word;
            // a space or a new line starts the next.
            let joins = match (undo.last().and_then(|l| l.typed), step.typed) {
                (Some((last_at, _)), Some((at, c))) => last_at + 1 == at && !c.is_whitespace(),
                _ => false,
            };
            if joins && let Some(last) = undo.last_mut() {
                last.ops.extend(step.ops);
                last.typed = step.typed;
            } else {
                undo.push(step);
                if undo.len() > LIMIT {
                    undo.remove(0);
                }
            }
        }
        self.state.redo.borrow_mut().clear();
        self.changed();
    }

    pub fn undo(&self) {
        let Some(step) = self.state.undo.borrow_mut().pop() else { return };
        let at = self.replay(&step, true);
        self.state.redo.borrow_mut().push(step);
        self.after_replay(at);
    }

    pub fn redo(&self) {
        let Some(step) = self.state.redo.borrow_mut().pop() else { return };
        let at = self.replay(&step, false);
        self.state.undo.borrow_mut().push(step);
        self.after_replay(at);
    }

    fn after_replay(&self, at: Option<i32>) {
        if let Some(at) = at {
            self.buffer.place_cursor(&self.buffer.iter_at_offset(at));
        }
        self.buffer.set_modified(true);
        self.changed();
    }

    /// Play a step back (`backwards`, for undo) or forwards again; the
    /// offset of the last change, for the cursor.
    fn replay(&self, step: &Step, backwards: bool) -> Option<i32> {
        let buffer = &self.buffer;
        self.state.replaying.set(true);
        let mut cursor = None;
        let ops: Box<dyn Iterator<Item = &Op>> = if backwards { Box::new(step.ops.iter().rev()) } else { Box::new(step.ops.iter()) };
        for op in ops {
            match (op, backwards) {
                (Op::Insert { at, content }, false) | (Op::Delete { at, content }, true) => {
                    let mut iter = buffer.iter_at_offset(*at);
                    for piece in &content.pieces {
                        match piece {
                            Piece::Text(text, tags) => {
                                let start = iter.offset();
                                buffer.insert(&mut iter, text);
                                let s = buffer.iter_at_offset(start);
                                for tag in tags {
                                    buffer.apply_tag(tag, &s, &iter);
                                }
                            }
                            Piece::Paintable(p) => buffer.insert_paintable(&mut iter, p),
                        }
                    }
                    for (name, rel) in &content.marks {
                        buffer.move_mark_by_name(name, &buffer.iter_at_offset(at + rel));
                    }
                    cursor = Some(at + content.len);
                }
                (Op::Insert { at, content }, true) | (Op::Delete { at, content }, false) => {
                    let (mut s, mut e) = (buffer.iter_at_offset(*at), buffer.iter_at_offset(at + content.len));
                    buffer.delete(&mut s, &mut e);
                    cursor = Some(*at);
                }
                (Op::Tag { tag, ranges, applied }, backwards) => {
                    for (a, b) in ranges {
                        let (s, e) = (buffer.iter_at_offset(*a), buffer.iter_at_offset(*b));
                        if *applied != backwards {
                            buffer.apply_tag(tag, &s, &e);
                        } else {
                            buffer.remove_tag(tag, &s, &e);
                        }
                    }
                }
            }
        }
        self.state.replaying.set(false);
        cursor
    }
}

/// What `start..end` holds: text by its tags, pictures, named marks.
fn capture(buffer: &gtk::TextBuffer, start: &gtk::TextIter, end: &gtk::TextIter) -> Content {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut at = *start;
    while at < *end {
        let mut next = at;
        if !next.forward_to_tag_toggle(None::<&gtk::TextTag>) || next > *end {
            next = *end;
        }
        if next == at {
            break;
        }
        let tags = at.tags();
        let mut text = String::new();
        for (k, c) in buffer.slice(&at, &next, true).chars().enumerate() {
            if c == '\u{FFFC}'
                && let Some(p) = buffer.iter_at_offset(at.offset() + k as i32).paintable()
            {
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text), tags.clone()));
                }
                pieces.push(Piece::Paintable(p));
                continue;
            }
            text.push(c);
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text, tags));
        }
        at = next;
    }
    // Marks past the start end up at it once the range is gone; they are
    // put back where they were.
    let mut marks = Vec::new();
    let mut probe = *start;
    while probe <= *end {
        if probe != *start {
            marks.extend(probe.marks().iter().filter_map(|m| Some((m.name()?.to_string(), probe.offset() - start.offset()))));
        }
        if !probe.forward_char() {
            break;
        }
    }
    Content { pieces, marks, len: end.offset() - start.offset() }
}

/// The parts of `start..end` that `tag` is about to be put on (`applied`)
/// or taken off.
fn changing(tag: &gtk::TextTag, start: &gtk::TextIter, end: &gtk::TextIter, applied: bool) -> Vec<(i32, i32)> {
    let mut ranges = Vec::new();
    let mut at = *start;
    while at < *end {
        let mut next = at;
        if !next.forward_to_tag_toggle(Some(tag)) || next > *end {
            next = *end;
        }
        if at.has_tag(tag) != applied {
            ranges.push((at.offset(), next.offset()));
        }
        if next == at {
            break;
        }
        at = next;
    }
    ranges
}
