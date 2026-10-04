//! Word 97–2003 documents (`.doc`, [MS-DOC]), read into the same paragraphs,
//! runs, tables and pictures a DOCX is read into — and written from them.
//!
//! A `.doc` is a compound file. Its `WordDocument` stream starts with the File
//! Information Block (the FIB), a directory of everything else: where the
//! text is (the piece table, in the `0Table` or `1Table` stream), the
//! character and paragraph formatting (pages of runs, "FKPs", each pointing
//! at a list of property changes, "sprms"), the styles, the fonts and the
//! page setup. Inline pictures live in the `Data` stream.
//!
//! Reading keeps what the editor shows: headings, lists, quotes, alignment,
//! bold, italic, underline, highlight, font, size and colour, tables,
//! pictures and page breaks. Headers, footers, notes, comments and floating
//! shapes are not read.

use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::cfb::{self, Cfb};
use crate::docx::{self, Align, Block, Image, LINE_BREAK, Paper, ParaStyle, Run, TextDefaults};

fn u16_at(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn i16_at(b: &[u8], at: usize) -> i16 {
    u16_at(b, at) as i16
}

// ── Sprms ────────────────────────────────────────────────────────────────

const SPRM_TDEF_TABLE: u16 = 0xD608;

/// One property change: its code and its operand.
struct Sprm<'a> {
    code: u16,
    arg: &'a [u8],
}

impl Sprm<'_> {
    fn byte(&self) -> u8 {
        self.arg.first().copied().unwrap_or(0)
    }
    fn word(&self) -> u16 {
        u16_at(self.arg, 0)
    }
    fn long(&self) -> u32 {
        u32_at(self.arg, 0)
    }
}

/// The property changes in `grpprl`. An operand's size is in the code's top
/// three bits, except for the two whose size is written in front of them.
fn sprms(grpprl: &[u8]) -> Vec<Sprm<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 2 <= grpprl.len() {
        let code = u16_at(grpprl, at);
        at += 2;
        // How many bytes the operand takes, and where in them its value starts.
        let (len, skip) = match code >> 13 {
            0 | 1 => (1, 0),
            2 | 4 | 5 => (2, 0),
            3 => (4, 0),
            7 => (3, 0),
            _ => match code {
                // sprmTDefTable: a two-byte size, then the cell count, the
                // cell edges and 20 bytes per cell. Its size is counted
                // differently by different writers, so it is measured.
                SPRM_TDEF_TABLE => {
                    let cells = grpprl.get(at + 2).copied().unwrap_or(0) as usize;
                    (2 + 1 + 2 * (cells + 1) + 20 * cells, 2)
                }
                // sprmPChgTabs: a one-byte size, unless it says 255.
                0xC615 if grpprl.get(at) == Some(&255) => {
                    let del = grpprl.get(at + 1).copied().unwrap_or(0) as usize;
                    let add = grpprl.get(at + 2 + del * 4).copied().unwrap_or(0) as usize;
                    (2 + del * 4 + 1 + add * 3, 1)
                }
                _ => (1 + grpprl.get(at).copied().unwrap_or(0) as usize, 1),
            },
        };
        let (start, end) = (at + skip, at + len);
        let Some(arg) = grpprl.get(start..end.min(grpprl.len())) else { break };
        out.push(Sprm { code, arg });
        at += len;
    }
    out
}

/// A toggle operand: 1 on, 0 off; 0x80 and 0x81 mean "as the style" and
/// "the opposite of the style", which, styles' own emphasis aside, is off
/// and on.
fn toggle(v: u8) -> bool {
    matches!(v, 1 | 0x81)
}

#[derive(Clone, Default, PartialEq)]
struct CharProps {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    highlight: bool,
    hidden: bool,
    /// In half-points.
    size: Option<u16>,
    font: Option<u16>,
    color: Option<[u8; 3]>,
    /// The character is a special one: a picture, a note reference…
    special: bool,
    picture: Option<u32>,
}

impl CharProps {
    fn apply(&mut self, grpprl: &[u8]) {
        for s in sprms(grpprl) {
            match s.code {
                0x0835 => self.bold = toggle(s.byte()),
                0x0836 => self.italic = toggle(s.byte()),
                0x0837 => self.strike = toggle(s.byte()),
                0x083C => self.hidden = toggle(s.byte()),
                0x2A3E => self.underline = s.byte() != 0,
                0x2A0C => self.highlight = s.byte() != 0,
                0x4A43 => self.size = Some(s.word()),
                0x4A4F => self.font = Some(s.word()),
                0x2A42 => self.color = ico(s.byte()),
                0x6870 => {
                    let v = s.long();
                    // An "automatic" colour has its top byte set.
                    self.color = (v >> 24 == 0).then_some([v as u8, (v >> 8) as u8, (v >> 16) as u8]);
                }
                0x0855 => self.special = toggle(s.byte()),
                0x6A03 => self.picture = Some(s.long()),
                _ => {}
            }
        }
    }
}

/// The sixteen colours of Word 6 and before.
fn ico(i: u8) -> Option<[u8; 3]> {
    const ICO: [[u8; 3]; 16] = [
        [0, 0, 0], [0, 0, 0], [0, 0, 255], [0, 255, 255], [0, 255, 0], [255, 0, 255], [255, 0, 0], [255, 255, 0],
        [255, 255, 255], [0, 0, 128], [0, 128, 128], [0, 128, 0], [128, 0, 128], [128, 0, 0], [128, 128, 0], [192, 192, 192],
    ];
    (i != 0).then(|| ICO.get(i as usize).copied()).flatten()
}

#[derive(Clone, Default)]
struct ParaProps {
    istd: u16,
    align: Align,
    in_table: bool,
    /// The paragraph that ends a table row.
    row_end: bool,
    list: bool,
    level: u8,
    outline: Option<u8>,
    /// Left indent, in twips.
    left: i16,
}

impl ParaProps {
    fn apply(&mut self, grpprl: &[u8]) {
        for s in sprms(grpprl) {
            match s.code {
                0x2403 | 0x2461 => {
                    self.align = match s.byte() {
                        1 => Align::Center,
                        2 => Align::End,
                        3 | 4 => Align::Justify,
                        _ => Align::Start,
                    }
                }
                0x2416 => self.in_table = s.byte() != 0,
                0x2417 => self.row_end = s.byte() != 0,
                0x6649 => self.in_table |= s.long() > 0,
                0x460B => self.list = s.word() != 0,
                0x260A => self.level = s.byte().min(8),
                0x2640 => self.outline = (s.byte() < 9).then(|| s.byte()),
                0x840F | 0x845E => self.left = s.word() as i16,
                _ => {}
            }
        }
    }
}

// ── Reading ──────────────────────────────────────────────────────────────

struct Style {
    sti: u16,
    name: String,
    /// The style's own character formatting.
    chpx: Vec<u8>,
}

/// Properties by stream offset: `(from, to, value)`, in order.
type Spans<T> = Vec<(u32, u32, T)>;

fn find<T>(spans: &Spans<T>, fc: u32) -> Option<&T> {
    let i = spans.partition_point(|s| s.1 <= fc);
    spans.get(i).filter(|s| s.0 <= fc).map(|s| &s.2)
}

pub struct Read {
    pub blocks: Vec<Block>,
    pub paper: Paper,
    pub text: TextDefaults,
}

/// Whether `bytes` is a Word document of the kind read here (or one too old
/// or protected to read, which is reported when it is read).
pub fn is_doc(bytes: &[u8]) -> bool {
    cfb::is_compound(bytes) && Cfb::open(bytes).is_ok_and(|c| c.stream("WordDocument").is_some())
}

