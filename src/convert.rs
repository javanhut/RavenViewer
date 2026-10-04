//! What a file is, and turning it into another kind: PDF, Word (DOCX and
//! Word 97–2003) and plain text, in whichever direction makes sense. PDFs are
//! only ever made, never taken apart into text.

use std::path::Path;

use anyhow::{Result, bail};

use crate::docx::{self, Docx, Paper, TextDefaults};
use crate::{cfb, doc, pdfedit, render};

/// A kind of file Raven Viewer reads or writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Docx,
    /// Word 97–2003.
    Doc,
    Text,
}

impl Format {
    /// The format a file name asks for; DOCX when it does not say.
    pub fn of(path: &Path) -> Format {
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "pdf" => Format::Pdf,
            "doc" | "dot" => Format::Doc,
            "txt" | "text" | "md" | "markdown" | "log" => Format::Text,
            _ => Format::Docx,
        }
    }

    /// What the bytes are, whatever the file is called.
    pub fn sniff(bytes: &[u8]) -> Result<Format> {
        if bytes.windows(5).take(1024).any(|w| w == b"%PDF-") {
            return Ok(Format::Pdf);
        }
        if bytes.starts_with(b"PK") {
            return Ok(Format::Docx);
        }
        if cfb::is_compound(bytes) {
            if doc::is_doc(bytes) {
                return Ok(Format::Doc);
            }
            bail!("this is an Office file of another kind (a spreadsheet or a presentation?), not a Word document");
        }
        if looks_like_text(bytes) {
            return Ok(Format::Text);
        }
        bail!("not a PDF, Word document or text file")
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Docx => "docx",
            Format::Doc => "doc",
            Format::Text => "txt",
        }
    }
}

/// Text, as opposed to some other binary file: no NUL bytes (unless it is
/// UTF-16, which says so up front).
fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    !bytes.iter().take(64 * 1024).any(|&b| b == 0)
}

/// The paper new documents are set on where the reader is: Letter in the
/// Americas that use it, A4 elsewhere.
pub fn local_paper() -> Paper {
    let locale = ["LC_ALL", "LC_PAPER", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let country = locale.split(['.', '@']).next().and_then(|l| l.split_once('_')).map(|(_, c)| c.to_ascii_uppercase());
    match country.as_deref() {
        Some("US" | "CA" | "MX" | "CL" | "CO" | "VE" | "PH" | "PR" | "GT" | "CR" | "PA" | "DO" | "SV" | "NI" | "BZ") => Paper::LETTER,
        // No locale at all: Word's own default.
        None => Paper::LETTER,
        Some(_) => Paper::A4,
    }
}

/// A Word or text file as a DOCX package, and what it was.
pub fn as_docx(bytes: &[u8]) -> Result<(Vec<u8>, Format)> {
    match Format::sniff(bytes)? {
        Format::Docx => Ok((bytes.to_vec(), Format::Docx)),
        Format::Doc => Ok((doc::to_docx(bytes)?, Format::Doc)),
        Format::Text => {
            let blocks = docx::from_text(&docx::decode_text(bytes));
            Ok((docx::build(&blocks, local_paper(), &TextDefaults::plain_text())?, Format::Text))
        }
        Format::Pdf => bail!("a PDF can’t be turned back into a document"),
    }
}

/// A Word or text file read for showing and editing.
pub fn open_document(bytes: &[u8]) -> Result<(Docx, Format)> {
    let (package, format) = as_docx(bytes)?;
    Ok((docx::load(&package)?, format))
}

/// A document's whole content written as `to`.
pub fn write(doc: &Docx, blocks: &[docx::Block], to: Format, title: &str) -> Result<Vec<u8>> {
    match to {
        Format::Pdf => render::pdf(blocks, &doc.sections, title),
        Format::Text => Ok(docx::text_of(blocks).into_bytes()),
        Format::Doc => doc::write(blocks, &doc.page),
        Format::Docx => {
            let keep: Vec<docx::Out> = (0..doc.items.len()).filter(|&i| doc.items[i].visible()).map(docx::Out::Keep).collect();
            doc.save(&keep)
        }
    }
}

/// A file of any kind Raven Viewer reads, as `to`.
pub fn convert(bytes: &[u8], to: Format, title: &str) -> Result<Vec<u8>> {
    let from = Format::sniff(bytes)?;
    if from == to {
        return Ok(bytes.to_vec());
    }
    if from == Format::Pdf {
        bail!("a PDF can only be combined with others, not turned into a {}", to.extension().to_uppercase());
    }
    let (doc, _) = open_document(bytes)?;
    write(&doc, &doc.blocks(), to, title)
}

/// Files put one after another: PDFs page by page, Word and text files as
/// one document, each starting on a new page. A mix of PDFs and documents
/// becomes a PDF, the documents set on paper first.
pub fn combine(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    if files.len() < 2 {
        bail!("choose at least two files");
    }
    let formats: Vec<Format> = files
        .iter()
        .map(|(name, b)| Format::sniff(b).map_err(|e| anyhow::anyhow!("{name}: {e}")))
        .collect::<Result<_>>()?;
    let titled = |name: &str| Path::new(name).file_stem().map_or(name.to_string(), |s| s.to_string_lossy().into_owned());
    if formats.contains(&Format::Pdf) {
        let pdfs: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(name, b)| Ok((name.clone(), convert(b, Format::Pdf, &titled(name)).map_err(|e| anyhow::anyhow!("{name}: {e}"))?)))
            .collect::<Result<_>>()?;
        return pdfedit::merge(&pdfs);
    }
    let docs: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(name, b)| Ok((name.clone(), as_docx(b).map_err(|e| anyhow::anyhow!("{name}: {e}"))?.0)))
        .collect::<Result<_>>()?;
    docx::merge(&docs)
}

