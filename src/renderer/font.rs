//! Font discovery (fontconfig) and glyph rasterization (ab_glyph).
//!
//! Font files are memory-mapped and outlines are parsed lazily per glyph, so
//! large fonts (Nerd Fonts, CJK collections) cost only the pages in use.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::fs::File;
use std::ops::Deref;
use std::process::Command;
use std::rc::Rc;

use ab_glyph::{Font, FontRef, GlyphId, PxScale, point};
use memmap2::Mmap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Style {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

impl Style {
    pub fn new(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (false, false) => Style::Regular,
            (true, false) => Style::Bold,
            (false, true) => Style::Italic,
            (true, true) => Style::BoldItalic,
        }
    }
}

/// Identifies a loaded font: 0 regular, 1..=3 the style variants (see
/// [`Style`]), `FALLBACK_BASE + i` the i-th fallback font.
pub type FontId = u16;
const FALLBACK_BASE: FontId = 4;
const SHAPE_CACHE_LIMIT: usize = 4096;

/// One glyph of a shaped run, positioned relative to the cell of its cluster.
#[derive(Clone, Copy, Debug)]
pub struct ShapedGlyph {
    pub glyph: u16,
    /// Byte offset into the shaped text of the first char this glyph covers.
    pub cluster: u32,
    pub x_offset: f32,
    pub y_offset: f32,
    pub x_advance: f32,
}

pub struct RasterizedGlyph {
    /// Horizontal offset of the bitmap from the pen position.
    pub left: f32,
    /// Vertical offset of the bitmap top from the baseline (negative = above).
    pub top: f32,
    pub width: usize,
    pub height: usize,
    pub advance: f32,
    /// Coverage per pixel, or RGBA (straight alpha) when `color` is set.
    pub bitmap: Vec<u8>,
    pub color: bool,
}