pub fn read(bytes: &[u8]) -> Result<Read> {
    let cfb = Cfb::open(bytes).context("not a Word document")?;
    let word = cfb.stream("WordDocument").context("not a Word document (no WordDocument stream)")?;
    if u16_at(&word, 0) != 0xA5EC {
        bail!("not a Word document");
    }
    let n_fib = u16_at(&word, 2);
    if n_fib < 0x00C1 {
        bail!("this is a Word 6 or Word 95 document, which isn’t supported — only Word 97 and later");
    }
    let flags = u16_at(&word, 0x0A);
    if flags & 0x0100 != 0 {
        bail!("the document is password-protected");
    }
    let table_name = if flags & 0x0200 != 0 { "1Table" } else { "0Table" };
    let table = cfb.stream(table_name).with_context(|| format!("the document has no {table_name} stream"))?;
    let data = cfb.stream("Data").unwrap_or_default();

    let csw = u16_at(&word, 32) as usize;
    let lw = 34 + csw * 2;
    let cslw = u16_at(&word, lw) as usize;
    let rg_lw = lw + 2;
    let ccp_text = u32_at(&word, rg_lw + 12) as usize;
    let fc_lcb = rg_lw + cslw * 4 + 2;
    let pair = |i: usize| (u32_at(&word, fc_lcb + 8 * i) as usize, u32_at(&word, fc_lcb + 8 * i + 4) as usize);
    let slice = |(fc, lcb): (usize, usize)| table.get(fc..fc + lcb).unwrap_or_default();

    // The text: pieces of 8-bit (Windows-1252) or UTF-16 text, each with the
    // stream offset of every character for looking its formatting up.
    let clx = slice(pair(33));
    let mut at = 0;
    while clx.get(at) == Some(&1) {
        at += 3 + i16_at(clx, at + 1).max(0) as usize;
    }
    if clx.get(at) != Some(&2) {
        bail!("the document’s text could not be found");
    }
    let plc = clx.get(at + 5..at + 5 + u32_at(clx, at + 1) as usize).unwrap_or_default();
    let pieces = plc.len().saturating_sub(4) / 12;
    let mut units: Vec<u16> = Vec::with_capacity(ccp_text);
    let mut fcs: Vec<u32> = Vec::with_capacity(ccp_text);
    for p in 0..pieces {
        let (cp0, cp1) = (u32_at(plc, 4 * p) as usize, u32_at(plc, 4 * (p + 1)) as usize);
        if cp0 >= ccp_text {
            continue;
        }
        let pcd = 4 * (pieces + 1) + 8 * p;
        let raw = u32_at(plc, pcd + 2);
        let compressed = raw & 0x4000_0000 != 0;
        let fc = (raw & 0x3FFF_FFFF) as usize;
        for cp in cp0..cp1.min(ccp_text) {
            let k = cp - cp0;
            if compressed {
                let off = fc / 2 + k;
                units.push(docx::cp1252(word.get(off).copied().unwrap_or(0)) as u16);
                fcs.push(off as u32);
            } else {
                let off = fc + 2 * k;
                units.push(u16_at(&word, off));
                fcs.push(off as u32);
            }
        }
    }

    // Character runs and paragraphs, from their formatted disk pages.
    let fkps = |(fc, lcb): (usize, usize)| -> Vec<&[u8]> {
        let plc = table.get(fc..fc + lcb).unwrap_or_default();
        let n = plc.len().saturating_sub(4) / 8;
        (0..n)
            .filter_map(|i| {
                let pn = (u32_at(plc, 4 * (n + 1) + 4 * i) & 0x3F_FFFF) as usize;
                word.get(pn * 512..pn * 512 + 512)
            })
            .collect()
    };
    let mut chpx: Spans<Vec<u8>> = Vec::new();
    for page in fkps(pair(12)) {
        let crun = page[511] as usize;
        for j in 0..crun {
            let (a, b) = (u32_at(page, 4 * j), u32_at(page, 4 * (j + 1)));
            let off = page.get(4 * (crun + 1) + j).copied().unwrap_or(0) as usize * 2;
            let grpprl = if off == 0 {
                Vec::new()
            } else {
                let cb = page.get(off).copied().unwrap_or(0) as usize;
                page.get(off + 1..off + 1 + cb).unwrap_or_default().to_vec()
            };
            chpx.push((a, b, grpprl));
        }
    }
    let mut papx: Spans<(u16, Vec<u8>)> = Vec::new();
    for page in fkps(pair(13)) {
        let cpara = page[511] as usize;
        for j in 0..cpara {
            let (a, b) = (u32_at(page, 4 * j), u32_at(page, 4 * (j + 1)));
            let off = page.get(4 * (cpara + 1) + 13 * j).copied().unwrap_or(0) as usize * 2;
            let cb = page.get(off).copied().unwrap_or(0) as usize;
            let body = if cb == 0 {
                let cb2 = page.get(off + 1).copied().unwrap_or(0) as usize;
                page.get(off + 2..off + 2 + 2 * cb2)
            } else {
                page.get(off + 1..off + 2 * cb)
            };
            let body = body.unwrap_or_default();
            papx.push((a, b, (u16_at(body, 0), body.get(2..).unwrap_or_default().to_vec())));
        }
    }
    chpx.sort_by_key(|s| s.0);
    papx.sort_by_key(|s| s.0);

    let styles = read_styles(slice(pair(1)));
    let fonts = read_fonts(slice(pair(15)));
    let paper = read_paper(&word, slice(pair(6)));

    // The text defaults are the Normal style's, over the stylesheet's
    // standard font and Word's standard 10 points.
    let mut normal = CharProps { font: Some(u16_at(slice(pair(1)), 2 + 12)), size: Some(20), ..Default::default() };
    if let Some(s) = styles.first().and_then(|s| s.as_ref()) {
        normal.apply(&s.chpx);
    }
    let text = TextDefaults {
        font: normal.font.and_then(|f| fonts.get(f as usize).cloned()).unwrap_or_else(|| "Times New Roman".into()),
        size: normal.size.map_or(20, u32::from),
        after: 0,
    };

    let mut reader = Reader {
        units: &units,
        fcs: &fcs,
        chpx: &chpx,
        papx: &papx,
        styles: &styles,
        fonts: &fonts,
        data: &data,
        normal,
        blocks: Vec::new(),
        rows: Vec::new(),
        cells: Vec::new(),
        cell: String::new(),
        fields: Vec::new(),
    };
    reader.run();
    Ok(Read { blocks: reader.blocks, paper, text })
}

/// The document as a DOCX package.
pub fn to_docx(bytes: &[u8]) -> Result<Vec<u8>> {
    let read = read(bytes)?;
    docx::build(&read.blocks, read.paper, &read.text)
}

/// Style names and formatting, by style index.
fn read_styles(stsh: &[u8]) -> Vec<Option<Style>> {
    let cb_stshi = u16_at(stsh, 0) as usize;
    let cstd = u16_at(stsh, 2) as usize;
    let cb_base = u16_at(stsh, 4) as usize;
    let mut at = 2 + cb_stshi;
    let mut out = Vec::new();
    for _ in 0..cstd {
        let cb = u16_at(stsh, at) as usize;
        let std = stsh.get(at + 2..at + 2 + cb).unwrap_or_default();
        at += 2 + cb;
        if cb == 0 || std.len() < cb_base + 2 {
            out.push(None);
            continue;
        }
        let sti = u16_at(std, 0) & 0x0FFF;
        let stk = u16_at(std, 2) & 0x000F;
        let cupx = (u16_at(std, 4) & 0x000F) as usize;
        let cch = u16_at(std, cb_base) as usize;
        let name: Vec<u16> = (0..cch).map(|i| u16_at(std, cb_base + 2 + 2 * i)).collect();
        let name = String::from_utf16_lossy(&name);
        // The formatting after the name: a paragraph style's paragraph
        // properties, then its character properties; a character style's
        // character properties.
        let mut p = cb_base + 2 + 2 * cch + 2;
        let mut chpx = Vec::new();
        for u in 0..cupx {
            p += p % 2;
            let len = u16_at(std, p) as usize;
            let body = std.get(p + 2..p + 2 + len).unwrap_or_default();
            let is_chpx = (stk == 1 && u == 1) || (stk == 2 && u == 0);
            if is_chpx {
                chpx = body.to_vec();
            }
            p += 2 + len;
        }
        out.push(Some(Style { sti, name, chpx }));
    }
    out
}