/// What combining these files makes.
pub fn combined_format(formats: &[Format]) -> Format {
    if formats.contains(&Format::Pdf) { Format::Pdf } else { Format::Docx }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf_pages(bytes: Vec<u8>) -> Result<usize> {
        Ok(crate::pdf::load_info(&std::sync::Arc::new(bytes))?.page_sizes.len())
    }

    #[test]
    fn formats_are_told_by_content() {
        assert_eq!(Format::sniff(b"%PDF-1.7 ...").unwrap(), Format::Pdf);
        assert_eq!(Format::sniff("plain words\nand lines — ünïcödé\n".as_bytes()).unwrap(), Format::Text);
        assert_eq!(Format::sniff(&docx::blank(Paper::A4, &TextDefaults::document())).unwrap(), Format::Docx);
        assert!(Format::sniff(&[0u8, 1, 2, 3, 0, 0]).is_err());
        assert_eq!(Format::of(Path::new("a/b.TXT")), Format::Text);
        assert_eq!(Format::of(Path::new("report")), Format::Docx);
    }

    #[test]
    fn text_becomes_a_document_and_back() {
        let text = "First line\n\tIndented\n\nAfter a blank\n";
        let (doc, format) = open_document(text.as_bytes()).unwrap();
        assert_eq!(format, Format::Text);
        let back = write(&doc, &doc.blocks(), Format::Text, "t").unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), text);
        let pdf = convert(text.as_bytes(), Format::Pdf, "t").unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
    }

    #[test]
    fn a_mix_of_pdfs_and_documents_combines_into_a_pdf() {
        let pdf = convert(b"one\n", Format::Pdf, "one").unwrap();
        let merged = combine(&[("a.pdf".into(), pdf), ("b.txt".into(), b"two\nlines\n".to_vec())]).unwrap();
        assert_eq!(pdf_pages(merged).unwrap(), 2);
        let docs = combine(&[("a.txt".into(), b"one\n".to_vec()), ("b.txt".into(), b"two\n".to_vec())]).unwrap();
        let text = String::from_utf8(convert(&docs, Format::Text, "x").unwrap()).unwrap();
        assert!(text.contains("one") && text.contains("two"), "{text}");
    }
}
