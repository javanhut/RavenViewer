//! Fonts a document carries with it.
//!
//! Word can embed the fonts a document is set in, so that it reads the same
//! where they aren't installed. They are stored lightly obfuscated: the
//! first 32 bytes XORed with the font's key. Unpacked, they go to the cache
//! and from there into each font map that sets a document — the PDF
//! writer's own and the editor's.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use pango::prelude::*;

/// Every font unpacked so far, in the order it was.
static FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// How many there are: changes when a document brings new ones.
static COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The font maps on this thread and how many of the files each has.
    static MAPS: std::cell::RefCell<Vec<(glib::WeakRef<pango::FontMap>, usize)>> = Default::default();
}

/// A font a document embeds: its family and style, its key and its bytes.
pub struct Embedded {
    pub family: String,
    /// "Regular", "Bold", "Italic" or "Bold Italic".
    pub style: String,
    pub key: String,
    pub data: Vec<u8>,
}

/// The font's bytes with the obfuscation undone: the key is a GUID, whose
/// 16 bytes, last first, are XORed over the first 32 of the font.
pub fn deobfuscate(key: &str, mut data: Vec<u8>) -> Vec<u8> {
    let hex: String = key.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 32 {
        return data;
    }
    let mut guid = [0u8; 16];
    for (i, b) in guid.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    for (i, byte) in data.iter_mut().take(32).enumerate() {
        *byte ^= guid[15 - i % 16];
    }
    data
}

/// Unpack a document's fonts, those of families not installed here, to
/// the cache, for documents to be set in.
pub fn embed(fonts: Vec<Embedded>) {
    if fonts.is_empty() {
        return;
    }
    let installed: Vec<String> = pangocairo::FontMap::new().list_families().iter().map(|f| f.name().to_lowercase()).collect();
    let dir = glib::user_cache_dir().join("raven-viewer").join("fonts");
    let mut files = FILES.lock().unwrap_or_else(|e| e.into_inner());
    for font in fonts {
        if installed.contains(&font.family.to_lowercase()) {
            continue;
        }
        let data = deobfuscate(&font.key, font.data);
        // A font file starts with its version: TrueType's, or OpenType's
        // "OTTO".
        if !(data.starts_with(&[0, 1, 0, 0]) || data.starts_with(b"OTTO") || data.starts_with(b"true")) {
            continue;
        }
        let data = named(data, &font.family, &font.style);
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        data.hash(&mut hasher);
        let path = dir.join(format!("{:016x}.ttf", hasher.finish()));
        if files.contains(&path) {
            continue;
        }
        if !path.exists() && (std::fs::create_dir_all(&dir).is_err() || std::fs::write(&path, &data).is_err()) {
            continue;
        }
        files.push(path);
    }
    COUNT.store(files.len(), Ordering::Release);
}

