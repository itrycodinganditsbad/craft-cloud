//! `cargo xtask icons [--check]`: draws Craft Cloud's own icons (made for this repository: a cloud, no other mark) and
//! writes them to `apps/craft-cloud-desktop/assets/`: the app's icon as PNG, ICO (Windows) and ICNS (macOS), and the
//! tray's icon in each state. `--check` only compares (CI): the committed files must be what this draws.
//!
//! Shapes are filled by coverage (16 samples a pixel) and written as PNG with a small deflate of its own (fixed
//! codes, runs of the pixel before or above), so this needs nothing beyond `std`; ICO and ICNS hold the same PNGs.

use std::path::Path;

/// A colour, straight (not premultiplied) RGBA.
type Rgba = [u8; 4];

const BLUE: Rgba = [0x2f, 0x7d, 0xe1, 0xff];
const GRAY: Rgba = [0x8a, 0x8f, 0x98, 0xff];
const WHITE: Rgba = [0xff, 0xff, 0xff, 0xff];
const SKY: Rgba = [0x3a, 0xa8, 0x5c, 0xff];
const RED: Rgba = [0xe5, 0x48, 0x4d, 0xff];
const SLATE: Rgba = [0x5b, 0x63, 0x70, 0xff];

/// The tray's states, by file name.
const TRAY: &[(&str, Tray)] = &[
    ("tray-synced", Tray::Synced),
    ("tray-syncing", Tray::Syncing),
    ("tray-offline", Tray::Offline),
    ("tray-attention", Tray::Attention),
    ("tray-paused", Tray::Paused),
];
/// The tray icon's size.
const TRAY_SIZE: u32 = 32;
/// The app icon's sizes in the ICO, and the PNG's.
const ICO_SIZES: &[u32] = &[16, 24, 32, 48, 64, 256];
const APP_SIZE: u32 = 256;
/// The ICNS entries: type and size.
const ICNS_ENTRIES: &[(&[u8; 4], u32)] = &[(b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tray {
    Synced,
    Syncing,
    Offline,
    Attention,
    Paused,
}

/// Every file and its bytes.
pub fn files() -> Vec<(String, Vec<u8>)> {
    let mut files = vec![
        ("icon.png".to_owned(), png(APP_SIZE, &app_icon(APP_SIZE))),
        ("icon.ico".to_owned(), ico(&ICO_SIZES.iter().map(|&s| (s, png(s, &app_icon(s)))).collect::<Vec<_>>())),
        ("icon.icns".to_owned(), icns(&ICNS_ENTRIES.iter().map(|&(kind, s)| (*kind, png(s, &app_icon(s)))).collect::<Vec<_>>())),
    ];
    for (name, state) in TRAY {
        files.push((format!("{name}.png"), png(TRAY_SIZE, &tray_icon(TRAY_SIZE, *state))));
    }
    files
}

pub fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let check = match args {
        [] => false,
        ["--check"] => true,
        _ => return Err("usage: cargo xtask icons [--check]".into()),
    };
    let dir = root.join("apps").join("craft-cloud-desktop").join("assets");
    let mut stale = Vec::new();
    for (name, bytes) in files() {
        let path = dir.join(&name);
        if check {
            if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                stale.push(name);
            }
        } else {
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("wrote {}", path.display());
        }
    }
    if stale.is_empty() {
        if check {
            println!("icons: all up to date");
        }
        Ok(())
    } else {
        Err(format!("icons out of date (run `cargo xtask icons`): {}", stale.join(", ")))
    }
}

/// A shape: whether a point (in units of the icon's side, 0 to 1) is inside.
trait Shape {
    fn contains(&self, x: f64, y: f64) -> bool;
}

struct Circle(f64, f64, f64);

impl Shape for Circle {
    fn contains(&self, x: f64, y: f64) -> bool {
        let (dx, dy) = (x - self.0, y - self.1);
        dx * dx + dy * dy <= self.2 * self.2
    }
}