/// Font names, by font index.
fn read_fonts(sttbf: &[u8]) -> Vec<String> {
    let count = u16_at(sttbf, 0) as usize;
    let mut at = 4;
    let mut out = Vec::new();
    for _ in 0..count {
        let Some(&cb) = sttbf.get(at) else { break };
        let ffn = sttbf.get(at + 1..at + 1 + cb as usize).unwrap_or_default();
        let name: Vec<u16> = (39..ffn.len()).step_by(2).map(|i| u16_at(ffn, i)).take_while(|&c| c != 0).collect();
        out.push(String::from_utf16_lossy(&name));
        at += 1 + cb as usize;
    }
    out
}

/// The first section's paper and margins.
fn read_paper(word: &[u8], plcf_sed: &[u8]) -> Paper {
    // Word 97's own defaults for a section that does not say.
    let mut paper = Paper { width: 12240, height: 15840, margins: [1440, 1800, 1440, 1800] };
    if plcf_sed.len() < 8 + 12 {
        return paper;
    }
    let n = (plcf_sed.len() - 4) / 16;
    let fc_sepx = u32_at(plcf_sed, 4 * (n + 1) + 2);
    if fc_sepx == u32::MAX {
        return paper;
    }
    let at = fc_sepx as usize;
    let cb = u16_at(word, at) as usize;
    for s in sprms(word.get(at + 2..at + 2 + cb).unwrap_or_default()) {
        let v = s.word() as u32;
        match s.code {
            0xB01F => paper.width = v,
            0xB020 => paper.height = v,
            0x9023 => paper.margins[0] = (s.word() as i16).unsigned_abs() as u32,
            0xB022 => paper.margins[1] = v,
            0x9024 => paper.margins[2] = (s.word() as i16).unsigned_abs() as u32,
            0xB021 => paper.margins[3] = v,
            _ => {}
        }
    }
    if paper.width < 1440 || paper.height < 1440 {
        paper.width = 12240;
        paper.height = 15840;
    }
    paper
}

struct Reader<'a> {
    units: &'a [u16],
    fcs: &'a [u32],
    chpx: &'a Spans<Vec<u8>>,
    papx: &'a Spans<(u16, Vec<u8>)>,
    styles: &'a [Option<Style>],
    fonts: &'a [String],
    data: &'a [u8],
    normal: CharProps,
    blocks: Vec<Block>,
    /// A table being read: its rows, the current row's cells, and the
    /// current cell's text.
    rows: Vec<Vec<String>>,
    cells: Vec<String>,
    cell: String,
    /// Open fields: whether each is past its separator (showing its result).
    fields: Vec<bool>,
}

impl Reader<'_> {
    fn run(&mut self) {
        let mut start = 0;
        for i in 0..self.units.len() {
            let u = self.units[i];
            if u == 0x0D || u == 0x07 {
                self.paragraph(start, i);
                start = i + 1;
            }
        }
        if start < self.units.len() {
            self.paragraph(start, self.units.len());
        }
        self.end_table();
    }

    fn para_props(&self, mark: usize) -> ParaProps {
        let mut p = ParaProps::default();
        if let Some((istd, grpprl)) = self.fcs.get(mark).and_then(|&fc| find(self.papx, fc)) {
            p.istd = *istd;
            p.apply(grpprl);
        }
        p
    }

    /// The characters `from..mark` and the paragraph mark at `mark`.
    fn paragraph(&mut self, from: usize, mark: usize) {
        let props = self.para_props(mark);
        let cell_mark = self.units.get(mark) == Some(&0x07);
        if props.in_table && props.row_end {
            // A row's end mark: its cells are complete.
            if !self.cells.is_empty() || !self.cell.is_empty() {
                self.rows.push(std::mem::take(&mut self.cells));
            }
            self.cell.clear();
            return;
        }
        let runs = self.runs(from, mark);
        if props.in_table || cell_mark {
            let text: String = runs.iter().filter(|r| r.image.is_none() && !r.placeholder).map(|r| r.text.replace(LINE_BREAK, "\n")).collect();
            if !self.cell.is_empty() {
                self.cell.push('\n');
            }
            self.cell.push_str(&text);
            if cell_mark {
                self.cells.push(std::mem::take(&mut self.cell));
            }
            return;
        }
        self.end_table();

        let style = self.styles.get(props.istd as usize).and_then(|s| s.as_ref());
        let name = style.map(|s| s.name.to_ascii_lowercase()).unwrap_or_default();
        let sti = style.map_or(0, |s| s.sti);
        let mut runs = runs;
        let mut para_style = if (1..=9).contains(&sti) {
            ParaStyle::Heading((sti as u8).min(6))
        } else if let Some(n) = name.strip_prefix("heading ") {
            ParaStyle::Heading(n.trim().parse::<u8>().unwrap_or(1).clamp(1, 6))
        } else if sti == 62 || name == "title" {
            ParaStyle::Title
        } else if name.contains("quote") {
            ParaStyle::Quote
        } else if props.list || name.starts_with("list") {
            ParaStyle::ListItem(props.level)
        } else if let Some(level) = props.outline {
            ParaStyle::Heading((level + 1).min(6))
        } else {
            ParaStyle::Normal
        };
        // A bullet typed as text (as written by `write`, or by hand).
        if para_style == ParaStyle::Normal
            && let Some(first) = runs.first_mut()
            && let Some(rest) = ["•\t", "◦\t", "▪\t"].iter().find_map(|b| first.text.strip_prefix(b))
        {
            // Its level from its indent, as `write` sets it.
            let level = (props.left / LIST_INDENT - 1).clamp(0, 8) as u8;
            first.text = rest.to_string();
            para_style = ParaStyle::ListItem(level);
        }
        runs.retain(|r| !r.text.is_empty());
        self.blocks.push(Block::Paragraph { style: para_style, runs, align: props.align });
    }

    fn end_table(&mut self) {
        if !self.cell.is_empty() {
            self.cells.push(std::mem::take(&mut self.cell));
        }
        if !self.cells.is_empty() {
            self.rows.push(std::mem::take(&mut self.cells));
        }
        if !self.rows.is_empty() {
            self.blocks.push(Block::Table { rows: std::mem::take(&mut self.rows) });
        }
    }

    fn runs(&mut self, from: usize, to: usize) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        let mut pending: Vec<u16> = Vec::new();
        let mut props_of_pending: Option<CharProps> = None;
        let flush = |runs: &mut Vec<Run>, pending: &mut Vec<u16>, props: &Option<CharProps>, fonts: &[String], normal: &CharProps| {
            if pending.is_empty() {
                return;
            }
            let text = String::from_utf16_lossy(pending);
            pending.clear();
            let p = props.clone().unwrap_or_default();
            runs.push(Run {
                text,
                bold: p.bold,
                italic: p.italic,
                underline: p.underline,
                highlight: p.highlight,
                props: run_props(&p, fonts, normal),
                ..Default::default()
            });
        };
        for i in from..to {
            let u = self.units[i];
            let mut p = self.normal.clone();
            p.bold = false;
            p.italic = false;
            p.underline = false;
            if let Some(grpprl) = self.fcs.get(i).and_then(|&fc| find(self.chpx, fc)) {
                p.apply(grpprl);
            }
            // Fields show their result, not their instructions.
            match u {
                0x13 => {
                    self.fields.push(false);
                    continue;
                }
                0x14 => {
                    if let Some(f) = self.fields.last_mut() {
                        *f = true;
                    }
                    continue;
                }
                0x15 => {
                    self.fields.pop();
                    continue;
                }
                _ => {}
            }
            if self.fields.iter().any(|shown| !shown) || p.hidden {
                continue;
            }
            let ch = match u {
                0x09 => Some('\t'),
                0x0B => Some(LINE_BREAK),
                0x0C => {
                    flush(&mut runs, &mut pending, &props_of_pending, self.fonts, &self.normal);
                    runs.push(Run::page_break());
                    None
                }
                0x1E => Some('-'),
                0x01 if p.special => {
                    flush(&mut runs, &mut pending, &props_of_pending, self.fonts, &self.normal);
                    match p.picture.and_then(|at| picture(self.data, at as usize)) {
                        Some(image) => runs.push(Run::picture(image)),
                        // A metafile, or a picture not stored in the file.
                        None => runs.push(Run { text: "[image]".into(), placeholder: true, ..Default::default() }),
                    }
                    None
                }
                0x08 if p.special => {
                    flush(&mut runs, &mut pending, &props_of_pending, self.fonts, &self.normal);
                    runs.push(Run { text: "[image]".into(), placeholder: true, ..Default::default() });
                    None
                }
                u if u < 0x20 => None,
                0xFFFC => None,
                _ => Some('\0'),
            };
            match ch {
                None => {}
                Some(c) => {
                    if props_of_pending.as_ref() != Some(&p) {
                        flush(&mut runs, &mut pending, &props_of_pending, self.fonts, &self.normal);
                        props_of_pending = Some(p.clone());
                    }
                    if c == '\0' {
                        pending.push(u);
                    } else {
                        let mut b = [0u16; 2];
                        pending.extend_from_slice(c.encode_utf16(&mut b));
                    }
                }
            }
        }
        flush(&mut runs, &mut pending, &props_of_pending, self.fonts, &self.normal);
        runs
    }
}

