//! Compound files ([MS-CFB]): the little file system inside a Word 97–2003
//! `.doc`. A header, a table of sector chains (the FAT), a directory of named
//! streams, and the streams themselves cut into sectors.
//!
//! Reading follows the chains wherever they go; writing lays every stream
//! out in one run of sectors. Streams written are at least the 4096 bytes
//! below which a stream would have to live in the "mini stream", so there is
//! no mini stream to write: the formats stored in them ignore what follows
//! their content.

use anyhow::{Result, bail};

const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
const FREE: u32 = 0xFFFF_FFFF;
const FAT_SECTOR: u32 = 0xFFFF_FFFD;
const DIFAT_SECTOR: u32 = 0xFFFF_FFFC;
const NO_STREAM: u32 = 0xFFFF_FFFF;
const MINI_CUTOFF: usize = 4096;

pub fn is_compound(bytes: &[u8]) -> bool {
    bytes.starts_with(&SIGNATURE)
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

struct Entry {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

pub struct Cfb<'a> {
    bytes: &'a [u8],
    sector: usize,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    mini_stream: Vec<u8>,
    entries: Vec<Entry>,
}

impl<'a> Cfb<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        if !is_compound(bytes) || bytes.len() < 512 {
            bail!("not a compound file");
        }
        let shift = u16_at(bytes, 0x1E).unwrap_or(9);
        if shift != 9 && shift != 12 {
            bail!("the file's sector size is not one Word writes");
        }
        let sector = 1usize << shift;
        let mut cfb = Cfb { bytes, sector, fat: Vec::new(), mini_fat: Vec::new(), mini_stream: Vec::new(), entries: Vec::new() };

        // The FAT's own sectors: the first 109 listed in the header, the rest
        // in a chain of DIFAT sectors.
        let mut fat_sectors: Vec<u32> = (0..109).filter_map(|i| u32_at(bytes, 0x4C + 4 * i)).filter(|&s| s < DIFAT_SECTOR).collect();
        let mut difat = u32_at(bytes, 0x44).unwrap_or(END_OF_CHAIN);
        let per = sector / 4 - 1;
        let mut guard = 0;
        while difat < DIFAT_SECTOR && guard < 1 << 20 {
            let Some(s) = cfb.sector_bytes(difat) else { break };
            fat_sectors.extend((0..per).filter_map(|i| u32_at(s, 4 * i)).filter(|&x| x < DIFAT_SECTOR));
            difat = u32_at(s, 4 * per).unwrap_or(END_OF_CHAIN);
            guard += 1;
        }
        for s in fat_sectors {
            if let Some(data) = cfb.sector_bytes(s) {
                cfb.fat.extend((0..sector / 4).filter_map(|i| u32_at(data, 4 * i)));
            }
        }

        let dir = cfb.chain(u32_at(bytes, 0x30).unwrap_or(END_OF_CHAIN), None);
        for e in dir.as_chunks::<128>().0 {
            let len = (u16_at(e, 0x40).unwrap_or(0) as usize).min(64);
            let units: Vec<u16> = (0..len.saturating_sub(2) / 2).filter_map(|i| u16_at(e, 2 * i)).collect();
            let size = if sector == 512 { u32_at(e, 0x78).unwrap_or(0) as u64 } else { u32_at(e, 0x78).unwrap_or(0) as u64 | (u32_at(e, 0x7C).unwrap_or(0) as u64) << 32 };
            cfb.entries.push(Entry {
                name: String::from_utf16_lossy(&units),
                kind: e[0x42],
                left: u32_at(e, 0x44).unwrap_or(NO_STREAM),
                right: u32_at(e, 0x48).unwrap_or(NO_STREAM),
                child: u32_at(e, 0x4C).unwrap_or(NO_STREAM),
                start: u32_at(e, 0x74).unwrap_or(END_OF_CHAIN),
                size,
            });
        }
        let Some(root) = cfb.entries.first() else { bail!("the compound file has no directory") };
        let (root_start, root_size) = (root.start, root.size);
        let mini_fat = cfb.chain(u32_at(bytes, 0x3C).unwrap_or(END_OF_CHAIN), None);
        cfb.mini_fat = mini_fat.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        cfb.mini_stream = cfb.chain(root_start, Some(root_size as usize));
        Ok(cfb)
    }

    fn sector_bytes(&self, n: u32) -> Option<&'a [u8]> {
        let at = (n as usize + 1).checked_mul(self.sector)?;
        self.bytes.get(at..at + self.sector).or_else(|| self.bytes.get(at..))
    }

    /// The bytes of the sector chain starting at `start`, cut to `size`.
    fn chain(&self, start: u32, size: Option<usize>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut s = start;
        let mut seen = 0;
        while s < DIFAT_SECTOR && seen <= self.fat.len() {
            let Some(data) = self.sector_bytes(s) else { break };
            out.extend_from_slice(data);
            if size.is_some_and(|n| out.len() >= n) {
                break;
            }
            s = self.fat.get(s as usize).copied().unwrap_or(END_OF_CHAIN);
            seen += 1;
        }
        if let Some(n) = size {
            out.truncate(n);
        }
        out
    }

    fn mini_chain(&self, start: u32, size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut s = start;
        let mut seen = 0;
        while s < DIFAT_SECTOR && out.len() < size && seen <= self.mini_fat.len() {
            let at = s as usize * 64;
            let Some(data) = self.mini_stream.get(at..(at + 64).min(self.mini_stream.len())) else { break };
            out.extend_from_slice(data);
            s = self.mini_fat.get(s as usize).copied().unwrap_or(END_OF_CHAIN);
            seen += 1;
        }
        out.truncate(size);
        out
    }

    /// A stream at the top of the file, by name (ignoring case). Streams of
    /// the same name inside embedded objects are not found.
    pub fn stream(&self, name: &str) -> Option<Vec<u8>> {
        let mut stack = vec![self.entries.first()?.child];
        let mut guard = 0;
        while let Some(i) = stack.pop() {
            guard += 1;
            if i == NO_STREAM || guard > self.entries.len() * 2 + 2 {
                continue;
            }
            let e = self.entries.get(i as usize)?;
            stack.push(e.left);
            stack.push(e.right);
            if e.kind == 2 && e.name.eq_ignore_ascii_case(name) {
                let size = e.size as usize;
                return Some(if size < MINI_CUTOFF { self.mini_chain(e.start, size) } else { self.chain(e.start, Some(size)) });
            }
        }
        None
    }
}