/// A rectangle (left, top, right, bottom) with corners of radius `r`.
struct RoundRect(f64, f64, f64, f64, f64);

impl Shape for RoundRect {
    fn contains(&self, x: f64, y: f64) -> bool {
        let RoundRect(l, t, r, b, radius) = *self;
        if x < l || x > r || y < t || y > b {
            return false;
        }
        // A side shorter than two radii: the corners meet in its middle.
        let between = |v: f64, lo: f64, hi: f64| if lo <= hi { v.clamp(lo, hi) } else { (lo + hi) / 2.0 };
        let cx = between(x, l + radius, r - radius);
        let cy = between(y, t + radius, b - radius);
        let (dx, dy) = (x - cx, y - cy);
        dx * dx + dy * dy <= radius * radius
    }
}

/// The cloud: three puffs over a rounded base, in a box at (`x`, `y`) of side `s`.
fn cloud(x: f64, y: f64, s: f64) -> Vec<Box<dyn Shape>> {
    let at = |u: f64, v: f64| (x + u * s, y + v * s);
    let (a, b, c) = (at(0.33, 0.57), at(0.53, 0.44), at(0.72, 0.59));
    let (l, t) = at(0.14, 0.55);
    let (r, bottom) = at(0.89, 0.76);
    vec![
        Box::new(Circle(a.0, a.1, 0.19 * s)),
        Box::new(Circle(b.0, b.1, 0.24 * s)),
        Box::new(Circle(c.0, c.1, 0.17 * s)),
        Box::new(RoundRect(l, t, r, bottom, 0.1 * s)),
    ]
}

/// A canvas of `size`² pixels, transparent.
struct Canvas {
    size: u32,
    pixels: Vec<Rgba>,
}

impl Canvas {
    fn new(size: u32) -> Self {
        Self { size, pixels: vec![[0, 0, 0, 0]; (size * size) as usize] }
    }