/// The DOCX run properties for a font, size, colour or strike that differs
/// from the document's defaults.
fn run_props(p: &CharProps, fonts: &[String], normal: &CharProps) -> String {
    let mut xml = String::new();
    if p.font != normal.font
        && let Some(name) = p.font.and_then(|f| fonts.get(f as usize))
    {
        let name = name.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
        xml.push_str(&format!(r#"<w:rFonts w:ascii="{name}" w:hAnsi="{name}" w:cs="{name}"/>"#));
    }
    if p.strike {
        xml.push_str("<w:strike/>");
    }
    if let Some([r, g, b]) = p.color {
        xml.push_str(&format!(r#"<w:color w:val="{r:02X}{g:02X}{b:02X}"/>"#));
    }
    if p.size != normal.size
        && let Some(s) = p.size
    {
        xml.push_str(&format!(r#"<w:sz w:val="{s}"/><w:szCs w:val="{s}"/>"#));
    }
    xml
}

/// The picture at `at` in the Data stream: a PICF header, then the shape and
/// the picture file it shows.
fn picture(data: &[u8], at: usize) -> Option<Image> {
    let lcb = u32_at(data, at) as usize;
    let cb_header = u16_at(data, at + 4) as usize;
    if lcb < cb_header || cb_header < 0x44 {
        return None;
    }
    let end = (at + lcb).min(data.len());
    let mm = u16_at(data, at + 6);
    let (goal_x, goal_y) = (i16_at(data, at + 28) as i64, i16_at(data, at + 30) as i64);
    let (mx, my) = (u16_at(data, at + 32) as i64, u16_at(data, at + 34) as i64);
    let mut p = at + cb_header;
    if mm == 0x66 {
        // A linked picture names its file first.
        p += 1 + data.get(p).copied().unwrap_or(0) as usize;
    }
    let file = blip_in(data, p, end)?;
    // Twips scaled by thousandths, to EMU.
    let scale = |goal: i64, m: i64| goal.max(0) * if m == 0 { 1000 } else { m } / 1000 * 635;
    Some(Image { data: Arc::new(file), cx: scale(goal_x, mx), cy: scale(goal_y, my), origin: None })
}

/// The first picture file among the drawing records in `data[from..to]`.
fn blip_in(data: &[u8], from: usize, to: usize) -> Option<Vec<u8>> {
    let mut p = from;
    while p + 8 <= to {
        let ver_inst = u16_at(data, p);
        let kind = u16_at(data, p + 2);
        let len = u32_at(data, p + 4) as usize;
        let body = p + 8;
        match kind {
            // Containers: look inside.
            0xF000..=0xF006 if ver_inst & 0xF == 0xF => {
                if let Some(found) = blip_in(data, body, (body + len).min(to)) {
                    return Some(found);
                }
            }
            // A blip store entry: 36 bytes, a name, then the picture.
            0xF007 => {
                let name = data.get(body + 33).copied().unwrap_or(0) as usize;
                if let Some(found) = blip_in(data, body + 36 + name, (body + len).min(to)) {
                    return Some(found);
                }
            }
            0xF01D..=0xF01F | 0xF029 | 0xF02A => {
                let inst = ver_inst >> 4;
                let two_ids = matches!(inst, 0x46B | 0x6E3 | 0x6E1 | 0x7A9 | 0x6E5);
                let start = body + 16 + if two_ids { 16 } else { 0 } + 1;
                let bytes = data.get(start..(body + len).min(data.len()))?.to_vec();
                if kind == 0xF01F {
                    // A DIB is a BMP file without its file header.
                    let mut bmp = b"BM".to_vec();
                    let size = 14 + bytes.len() as u32;
                    let header = u32_at(&bytes, 0);
                    let colors = u32_at(&bytes, 32);
                    let bits = u16_at(&bytes, 14);
                    let palette = if colors > 0 { colors } else if bits <= 8 { 1 << bits } else { 0 };
                    bmp.extend_from_slice(&size.to_le_bytes());
                    bmp.extend_from_slice(&[0; 4]);
                    bmp.extend_from_slice(&(14 + header + palette * 4).to_le_bytes());
                    bmp.extend_from_slice(&bytes);
                    return Some(bmp);
                }
                return Some(bytes);
            }
            _ => {}
        }
        p = body + len;
    }
    None
}

// ── Writing ──────────────────────────────────────────────────────────────

/// A bulleted paragraph's indent per level, in twips.
const LIST_INDENT: i16 = 360;
/// Where the text starts in the WordDocument stream, past the FIB.
const FC_TEXT: usize = 1024;
const ISTD_TITLE: u16 = 15;
const ISTD_QUOTE: u16 = 16;
const STYLE_COUNT: u16 = 17;

fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn sprm(v: &mut Vec<u8>, code: u16, arg: &[u8]) {
    put16(v, code);
    v.extend_from_slice(arg);
}

/// `blocks` as a Word 97–2003 document on `page`'s paper and in its font.
///
/// What it keeps: paragraph styles (headings, title, quote), alignment,
/// bullets (as bullet characters with a hanging indent, the way Word writes
/// them without a list definition), bold, italic, underline, highlight,
/// strike, font, size and colour, tables, inline pictures and page breaks.
pub fn write(blocks: &[Block], page: &docx::PageSetup) -> Result<Vec<u8>> {
    let mut w = Writer { fonts: vec![page.font.clone()], width: ((page.width - page.margins[1] - page.margins[3]) * 20.0) as i32, ..Default::default() };
    for block in blocks {
        w.block(block);
    }
    if w.paras.last().is_none_or(|p| p.in_table) {
        w.end_paragraph(0x0D, 0, Vec::new(), false);
    }
    w.finish(page)
}

#[derive(Default)]
struct Writer {
    text: Vec<u16>,
    /// Character runs: where each ends (as a character position) and its
    /// properties.
    chars: Vec<(usize, Vec<u8>)>,
    paras: Vec<Para>,
    fonts: Vec<String>,
    data: Vec<u8>,
    /// The text width, in twips.
    width: i32,
    shapes: u32,
}

struct Para {
    /// Where it ends, past its mark.
    end: usize,
    istd: u16,
    grpprl: Vec<u8>,
    in_table: bool,
}

impl Writer {
    fn push(&mut self, units: &[u16], grpprl: Vec<u8>) {
        if units.is_empty() {
            return;
        }
        self.text.extend_from_slice(units);
        match self.chars.last_mut() {
            Some(last) if last.1 == grpprl => last.0 = self.text.len(),
            _ => self.chars.push((self.text.len(), grpprl)),
        }
    }

    fn end_paragraph(&mut self, mark: u16, istd: u16, grpprl: Vec<u8>, in_table: bool) {
        self.push(&[mark], Vec::new());
        self.paras.push(Para { end: self.text.len(), istd, grpprl, in_table });
    }

    fn font(&mut self, name: &str) -> u16 {
        match self.fonts.iter().position(|f| f == name) {
            Some(i) => i as u16,
            None => {
                self.fonts.push(name.to_string());
                (self.fonts.len() - 1) as u16
            }
        }
    }

    fn block(&mut self, block: &Block) {
        match block {
            Block::Paragraph { style, runs, align } => {
                let istd = match style {
                    ParaStyle::Title => ISTD_TITLE,
                    ParaStyle::Heading(n) => (*n).clamp(1, 6) as u16,
                    ParaStyle::Quote => ISTD_QUOTE,
                    _ => 0,
                };
                let mut grpprl = Vec::new();
                let jc = match align {
                    Align::Start => None,
                    Align::Center => Some(1),
                    Align::End => Some(2),
                    Align::Justify => Some(3),
                };
                if let Some(jc) = jc {
                    sprm(&mut grpprl, 0x2403, &[jc]);
                }
                if let ParaStyle::Heading(n) = style {
                    sprm(&mut grpprl, 0x2640, &[(*n).clamp(1, 9) - 1]);
                }
                if let ParaStyle::ListItem(level) = style {
                    let indent = LIST_INDENT * (*level as i16 + 1);
                    sprm(&mut grpprl, 0x840F, &indent.to_le_bytes());
                    sprm(&mut grpprl, 0x8411, &(-LIST_INDENT).to_le_bytes());
                    let bullet = ["•\t", "◦\t", "▪\t"][*level as usize % 3];
                    self.push(&bullet.encode_utf16().collect::<Vec<_>>(), Vec::new());
                }
                for run in runs {
                    if let Some(image) = &run.image {
                        self.picture(image);
                    } else if run.is_page_break() {
                        self.push(&[0x0C], Vec::new());
                    } else if !run.placeholder {
                        let units = units_of(&run.text);
                        let props = self.chpx(run);
                        self.push(&units, props);
                    }
                }
                self.end_paragraph(0x0D, istd, grpprl, false);
            }
            Block::Table { rows } => {
                let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
                if cols == 0 {
                    return;
                }
                let mut cell_props = Vec::new();
                sprm(&mut cell_props, 0x2416, &[1]);
                sprm(&mut cell_props, 0x6649, &1u32.to_le_bytes());
                for row in rows {
                    for c in 0..cols {
                        let cell = row.get(c).map_or("", String::as_str);
                        let lines: Vec<&str> = cell.split('\n').collect();
                        for (k, line) in lines.iter().enumerate() {
                            self.push(&units_of(line), Vec::new());
                            let mark = if k + 1 == lines.len() { 0x07 } else { 0x0D };
                            self.end_paragraph(mark, 0, cell_props.clone(), true);
                        }
                    }
                    let mut row_props = cell_props.clone();
                    sprm(&mut row_props, 0x2417, &[1]);
                    row_props.extend(self.row_definition(cols));
                    self.end_paragraph(0x07, 0, row_props, true);
                }
            }
        }
    }

    /// A row's cells: their edges, and a thin border round each.
    fn row_definition(&self, cols: usize) -> Vec<u8> {
        let gap: i16 = 108;
        let mut v = Vec::new();
        // The cells' padding, and the table's borders.
        sprm(&mut v, 0x9602, &gap.to_le_bytes());
        let brc = [4u8, 1, 0, 0];
        let mut borders = vec![24u8];
        for _ in 0..6 {
            borders.extend_from_slice(&brc);
        }
        sprm(&mut v, 0xD605, &borders);
        let mut def = Vec::new();
        def.push(cols as u8);
        let col = self.width / cols as i32;
        for i in 0..=cols {
            let x = (i as i32 * col - gap as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            def.extend_from_slice(&x.to_le_bytes());
        }
        for _ in 0..cols {
            def.extend_from_slice(&[0, 0, 0, 0]);
            for _ in 0..4 {
                def.extend_from_slice(&brc);
            }
        }
        put16(&mut v, SPRM_TDEF_TABLE);
        put16(&mut v, def.len() as u16 + 1);
        v.extend(def);
        v
    }

    /// A run's character properties.
    fn chpx(&mut self, run: &Run) -> Vec<u8> {
        let mut g = Vec::new();
        if run.bold {
            sprm(&mut g, 0x0835, &[1]);
        }
        if run.italic {
            sprm(&mut g, 0x0836, &[1]);
        }
        if docx::run_prop(&run.props, "strike").is_some() {
            sprm(&mut g, 0x0837, &[1]);
        }
        if run.underline {
            sprm(&mut g, 0x2A3E, &[1]);
        }
        if run.highlight {
            sprm(&mut g, 0x2A0C, &[7]);
        }
        if let Some(font) = docx::run_font(&run.props) {
            let f = self.font(&font).to_le_bytes();
            for code in [0x4A4F, 0x4A50, 0x4A51] {
                sprm(&mut g, code, &f);
            }
        }
        if let Some(sz) = docx::run_prop(&run.props, "sz").and_then(|v| v.parse::<u16>().ok()) {
            sprm(&mut g, 0x4A43, &sz.to_le_bytes());
            sprm(&mut g, 0x4A61, &sz.to_le_bytes());
        }
        if let Some([r, gr, b]) = docx::run_prop(&run.props, "color").and_then(|v| docx::hex_color(&v)) {
            let cv = (r >> 8) as u32 | ((gr >> 8) as u32) << 8 | ((b >> 8) as u32) << 16;
            sprm(&mut g, 0x6870, &cv.to_le_bytes());
        }
        g
    }

    /// An inline picture: its header and drawing records in the Data
    /// stream, and a special character pointing at them.
    fn picture(&mut self, image: &Image) {
        let Some((kind, instance, bt, file)) = blip_of(&image.data) else {
            return;
        };
        let (cx, cy) = if image.cx > 0 && image.cy > 0 {
            (image.cx, image.cy)
        } else {
            let (w, h) = pixels_of(&image.data).unwrap_or((96, 96));
            (w as i64 * 9525, h as i64 * 9525)
        };
        let twips = |emu: i64| (emu / 635).clamp(1, i16::MAX as i64) as u16;
        let uid = uid_of(&file);
        let blip_len = 16 + 1 + file.len();
        let fbse_len = 36 + 8 + blip_len;
        let sp_len = (8 + 8) + (8 + 6);
        let lcb = 68 + 8 + sp_len + 8 + fbse_len;

        let at = self.data.len();
        let d = &mut self.data;
        put32(d, lcb as u32);
        put16(d, 0x44);
        // MM_SHAPE: the picture is a drawing that follows.
        put16(d, 0x64);
        d.extend_from_slice(&[0; 6 + 14]);
        put16(d, twips(cx));
        put16(d, twips(cy));
        put16(d, 1000);
        put16(d, 1000);
        d.extend_from_slice(&[0; 8 + 2 + 16 + 4]);
        put16(d, 0);
        // The shape: a picture frame showing the first picture below.
        let header = |d: &mut Vec<u8>, ver_inst: u16, kind: u16, len: usize| {
            put16(d, ver_inst);
            put16(d, kind);
            put32(d, len as u32);
        };
        header(d, 0x000F, 0xF004, sp_len);
        header(d, (75 << 4) | 2, 0xF00A, 8);
        self.shapes += 1;
        put32(d, 1024 + self.shapes);
        put32(d, 0x0A00);
        header(d, (1 << 4) | 3, 0xF00B, 6);
        put16(d, 0x4104);
        put32(d, 1);
        // The picture itself, in a blip store entry.
        header(d, ((bt as u16) << 4) | 2, 0xF007, fbse_len);
        d.push(bt);
        d.push(bt);
        d.extend_from_slice(&uid);
        put16(d, 0x00FF);
        put32(d, (8 + blip_len) as u32);
        put32(d, 1);
        put32(d, 0);
        d.extend_from_slice(&[0, 0, 0, 0]);
        header(d, instance << 4, kind, blip_len);
        d.extend_from_slice(&uid);
        d.push(0xFF);
        d.extend_from_slice(&file);

        let mut g = Vec::new();
        sprm(&mut g, 0x0855, &[1]);
        sprm(&mut g, 0x6A03, &(at as u32).to_le_bytes());
        self.push(&[0x01], g);
    }

    fn finish(self, page: &docx::PageSetup) -> Result<Vec<u8>> {
        let ccp = self.text.len();
        let mut word = vec![0u8; FC_TEXT];
        for u in &self.text {
            put16(&mut word, *u);
        }
        let fc = |cp: usize| (FC_TEXT + 2 * cp) as u32;

        // The section: paper and margins.
        let tw = |pt: f64| (pt * 20.0).round().clamp(0.0, u16::MAX as f64) as u16;
        let mut sepx = Vec::new();
        sprm(&mut sepx, 0xB01F, &tw(page.width).to_le_bytes());
        sprm(&mut sepx, 0xB020, &tw(page.height).to_le_bytes());
        sprm(&mut sepx, 0xB021, &tw(page.margins[3]).to_le_bytes());
        sprm(&mut sepx, 0xB022, &tw(page.margins[1]).to_le_bytes());
        sprm(&mut sepx, 0x9023, &tw(page.margins[0]).to_le_bytes());
        sprm(&mut sepx, 0x9024, &tw(page.margins[2]).to_le_bytes());
        sprm(&mut sepx, 0x301D, &[if page.width > page.height { 2 } else { 1 }]);
        word.resize(word.len().div_ceil(2) * 2, 0);
        let sepx_at = word.len();
        put16(&mut word, sepx.len() as u16);
        word.extend(&sepx);

        // Character runs and paragraphs, packed into pages.
        let mut runs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        let mut start = 0;
        for (end, g) in &self.chars {
            runs.push((fc(start), fc(*end), g.clone()));
            start = *end;
        }
        let mut paras: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        let mut start = 0;
        for p in &self.paras {
            let mut body = p.istd.to_le_bytes().to_vec();
            body.extend(&p.grpprl);
            paras.push((fc(start), fc(p.end), body));
            start = p.end;
        }
        let bte = |pages: Vec<([u8; 512], u32, u32)>, word: &mut Vec<u8>| {
            word.resize(word.len().div_ceil(512) * 512, 0);
            let mut plc = Vec::new();
            let mut pns = Vec::new();
            for (k, (bytes, first, last)) in pages.iter().enumerate() {
                pns.push((word.len() / 512) as u32);
                word.extend_from_slice(bytes);
                put32(&mut plc, *first);
                if k + 1 == pages.len() {
                    put32(&mut plc, *last);
                }
            }
            for pn in pns {
                put32(&mut plc, pn);
            }
            plc
        };
        let plc_chpx = bte(fkp_pages(&runs, false), &mut word);
        let plc_papx = bte(fkp_pages(&paras, true), &mut word);

        let mut table = Vec::new();
        let place = |table: &mut Vec<u8>, bytes: &[u8]| {
            let at = table.len() as u32;
            table.extend_from_slice(bytes);
            (at, bytes.len() as u32)
        };
        let stsh = place(&mut table, &stylesheet(page));
        let ffn = place(&mut table, &font_table(&self.fonts));
        let mut dop = vec![0u8; 500];
        dop[0] = 0x02;
        dop[10..12].copy_from_slice(&720u16.to_le_bytes());
        let dop = place(&mut table, &dop);
        let mut sed = Vec::new();
        put32(&mut sed, 0);
        put32(&mut sed, ccp as u32);
        put16(&mut sed, 0);
        put32(&mut sed, sepx_at as u32);
        put16(&mut sed, 0);
        put32(&mut sed, u32::MAX);
        let sed = place(&mut table, &sed);
        let mut clx = vec![2u8];
        put32(&mut clx, 16);
        put32(&mut clx, 0);
        put32(&mut clx, ccp as u32);
        put16(&mut clx, 0);
        put32(&mut clx, FC_TEXT as u32);
        put16(&mut clx, 0);
        let clx = place(&mut table, &clx);
        let chpx = place(&mut table, &plc_chpx);
        let papx = place(&mut table, &plc_papx);

        // The FIB, at the start of the WordDocument stream.
        let mut fib = Vec::new();
        put16(&mut fib, 0xA5EC);
        put16(&mut fib, 0x00C1);
        put16(&mut fib, 0);
        put16(&mut fib, 0x0409);
        put16(&mut fib, 0);
        let pictures = if self.data.is_empty() { 0 } else { 0x0008 };
        put16(&mut fib, 0x0200 | 0x1000 | pictures);
        put16(&mut fib, 0x00BF);
        put32(&mut fib, 0);
        fib.extend_from_slice(&[0, 0]);
        put16(&mut fib, 0);
        put16(&mut fib, 0);
        put32(&mut fib, FC_TEXT as u32);
        put32(&mut fib, fc(ccp));
        put16(&mut fib, 14);
        fib.extend_from_slice(&[0; 28]);
        put16(&mut fib, 22);
        let mut lw = [0u32; 22];
        lw[0] = word.len() as u32;
        lw[3] = ccp as u32;
        for v in lw {
            put32(&mut fib, v);
        }
        put16(&mut fib, 93);
        let mut pairs = [(0u32, 0u32); 93];
        pairs[0] = stsh;
        pairs[1] = stsh;
        pairs[6] = sed;
        pairs[12] = chpx;
        pairs[13] = papx;
        pairs[15] = ffn;
        pairs[31] = dop;
        pairs[33] = clx;
        for (fc, lcb) in pairs {
            put32(&mut fib, fc);
            put32(&mut fib, lcb);
        }
        put16(&mut fib, 0);
        word[..fib.len()].copy_from_slice(&fib);

        // Streams of less than 4096 bytes would belong in the mini stream;
        // what follows the content is never read.
        let mut data = self.data;
        for s in [&mut word, &mut table, &mut data] {
            if s.len() < 4096 {
                s.resize(4096, 0);
            }
        }
        let mut streams: Vec<(&str, &[u8])> = vec![("WordDocument", &word), ("1Table", &table)];
        if pictures != 0 {
            streams.push(("Data", &data));
        }
        Ok(cfb::write(&streams))
    }
}

/// Text as UTF-16 for the document: tabs and line breaks as Word writes
/// them, other control characters left out.
fn units_of(text: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for c in text.chars() {
        match c {
            '\t' => out.push(0x09),
            LINE_BREAK | '\n' => out.push(0x0B),
            '\u{FFFC}' => {}
            c if (c as u32) < 0x20 => {}
            c => {
                let mut b = [0u16; 2];
                out.extend_from_slice(c.encode_utf16(&mut b));
            }
        }
    }
    out
}

/// Formatted disk pages: spans of the text (by stream offset) with their
/// properties, packed 512 bytes at a time — the offsets at the front, the
/// properties stored down from the end. Paragraph pages (`papx`) give each
/// span 13 bytes of index and store "istd + sprms"; character pages give
/// each one byte and store the sprms.
fn fkp_pages(spans: &[(u32, u32, Vec<u8>)], papx: bool) -> Vec<([u8; 512], u32, u32)> {
    let mut pages = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        let mut page = [0u8; 512];
        let mut top = 511usize;
        let mut entries: Vec<(u32, u32, u8)> = Vec::new();
        let mut stored: Vec<(&[u8], u8)> = Vec::new();
        while i < spans.len() {
            let (a, b, g) = &spans[i];
            let n = entries.len() + 1;
            let low = 4 * (n + 1) + n * if papx { 13 } else { 1 };
            // How the properties are stored: a size, then them.
            let stored_bytes: Vec<u8> = if papx {
                if g.len() % 2 == 1 {
                    std::iter::once(g.len().div_ceil(2) as u8).chain(g.iter().copied()).collect()
                } else {
                    [0, (g.len() / 2) as u8].into_iter().chain(g.iter().copied()).collect()
                }
            } else if g.is_empty() {
                Vec::new()
            } else {
                std::iter::once(g.len() as u8).chain(g.iter().copied()).collect()
            };
            let (offset, new_top) = if stored_bytes.is_empty() {
                (0, top)
            } else if let Some((_, o)) = stored.iter().find(|(x, _)| *x == g.as_slice()) {
                (*o, top)
            } else if stored_bytes.len() < top {
                let pos = (top - stored_bytes.len()) & !1;
                ((pos / 2) as u8, pos)
            } else {
                (0, 0)
            };
            if low > new_top {
                if entries.is_empty() {
                    // Properties too long for a page of their own: dropped.
                    entries.push((*a, *b, 0));
                    i += 1;
                }
                break;
            }
            if offset != 0 && new_top != top {
                page[new_top..new_top + stored_bytes.len()].copy_from_slice(&stored_bytes);
                stored.push((g.as_slice(), offset));
            }
            entries.push((*a, *b, offset));
            top = new_top;
            i += 1;
        }
        let n = entries.len();
        for (k, (a, _, _)) in entries.iter().enumerate() {
            page[4 * k..4 * k + 4].copy_from_slice(&a.to_le_bytes());
        }
        let last = entries.last().map_or(0, |e| e.1);
        page[4 * n..4 * n + 4].copy_from_slice(&last.to_le_bytes());
        for (k, (_, _, offset)) in entries.iter().enumerate() {
            let at = 4 * (n + 1) + k * if papx { 13 } else { 1 };
            page[at] = *offset;
        }
        page[511] = n as u8;
        pages.push((page, entries.first().map_or(0, |e| e.0), last));
    }
    pages
}

/// A style to write: its built-in id, kind, base, name, and paragraph and
/// character properties.
type StyleDef = (u16, u16, u16, &'static str, Option<Vec<u8>>, Vec<u8>);

/// The styles `write` uses, at the places Word expects the built-in ones.
fn stylesheet(page: &docx::PageSetup) -> Vec<u8> {
    let mut out = Vec::new();
    // STSHI: the count, the base size of a style, built-in names written,
    // the highest built-in style id, the fixed styles, the standard fonts.
    put16(&mut out, 18);
    for v in [STYLE_COUNT, 10, 1, 0x5B, 15, 0, 0, 0, 0] {
        put16(&mut out, v);
    }
    let after = (page.after * 20.0).round() as u16;
    let size = (page.size * 2.0).round() as u16;
    let line = (page.line * 240.0).round() as i16;
    let navy = 0x0064_381Fu32.to_le_bytes();
    let heading = |n: u16| {
        let (size, before): (u16, u16) = match n {
            1 => (32, 360),
            2 => (26, 160),
            3 => (24, 160),
            _ => (22, 80),
        };
        let mut p = Vec::new();
        sprm(&mut p, 0x2406, &[1]);
        sprm(&mut p, 0xA413, &before.to_le_bytes());
        sprm(&mut p, 0xA414, &80u16.to_le_bytes());
        sprm(&mut p, 0x2640, &[(n - 1) as u8]);
        let mut c = Vec::new();
        sprm(&mut c, 0x0835, &[1]);
        sprm(&mut c, 0x4A43, &size.to_le_bytes());
        sprm(&mut c, 0x6870, &navy);
        (p, c)
    };
    for istd in 0..STYLE_COUNT {
        let style: Option<StyleDef> = match istd {
            0 => {
                let mut p = Vec::new();
                sprm(&mut p, 0xA414, &after.to_le_bytes());
                let mut lspd = line.to_le_bytes().to_vec();
                lspd.extend_from_slice(&1u16.to_le_bytes());
                sprm(&mut p, 0x6412, &lspd);
                let mut c = Vec::new();
                for code in [0x4A4F, 0x4A50, 0x4A51] {
                    sprm(&mut c, code, &0u16.to_le_bytes());
                }
                sprm(&mut c, 0x4A43, &size.to_le_bytes());
                sprm(&mut c, 0x4A61, &size.to_le_bytes());
                Some((0, 1, 0xFFF, "Normal", Some(p), c))
            }
            1..=6 => {
                let (p, c) = heading(istd);
                let name = ["heading 1", "heading 2", "heading 3", "heading 4", "heading 5", "heading 6"][istd as usize - 1];
                Some((istd, 1, 0, name, Some(p), c))
            }
            10 => Some((65, 2, 0xFFF, "Default Paragraph Font", None, Vec::new())),
            ISTD_TITLE => {
                let mut p = Vec::new();
                sprm(&mut p, 0xA414, &160u16.to_le_bytes());
                let mut c = Vec::new();
                sprm(&mut c, 0x4A43, &56u16.to_le_bytes());
                Some((62, 1, 0, "Title", Some(p), c))
            }
            ISTD_QUOTE => {
                let mut p = Vec::new();
                sprm(&mut p, 0x840F, &864i16.to_le_bytes());
                sprm(&mut p, 0x840E, &864i16.to_le_bytes());
                let mut c = Vec::new();
                sprm(&mut c, 0x0836, &[1]);
                sprm(&mut c, 0x6870, &0x0040_4040u32.to_le_bytes());
                Some((0xFFE, 1, 0, "Quote", Some(p), c))
            }
            _ => None,
        };
        let Some((sti, stk, base, name, papx, chpx)) = style else {
            put16(&mut out, 0);
            continue;
        };
        let next = if stk == 2 { istd } else { 0 };
        let mut std = Vec::new();
        put16(&mut std, sti);
        put16(&mut std, stk | (base << 4));
        put16(&mut std, (if stk == 1 { 2 } else { 1 }) | (next << 4));
        put16(&mut std, 0);
        put16(&mut std, 0);
        let units: Vec<u16> = name.encode_utf16().collect();
        put16(&mut std, units.len() as u16);
        for u in units {
            put16(&mut std, u);
        }
        put16(&mut std, 0);
        if let Some(p) = papx {
            put16(&mut std, 2 + p.len() as u16);
            put16(&mut std, istd);
            std.extend(&p);
            std.resize(std.len().div_ceil(2) * 2, 0);
        }
        put16(&mut std, chpx.len() as u16);
        std.extend(&chpx);
        std.resize(std.len().div_ceil(2) * 2, 0);
        let len = std.len() as u16;
        std[6..8].copy_from_slice(&len.to_le_bytes());
        put16(&mut out, len);
        out.extend(std);
    }
    out
}

fn font_table(fonts: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    put16(&mut out, fonts.len() as u16);
    put16(&mut out, 0);
    for name in fonts {
        let units: Vec<u16> = name.encode_utf16().take(31).collect();
        let mut ffn = Vec::new();
        // Variable pitch, TrueType, a sans or serif family by its name.
        let family = if name.to_ascii_lowercase().contains("sans") || name == "Calibri" || name == "Arial" { 0x20 } else { 0x10 };
        ffn.push(0x06 | family);
        put16(&mut ffn, 400);
        ffn.push(0);
        ffn.push(0);
        ffn.extend_from_slice(&[0; 10 + 24]);
        for u in units {
            put16(&mut ffn, u);
        }
        put16(&mut ffn, 0);
        out.push(ffn.len() as u8);
        out.extend(ffn);
    }
    out
}

/// A picture file as an Office drawing "blip": its record type and
/// instance, its type number, and its bytes (a BMP without its file
/// header). Kinds Word cannot hold this way are turned into PNG.
fn blip_of(data: &[u8]) -> Option<(u16, u16, u8, Vec<u8>)> {
    match docx::picture_type(data).map(|(ext, _)| ext) {
        Some("png") => Some((0xF01E, 0x6E0, 6, data.to_vec())),
        Some("jpeg") => Some((0xF01D, 0x46A, 5, data.to_vec())),
        Some("bmp") if data.len() > 14 => Some((0xF01F, 0x7A8, 7, data[14..].to_vec())),
        Some("tiff") => Some((0xF029, 0x6E4, 17, data.to_vec())),
        _ if data.is_empty() => None,
        _ => {
            use gtk4::gdk_pixbuf::prelude::*;
            let loader = gtk4::gdk_pixbuf::PixbufLoader::new();
            loader.write(data).ok()?;
            loader.close().ok()?;
            let png = loader.pixbuf()?.save_to_bufferv("png", &[]).ok()?;
            Some((0xF01E, 0x6E0, 6, png))
        }
    }
}

fn pixels_of(data: &[u8]) -> Option<(i32, i32)> {
    use gtk4::gdk_pixbuf::prelude::*;
    let loader = gtk4::gdk_pixbuf::PixbufLoader::new();
    loader.write(data).ok()?;
    loader.close().ok()?;
    loader.pixbuf().map(|p| (p.width(), p.height()))
}

/// A 16-byte id for a picture, from its bytes.
fn uid_of(data: &[u8]) -> [u8; 16] {
    let hash = |seed: u64| data.iter().fold(seed, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01B3));
    let mut uid = [0u8; 16];
    uid[..8].copy_from_slice(&hash(0xCBF2_9CE4_8422_2325).to_le_bytes());
    uid[8..].copy_from_slice(&hash(0x8422_2325_CBF2_9CE4).to_le_bytes());
    uid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        // A 2×1 PNG: one red and one blue pixel.
        let pixbuf = gtk4::gdk_pixbuf::Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, 2, 1).unwrap();
        pixbuf.put_pixel(0, 0, 255, 0, 0, 255);
        pixbuf.put_pixel(1, 0, 0, 0, 255, 255);
        pixbuf.save_to_bufferv("png", &[]).unwrap()
    }

    /// What is written reads back as itself.
    #[test]
    fn a_written_document_reads_back() {
        let r = |text: &str| Run { text: text.into(), ..Default::default() };
        let mut big = r("big red");
        big.props = r#"<w:rFonts w:ascii="Georgia" w:hAnsi="Georgia"/><w:color w:val="C00000"/><w:sz w:val="36"/>"#.into();
        let picture = Image { data: std::sync::Arc::new(png()), cx: 914_400, cy: 457_200, origin: None };
        let mut long = String::new();
        for i in 0..400 {
            long.push_str(&format!("word{i} "));
        }
        let blocks = vec![
            Block::paragraph(ParaStyle::Title, vec![r("The Title")]),
            Block::paragraph(ParaStyle::Heading(1), vec![r("Chapter — ünïcödé 中文")]),
            Block::Paragraph { style: ParaStyle::Normal, align: Align::Center, runs: vec![
                r("plain "), Run { bold: true, ..r("bold") }, r(" "), Run { italic: true, underline: true, ..r("both") },
                r(" "), Run { highlight: true, ..r("marked") }, r(" "), big,
            ] },
            Block::paragraph(ParaStyle::ListItem(0), vec![r("first")]),
            Block::paragraph(ParaStyle::ListItem(1), vec![r("nested")]),
            Block::paragraph(ParaStyle::Quote, vec![r("quoted")]),
            Block::Table { rows: vec![vec!["a".into(), "b\nc".into()], vec!["1".into(), "2".into()]] },
            Block::paragraph(ParaStyle::Normal, vec![r("before"), Run::picture(picture), r("after")]),
            Block::paragraph(ParaStyle::Normal, vec![r(&long)]),
            Block::paragraph(ParaStyle::Normal, vec![r("x"), Run::page_break(), r("y")]),
        ];
        let page = docx::PageSetup { width: 595.3, height: 841.9, ..Default::default() };
        let file = write(&blocks, &page).unwrap();
        assert!(is_doc(&file));
        let back = read(&file).unwrap();
        assert_eq!((back.paper.width, back.paper.height), (11906, 16838));
        assert_eq!(back.text.font, "Calibri");
        let text = |b: &Block| match b {
            Block::Paragraph { runs, .. } => runs.iter().filter(|r| r.image.is_none() && !r.placeholder).map(|r| r.text.as_str()).collect::<String>(),
            Block::Table { rows } => format!("{rows:?}"),
        };
        let style = |b: &Block| match b {
            Block::Paragraph { style, .. } => Some(*style),
            _ => None,
        };
        assert_eq!(back.blocks.len(), blocks.len(), "{:?}", back.blocks.iter().map(text).collect::<Vec<_>>());
        for (a, b) in blocks.iter().zip(&back.blocks) {
            assert_eq!(text(a), text(b));
            assert_eq!(style(a), style(b), "{}", text(a));
        }
        let Block::Paragraph { runs, align, .. } = &back.blocks[2] else { panic!() };
        assert_eq!(*align, Align::Center);
        let find = |t: &str| runs.iter().find(|r| r.text == t).unwrap();
        assert!(find("bold").bold && !find("plain ").bold);
        assert!(find("both").italic && find("both").underline);
        assert!(find("marked").highlight);
        let props = &find("big red").props;
        assert!(props.contains("Georgia") && props.contains("C00000") && props.contains(r#"w:val="36""#), "{props}");
        let Block::Paragraph { runs, .. } = &back.blocks[7] else { panic!() };
        let image = runs.iter().find_map(|r| r.image.clone()).expect("the picture reads back");
        assert_eq!(image.data.as_slice(), png().as_slice());
        assert_eq!((image.cx, image.cy), (914_400, 457_200));
        let Block::Paragraph { runs, .. } = &back.blocks[9] else { panic!() };
        assert!(runs.iter().any(Run::is_page_break));
    }
}

/// `RAVEN_TEST_DOC=a.doc cargo test doc_real -- --ignored --nocapture` prints
/// what a real file reads as, and writes it as a DOCX beside it.
#[cfg(test)]
#[test]
#[ignore]
fn doc_real() {
    let path = std::env::var("RAVEN_TEST_DOC").unwrap();
    let read = read(&std::fs::read(&path).unwrap()).unwrap();
    eprintln!("paper {:?}, text {:?}", read.paper, read.text);
    for b in &read.blocks {
        match b {
            Block::Paragraph { style, runs, align } => eprintln!("{style:?} {align:?} {:?}", runs.iter().map(|r| {
                if let Some(i) = &r.image { format!("<image {} bytes {}x{}>", i.data.len(), i.cx, i.cy) }
                else { format!("{}{}{}{}[{}]", if r.bold {"*"} else {""}, if r.italic {"/"} else {""}, if r.underline {"_"} else {""}, r.text, r.props) }
            }).collect::<Vec<_>>()),
            Block::Table { rows } => eprintln!("table {rows:?}"),
        }
    }
    let built = docx::build(&read.blocks, read.paper, &read.text).unwrap();
    let page = docx::load(&built).unwrap().page;
    std::fs::write(format!("{path}.pdf"), crate::render::pdf(&read.blocks, &page, "test").unwrap()).unwrap();
    std::fs::write(format!("{path}.docx"), built).unwrap();
    std::fs::write(format!("{path}.written.doc"), write(&read.blocks, &page).unwrap()).unwrap();
}
