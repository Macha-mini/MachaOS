//! Minimal PNG decoder for the image viewer.
//!
//! Scope (see the roadmap): 8-bit, non-interlaced PNGs — grayscale,
//! grayscale+alpha, RGB, RGBA and palette — with all five scanline
//! filters. A full zlib/DEFLATE inflate is included (fixed and dynamic
//! Huffman blocks, stored blocks, LZ77 window, adler32 trailer check),
//! so no external compression code is needed. Adam7 interlacing and
//! 16-bit channels are rejected up front.
//!
//! Output: `(width, height, pixels)` where each pixel is
//! `(a << 24) | (r << 16) | (g << 8) | b` — the same u32 layout the
//! image viewer and framebuffer use.

use alloc::vec;
use alloc::vec::Vec;

const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

fn be32(b: &[u8], off: usize) -> u32 {
    ((b[off] as u32) << 24)
        | ((b[off + 1] as u32) << 16)
        | ((b[off + 2] as u32) << 8)
        | (b[off + 3] as u32)
}

// ---------------------------------------------------------------------
// DEFLATE (RFC 1951) inflate
// ---------------------------------------------------------------------

/// Bit reader over a byte slice, LSB-first (DEFLATE's packing order).
struct BitReader<'a> {
    data: &'a [u8],
    bitpos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bitpos: 0 }
    }
    /// Reads one bit, LSB-first. Past the end of the buffer it returns
    /// zeros instead of panicking — a truncated stream then decodes to
    /// garbage that the strict trailer and size checks below reject,
    /// rather than crashing the kernel on a malformed PNG.
    fn read_bit(&mut self) -> u8 {
        let off = self.bitpos >> 3;
        let bit = if off < self.data.len() {
            (self.data[off] >> (self.bitpos & 7)) & 1
        } else {
            0
        };
        self.bitpos += 1;
        bit
    }
    /// Reads `n` bits, least significant first (extra bits, counts).
    fn read_bits_lsb(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            v |= (self.read_bit() as u32) << i;
        }
        v
    }
    fn align_byte(&mut self) {
        self.bitpos = (self.bitpos + 7) & !7;
    }
}

/// Canonical Huffman decoder. `build` precomputes the per-length code
/// counts, the first canonical code of each length, and the symbols
/// ordered by (length, symbol) — `decode` then walks bit by bit.
struct Huff {
    counts: [u16; 16],
    first: [u32; 16],
    symbols: Vec<u16>,
}

fn build_huff(lengths: &[u8]) -> Option<Huff> {
    let mut counts = [0u16; 16];
    for &l in lengths {
        if l > 15 {
            return None;
        }
        if l > 0 {
            counts[l as usize] += 1;
        }
    }
    if counts[1..].iter().all(|&c| c == 0) {
        return None; // no codes at all — invalid
    }
    let mut first = [0u32; 16];
    let mut code = 0u32;
    for len in 1..16usize {
        code = (code + counts[len - 1] as u32) << 1;
        first[len] = code;
    }
    let mut symbols = Vec::with_capacity(lengths.len());
    for len in 1..16u8 {
        for (sym, &l) in lengths.iter().enumerate() {
            if l == len {
                symbols.push(sym as u16);
            }
        }
    }
    for len in 1..16usize {
        if (first[len] + counts[len] as u32) > (1u32 << len) {
            return None; // codes overflow the length — malformed
        }
    }
    Some(Huff {
        counts,
        first,
        symbols,
    })
}

