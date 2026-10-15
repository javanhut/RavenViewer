//! Scanned TIFFs, shown as PDFs. Each image in the file becomes a page
//! holding that image at the size its resolution gives it, so a TIFF is read,
//! zoomed, laid out, turned, rotated, marked up and combined exactly like a
//! PDF — the viewer keeps one kind of page, not two.
//!
//! Only the pixels are kept: the scan is decoded once, here, and stored
//! Deflate-compressed in the PDF, which hayro reads like any other image.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use lopdf::{Document, Object, Stream, dictionary};
use tiff::ColorType;
use tiff::decoder::ifd::Value;
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;

/// The TIFF spec's own default, for files that do not say.
const DEFAULT_DPI: f64 = 72.0;

/// Whether the bytes are a TIFF (or a BigTIFF), whatever the file is called.
pub fn is_tiff(bytes: &[u8]) -> bool {
    matches!(bytes.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+"))
}

/// Whether a file name says TIFF.
pub fn is_tiff_name(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
}

/// One PDF page for every image in the TIFF.
pub fn to_pdf(bytes: &[u8]) -> Result<Vec<u8>> {
    let relabelled = relabel_palettes(bytes);
    let (bytes, palettes) = match &relabelled {
        Some((copy, palettes)) => (&copy[..], &palettes[..]),
        None => (bytes, &[][..]),
    };
    let mut decoder = Decoder::new(Cursor::new(bytes)).context("not a readable TIFF")?;
    // A poster scanned at 600 dpi is well past the default 256 MB.
    let mut limits = Limits::default();
    limits.decoding_buffer_size = 2 << 30;
    limits.intermediate_buffer_size = 1 << 30;
    decoder = decoder.with_limits(limits);

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::new();
    loop {
        let number = kids.len() + 1;
        let palette = palettes.get(number - 1).copied().unwrap_or(false);
        let image = read_image(&mut decoder, palette).with_context(|| format!("page {number}"))?;
        kids.push(add_page(&mut doc, pages_id, image)?.into());
        if !decoder.more_images() {
            break;
        }
        decoder.next_image().with_context(|| format!("page {}", number + 1))?;
    }
    let count = kids.len() as i64;
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

/// One decoded image, ready to be a PDF image.
struct Image {
    width: u32,
    height: u32,
    color_space: &'static str,
    bits: u8,
    /// Rows start on a byte, as in both TIFF and PDF.
    samples: Vec<u8>,
    /// Pixels per inch across and down: faxes are 204 × 196 or 204 × 98.
    dpi: (f64, f64),
    /// Quarter turns clockwise to show it upright.
    turn: u8,
}

/// `palette`: the image was relabelled grey by `relabel_palettes`, and its
/// grey levels are palette indices.
fn read_image(decoder: &mut Decoder<Cursor<&[u8]>>, palette: bool) -> Result<Image> {
    let (width, height) = decoder.dimensions()?;
    let color = match decoder.colortype()? {
        ColorType::Gray(bits) if palette => ColorType::Palette(bits),
        color => color,
    };
    let dpi = resolution(decoder);
    // Mirrored orientations (2, 4, 5, 7) are left as stored.
    let turn = match decoder.find_tag_unsigned::<u16>(Tag::Orientation).ok().flatten() {
        Some(3) => 2,
        Some(6) => 1,
        Some(8) => 3,
        _ => 0,
    };
    let colormap = match color {
        ColorType::Palette(_) => Some(decoder.get_tag_u16_vec(Tag::ColorMap).context("a palette image without its palette")?),
        _ => None,
    };
    let pixels = width as usize * height as usize;
    // White-is-zero scans come back already turned black-is-zero.
    let (color_space, bits, samples) = match (color, decoder.read_image()?) {
        (ColorType::Gray(bits @ (1 | 2 | 4 | 8)), DecodingResult::U8(d)) => ("DeviceGray", bits, d),
        (ColorType::Gray(16), DecodingResult::U16(d)) => ("DeviceGray", 8, high_bytes(&d)),
        (ColorType::GrayA(8), DecodingResult::U8(d)) => ("DeviceGray", 8, on_white(&d, 1)),
        (ColorType::GrayA(16), DecodingResult::U16(d)) => ("DeviceGray", 8, on_white(&high_bytes(&d), 1)),
        (ColorType::RGB(8), DecodingResult::U8(d)) => ("DeviceRGB", 8, d),
        (ColorType::RGB(16), DecodingResult::U16(d)) => ("DeviceRGB", 8, high_bytes(&d)),
        (ColorType::RGBA(8), DecodingResult::U8(d)) => ("DeviceRGB", 8, on_white(&d, 3)),
        (ColorType::RGBA(16), DecodingResult::U16(d)) => ("DeviceRGB", 8, on_white(&high_bytes(&d), 3)),
        (ColorType::CMYK(8), DecodingResult::U8(d)) => ("DeviceCMYK", 8, d),
        (ColorType::YCbCr(8), DecodingResult::U8(d)) => ("DeviceRGB", 8, ycbcr_to_rgb(&d)),
        (ColorType::Palette(bits @ (1 | 2 | 4 | 8)), DecodingResult::U8(d)) => {
            ("DeviceRGB", 8, expand_palette(&d, bits, width, height, colormap.as_deref().unwrap_or_default())?)
        }
        (other, _) => bail!("{other:?} images aren’t supported"),
    };
    let row = (width as usize * bits as usize * channels(color_space)).div_ceil(8);
    if samples.len() < row * height as usize || (bits == 8 && samples.len() < pixels * channels(color_space)) {
        bail!("the image is shorter than its size says");
    }
    Ok(Image { width, height, color_space, bits, samples, dpi, turn })
}

/// The crate decodes no palette images, though their pixels are only
/// indices, stored exactly as grey levels are. A copy of the file with each
/// palette image's PhotometricInterpretation (262) changed from palette (3)
/// to grey (1) decodes, and its ColorMap is still there to look the indices
/// up in. Returns that copy and which images were palettes; nothing when
/// there were none, or the file's directories cannot be walked.
fn relabel_palettes(bytes: &[u8]) -> Option<(Vec<u8>, Vec<bool>)> {
    let little = match bytes.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let read = |at: usize, len: usize| -> Option<u64> {
        let b = bytes.get(at..at.checked_add(len)?)?;
        let mut v = 0u64;
        for i in 0..len {
            let byte = if little { b[len - 1 - i] } else { b[i] };
            v = (v << 8) | byte as u64;
        }
        Some(v)
    };
    // Classic TIFF: 2-byte entry counts, 12-byte entries, 4-byte offsets.
    // BigTIFF: 8, 20 and 8.
    let big = read(2, 2)? == 43;
    let (count_len, entry_len, offset_len, value_at) = if big { (8, 20, 8, 12) } else { (2, 12, 4, 8) };
    let mut ifd = read(if big { 8 } else { 4 }, offset_len)? as usize;
    let mut copy = bytes.to_vec();
    let mut palettes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while ifd != 0 && seen.insert(ifd) {
        let entries = read(ifd, count_len)? as usize;
        let mut palette = false;
        for i in 0..entries {
            let entry = ifd + count_len + i * entry_len;
            // Tag 262, of type SHORT, holding 3.
            if read(entry, 2)? == 262 && read(entry + 2, 2)? == 3 && read(entry + value_at, 2)? == 3 {
                let grey: [u8; 2] = if little { 1u16.to_le_bytes() } else { 1u16.to_be_bytes() };
                copy[entry + value_at..entry + value_at + 2].copy_from_slice(&grey);
                palette = true;
            }
        }
        palettes.push(palette);
        ifd = read(ifd + count_len + entries * entry_len, offset_len)? as usize;
    }
    palettes.contains(&true).then_some((copy, palettes))
}

fn channels(color_space: &str) -> usize {
    match color_space {
        "DeviceRGB" => 3,
        "DeviceCMYK" => 4,
        _ => 1,
    }
}

/// Pixels per inch across and down. A file that gives no unit (or no
/// resolution) is taken at 72, the spec's default.
fn resolution(decoder: &mut Decoder<Cursor<&[u8]>>) -> (f64, f64) {
    let unit = decoder.find_tag_unsigned::<u16>(Tag::ResolutionUnit).ok().flatten().unwrap_or(2);
    let per_inch = match unit {
        2 => 1.0,
        3 => 2.54,
        _ => return (DEFAULT_DPI, DEFAULT_DPI),
    };
    // Stored as a fraction, which the crate's own `into_f64` does not read.
    let mut read = |tag| {
        let value = match decoder.find_tag(tag).ok().flatten()? {
            Value::Rational(n, d) if d != 0 => n as f64 / d as f64,
            Value::Double(v) => v,
            Value::Float(v) => v as f64,
            other => other.into_u64().ok()? as f64,
        };
        Some(value).filter(|r| r.is_finite() && *r >= 1.0)
    };
    let (x, y) = (read(Tag::XResolution), read(Tag::YResolution));
    match (x.or(y), y.or(x)) {
        (Some(x), Some(y)) => (x * per_inch, y * per_inch),
        _ => (DEFAULT_DPI, DEFAULT_DPI),
    }
}

fn add_page(doc: &mut Document, parent: lopdf::ObjectId, image: Image) -> Result<lopdf::ObjectId> {
    let width = image.width as f64 / image.dpi.0 * 72.0;
    let height = image.height as f64 / image.dpi.1 * 72.0;
    let mut xobject = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => image.width as i64, "Height" => image.height as i64,
            "ColorSpace" => image.color_space, "BitsPerComponent" => image.bits as i64,
        },
        image.samples,
    );
    xobject.compress()?;
    let xobject = doc.add_object(xobject);
    let content = format!("q {width:.4} 0 0 {height:.4} 0 0 cm /Im0 Do Q");
    let content = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let mut page = dictionary! {
        "Type" => "Page", "Parent" => parent, "Contents" => content,
        "MediaBox" => vec![0.into(), 0.into(), Object::Real(width as f32), Object::Real(height as f32)],
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => xobject } },
    };
    if image.turn != 0 {
        page.set("Rotate", image.turn as i64 * 90);
    }
    Ok(doc.add_object(page))
}