/// The font under the name the document knows it by. Fonts licensed for
/// the web often come with their names taken out, and a font no one can
/// ask for by name is never used: so the name table is written anew, with
/// the family and style the document gives it.
fn named(data: Vec<u8>, family: &str, style: &str) -> Vec<u8> {
    let u16_at = |at: usize| data.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let u32_at = |at: usize| data.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let Some(tables) = u16_at(4) else { return data };
    let Some(entry) = (0..tables as usize).map(|i| 12 + 16 * i).find(|&e| data.get(e..e + 4) == Some(b"name")) else {
        return data;
    };
    let (Some(offset), Some(length)) = (u32_at(entry + 8), u32_at(entry + 12)) else { return data };
    let (offset, length) = (offset as usize, length as usize);
    if offset + length > data.len() {
        return data;
    }
    // Keep what else the table says — copyright, licence, version — in
    // the languages it says it.
    let mut records: Vec<(u16, u16, u16, u16, Vec<u8>)> = Vec::new();
    if let (Some(count), Some(strings)) = (u16_at(offset + 2), u16_at(offset + 4)) {
        for i in 0..count as usize {
            let at = offset + 6 + 12 * i;
            let fields: Option<Vec<u16>> = (0..6).map(|k| u16_at(at + 2 * k)).collect();
            let Some([platform, encoding, language, id, len, from]) = fields.as_deref().and_then(|f| <[u16; 6]>::try_from(f).ok()) else { break };
            let start = offset + strings as usize + from as usize;
            let (Some(bytes), false) = (data.get(start..start + len as usize), [1, 2, 3, 4, 6, 16, 17, 21, 22].contains(&id)) else { continue };
            if language < 0x8000 {
                records.push((platform, encoding, language, id, bytes.to_vec()));
            }
        }
    }
    let utf16 = |s: &str| s.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>();
    let full = if style == "Regular" { family.to_string() } else { format!("{family} {style}") };
    let postscript: String = format!("{family}-{style}").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    for (id, text) in [(1, family), (2, style), (3, full.as_str()), (4, full.as_str()), (6, postscript.as_str())] {
        records.push((3, 1, 0x409, id, utf16(text)));
    }
    records.sort_by_key(|r| (r.0, r.1, r.2, r.3));
    let mut table = Vec::new();
    let mut strings: Vec<u8> = Vec::new();
    table.extend(0u16.to_be_bytes());
    table.extend((records.len() as u16).to_be_bytes());
    table.extend((6 + 12 * records.len() as u16).to_be_bytes());
    for (platform, encoding, language, id, bytes) in &records {
        for v in [*platform, *encoding, *language, *id, bytes.len() as u16, strings.len() as u16] {
            table.extend(v.to_be_bytes());
        }
        strings.extend(bytes);
    }
    table.extend(strings);
    // The new table goes at the end; the old one is left where it was.
    let mut data = data;
    data.resize(data.len().next_multiple_of(4), 0);
    let at = data.len() as u32;
    let sum = table.chunks(4).fold(0u32, |sum, c| {
        let mut word = [0u8; 4];
        word[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(word))
    });
    data[entry + 4..entry + 8].copy_from_slice(&sum.to_be_bytes());
    data[entry + 8..entry + 12].copy_from_slice(&at.to_be_bytes());
    data[entry + 12..entry + 16].copy_from_slice(&(table.len() as u32).to_be_bytes());
    data.extend(table);
    data.resize(data.len().next_multiple_of(4), 0);
    data
}

/// How many fonts documents have brought: line metrics measured before a
/// change may be from a stand-in.
pub fn generation() -> usize {
    COUNT.load(Ordering::Acquire)
}

/// Give `map` the fonts documents have brought that it doesn't have yet.
pub fn apply(map: &pango::FontMap) {
    if generation() == 0 {
        return;
    }
    let files = FILES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    MAPS.with(|maps| {
        let mut maps = maps.borrow_mut();
        maps.retain(|(m, _)| m.upgrade().is_some());
        let at = match maps.iter().position(|(m, _)| m.upgrade().as_ref() == Some(map)) {
            Some(at) => at,
            None => {
                maps.push((map.downgrade(), 0));
                maps.len() - 1
            }
        };
        let had = maps[at].1;
        for file in &files[had.min(files.len())..] {
            let _ = map.add_font_file(file);
        }
        maps[at].1 = files.len();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_undone_back_to_front() {
        let key = "{00112233-4455-6677-8899-AABBCCDDEEFF}";
        let font: Vec<u8> = (0..40).collect();
        let hidden = deobfuscate(key, font.clone());
        assert_eq!(hidden[0], 0xFF);
        assert_eq!(hidden[1], 1 ^ 0xEE);
        assert_eq!(hidden[15], 15);
        assert_eq!(hidden[16], 16 ^ 0xFF);
        assert_eq!(&hidden[32..], &font[32..]);
        assert_eq!(deobfuscate(key, hidden), font);
    }
}