    /// Paints the union of `shapes` in `color` over what is there, by coverage.
    fn fill(&mut self, shapes: &[Box<dyn Shape>], color: Rgba) {
        const N: u32 = 4;
        let side = f64::from(self.size);
        for py in 0..self.size {
            for px in 0..self.size {
                let mut hits = 0u32;
                for sy in 0..N {
                    for sx in 0..N {
                        let x = (f64::from(px) + (f64::from(sx) + 0.5) / f64::from(N)) / side;
                        let y = (f64::from(py) + (f64::from(sy) + 0.5) / f64::from(N)) / side;
                        if shapes.iter().any(|s| s.contains(x, y)) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    let coverage = f64::from(hits) / f64::from(N * N);
                    let index = (py * self.size + px) as usize;
                    if let Some(pixel) = self.pixels.get_mut(index) {
                        *pixel = over(*pixel, color, coverage);
                    }
                }
            }
        }
    }

    fn rgba(&self) -> Vec<u8> {
        self.pixels.iter().flatten().copied().collect()
    }
}

/// `top` with alpha times `coverage` over `bottom` (straight alpha).
fn over(bottom: Rgba, top: Rgba, coverage: f64) -> Rgba {
    let ta = f64::from(top[3]) / 255.0 * coverage;
    let ba = f64::from(bottom[3]) / 255.0;
    let a = ta + ba * (1.0 - ta);
    if a <= 0.0 {
        return [0, 0, 0, 0];
    }
    let mut out = [0u8; 4];
    for i in 0..3 {
        let c = (f64::from(top[i]) * ta + f64::from(bottom[i]) * ba * (1.0 - ta)) / a;
        out[i] = c.round().clamp(0.0, 255.0) as u8;
    }
    out[3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
    out
}

/// The app's icon: a white cloud on a blue rounded square.
fn app_icon(size: u32) -> Vec<u8> {
    let mut canvas = Canvas::new(size);
    canvas.fill(&[Box::new(RoundRect(0.04, 0.04, 0.96, 0.96, 0.2))], BLUE);
    canvas.fill(&cloud(0.08, 0.04, 0.84), WHITE);
    canvas.rgba()
}

/// The tray's icon: the cloud (grey while offline), with a badge for syncing, needing attention and paused.
fn tray_icon(size: u32, state: Tray) -> Vec<u8> {
    let mut canvas = Canvas::new(size);
    let body = if state == Tray::Offline { GRAY } else { BLUE };
    canvas.fill(&cloud(0.0, 0.0, 1.0), body);
    let badge = match state {
        Tray::Synced | Tray::Offline => return canvas.rgba(),
        Tray::Syncing => SKY,
        Tray::Attention => RED,
        Tray::Paused => SLATE,
    };
    let (bx, by, br) = (0.74, 0.74, 0.22);
    canvas.fill(&[Box::new(Circle(bx, by, br + 0.05))], WHITE);
    canvas.fill(&[Box::new(Circle(bx, by, br))], badge);
    let glyph: Vec<Box<dyn Shape>> = match state {
        // Three dots: under way.
        Tray::Syncing => vec![Box::new(Circle(bx - 0.1, by, 0.04)), Box::new(Circle(bx, by, 0.04)), Box::new(Circle(bx + 0.1, by, 0.04))],
        // "!".
        Tray::Attention => vec![Box::new(RoundRect(bx - 0.035, by - 0.14, bx + 0.035, by + 0.04, 0.03)), Box::new(Circle(bx, by + 0.1, 0.04))],
        // Two bars.
        Tray::Paused => vec![
            Box::new(RoundRect(bx - 0.09, by - 0.11, bx - 0.03, by + 0.11, 0.02)),
            Box::new(RoundRect(bx + 0.03, by - 0.11, bx + 0.09, by + 0.11, 0.02)),
        ],
        Tray::Synced | Tray::Offline => Vec::new(),
    };
    canvas.fill(&glyph, WHITE);
    canvas.rgba()
}

/// `rgba` (`size`² pixels) as a PNG: 8-bit RGBA, no filter, stored deflate blocks.
fn png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(rgba.len() + size as usize);
    for row in rgba.chunks((size * 4) as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut zlib = vec![0x78, 0x01];
    zlib.extend_from_slice(&deflate(&raw, (size * 4 + 1) as usize));
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The bits of a deflate stream, least significant first.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    /// `count` bits of `value`, low bit first (extra bits, header fields).
    fn put(&mut self, value: u32, count: u32) {
        for i in 0..count {
            self.acc |= ((value >> i) & 1) << self.n;
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    /// A Huffman code of `count` bits, high bit first.
    fn code(&mut self, code: u32, count: u32) {
        for i in (0..count).rev() {
            self.put((code >> i) & 1, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Deflate (RFC 1951) with one block of the fixed Huffman codes, matching only runs that repeat the pixel before
/// (4 bytes back) or the one above (`stride` back): enough for icons of flat colours.
fn deflate(data: &[u8], stride: usize) -> Vec<u8> {
    const LENGTH_BASE: [usize; 29] =
        [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
    const LENGTH_EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
    const DIST_BASE: [usize; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u32; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
    let mut bits = Bits::default();
    // The last block, fixed codes.
    bits.put(1, 1);
    bits.put(1, 2);
    let symbol = |bits: &mut Bits, s: u32| match s {
        0..=143 => bits.code(0x30 + s, 8),
        144..=255 => bits.code(0x190 + s - 144, 9),
        256..=279 => bits.code(s - 256, 7),
        _ => bits.code(0xc0 + s - 280, 8),
    };
    let mut i = 0;
    while i < data.len() {
        let mut best = (0usize, 0usize);
        for distance in [4, stride] {
            if distance == 0 || distance > i || distance > 32_768 {
                continue;
            }
            let mut len = 0;
            while len < 258 && i + len < data.len() && data.get(i + len) == data.get(i + len - distance) {
                len += 1;
            }
            if len > best.0 {
                best = (len, distance);
            }
        }
        let (len, distance) = best;
        if len < 3 {
            symbol(&mut bits, u32::from(data.get(i).copied().unwrap_or(0)));
            i += 1;
            continue;
        }
        let code = LENGTH_BASE.iter().rposition(|&b| b <= len).unwrap_or(0);
        symbol(&mut bits, 257 + code as u32);
        bits.put((len - LENGTH_BASE.get(code).copied().unwrap_or(3)) as u32, LENGTH_EXTRA.get(code).copied().unwrap_or(0));
        let dcode = DIST_BASE.iter().rposition(|&b| b <= distance).unwrap_or(0);
        bits.code(dcode as u32, 5);
        bits.put((distance - DIST_BASE.get(dcode).copied().unwrap_or(1)) as u32, DIST_EXTRA.get(dcode).copied().unwrap_or(0));
        i += len;
    }
    symbol(&mut bits, 256);
    bits.finish()
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(out.get(start..).unwrap_or_default());
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in bytes {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

/// An ICO holding `images` (size and PNG), as Windows reads them since Vista.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in images {
        // 256 is written as 0.
        let side = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend_from_slice(data);
    }
    out
}

/// An ICNS holding `entries` (type and PNG).
fn icns(entries: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let total: usize = 8 + entries.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = b"icns".to_vec();
    out.extend_from_slice(&(total as u32).to_be_bytes());
    for (kind, data) in entries {
        out.extend_from_slice(kind);
        out.extend_from_slice(&((8 + data.len()) as u32).to_be_bytes());
        out.extend_from_slice(data);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_the_standard_ones() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn a_png_is_laid_out_as_the_format_says() {
        let image = png(2, &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 0]);
        assert!(image.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x02\0\0\0\x02\x08\x06"));
        assert!(image.ends_with(b"IEND\xae\x42\x60\x82"));
        // Flat colours compress well (the app's tests decode every icon with the `png` crate).
        let big = png(256, &app_icon(256));
        assert!(big.len() < 256 * 256, "{}", big.len());
    }

    /// The deflate stream of one byte, worked out by hand; and a repeated pixel becomes a match.
    #[test]
    fn deflate_writes_fixed_codes() {
        // Literal 'a' (0x61) is code 0x30 + 0x61 = 0x91 in 8 bits; end of block is 7 zero bits.
        let out = deflate(b"a", 100);
        // Header bits 1, 1 0; then 1001 0001 high bit first; then 0000000.
        let mut expect = Bits::default();
        expect.put(1, 1);
        expect.put(1, 2);
        expect.code(0x91, 8);
        expect.code(0, 7);
        assert_eq!(out, expect.finish());
        // A run of a repeated pixel becomes a match.
        let pixels: Vec<u8> = [1u8, 2, 3, 4].iter().copied().cycle().take(400).collect();
        assert!(deflate(&pixels, 1000).len() < 20);
    }

    #[test]
    fn the_icons_are_drawn_the_same_every_time_and_differ_by_state() {
        let a = files();
        assert_eq!(a, files());
        let names: Vec<&str> = a.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"icon.ico") && names.contains(&"icon.icns") && names.contains(&"tray-paused.png"));
        let trays: Vec<&Vec<u8>> = a.iter().filter(|(n, _)| n.starts_with("tray-")).map(|(_, b)| b).collect();
        for (i, x) in trays.iter().enumerate() {
            for y in trays.iter().skip(i + 1) {
                assert_ne!(x, y);
            }
        }
        let ico = &a.iter().find(|(n, _)| n == "icon.ico").unwrap().1;
        assert_eq!(ico[..6], [0, 0, 1, 0, ICO_SIZES.len() as u8, 0]);
        let icns = &a.iter().find(|(n, _)| n == "icon.icns").unwrap().1;
        assert_eq!(u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize, icns.len());
    }
}