/// Locates a font file with fontconfig. Returns `(path, collection index)`.
fn fc_match(pattern: &str) -> Option<(String, u32)> {
    let output = Command::new("fc-match")
        .args(["--format=%{file}\n%{index}", pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines();
    let file = lines.next()?.to_string();
    let index = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (!file.is_empty()).then_some((file, index))
}

/// A memory-mapped font file: only the pages touched while rasterizing become
/// resident, and they are shared with the page cache rather than copied.
pub struct Face {
    font: FontRef<'static>,
    shaper: rustybuzz::Face<'static>,
    // Declared after the fields borrowing from it, so it is dropped last.
    _map: Mmap,
}

impl Face {
    fn open(path: &str, index: u32) -> Option<Face> {
        let file = File::open(path).ok()?;
        // SAFETY: font files are not expected to be modified while mapped.
        let map = unsafe { Mmap::map(&file) }.ok()?;
        // SAFETY: the mapping never moves and outlives `font` (see field order).
        let data: &'static [u8] = unsafe { std::slice::from_raw_parts(map.as_ptr(), map.len()) };
        let font = match FontRef::try_from_slice_and_index(data, index) {
            Ok(font) => font,
            Err(e) => {
                log::warn!("failed to parse font {path}: {e}");
                return None;
            }
        };
        let shaper = rustybuzz::Face::from_slice(data, index)?;
        Some(Face {
            font,
            shaper,
            _map: map,
        })
    }
}

impl Face {
    /// Whether glyphs are stored as images (CBDT / sbix), as in colour emoji fonts.
    fn has_color_bitmaps(&self) -> bool {
        let tables = self.shaper.tables();
        tables.cbdt.is_some() || tables.sbix.is_some()
    }
}

/// Characters displayed as colour emoji by default (Emoji_Presentation):
/// these are the double-width ones among the emoji and symbol blocks.
fn is_emoji_presentation(c: char) -> bool {
    use unicode_width::UnicodeWidthChar;
    let n = c as u32;
    (n >= 0x1f000 || (0x2300..=0x2bff).contains(&n)) && c.width() == Some(2)
}

impl Deref for Face {
    type Target = FontRef<'static>;

    fn deref(&self) -> &Self::Target {
        &self.font
    }
}

const VARIANT_PATTERNS: [&str; 3] = ["weight=bold", "slant=italic", "weight=bold:slant=italic"];

const LAST_RESORT: &[&str] = &[
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/System/Library/Fonts/Menlo.ttc",
];

fn has_glyph(font: &Face, c: char) -> bool {
    font.glyph_id(c) != GlyphId(0)
}

/// Pixels per font unit for an em size of `px`.
fn units_scale(font: &Face, px: f32) -> f32 {
    px / font.units_per_em().unwrap_or(1000.0)
}

pub struct FontSet {
    family: String,
    regular: Face,
    regular_file: Option<(String, u32)>,
    /// Bold, italic and bold-italic faces, loaded on first use (`None` when
    /// the family has no such face).
    variants: [OnceCell<Option<Face>>; 3],
    fallbacks: Vec<Face>,
    loaded_fallback_files: Vec<(String, u32)>,
    /// Fallback index able to render a char (`None`: no font has it).
    fallback_cache: HashMap<char, Option<usize>>,
    /// Colour font chosen for emoji-presentation characters, if any.
    emoji_cache: HashMap<char, Option<usize>>,
    /// Shaping results per face, keyed by text. Offsets are in pixels, so
    /// the cache is cleared when the size changes.
    shape_cache: HashMap<FontId, HashMap<String, Rc<[ShapedGlyph]>>>,
    pub px: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    /// Distance from the cell top to the baseline.
    pub baseline: f32,
    pub underline_offset: f32,
    pub stroke: f32,
}

impl FontSet {
    /// `fallback` lists families tried, in order, for characters the main
    /// font lacks before asking fontconfig.
    pub fn new(family: &str, px: f32, fallback: &[String]) -> Self {
        let regular_file = fc_match(family);
        let regular = regular_file
            .as_ref()
            .and_then(|(path, index)| {
                log::info!("font: {path} (index {index})");
                Face::open(path, *index)
            })
            .or_else(|| LAST_RESORT.iter().find_map(|p| Face::open(p, 0)))
            .expect("no usable font found (is fontconfig installed?)");

        let mut set = Self {
            family: family.to_string(),
            regular,
            regular_file,
            variants: Default::default(),
            fallbacks: Vec::new(),
            loaded_fallback_files: Vec::new(),
            fallback_cache: HashMap::new(),
            emoji_cache: HashMap::new(),
            shape_cache: HashMap::new(),
            px,
            cell_width: 0.0,
            cell_height: 0.0,
            baseline: 0.0,
            underline_offset: 0.0,
            stroke: 1.0,
        };
        set.compute_metrics();
        for name in fallback {
            let Some((path, index)) = fc_match(name) else {
                log::warn!("fallback font {name:?} not found");
                continue;
            };
            if set.loaded_fallback_files.contains(&(path.clone(), index))
                || set.regular_file.as_ref() == Some(&(path.clone(), index))
            {
                continue;
            }
            set.loaded_fallback_files.push((path.clone(), index));
            if let Some(face) = Face::open(&path, index) {
                log::info!("fallback font {name:?}: {path}");
                set.fallbacks.push(face);
            }
        }
        set
    }

    pub fn set_size(&mut self, px: f32) {
        self.px = px;
        self.shape_cache.clear();
        self.compute_metrics();
    }

    /// The font used for `style`, loading the variant on first use.
    fn face_id(&self, style: Style) -> FontId {
        if style == Style::Regular {
            return 0;
        }
        let i = style as usize - 1;
        let loaded = self.variants[i].get_or_init(|| {
            fc_match(&format!("{}:{}", self.family, VARIANT_PATTERNS[i]))
                .filter(|file| Some(file) != self.regular_file.as_ref())
                .and_then(|(path, index)| Face::open(&path, index))
        });
        if loaded.is_some() { style as FontId } else { 0 }
    }

    fn face_by_id(&self, id: FontId) -> &Face {
        match id {
            0 => &self.regular,
            1..=3 => self.variants[id as usize - 1]
                .get()
                .and_then(Option::as_ref)
                .unwrap_or(&self.regular),
            _ => &self.fallbacks[(id - FALLBACK_BASE) as usize],
        }
    }

    fn face(&self, style: Style) -> &Face {
        self.face_by_id(self.face_id(style))
    }

    /// Whether the face for `style` itself has a glyph for `c` (no fallback).
    pub fn has_char(&self, c: char, style: Style) -> bool {
        has_glyph(self.face(style), c)
    }

    /// Shape `text` with the face for `style`, applying the font's default
    /// features (including `liga` and `calt` ligatures).
    pub fn shape(&mut self, text: &str, style: Style) -> (FontId, Rc<[ShapedGlyph]>) {
        let id = self.face_id(style);
        (id, self.shape_with(id, text))
    }

    /// Shape a multi-char cluster with the font that should draw it: a
    /// colour font for emoji sequences, else the one with its first char.
    pub fn shape_cluster(&mut self, text: &str, style: Style) -> (FontId, Rc<[ShapedGlyph]>) {
        let first = text.chars().next().unwrap_or(' ');
        let emoji = (crate::terminal::cluster::is_emoji(text) || is_emoji_presentation(first))
            .then(|| self.emoji_font(first))
            .flatten();
        let id = emoji.unwrap_or_else(|| self.resolve(first, style).0);
        (id, self.shape_with(id, text))
    }

    fn emoji_font(&mut self, c: char) -> Option<FontId> {
        let choice = match self.emoji_cache.get(&c) {
            Some(choice) => *choice,
            None => {
                let choice = self.find_emoji_fallback(c);
                self.emoji_cache.insert(c, choice);
                choice
            }
        };
        choice.map(|i| FALLBACK_BASE + i as FontId)
    }

    fn shape_with(&mut self, id: FontId, text: &str) -> Rc<[ShapedGlyph]> {
        if let Some(glyphs) = self.shape_cache.get(&id).and_then(|c| c.get(text)) {
            return glyphs.clone();
        }

        let face = self.face_by_id(id);
        let s = units_scale(face, self.px);
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.set_direction(rustybuzz::Direction::LeftToRight);
        let output = rustybuzz::shape(&face.shaper, &[], buffer);
        let glyphs: Rc<[ShapedGlyph]> = output
            .glyph_infos()
            .iter()
            .zip(output.glyph_positions())
            .map(|(info, pos)| ShapedGlyph {
                glyph: info.glyph_id as u16,
                cluster: info.cluster,
                x_offset: pos.x_offset as f32 * s,
                y_offset: pos.y_offset as f32 * s,
                x_advance: pos.x_advance as f32 * s,
            })
            .collect();

        let cache = self.shape_cache.entry(id).or_default();
        if cache.len() >= SHAPE_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(text.to_owned(), glyphs.clone());
        glyphs
    }

    fn compute_metrics(&mut self) {
        let font = &self.regular;
        let px = self.px;
        let s = units_scale(font, px);
        let ascent = font.ascent_unscaled() * s;
        let descent = font.descent_unscaled() * s;
        let gap = font.line_gap_unscaled() * s;
        let content = ascent - descent;
        self.cell_height = (content + gap).ceil().max(1.0);
        self.cell_width = (font.h_advance_unscaled(font.glyph_id('M')) * s)
            .round()
            .max(1.0);
        self.baseline = ((self.cell_height - content) / 2.0 + ascent).round();
        self.stroke = (px / 14.0).round().max(1.0);
        self.underline_offset = (self.baseline + (-descent * 0.4).max(self.stroke)).round();
        log::info!(
            "font metrics: px={px:.1} cell={}x{} baseline={}",
            self.cell_width,
            self.cell_height,
            self.baseline
        );
    }

    /// Find the font and glyph that render `c`, consulting fontconfig for
    /// fallbacks.
    pub fn resolve(&mut self, c: char, style: Style) -> (FontId, u16) {
        let id = self.face_id(style);
        let glyph_of = |face: &Face| face.glyph_id(c).0;
        // Emoji shown as pictures by default come from a colour font even
        // when the text font has a monochrome version (✅, ⌚, ...).
        if is_emoji_presentation(c)
            && let Some(id) = self.emoji_font(c)
        {
            return (id, glyph_of(self.face_by_id(id)));
        }
        if c == ' ' || has_glyph(self.face_by_id(id), c) {
            return (id, glyph_of(self.face_by_id(id)));
        }
        if has_glyph(&self.regular, c) {
            return (0, glyph_of(&self.regular));
        }
        let choice = match self.fallback_cache.get(&c) {
            Some(choice) => *choice,
            None => {
                let choice = self.find_fallback(c);
                self.fallback_cache.insert(c, choice);
                choice
            }
        };
        match choice {
            Some(i) => (FALLBACK_BASE + i as FontId, glyph_of(&self.fallbacks[i])),
            None => (id, 0),
        }
    }

    fn find_emoji_fallback(&mut self, c: char) -> Option<usize> {
        if let Some(i) = self
            .fallbacks
            .iter()
            .position(|f| f.has_color_bitmaps() && has_glyph(f, c))
        {
            return Some(i);
        }
        let (path, index) = fc_match(&format!("emoji:charset={:x}", c as u32))?;
        // Already loaded, and the search above found it unsuitable.
        if self
            .loaded_fallback_files
            .iter()
            .any(|(p, i)| *p == path && *i == index)
        {
            return None;
        }
        self.loaded_fallback_files.push((path.clone(), index));
        let font = Face::open(&path, index)?;
        if !font.has_color_bitmaps() || !has_glyph(&font, c) {
            return None;
        }
        log::info!("emoji font for {c:?}: {path}");
        self.fallbacks.push(font);
        Some(self.fallbacks.len() - 1)
    }

    fn find_fallback(&mut self, c: char) -> Option<usize> {
        if let Some(i) = self.fallbacks.iter().position(|f| has_glyph(f, c)) {
            return Some(i);
        }
        let pattern = format!("{}:charset={:x}", self.family, c as u32);
        let (path, index) = fc_match(&pattern)?;
        if self
            .loaded_fallback_files
            .iter()
            .any(|(p, i)| *p == path && *i == index)
        {
            return None;
        }
        self.loaded_fallback_files.push((path.clone(), index));
        let font = Face::open(&path, index)?;
        if !has_glyph(&font, c) {
            return None;
        }
        log::info!("fallback font for {c:?}: {path}");
        self.fallbacks.push(font);
        Some(self.fallbacks.len() - 1)
    }

    pub fn rasterize(&self, font: FontId, glyph: u16) -> RasterizedGlyph {
        let font = self.face_by_id(font);
        let s = units_scale(font, self.px);
        let id = GlyphId(glyph);
        let advance = font.h_advance_unscaled(id) * s;
        let scale = PxScale::from(font.height_unscaled() * s);
        let empty = RasterizedGlyph {
            left: 0.0,
            top: 0.0,
            width: 0,
            height: 0,
            advance,
            bitmap: Vec::new(),
            color: false,
        };

        let Some(outline) = font.outline_glyph(id.with_scale_and_position(scale, point(0.0, 0.0)))
        else {
            return self.rasterize_bitmap(font, glyph).unwrap_or(empty);
        };
        let bounds = outline.px_bounds();
        let (width, height) = (bounds.width() as usize, bounds.height() as usize);
        if width == 0 || height == 0 {
            return empty;
        }
        let mut bitmap = vec![0u8; width * height];
        outline.draw(|x, y, coverage| {
            let i = y as usize * width + x as usize;
            if let Some(px) = bitmap.get_mut(i) {
                *px = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });
        RasterizedGlyph {
            left: bounds.min.x,
            top: bounds.min.y,
            width,
            height,
            advance,
            bitmap,
            color: false,
        }
    }

    /// Glyphs stored as images (colour emoji fonts such as Noto Color Emoji
    /// use PNG strikes), scaled to fit the cell height and two cells wide.
    fn rasterize_bitmap(&self, font: &Face, glyph: u16) -> Option<RasterizedGlyph> {
        use rustybuzz::ttf_parser::{GlyphId, RasterImageFormat};
        let image = font.shaper.glyph_raster_image(GlyphId(glyph), u16::MAX)?;
        if image.format != RasterImageFormat::PNG {
            return None;
        }
        let (rgba, w, h) = decode_png(image.data)?;
        let scale = (self.cell_height / h as f32).min(2.0 * self.cell_width / w as f32);
        let (width, height) = (
            ((w as f32 * scale).round() as usize).max(1),
            ((h as f32 * scale).round() as usize).max(1),
        );
        Some(RasterizedGlyph {
            left: 0.0,
            top: ((self.cell_height - height as f32) / 2.0).round() - self.baseline,
            width,
            height,
            advance: width as f32,
            bitmap: downscale_rgba(&rgba, w, h, width, height),
            color: true,
        })
    }
}

/// Decode a PNG into straight-alpha RGBA8.
pub(crate) fn decode_png(data: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let pixels = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((rgba, w, h))
}

/// Area-average downscale; colour is weighted by alpha so transparent pixels
/// don't darken the edges.
pub(crate) fn downscale_rgba(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        let (y0, y1) = (dy * sh / dh, ((dy + 1) * sh).div_ceil(dh).min(sh));
        for dx in 0..dw {
            let (x0, x1) = (dx * sw / dw, ((dx + 1) * sw).div_ceil(dw).min(sw));
            let mut acc = [0u64; 4];
            for y in y0..y1.max(y0 + 1) {
                for x in x0..x1.max(x0 + 1) {
                    let p = &src[(y * sw + x) * 4..][..4];
                    let a = p[3] as u64;
                    acc[0] += p[0] as u64 * a;
                    acc[1] += p[1] as u64 * a;
                    acc[2] += p[2] as u64 * a;
                    acc[3] += a;
                }
            }
            let n = ((y1.max(y0 + 1) - y0) * (x1.max(x0 + 1) - x0)) as u64;
            let o = &mut out[(dy * dw + dx) * 4..][..4];
            for i in 0..3 {
                o[i] = acc[i].checked_div(acc[3]).unwrap_or(0) as u8;
            }
            o[3] = (acc[3] / n) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_keeps_colour_at_transparent_edges() {
        // Left half opaque red, right half fully transparent black.
        let src: Vec<u8> = (0..4)
            .flat_map(|i| {
                if i % 2 == 0 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 0, 0]
                }
            })
            .collect();
        let out = downscale_rgba(&src, 2, 2, 1, 1);
        assert_eq!(out, [255, 0, 0, 127]);
    }

    #[test]
    fn colour_emoji_are_rasterized_as_images() {
        if fc_match("monospace:charset=1f980").is_none_or(|(path, _)| !path.contains("Emoji")) {
            return; // no colour emoji font on this machine
        }
        let mut fonts = FontSet::new("monospace", 16.0, &[]);
        let (id, glyph) = fonts.resolve('🦀', Style::Regular);
        assert_ne!(glyph, 0);
        let g = fonts.rasterize(id, glyph);
        assert!(g.color);
        assert_eq!(g.bitmap.len(), g.width * g.height * 4);
        assert!(g.height as f32 <= fonts.cell_height && g.width as f32 <= 2.0 * fonts.cell_width);
        // Sequences become one picture: a family (ZWJ), a flag, a skin tone.
        for seq in ["👨\u{200d}👩\u{200d}👧", "🇯🇵", "👍🏽", "\u{2764}\u{fe0f}"] {
            let (id, glyphs) = fonts.shape_cluster(seq, Style::Regular);
            assert_eq!(glyphs.len(), 1, "{seq}");
            assert!(fonts.rasterize(id, glyphs[0].glyph).color, "{seq}");
        }
        assert!(
            g.bitmap
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] == 255 && (p[0] != p[1] || p[1] != p[2]))
        );
    }
}
