//! Text for the agent overlay: Inter Regular and SemiBold (an ASCII subset,
//! see `font/README.md`) rasterized with fontdue and blended onto a pixmap.
//!
//! Positions and sizes are logical px; `scale` maps them to the pixmap's
//! pixels. Glyphs are rasterized at the physical size, so text stays crisp
//! at 2x instead of being an upscaled 1x bitmap, and cached per thread by
//! (char, size, weight, scale). No kerning: fontdue reads only a legacy
//! `kern` table, and Inter has none.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use fontdue::{Font, FontSettings};
use tiny_skia::Pixmap;

static REGULAR_TTF: &[u8] = include_bytes!("font/Inter-Regular.ttf");
static SEMIBOLD_TTF: &[u8] = include_bytes!("font/Inter-SemiBold.ttf");

/// Drawn for a character the subset has no glyph for.
const FALLBACK: char = '?';

/// Appended to a string cut to fit ([`ellipsize`]).
const ELLIPSIS: char = '\u{2026}';

/// Cached glyphs before the cache is dropped and starts over. ASCII in two
/// weights at a few sizes and scales needs a few hundred.
const CACHE_LIMIT: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Weight {
    Regular,
    SemiBold,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TextStyle {
    pub(super) weight: Weight,
    /// Logical px.
    pub(super) size: f32,
    /// Every digit takes the widest digit's advance, so a ticking timer
    /// keeps its width (Inter's default figures are proportional).
    pub(super) tabular: bool,
}

impl TextStyle {
    pub(super) const fn new(weight: Weight, size: f32) -> Self {
        Self {
            weight,
            size,
            tabular: false,
        }
    }

    pub(super) const fn tabular(self) -> Self {
        Self {
            tabular: true,
            ..self
        }
    }
}

fn font(weight: Weight) -> &'static Font {
    static FONTS: OnceLock<[Font; 2]> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        // The bytes are compiled in; a parse failure is a build defect,
        // caught by `the_embedded_fonts_parse`.
        let load = |bytes: &[u8]| {
            Font::from_bytes(bytes, FontSettings::default()).expect("embedded Inter subset parses")
        };
        [load(REGULAR_TTF), load(SEMIBOLD_TTF)]
    });
    match weight {
        Weight::Regular => &fonts[0],
        Weight::SemiBold => &fonts[1],
    }
}

/// The character actually drawn for `ch`: whitespace and control
/// characters as a space, anything the subset lacks as [`FALLBACK`].
fn drawn(font: &Font, ch: char) -> char {
    if ch.is_whitespace() || ch.is_control() {
        ' '
    } else if font.has_glyph(ch) {
        ch
    } else {
        FALLBACK
    }
}

/// The widest digit's advance at `px`.
fn digit_cell(font: &Font, px: f32) -> f32 {
    ('0'..='9')
        .map(|d| font.metrics(d, px).advance_width)
        .fold(0.0, f32::max)
}

/// Advance of one drawn character at `px` (physical or logical: fontdue
/// does not hint, so advances scale linearly).
fn advance(font: &Font, ch: char, px: f32, tabular: bool) -> f32 {
    if tabular && ch.is_ascii_digit() {
        digit_cell(font, px)
    } else {
        font.metrics(ch, px).advance_width
    }
}

/// Width of `text` in logical px.
pub(super) fn measure(text: &str, style: TextStyle) -> f32 {
    let font = font(style.weight);
    text.chars()
        .map(|ch| advance(font, drawn(font, ch), style.size, style.tabular))
        .sum()
}

/// `text` cut to fit `max_width` logical px, with an ellipsis where it was
/// cut (trailing spaces dropped before it). Unchanged when it fits.
pub(super) fn ellipsize(text: &str, style: TextStyle, max_width: f32) -> Cow<'_, str> {
    if measure(text, style) <= max_width {
        return Cow::Borrowed(text);
    }
    let font = font(style.weight);
    let room = max_width - advance(font, ELLIPSIS, style.size, false);
    let mut width = 0.0;
    let mut end = 0;
    for (i, ch) in text.char_indices() {
        width += advance(font, drawn(font, ch), style.size, style.tabular);
        if width > room {
            break;
        }
        end = i + ch.len_utf8();
    }
    let mut cut = text[..end].trim_end().to_string();
    cut.push(ELLIPSIS);
    Cow::Owned(cut)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    ch: char,
    weight: Weight,
    size: u32,
    scale: u32,
}