impl Huff {
    fn decode(&self, r: &mut BitReader) -> Option<u16> {
        let mut code = 0u32;
        let mut index = 0u32;
        for len in 1..16u32 {
            code = (code << 1) | r.read_bit() as u32;
            let count = self.counts[len as usize] as u32;
            if count > 0 {
                let f = self.first[len as usize];
                if code >= f && code < f + count {
                    return Some(self.symbols[(index + code - f) as usize]);
                }
                index += count;
            }
        }
        None
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// The fixed Huffman tables (RFC 1951 3.2.6).
fn fixed_lit() -> Huff {
    let mut lengths = [0u8; 288];
    for (i, l) in lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    build_huff(&lengths).expect("fixed literal table")
}
fn fixed_dist() -> Huff {
    build_huff(&[5u8; 32]).expect("fixed distance table")
}

/// Decodes a DEFLATE block using `lit`/`dist` Huffman tables, emitting
/// into `out`.
fn decode_blocks(r: &mut BitReader, lit: &Huff, dist: &Huff, out: &mut Vec<u8>) -> Option<()> {
    loop {
        let sym = lit.decode(r)?;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Some(());
        } else {
            let li = (sym - 257) as usize;
            if li >= 29 {
                return None;
            }
            let len = LENGTH_BASE[li] as usize
                + r.read_bits_lsb(LENGTH_EXTRA[li] as u32) as usize;
            let dsym = dist.decode(r)? as usize;
            if dsym >= 30 {
                return None;
            }
            let d = DIST_BASE[dsym] as usize + r.read_bits_lsb(DIST_EXTRA[dsym] as u32) as usize;
            if d > out.len() {
                return None;
            }
            // Overlapping copy: each byte reads `d` back from the
            // growing end, which naturally handles len > d (run fills).
            for _ in 0..len {
                let b = out[out.len() - d];
                out.push(b);
            }
        }
    }
}

/// Inflates a zlib stream (`RFC 1950` header + DEFLATE + adler32).
fn inflate(data: &[u8], out: &mut Vec<u8>) -> Option<()> {
    if data.len() < 6 {
        return None;
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 || (((cmf as u16) << 8) | flg as u16) % 31 != 0 {
        return None; // not DEFLATE, or bad FCHECK
    }
    if flg & 0x20 != 0 {
        return None; // preset dictionary not supported
    }
    let mut r = BitReader::new(&data[2..]);
    loop {
        let bfinal = r.read_bit();
        let btype = r.read_bits_lsb(2);
        match btype {
            0 => {
                // Stored block: byte-aligned LEN/NLEN then raw bytes.
                r.align_byte();
                let pos = r.bitpos >> 3;
                if pos + 4 > r.data.len() {
                    return None;
                }
                let len = (r.data[pos] as usize) | ((r.data[pos + 1] as usize) << 8);
                let nlen = (r.data[pos + 2] as usize) | ((r.data[pos + 3] as usize) << 8);
                if (len ^ 0xFFFF) & 0xFFFF != nlen {
                    return None;
                }
                if pos + 4 + len > r.data.len() {
                    return None;
                }
                out.extend_from_slice(&r.data[pos + 4..pos + 4 + len]);
                r.bitpos = (pos + 4 + len) << 3;
            }
            1 => decode_blocks(&mut r, &fixed_lit(), &fixed_dist(), out)?,
            2 => {
                // Dynamic Huffman block.
                let hlit = r.read_bits_lsb(5) + 257;
                let hdist = r.read_bits_lsb(5) + 1;
                let hclen = r.read_bits_lsb(4) + 4;
                const ORDER: [u8; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut cl = [0u8; 19];
                for i in 0..hclen as usize {
                    cl[ORDER[i] as usize] = r.read_bits_lsb(3) as u8;
                }
                let cl_huff = build_huff(&cl)?;
                let total = (hlit + hdist) as usize;
                let mut lengths = Vec::with_capacity(total);
                while lengths.len() < total {
                    let sym = cl_huff.decode(&mut r)?;
                    match sym {
                        0..=15 => lengths.push(sym as u8),
                        16 => {
                            if lengths.is_empty() {
                                return None;
                            }
                            let prev = *lengths.last().unwrap();
                            let rep = 3 + r.read_bits_lsb(2) as usize;
                            for _ in 0..rep {
                                lengths.push(prev);
                            }
                        }
                        17 => {
                            let rep = 3 + r.read_bits_lsb(3) as usize;
                            for _ in 0..rep {
                                lengths.push(0);
                            }
                        }
                        18 => {
                            let rep = 11 + r.read_bits_lsb(7) as usize;
                            for _ in 0..rep {
                                lengths.push(0);
                            }
                        }
                        _ => return None,
                    }
                }
                if lengths.len() != total {
                    return None;
                }
                if lengths[hlit as usize..].iter().all(|&l| l == 0) {
                    return None; // no distance codes — invalid
                }
                let lit = build_huff(&lengths[..hlit as usize])?;
                let dist = build_huff(&lengths[hlit as usize..])?;
                decode_blocks(&mut r, &lit, &dist, out)?;
            }
            _ => return None,
        }
        if bfinal == 1 {
            break;
        }
    }
    // zlib trailer: adler32 of the output (big-endian). A missing
    // trailer means the stream was truncated — reject it.
    r.align_byte();
    let pos = r.bitpos >> 3;
    if pos + 4 > r.data.len() || be32(r.data, pos) != adler32(out) {
        return None;
    }
    Some(())
}

// ---------------------------------------------------------------------
// PNG format
// ---------------------------------------------------------------------

/// Decodes an 8-bit, non-interlaced PNG into
/// `(width, height, pixels)` with `(a<<24)|(r<<16)|(g<<8)|b` pixels.
pub fn decode(data: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    if data.len() < 8 || data[..8] != PNG_SIG {
        return None;
    }
    let mut pos = 8usize;
    let (mut width, mut height) = (0u32, 0u32);
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    let mut seen_ihdr = false;

    while pos + 12 <= data.len() {
        let len = be32(data, pos) as usize;
        let ctype = &data[pos + 4..pos + 8];
        let cstart = pos + 8;
        if cstart + len + 4 > data.len() {
            return None; // chunk body or CRC runs off the end
        }
        let cdata = &data[cstart..cstart + len];
        match ctype {
            b"IHDR" => {
                if len != 13 {
                    return None;
                }
                width = be32(cdata, 0);
                height = be32(cdata, 4);
                bit_depth = cdata[8];
                color_type = cdata[9];
                if cdata[10] != 0 || cdata[11] != 0 {
                    return None; // compression/filter methods must be 0
                }
                if cdata[12] != 0 {
                    return None; // Adam7 interlacing out of scope
                }
                if bit_depth != 8 {
                    return None; // 16-bit and packed sub-8-bit out of scope
                }
                if !matches!(color_type, 0 | 2 | 3 | 4 | 6) {
                    return None;
                }
                seen_ihdr = true;
            }
            b"PLTE" => {
                if len % 3 != 0 {
                    return None;
                }
                for e in cdata.chunks_exact(3) {
                    palette.push([e[0], e[1], e[2]]);
                }
            }
            b"tRNS" => trns = cdata.to_vec(),
            b"IDAT" => idat.extend_from_slice(cdata),
            b"IEND" => break,
            _ => {} // unknown chunks (gAMA, tEXt, ...) — skip
        }
        pos = cstart + len + 4;
    }
    if !seen_ihdr || idat.is_empty() {
        return None;
    }

    let bpp = match color_type {
        0 | 3 => 1, // gray / palette index
        2 => 3,     // RGB
        4 => 2,     // gray + alpha
        6 => 4,     // RGBA
        _ => return None,
    };
    let stride = 1 + width as usize * bpp;

    let mut raw = Vec::new();
    inflate(&idat, &mut raw)?;
    if raw.len() < height as usize * stride {
        return None; // truncated scanlines
    }

    let mut pixels: Vec<u32> = Vec::with_capacity((width * height) as usize);
    let mut prev: Vec<u8> = vec![0; width as usize * bpp];
    for y in 0..height as usize {
        let row_off = y * stride;
        let f = raw[row_off];
        let row = &raw[row_off + 1..row_off + stride];
        let mut cur: Vec<u8> = Vec::with_capacity(width as usize * bpp);
        match f {
            0 => cur.extend_from_slice(row),
            1 => {
                for i in 0..row.len() {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    cur.push(row[i].wrapping_add(left));
                }
            }
            2 => {
                for i in 0..row.len() {
                    cur.push(row[i].wrapping_add(prev[i]));
                }
            }
            3 => {
                for i in 0..row.len() {
                    let left = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
                    let up = prev[i] as i32;
                    cur.push(row[i].wrapping_add(((left + up) / 2) as u8));
                }
            }
            4 => {
                for i in 0..row.len() {
                    let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
                    let b = prev[i] as i32;
                    let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                    cur.push(row[i].wrapping_add(paeth(a, b, c) as u8));
                }
            }
            _ => return None,
        }
        // Convert the row to 0xAARRGGBB.
        match color_type {
            0 => {
                for &g in &cur {
                    let g = g as u32;
                    pixels.push(0xFF00_0000 | (g << 16) | (g << 8) | g);
                }
            }
            2 => {
                for c in cur.chunks_exact(3) {
                    pixels.push(
                        0xFF00_0000 | ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32,
                    );
                }
            }
            3 => {
                for &i in &cur {
                    let (r, g, b) = match palette.get(i as usize) {
                        Some(p) => (p[0], p[1], p[2]),
                        None => (0, 0, 0),
                    };
                    let a = trns.get(i as usize).copied().unwrap_or(0xFF);
                    pixels.push(
                        ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
                    );
                }
            }
            4 => {
                for c in cur.chunks_exact(2) {
                    let g = c[0] as u32;
                    pixels.push(((c[1] as u32) << 24) | (g << 16) | (g << 8) | g);
                }
            }
            6 => {
                for c in cur.chunks_exact(4) {
                    pixels.push(
                        ((c[3] as u32) << 24) | ((c[0] as u32) << 16) | ((c[1] as u32) << 8)
                            | c[2] as u32,
                    );
                }
            }
            _ => return None,
        }
        prev = cur;
    }
    Some((width, height, pixels))
}

/// PNG Paeth predictor.
fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}