/// A compound file holding `streams` at its top level. Each stream should
/// be at least 4096 bytes long (see the module notes).
pub fn write(streams: &[(&str, &[u8])]) -> Vec<u8> {
    const SECTOR: usize = 512;
    const PER_FAT: usize = SECTOR / 4;
    const PER_DIFAT: usize = PER_FAT - 1;
    let sectors_of = |len: usize| len.div_ceil(SECTOR);

    // Directory: the root and one entry per stream, ordered as the format
    // orders names (shorter first, then by upper case) and linked as a
    // balanced red-black tree.
    let mut order: Vec<usize> = (0..streams.len()).collect();
    let key = |name: &str| (name.encode_utf16().count(), name.to_uppercase().encode_utf16().collect::<Vec<u16>>());
    order.sort_by_key(|&i| key(streams[i].0));
    let dir_entries = streams.len() + 1;
    let dir_sectors = sectors_of(dir_entries * 128);
    let data_sectors: usize = streams.iter().map(|(_, d)| sectors_of(d.len())).sum();

    // The FAT must map every sector, its own included, and past 109 FAT
    // sectors the DIFAT needs sectors too: grow until it all fits.
    let (mut fat_sectors, mut difat_sectors) = (1usize, 0usize);
    loop {
        let total = data_sectors + dir_sectors + fat_sectors + difat_sectors;
        let need_fat = total.div_ceil(PER_FAT);
        let need_difat = need_fat.saturating_sub(109).div_ceil(PER_DIFAT);
        if need_fat <= fat_sectors && need_difat <= difat_sectors {
            break;
        }
        fat_sectors = fat_sectors.max(need_fat);
        difat_sectors = difat_sectors.max(need_difat);
    }

    let mut fat: Vec<u32> = Vec::new();
    let mut starts = vec![END_OF_CHAIN; streams.len()];
    for (i, (_, data)) in streams.iter().enumerate() {
        let n = sectors_of(data.len());
        if n == 0 {
            continue;
        }
        starts[i] = fat.len() as u32;
        for _ in 1..n {
            fat.push((fat.len() + 1) as u32);
        }
        fat.push(END_OF_CHAIN);
    }
    let dir_start = fat.len() as u32;
    for _ in 1..dir_sectors {
        fat.push((fat.len() + 1) as u32);
    }
    fat.push(END_OF_CHAIN);
    let fat_start = fat.len() as u32;
    fat.extend(std::iter::repeat_n(FAT_SECTOR, fat_sectors));
    let difat_start = fat.len() as u32;
    fat.extend(std::iter::repeat_n(DIFAT_SECTOR, difat_sectors));
    fat.resize(fat_sectors * PER_FAT, FREE);

    let mut out = vec![0u8; SECTOR];
    out[..8].copy_from_slice(&SIGNATURE);
    let put16 = |b: &mut [u8], at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    let put32 = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put16(&mut out, 0x18, 0x003E);
    put16(&mut out, 0x1A, 0x0003);
    put16(&mut out, 0x1C, 0xFFFE);
    put16(&mut out, 0x1E, 9);
    put16(&mut out, 0x20, 6);
    put32(&mut out, 0x2C, fat_sectors as u32);
    put32(&mut out, 0x30, dir_start);
    put32(&mut out, 0x38, MINI_CUTOFF as u32);
    put32(&mut out, 0x3C, END_OF_CHAIN);
    put32(&mut out, 0x44, if difat_sectors > 0 { difat_start } else { END_OF_CHAIN });
    put32(&mut out, 0x48, difat_sectors as u32);
    for i in 0..109 {
        let v = if i < fat_sectors { fat_start + i as u32 } else { FREE };
        put32(&mut out, 0x4C + 4 * i, v);
    }

    for (_, data) in streams {
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(SECTOR) * SECTOR, 0);
    }

    // The tree over the sorted names: middle as the root; when the tree is
    // not full, its deepest level is red so every path has as many black
    // nodes.
    let n = order.len();
    let mut left = vec![NO_STREAM; n];
    let mut right = vec![NO_STREAM; n];
    let mut depth = vec![0usize; n];
    fn build(lo: usize, hi: usize, d: usize, left: &mut [u32], right: &mut [u32], depth: &mut [usize]) -> Option<usize> {
        if lo >= hi {
            return None;
        }
        let mid = (lo + hi) / 2;
        depth[mid] = d;
        left[mid] = build(lo, mid, d + 1, left, right, depth).map_or(NO_STREAM, |x| x as u32);
        right[mid] = build(mid + 1, hi, d + 1, left, right, depth).map_or(NO_STREAM, |x| x as u32);
        Some(mid)
    }
    let root = build(0, n, 0, &mut left, &mut right, &mut depth);
    let max_depth = depth.iter().copied().max().unwrap_or(0);
    let full = (n + 1).is_power_of_two();

    let entry = |name: &str, kind: u8, red: bool, l: u32, r: u32, child: u32, start: u32, size: u32| {
        let mut e = vec![0u8; 128];
        let units: Vec<u16> = name.encode_utf16().take(31).collect();
        for (i, u) in units.iter().enumerate() {
            e[2 * i..2 * i + 2].copy_from_slice(&u.to_le_bytes());
        }
        e[0x40..0x42].copy_from_slice(&(((units.len() + 1) * 2) as u16).to_le_bytes());
        e[0x42] = kind;
        e[0x43] = if red { 0 } else { 1 };
        e[0x44..0x48].copy_from_slice(&l.to_le_bytes());
        e[0x48..0x4C].copy_from_slice(&r.to_le_bytes());
        e[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
        e[0x74..0x78].copy_from_slice(&start.to_le_bytes());
        e[0x78..0x7C].copy_from_slice(&size.to_le_bytes());
        e
    };
    // Directory index of sorted position k is k + 1 (the root is 0).
    let index = |k: u32| if k == NO_STREAM { NO_STREAM } else { k + 1 };
    out.extend(entry("Root Entry", 5, false, NO_STREAM, NO_STREAM, root.map_or(NO_STREAM, |r| r as u32 + 1), END_OF_CHAIN, 0));
    for k in 0..n {
        let i = order[k];
        let red = !full && depth[k] == max_depth && max_depth > 0;
        out.extend(entry(streams[i].0, 2, red, index(left[k]), index(right[k]), NO_STREAM, starts[i], streams[i].1.len() as u32));
    }
    while !out.len().is_multiple_of(SECTOR) {
        let mut empty = vec![0u8; 128];
        empty[0x44..0x50].copy_from_slice(&[0xFF; 12]);
        out.extend(empty);
    }

    for v in &fat {
        out.extend_from_slice(&v.to_le_bytes());
    }
    // DIFAT sectors list the FAT sectors past the header's 109.
    let rest: Vec<u32> = (109..fat_sectors).map(|i| fat_start + i as u32).collect();
    for (d, chunk) in rest.chunks(PER_DIFAT).enumerate() {
        for k in 0..PER_DIFAT {
            out.extend_from_slice(&chunk.get(k).copied().unwrap_or(FREE).to_le_bytes());
        }
        let next = if d + 1 < difat_sectors { difat_start + d as u32 + 1 } else { END_OF_CHAIN };
        out.extend_from_slice(&next.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_written_read_back() {
        let a: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let b = vec![7u8; 4096];
        let c: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let file = write(&[("WordDocument", &a), ("1Table", &b), ("Data", &c)]);
        let cfb = Cfb::open(&file).unwrap();
        assert_eq!(cfb.stream("WordDocument").as_deref(), Some(&a[..]));
        assert_eq!(cfb.stream("1table").as_deref(), Some(&b[..]));
        assert_eq!(cfb.stream("Data").as_deref(), Some(&c[..]));
        assert!(cfb.stream("0Table").is_none());
    }

    /// Past 109 FAT sectors (about 7 MB) the FAT is found through the DIFAT.
    #[test]
    fn large_files_use_the_difat() {
        let big: Vec<u8> = (0..8_000_000u32).map(|i| (i % 253) as u8).collect();
        let small = vec![1u8; 4096];
        let file = write(&[("WordDocument", &small), ("Data", &big)]);
        let cfb = Cfb::open(&file).unwrap();
        assert_eq!(cfb.stream("Data").map(|d| d == big), Some(true));
        assert_eq!(cfb.stream("WordDocument").as_deref(), Some(&small[..]));
    }
}