/// A rasterized glyph: coverage rows top to bottom, placed relative to the
/// pen position on the baseline.
struct Glyph {
    xmin: i32,
    /// Bottom of the bitmap above the baseline (fontdue's `ymin`).
    ymin: i32,
    width: usize,
    height: usize,
    coverage: Vec<u8>,
}

thread_local! {
    static GLYPHS: RefCell<HashMap<Key, Glyph>> = RefCell::new(HashMap::new());
}

/// Run `f` on the cached glyph for `ch` (already [`drawn`]).
fn with_glyph<R>(
    cache: &mut HashMap<Key, Glyph>,
    ch: char,
    style: TextStyle,
    scale: f32,
    f: impl FnOnce(&Glyph) -> R,
) -> R {
    let key = Key {
        ch,
        weight: style.weight,
        size: style.size.to_bits(),
        scale: scale.to_bits(),
    };
    if cache.len() >= CACHE_LIMIT && !cache.contains_key(&key) {
        cache.clear();
    }
    let glyph = cache.entry(key).or_insert_with(|| {
        let (m, coverage) = font(style.weight).rasterize(ch, style.size * scale);
        Glyph {
            xmin: m.xmin,
            ymin: m.ymin,
            width: m.width,
            height: m.height,
            coverage,
        }
    });
    f(glyph)
}

/// Draw `text` with its left end at `at.0` and its baseline at `at.1`
/// (logical px) in `rgb` at `alpha`. Returns its width in logical px.
pub(super) fn draw(
    pixmap: &mut Pixmap,
    text: &str,
    style: TextStyle,
    at: (f32, f32),
    rgb: [u8; 3],
    alpha: f32,
    scale: f32,
) -> f32 {
    let font = font(style.weight);
    let px = style.size * scale;
    let cell = style.tabular.then(|| digit_cell(font, px));
    let baseline = (at.1 * scale).round() as i32;
    let start = (at.0 * scale).round();
    let mut pen = start;
    GLYPHS.with_borrow_mut(|cache| {
        for ch in text.chars() {
            let ch = drawn(font, ch);
            let own = font.metrics(ch, px).advance_width;
            // A tabular digit sits centered in the widest digit's cell.
            let (offset, step) = match cell {
                Some(cell) if ch.is_ascii_digit() => ((cell - own) / 2.0, cell),
                _ => (0.0, own),
            };
            if alpha > 0.0 && ch != ' ' {
                let x = (pen + offset).round() as i32;
                with_glyph(cache, ch, style, scale, |g| {
                    blit(pixmap, g, x, baseline, rgb, alpha)
                });
            }
            pen += step;
        }
    });
    (pen - start) / scale
}

/// Draw one character with its ink box centered on `center` (logical px):
/// a symbol inside a shape, not text on a baseline.
pub(super) fn draw_centered(
    pixmap: &mut Pixmap,
    ch: char,
    style: TextStyle,
    center: (f32, f32),
    rgb: [u8; 3],
    alpha: f32,
    scale: f32,
) {
    if alpha <= 0.0 {
        return;
    }
    let ch = drawn(font(style.weight), ch);
    GLYPHS.with_borrow_mut(|cache| {
        with_glyph(cache, ch, style, scale, |g| {
            let x = (center.0 * scale - g.width as f32 / 2.0).round() as i32;
            let top = (center.1 * scale - g.height as f32 / 2.0).round() as i32;
            let baseline = top + g.height as i32 + g.ymin;
            blit(pixmap, g, x - g.xmin, baseline, rgb, alpha);
        })
    });
}

