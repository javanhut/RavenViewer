//! Lays DOCX blocks out as a read-only, reflowing text sheet: headings by
//! weight and size, emphasis as tags, lists with bullets, tables as grids.

use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::docx::{Block, ParaStyle, Run};

pub fn build(blocks: &[Block]) -> gtk::ScrolledWindow {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .pixels_below_lines(6)
        .build();
    let buffer = view.buffer();
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
    tag("quote", &|t| {
        t.set_style(gtk::pango::Style::Italic);
        t.set_left_margin(28);
        t.set_foreground_rgba(Some(&gtk::gdk::RGBA::new(0.6, 0.62, 0.7, 1.0)));
    });
    for level in 0..6 {
        tag(&format!("list{level}"), &|t| {
            t.set_left_margin(18 + 22 * level);
            t.set_indent(-16);
        });
    }

    for block in blocks {
        match block {
            Block::Paragraph { style, runs } => append_paragraph(&buffer, *style, runs),
            Block::Table { rows } => {
                let mut end = buffer.end_iter();
                let anchor = buffer.create_child_anchor(&mut end);
                view.add_child_at_anchor(&table(rows), &anchor);
                buffer.insert(&mut buffer.end_iter(), "\n");
            }
        }
    }

    let sheet = gtk::Box::builder().css_classes(["docx-sheet"]).margin_top(28).margin_bottom(28).build();
    sheet.append(&view);
    view.set_hexpand(true);
    let clamp = adw::Clamp::builder().maximum_size(820).tightening_threshold(600).child(&sheet).build();
    gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .css_classes(["canvas", "docx-view"])
        .child(&clamp)
        .build()
}

fn append_paragraph(buffer: &gtk::TextBuffer, style: ParaStyle, runs: &[Run]) {
    let block_tag = match style {
        ParaStyle::Normal => "body".to_string(),
        ParaStyle::Title => "title".to_string(),
        ParaStyle::Heading(n) => format!("h{n}"),
        ParaStyle::ListItem(n) => format!("list{}", n.min(5)),
        ParaStyle::Quote => "quote".to_string(),
    };
    let start = buffer.create_mark(None, &buffer.end_iter(), true);
    if let ParaStyle::ListItem(level) = style {
        buffer.insert(&mut buffer.end_iter(), if level % 2 == 0 { "•  " } else { "◦  " });
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
        buffer.insert_with_tags_by_name(&mut buffer.end_iter(), &run.text, &names);
    }
    buffer.insert(&mut buffer.end_iter(), "\n");
    let (s, e) = (buffer.iter_at_mark(&start), buffer.end_iter());
    buffer.apply_tag_by_name(&block_tag, &s, &e);
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
