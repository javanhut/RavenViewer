//! DOCX as a reading view. A .docx is a zip; the body is
//! `word/document.xml`. We keep what reading needs — headings, paragraphs,
//! emphasis, lists, tables — and let the window lay it out as flowing text.

use std::io::Read;

use anyhow::{Context, Result};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
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

pub fn load(bytes: &[u8]) -> Result<Vec<Block>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("not a DOCX (zip) file")?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .context("the file has no word/document.xml")?
        .read_to_string(&mut xml)?;
    Ok(parse(&xml))
}

fn val(e: &BytesStart) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == b"val")
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// `w:b`, `w:i` are on unless `w:val` says false/0.
fn toggle(e: &BytesStart) -> bool {
    !matches!(val(e).as_deref(), Some("false" | "0" | "none"))
}

pub fn parse(xml: &str) -> Vec<Block> {
    let mut reader = Reader::from_str(xml);
    let mut blocks = Vec::new();

    let mut style = ParaStyle::Normal;
    let mut runs: Vec<Run> = Vec::new();
    let mut run = Run::default();
    let mut in_text = false;
    let mut list_level: Option<u8> = None;

    // Tables: collect cell text; nested paragraphs inside a cell become lines.
    let mut table_depth = 0usize;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cell = String::new();

    loop {
        let event = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(e) => e,
        };
        match event {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"p" => {
                    style = ParaStyle::Normal;
                    runs.clear();
                    list_level = None;
                }
                b"pStyle" => {
                    let name = val(&e).unwrap_or_default().to_ascii_lowercase();
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
                b"ilvl" => list_level = Some(val(&e).and_then(|v| v.parse().ok()).unwrap_or(0)),
                b"numPr" => list_level = list_level.or(Some(0)),
                b"r" => run = Run::default(),
                b"b" => run.bold = toggle(&e),
                b"i" => run.italic = toggle(&e),
                b"u" => run.underline = toggle(&e),
                b"t" => in_text = true,
                b"tab" => run.text.push('\t'),
                b"br" | b"cr" => run.text.push('\n'),
                b"tbl" => {
                    table_depth += 1;
                    if table_depth == 1 {
                        rows.clear();
                    }
                }
                b"tr" if table_depth == 1 => rows.push(Vec::new()),
                b"tc" if table_depth == 1 => cell.clear(),
                _ => {}
            },
            Event::Text(t) if in_text => {
                if let Ok(text) = t.unescape() {
                    run.text.push_str(&text);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"r" => {
                    if !run.text.is_empty() {
                        runs.push(std::mem::take(&mut run));
                    }
                }
                b"p" => {
                    if table_depth > 0 {
                        let line: String = runs.iter().map(|r| r.text.as_str()).collect();
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

#[cfg(test)]
mod tests {
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
}