/// Blend a glyph's coverage onto the pixmap (premultiplied RGBA,
/// source-over) with its pen at `x` on `baseline`, physical px.
fn blit(pixmap: &mut Pixmap, glyph: &Glyph, x: i32, baseline: i32, rgb: [u8; 3], alpha: f32) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let left = x + glyph.xmin;
    let top = baseline - glyph.ymin - glyph.height as i32;
    let alpha = alpha.clamp(0.0, 1.0);
    let data = pixmap.data_mut();
    for gy in 0..glyph.height {
        let py = top + gy as i32;
        if !(0..ph).contains(&py) {
            continue;
        }
        for gx in 0..glyph.width {
            let px = left + gx as i32;
            let c = glyph.coverage[gy * glyph.width + gx];
            if c == 0 || !(0..pw).contains(&px) {
                continue;
            }
            let a = c as f32 / 255.0 * alpha;
            let keep = 1.0 - a;
            let i = (py * pw + px) as usize * 4;
            for (ch, src) in rgb.iter().enumerate() {
                data[i + ch] = (*src as f32 * a + data[i + ch] as f32 * keep).round() as u8;
            }
            data[i + 3] = (255.0 * a + data[i + 3] as f32 * keep).round() as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: TextStyle = TextStyle::new(Weight::Regular, 12.0);

    #[test]
    fn the_embedded_fonts_parse() {
        for weight in [Weight::Regular, Weight::SemiBold] {
            let font = font(weight);
            for ch in (' '..='~').chain(['\u{b7}', ELLIPSIS]) {
                assert!(font.has_glyph(ch) || ch == ' ', "{weight:?} lacks {ch:?}");
            }
        }
    }

    #[test]
    fn semibold_is_wider_and_size_scales_width() {
        let regular = measure("Reading files", BODY);
        let bold = measure("Reading files", TextStyle::new(Weight::SemiBold, 12.0));
        assert!(bold > regular, "{bold} vs {regular}");
        let double = measure("Reading files", TextStyle::new(Weight::Regular, 24.0));
        assert!(
            (double - 2.0 * regular).abs() < 0.01,
            "{double} vs {regular}"
        );
    }

    #[test]
    fn tabular_digits_share_one_width() {
        let style = BODY.tabular();
        assert_eq!(measure("1:11", style), measure("0:00", style));
        assert!(measure("1:11", BODY) < measure("0:00", BODY));
    }

    #[test]
    fn ellipsize_cuts_to_fit() {
        assert!(matches!(
            ellipsize("short", BODY, 100.0),
            Cow::Borrowed("short")
        ));
        let long = "Searching the code for the overlay renderer";
        let cut = ellipsize(long, BODY, 80.0);
        assert!(cut.ends_with(ELLIPSIS), "{cut}");
        assert!(measure(&cut, BODY) <= 80.0, "{cut}");
        assert!(cut.len() > 4, "{cut}");
        // No space left dangling before the ellipsis.
        assert!(!cut.trim_end_matches(ELLIPSIS).ends_with(' '), "{cut}");
    }

    #[test]
    fn missing_glyphs_fall_back() {
        assert_eq!(measure("\u{e9}", BODY), measure("?", BODY));
        assert_eq!(measure("a\tb", BODY), measure("a b", BODY));
    }

    #[test]
    fn draw_paints_coverage_in_the_color() {
        // Large enough that the stems cover whole pixels (unhinted 12 px
        // stems are about 1.1 px wide and straddle two).
        let big = TextStyle::new(Weight::Regular, 24.0);
        let mut pm = Pixmap::new(80, 32).unwrap();
        let w = draw(&mut pm, "Hi", big, (2.0, 26.0), [232, 234, 237], 1.0, 1.0);
        assert!((w - measure("Hi", big)).abs() < 1.0, "{w}");
        let px = pm.data().as_chunks::<4>().0;
        // Fully covered stems take the exact color.
        assert!(px.contains(&[232, 234, 237, 255]));
        // Nothing above the cap height.
        let top = &pm.data()[..80 * 4 * 6];
        assert!(top.iter().all(|b| *b == 0));
        // Half alpha: never more than half opaque.
        let mut half = Pixmap::new(80, 32).unwrap();
        draw(&mut half, "Hi", big, (2.0, 26.0), [232, 234, 237], 0.5, 1.0);
        let max_a = half.data().as_chunks::<4>().0.iter().map(|p| p[3]).max();
        assert_eq!(max_a, Some(128));
    }

    #[test]
    fn centered_glyphs_sit_on_the_center() {
        let mut pm = Pixmap::new(40, 40).unwrap();
        let style = TextStyle::new(Weight::SemiBold, 20.0);
        draw_centered(&mut pm, '!', style, (20.0, 20.0), [255, 138, 61], 1.0, 1.0);
        let (mut x0, mut x1, mut y0, mut y1) = (40, 0, 40, 0);
        for (i, p) in pm.data().as_chunks::<4>().0.iter().enumerate() {
            if p[3] > 0 {
                let (x, y) = (i % 40, i / 40);
                (x0, x1, y0, y1) = (x0.min(x), x1.max(x), y0.min(y), y1.max(y));
            }
        }
        let (cx, cy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
        assert!(
            (cx - 19.5).abs() <= 1.0 && (cy - 19.5).abs() <= 1.0,
            "{cx},{cy}"
        );
    }
}