fn high_bytes(samples: &[u16]) -> Vec<u8> {
    samples.iter().map(|s| (s >> 8) as u8).collect()
}

/// Colour plus alpha, laid on white paper: a page has no transparency to
/// show through to.
fn on_white(samples: &[u8], colours: usize) -> Vec<u8> {
    samples
        .chunks_exact(colours + 1)
        .flat_map(|px| {
            let alpha = px[colours] as u32;
            px[..colours].iter().map(move |&c| ((c as u32 * alpha + 255 * (255 - alpha)) / 255) as u8)
        })
        .collect()
}

/// JPEG-compressed scans come back as YCbCr; PDF wants RGB.
fn ycbcr_to_rgb(samples: &[u8]) -> Vec<u8> {
    let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    samples
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|&[y, cb, cr]| {
            let (y, cb, cr) = (y as f32, cb as f32 - 128.0, cr as f32 - 128.0);
            [clamp(y + 1.402 * cr), clamp(y - 0.344_136 * cb - 0.714_136 * cr), clamp(y + 1.772 * cb)]
        })
        .collect()
}

/// Palette indices (packed when under 8 bits) to RGB. A TIFF palette is all
/// the reds, then all the greens, then all the blues, 16 bits each.
fn expand_palette(indices: &[u8], bits: u8, width: u32, height: u32, map: &[u16]) -> Result<Vec<u8>> {
    let entries = 1usize << bits;
    if map.len() < 3 * entries {
        bail!("the palette is too short");
    }
    let row = (width as usize * bits as usize).div_ceil(8);
    let per_byte = 8 / bits as usize;
    let mask = (entries - 1) as u8;
    let mut out = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height as usize {
        let line = indices.get(y * row..(y + 1) * row).context("the image is shorter than its size says")?;
        for x in 0..width as usize {
            let byte = line[x / per_byte];
            let shift = 8 - bits as usize * (x % per_byte + 1);
            let i = ((byte >> shift) & mask) as usize;
            out.extend([map[i], map[entries + i], map[2 * entries + i]].map(|c| (c >> 8) as u8));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tiff::encoder::{TiffEncoder, colortype};

    /// A two-page TIFF: an RGB page at 300 dpi and a grey one at 150.
    fn two_pages() -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut tiff = TiffEncoder::new(&mut out).unwrap();
        let rgb: Vec<u8> = (0..600 * 300).flat_map(|i| [(i % 251) as u8, 40, 200]).collect();
        let mut page = tiff.new_image::<colortype::RGB8>(600, 300).unwrap();
        page.resolution(tiff::tags::ResolutionUnit::Inch, tiff::encoder::Rational { n: 300, d: 1 });
        page.write_data(&rgb).unwrap();
        let grey: Vec<u8> = (0..150 * 225).map(|i| (i % 256) as u8).collect();
        let mut page = tiff.new_image::<colortype::Gray8>(150, 225).unwrap();
        page.resolution(tiff::tags::ResolutionUnit::Inch, tiff::encoder::Rational { n: 150, d: 1 });
        page.write_data(&grey).unwrap();
        out.into_inner()
    }

    #[test]
    fn every_image_is_a_page_at_the_size_its_resolution_gives() {
        let bytes = two_pages();
        assert!(is_tiff(&bytes));
        let pdf = to_pdf(&bytes).unwrap();
        let info = crate::pdf::load_info(&Arc::new(pdf)).unwrap();
        // 600 × 300 px at 300 dpi is 2 × 1 inches; 150 × 225 at 150 is 1 × 1.5.
        assert_eq!(info.page_sizes, vec![(144.0, 72.0), (72.0, 108.0)]);
    }

    /// The pixels come through: the PDF page, drawn, is the scan.
    #[test]
    fn the_page_shows_the_scan() {
        let mut out = Cursor::new(Vec::new());
        let mut tiff = TiffEncoder::new(&mut out).unwrap();
        // Left half black, right half white, 72 dpi: 100 × 50 points.
        let grey: Vec<u8> = (0..100 * 50).map(|i| if i % 100 < 50 { 0 } else { 255 }).collect();
        tiff.write_image::<colortype::Gray8>(100, 50, &grey).unwrap();
        let pdf = Arc::new(to_pdf(&out.into_inner()).unwrap());
        let (tx, rx) = async_channel::unbounded();
        let renderer = crate::pdf::Renderer::spawn(pdf, tx).unwrap();
        renderer.submit(1.0, vec![crate::pdf::TileKey { page: 0, tile: None }]);
        let page = rx.recv_blocking().unwrap();
        let px = |x: u32, y: u32| page.pixels[((y * page.width + x) * 4) as usize];
        assert!(px(10, 25) < 40, "the left half is black");
        assert!(px(90, 25) > 215, "the right half is white");
    }

    #[test]
    fn colour_with_alpha_is_laid_on_white() {
        assert_eq!(on_white(&[255, 0, 0, 255, 0, 0, 255, 0], 3), vec![255, 0, 0, 255, 255, 255]);
    }

    #[test]
    fn jpeg_colour_is_turned_into_rgb() {
        // Grey stays grey; pure red in, near enough pure red out.
        assert_eq!(ycbcr_to_rgb(&[128, 128, 128]), vec![128, 128, 128]);
        let red = ycbcr_to_rgb(&[76, 85, 255]);
        assert!(red[0] > 250 && red[1] < 5 && red[2] < 5, "{red:?}");
    }

    #[test]
    fn a_packed_palette_is_looked_up() {
        // Two colours, one bit each: black then red, ten pixels across.
        let mut map = vec![0u16; 6];
        map[1] = 0xFFFF; // red of entry 1
        let rgb = expand_palette(&[0b0101_0101, 0b0100_0000], 1, 10, 1, &map).unwrap();
        let reds: Vec<u8> = rgb.chunks(3).map(|p| p[0]).collect();
        assert_eq!(reds, vec![0, 255, 0, 255, 0, 255, 0, 255, 0, 255]);
    }

    /// Every TIFF in a folder, converted, its first page drawn to a PNG
    /// beside where the test is told to put them:
    /// `RAVEN_TIFF_DIR=… RAVEN_TIFF_OUT=… cargo test tiff_folder -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn tiff_folder() {
        let dir = std::env::var("RAVEN_TIFF_DIR").unwrap();
        let out = std::env::var("RAVEN_TIFF_OUT").unwrap();
        let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths.iter().filter(|p| is_tiff_name(p)) {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(path).unwrap();
            let t = std::time::Instant::now();
            let pdf = match to_pdf(&bytes) {
                Ok(pdf) => Arc::new(pdf),
                Err(e) => {
                    eprintln!("{name}: {e:#}");
                    continue;
                }
            };
            let took = t.elapsed();
            let info = crate::pdf::load_info(&pdf).unwrap();
            let (tx, rx) = async_channel::unbounded();
            let renderer = crate::pdf::Renderer::spawn(pdf.clone(), tx).unwrap();
            let scale = 400.0 / info.page_sizes[0].0.max(info.page_sizes[0].1);
            renderer.submit(scale, vec![crate::pdf::TileKey { page: 0, tile: None }]);
            let page = rx.recv_blocking().unwrap();
            let pixmap = hayro::vello_cpu::Pixmap::from_parts(
                page.pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| hayro::vello_cpu::color::PremulRgba8 { r: p[0], g: p[1], b: p[2], a: p[3] })
                    .collect(),
                page.width as u16,
                page.height as u16,
            );
            std::fs::write(format!("{out}/{name}.png"), pixmap.into_png().unwrap()).unwrap();
            eprintln!(
                "{name}: {} KB TIFF -> {} KB PDF in {took:.0?}, {} pages, first {:?} pt",
                bytes.len() / 1024,
                pdf.len() / 1024,
                info.page_sizes.len(),
                info.page_sizes[0]
            );
        }
    }

    /// A little-endian TIFF of one uncompressed strip, written by hand: the
    /// encoder writes neither palettes nor white-is-zero. `entries` are
    /// (tag, type, value) with SHORT (3) or LONG (4) values inline; `extra`
    /// is appended after the pixels for a tag of type SHORT with `count`
    /// values, at an offset this fills in.
    fn tiny_tiff(width: u32, height: u32, entries: &[(u16, u16, u32)], pixels: &[u8], extra: Option<(u16, &[u16])>) -> Vec<u8> {
        let mut all: Vec<(u16, u16, u32, u32)> = entries.iter().map(|&(t, ty, v)| (t, ty, 1, v)).collect();
        all.extend([(256, 4, 1, width), (257, 4, 1, height), (273, 4, 1, 0), (278, 4, 1, height), (279, 4, 1, pixels.len() as u32)]);
        if let Some((tag, values)) = extra {
            all.push((tag, 3, values.len() as u32, 0));
        }
        all.sort_by_key(|e| e.0);
        let ifd_len = 2 + 12 * all.len() + 4;
        let pixels_at = (8 + ifd_len) as u32;
        let extra_at = pixels_at + pixels.len() as u32;
        let mut out = b"II*\0".to_vec();
        out.extend(8u32.to_le_bytes());
        out.extend((all.len() as u16).to_le_bytes());
        for (tag, ty, count, value) in all {
            let value = match tag {
                273 => pixels_at,
                t if extra.is_some_and(|(x, _)| x == t) => extra_at,
                _ => value,
            };
            out.extend(tag.to_le_bytes());
            out.extend(ty.to_le_bytes());
            out.extend(count.to_le_bytes());
            out.extend(value.to_le_bytes());
        }
        out.extend(0u32.to_le_bytes());
        out.extend(pixels);
        if let Some((_, values)) = extra {
            out.extend(values.iter().flat_map(|v| v.to_le_bytes()));
        }
        out
    }

    fn first_page_pixels(tiff: &[u8]) -> Vec<[u8; 3]> {
        let pdf = Arc::new(to_pdf(tiff).unwrap());
        let (tx, rx) = async_channel::unbounded();
        let renderer = crate::pdf::Renderer::spawn(pdf, tx).unwrap();
        renderer.submit(1.0, vec![crate::pdf::TileKey { page: 0, tile: None }]);
        let page = rx.recv_blocking().unwrap();
        page.pixels.chunks(4).map(|p| [p[0], p[1], p[2]]).collect()
    }

    /// A palette image is read through its palette, which the crate alone
    /// would refuse.
    #[test]
    fn a_palette_image_shows_its_colours() {
        // 72 × 72 at 72 dpi: the left half index 1 (red), the right index 2 (blue).
        let indices: Vec<u8> = (0..72 * 72).map(|i| if i % 72 < 36 { 1 } else { 2 }).collect();
        let mut map = vec![0u16; 3 * 256];
        map[1] = 0xFFFF; // red of entry 1
        map[2 * 256 + 2] = 0xFFFF; // blue of entry 2
        let tiff = tiny_tiff(72, 72, &[(258, 3, 8), (259, 3, 1), (262, 3, 3), (277, 3, 1)], &indices, Some((320, &map)));
        let px = first_page_pixels(&tiff);
        let (left, right) = (px[36 * 72 + 10], px[36 * 72 + 60]);
        assert!(left[0] > 200 && left[2] < 60, "left is red: {left:?}");
        assert!(right[2] > 200 && right[0] < 60, "right is blue: {right:?}");
    }

    /// A fax stores 0 as white. Read as black-is-zero it would come out a
    /// negative.
    #[test]
    fn a_white_is_zero_scan_is_not_a_negative() {
        // One bit a pixel, 72 × 72: all 0 (white) but a black band of 1s.
        let row = 9;
        let mut bits = vec![0u8; row * 72];
        for y in 30..40 {
            bits[y * row..(y + 1) * row].fill(0xFF);
        }
        let tiff = tiny_tiff(72, 72, &[(258, 3, 1), (259, 3, 1), (262, 3, 0), (277, 3, 1)], &bits, None);
        let px = first_page_pixels(&tiff);
        assert!(px[10 * 72 + 36][0] > 215, "the paper is white");
        assert!(px[35 * 72 + 36][0] < 40, "the band is black");
    }

    #[test]
    fn other_files_are_not_tiffs() {
        assert!(!is_tiff(b"%PDF-1.7"));
        assert!(!is_tiff(b"II"));
        assert!(is_tiff_name(Path::new("scan_001.TIF")));
        assert!(!is_tiff_name(Path::new("notes.txt")));
    }
}
