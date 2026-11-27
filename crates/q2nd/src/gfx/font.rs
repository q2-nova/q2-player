//! Text: Open Sans for Latin, with fallback fonts for anything it lacks (on
//! the Q2, the stock firmware's own multilingual font).

use super::{Canvas, Color};
use ab_glyph::{Font as _, FontRef, FontVec, GlyphId};
#[cfg(test)]
use fontdue::{Font, FontSettings};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// One owned startup task on the finite local job pool. Early platform
/// errors wait for it too: font reads must not outlive startup or be
/// detached during a card/service handoff.
pub struct FontLoad {
    result: Option<std::sync::mpsc::Receiver<std::thread::Result<Result<Fonts, String>>>>,
}

impl FontLoad {
    pub fn start(fallbacks: Vec<PathBuf>) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        crate::rt::jobs().run_isolated(move |blocking| {
            let refs: Vec<_> = fallbacks.iter().map(PathBuf::as_path).collect();
            let _ = tx.send(std::panic::catch_unwind(|| Fonts::load(blocking, &refs)));
        });
        Self { result: Some(rx) }
    }

    pub fn finish(mut self) -> Result<Fonts, String> {
        let rx = self.result.take().expect("font load started");
        match rx.recv() {
            Ok(Ok(fonts)) => fonts,
            Ok(Err(panic)) => std::panic::resume_unwind(panic),
            Err(_) => panic!("font load ended without a result"),
        }
    }
}

impl Drop for FontLoad {
    fn drop(&mut self) {
        // A platform failure remains the primary error. finish() propagates a
        // font panic on the successful path; Drop must not double-panic.
        if let Some(rx) = self.result.take() { let _ = rx.recv(); }
    }
}

#[cfg(any(test, target_arch = "wasm32"))]
const REGULAR: &[u8] = include_bytes!("../../../../assets/fonts/OpenSans-Regular.ttf");
#[cfg(any(test, target_arch = "wasm32"))]
const SEMIBOLD: &[u8] = include_bytes!("../../../../assets/fonts/OpenSans-SemiBold.ttf");
#[cfg(any(test, target_arch = "wasm32"))]
const BOLD: &[u8] = include_bytes!("../../../../assets/fonts/OpenSans-Bold.ttf");
const PRIMARY_NAMES: [&str; 3] = ["OpenSans-Regular.ttf", "OpenSans-SemiBold.ttf", "OpenSans-Bold.ttf"];

const CACHE_LIMIT: usize = 4096;

/// Default-ignorable format characters: zero width, never a box.
fn invisible(ch: char) -> bool {
    matches!(ch, '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{2064}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0020}'..='\u{E007F}')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weight {
    Regular,
    Semibold,
    Bold,
}

struct Glyph {
    width: usize,
    height: usize,
    /// Bytes from one mask row to the next: `width` rounded up to four, the
    /// extra bytes zero, so spans blend whole groups of four pixels.
    stride: usize,
    xmin: i32,
    ymin: i32,
    #[cfg(test)]
    advance: f32,
    mask: Vec<u8>,
}

struct Layout {
    width: i32,
    glyphs: Vec<(f32, (usize,u16))>,
    /// UTF-8 prefix boundaries and their rounded advance, for clipping.
    ends: Vec<(usize, i32)>,
}

pub struct Fonts {
    /// 0: regular, 1: semibold, 2: bold.
    fonts: Vec<FontVec>,
    /// Raw font bytes, with outlines decoded only for requested glyphs.
    /// Eagerly expanding stock's 9.4 MB CJK font exceeded the Q2's RAM.
    /// Mapped from their files (`resident::map_file`): file cache, not heap.
    fallbacks: Vec<FontRef<'static>>,
    cache: HashMap<(usize, u16, u16), Glyph>,
    glyph_order: VecDeque<(usize, u16, u16)>,
    ids: HashMap<(char, Weight), (usize, u16)>,
    layouts: HashMap<(u16,Weight),HashMap<String,std::sync::Arc<Layout>>>,
    layout_order: VecDeque<(u16,Weight,String)>,
    /// Headless scripts only: every string drawn while set.
    capture: Option<Vec<String>>,
}

impl Fonts {
    /// Embedded primary fonts for the browser and test fixtures.
    #[cfg(any(test, target_arch = "wasm32"))]
    pub fn bundled() -> Fonts {
        let fonts = [REGULAR, SEMIBOLD, BOLD].into_iter()
            .map(|b| FontVec::try_from_vec(b.to_vec()).expect("built-in font")).collect();
        Self::from_fonts(fonts)
    }

    fn from_fonts(fonts: Vec<FontVec>) -> Self {
        Fonts { fonts, fallbacks: Vec::new(), cache: HashMap::new(), glyph_order: VecDeque::new(), ids: HashMap::new(), layouts: HashMap::new(), layout_order: VecDeque::new(), capture: None }
    }

    /// Load mandatory primary fonts and optional fallbacks on the finite owner.
    pub fn load(blocking: &crate::rt::Blocking, fallbacks: &[&Path]) -> Result<Fonts, String> {
        Self::load_from(blocking, &crate::resources::directory("fonts"), fallbacks)
    }

    fn load_from(blocking: &crate::rt::Blocking, directory: &Path, fallbacks: &[&Path]) -> Result<Fonts, String> {
        // Keep compact font tables; rasterize only glyphs actually drawn.
        let primary = PRIMARY_NAMES.into_iter().map(|name| {
            let bytes = crate::resources::read(blocking, &directory.join(name), 2 * 1024 * 1024)
                .map_err(|_| format!("Required font {name} is unavailable or too large"))?;
            let font = FontVec::try_from_vec(bytes).map_err(|_| format!("Required font {name} is invalid"))?;
            if font.units_per_em().is_none() { return Err(format!("Required font {name} has invalid units per em")); }
            Ok(font)
        }).collect::<Result<Vec<_>, String>>()?;
        let mut fonts = Self::from_fonts(primary);
        let mut lazy = Vec::new();
        for path in fallbacks {
            match crate::platform::resident::map_file(blocking, path, 16 * 1024 * 1024).map(FontRef::try_from_slice) {
                Ok(Ok(f)) if f.units_per_em().is_some() => lazy.push(f),
                Ok(Ok(_)) => log::warn!("font {}: invalid units per em", path.display()),
                Ok(Err(e)) => log::warn!("font {}: {e}", path.display()),
                Err(_) => {}
            }
        }
        fonts.fallbacks = lazy;
        Ok(fonts)
    }

    fn pick(&mut self, ch: char, weight: Weight) -> (usize, u16) {
        if let Some(id) = self.ids.get(&(ch,weight)) { return *id; }
        let id = self.lookup(ch,weight);
        if self.ids.len() >= CACHE_LIMIT { self.ids.clear(); }
        self.ids.insert((ch,weight),id);
        id
    }

    fn lookup(&self, ch: char, weight: Weight) -> (usize, u16) {
        let first = weight as usize;
        let idx = self.fonts[first].glyph_id(ch).0;
        if idx != 0 || ch.is_whitespace() {
            return (first, idx);
        }
        for (i, f) in self.fallbacks.iter().enumerate() {
            let idx = f.glyph_id(ch).0;
            if idx != 0 {
                return (i + self.fonts.len(), idx);
            }
        }
        // Joiners and variation selectors inside emoji sequences draw
        // nothing; without a font that maps them, use the zero-width space.
        if invisible(ch) {
            let zw = self.fonts[first].glyph_id('\u{200B}').0;
            if zw != 0 { return (first, zw); }
        }
        (first, 0)
    }

    fn glyph(&mut self, font: usize, index: u16, size: u16) -> &Glyph {
        let key = (font, index, size);
        // The common hit needs only the entry lookup below. Check for a
        // victim only at capacity; never flush every glyph at once.
        if self.cache.len() >= CACHE_LIMIT && !self.cache.contains_key(&key) {
            if let Some(old) = self.glyph_order.pop_front() { self.cache.remove(&old); }
        }
        let order = &mut self.glyph_order;
        let fonts = &self.fonts;
        let fallbacks = &self.fallbacks;
        self.cache.entry(key).or_insert_with(|| {
            order.push_back(key);
            if font >= fonts.len() {
                return rasterize_fallback(&fallbacks[font - fonts.len()], index, size);
            }
            rasterize_fallback(&fonts[font], index, size)
        })
    }

    fn font(&self, index: usize) -> &dyn ab_glyph::Font {
        if index < self.fonts.len() { &self.fonts[index] } else { &self.fallbacks[index-self.fonts.len()] }
    }

    fn advance(&self, font:usize, glyph:u16, size:u16) -> f32 {
        let f=self.font(font);
        f.h_advance_unscaled(GlyphId(glyph))*size as f32/f.units_per_em().unwrap()
    }

    /// Distance from a line's top to its baseline.
    pub fn ascent(&self, size:u16) -> i32 {
        (self.fonts[0].ascent_unscaled()*size as f32/self.fonts[0].units_per_em().unwrap()).round() as i32
    }

    pub fn line_height(&self, size:u16) -> i32 {
        (self.fonts[0].height_unscaled()*size as f32/self.fonts[0].units_per_em().unwrap()).round() as i32
    }

    /// Lays out `text`, calling `f(x, glyph-key)` for each glyph; returns the
    /// advance.
    fn layout(&mut self, text: &str, size: u16, weight: Weight, mut f: impl FnMut(&mut Self, f32, (usize, u16))) -> f32 {
        let mut x = 0.0f32;
        let mut prev: Option<(usize, u16)> = None;
        for ch in text.chars() {
            let (font, idx) = self.pick(ch, weight);
            if let Some((pf, pi)) = prev {
                if pf == font {
                    let f=self.font(font);
                    x += f.kern_unscaled(GlyphId(pi),GlyphId(idx))*size as f32/f.units_per_em().unwrap();
                }
            }
            f(self, x, (font, idx));
            x += self.advance(font, idx, size);
            prev = Some((font, idx));
        }
        x
    }

    fn shaped(&mut self, text: &str, size: u16, weight: Weight) -> std::sync::Arc<Layout> {
        if let Some(l) = self.layouts.get(&(size,weight)).and_then(|m| m.get(text)) { return l.clone(); }
        let mut glyphs=Vec::with_capacity(text.len().min(256));
        let mut ends=Vec::with_capacity(text.len().min(256));
        let mut boundaries=text.char_indices().map(|(i,c)| i+c.len_utf8());
        let width=self.layout(text,size,weight,|fonts,x,key| {
            glyphs.push((x,key));
            ends.push((boundaries.next().unwrap(), (x+fonts.advance(key.0,key.1,size)).ceil() as i32));
        }).ceil() as i32;
        let shaped=std::sync::Arc::new(Layout { width,glyphs,ends });
        // Bounded independently of library size; giant server strings aren't cached.
        if text.len() <= 512 {
            if self.layout_order.len() >= 128 {
                if let Some((size,weight,text))=self.layout_order.pop_front() {
                    if let Some(group)=self.layouts.get_mut(&(size,weight)) { group.remove(&text); }
                }
            }
            self.layouts.entry((size,weight)).or_default().insert(text.to_string(),shaped.clone());
            self.layout_order.push_back((size,weight,text.to_string()));
        }
        shaped
    }

    pub fn measure(&mut self, text: &str, size: u16, weight: Weight) -> i32 {
        self.shaped(text,size,weight).width
    }

    /// Draws `text` with its line's top at `y`; returns the width drawn.
    pub fn draw(&mut self, c: &mut Canvas, text: &str, x: i32, y: i32, size: u16, weight: Weight, color: Color) -> i32 {
        if let Some(capture) = &mut self.capture { if !text.is_empty() { capture.push(text.to_owned()); } }
        let baseline = y + self.ascent(size);
        let layout=self.shaped(text,size,weight);
        for &(gx,(font,idx)) in &layout.glyphs {
            let g = self.glyph(font, idx, size);
            if g.width > 0 {
                let px = x + (gx.round() as i32) + g.xmin;
                let py = baseline - g.height as i32 - g.ymin;
                c.draw_mask_rows(&g.mask, g.stride, true, g.width, g.height, px, py, color);
            }
        }
        layout.width
    }

    /// Starts (`true`) or ends recording drawn strings; returns those recorded.
    pub fn capture(&mut self, on: bool) -> Vec<String> {
        let taken = self.capture.take().unwrap_or_default();
        if on { self.capture = Some(Vec::new()); }
        taken
    }

    /// `text` cut to fit `max_w` pixels, ending in "…" when cut.
    pub fn ellipsize<'a>(&mut self, text: &'a str, size: u16, weight: Weight, max_w: i32) -> Cow<'a, str> {
        let layout = self.shaped(text, size, weight);
        if layout.width <= max_w {
            return Cow::Borrowed(text);
        }
        let ell = self.measure("…", size, weight);
        if ell > max_w { return Cow::Borrowed(""); }
        let mut end = 0;
        // Reuse the cached shaping work, including kerning and UTF-8
        // boundaries. Narrow rows must not lay out the same title each frame.
        for &(next, advance) in &layout.ends {
            if advance + ell <= max_w { end=next; }
        }
        Cow::Owned(format!("{}…", text[..end].trim_end()))
    }

    /// Draws `text` cut to `max_w`.
    pub fn draw_fit(
        &mut self,
        c: &mut Canvas,
        text: &str,
        x: i32,
        y: i32,
        max_w: i32,
        size: u16,
        weight: Weight,
        color: Color,
    ) -> i32 {
        let t = self.ellipsize(text, size, weight, max_w);
        self.draw(c, &t, x, y, size, weight, color)
    }

    /// Draws `text` centred on `cx`.
    pub fn draw_centered(&mut self, c: &mut Canvas, text: &str, cx: i32, y: i32, size: u16, weight: Weight, color: Color) {
        let w = self.measure(text, size, weight);
        self.draw(c, text, cx - w / 2, y, size, weight, color);
    }

    /// Draws `text` ending at `right`.
    pub fn draw_right(&mut self, c: &mut Canvas, text: &str, right: i32, y: i32, size: u16, weight: Weight, color: Color) {
        let w = self.measure(text, size, weight);
        self.draw(c, text, right - w, y, size, weight, color);
    }
}

fn rasterize_fallback<F: ab_glyph::Font>(font: &F, index: u16, size: u16) -> Glyph {
    let id = GlyphId(index);
    #[cfg(test)]
    let scale = size as f32 / font.units_per_em().unwrap();
    let mut glyph = Glyph {
        width: 0, height: 0, stride: 0, xmin: 0, ymin: 0,
        #[cfg(test)]
        advance: font.h_advance_unscaled(id) * scale,
        mask: Vec::new(),
    };
    // ab_glyph scales by ascender-to-descender height; fontdue uses pixels
    // per em. Convert so fallback sizes and the shared baseline stay intact.
    if let Some(outline) = font.outline_glyph(id.with_scale(size as f32 * font.height_unscaled() / font.units_per_em().unwrap())) {
        let bounds = outline.px_bounds();
        glyph.width = bounds.width() as usize;
        glyph.height = bounds.height() as usize;
        glyph.xmin = bounds.min.x as i32;
        glyph.ymin = -bounds.max.y as i32;
        glyph.stride = (glyph.width + 3) & !3;
        glyph.mask = vec![0; glyph.stride * glyph.height];
        outline.draw(|x, y, coverage| {
            glyph.mask[y as usize * glyph.stride + x as usize] = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
        });
    }
    glyph
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_fonts_preserve_pixels_and_refuse_missing_corrupt_or_overlong_primary() {
        let root = crate::library::generated_test_directory("packaged-fonts");
        std::fs::create_dir_all(&root).unwrap();
        for (name, bytes) in PRIMARY_NAMES.into_iter().zip([REGULAR, SEMIBOLD, BOLD]) {
            std::fs::write(root.join(name), bytes).unwrap();
        }
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut actual = Fonts::load_from(&blocking, &root, &[]).unwrap();
        let mut reference = Fonts::bundled();
        for weight in [Weight::Regular, Weight::Semibold, Weight::Bold] {
            let mut a = Canvas::new(375, 320);
            let mut b = Canvas::new(375, 320);
            let text = "AV café — Generated 0123";
            assert_eq!(actual.draw(&mut a, text, 5, 5, 20, weight, Color::WHITE),
                reference.draw(&mut b, text, 5, 5, 20, weight, Color::WHITE));
            assert_eq!(a.pixels, b.pixels);
        }
        let primary = root.join(PRIMARY_NAMES[1]);
        std::fs::remove_file(&primary).unwrap();
        assert!(Fonts::load_from(&blocking, &root, &[]).is_err());
        std::fs::write(&primary, b"invalid generated font").unwrap();
        assert!(Fonts::load_from(&blocking, &root, &[]).is_err());
        std::fs::File::create(&primary).unwrap().set_len(2 * 1024 * 1024 + 1).unwrap();
        assert!(Fonts::load_from(&blocking, &root, &[]).is_err());
        assert!(crate::resources::read(&blocking, Path::new("/dev/null"), 16).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn background_fonts_preserve_pixels_and_fallback_order() {
        let dir = crate::library::generated_test_directory("font-startup");
        std::fs::create_dir_all(&dir).unwrap();
        let fallback = dir.join("generated.ttf");
        let invalid = dir.join("invalid.ttf");
        std::fs::write(&fallback, BOLD).unwrap();
        std::fs::write(&invalid, b"invalid generated font").unwrap();
        let paths = vec![dir.join("missing.ttf"), invalid, fallback];
        let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
        let mut expected = Fonts::load(&crate::rt::Blocking::dedicated_thread(), &refs).unwrap();
        let mut actual = FontLoad::start(paths).finish().unwrap();
        assert_eq!(actual.fallbacks.len(), 1);
        for weight in [Weight::Regular, Weight::Semibold, Weight::Bold] {
            let mut a = Canvas::new(375, 320);
            let mut b = Canvas::new(375, 320);
            let text = "AV café — 東京 🎵";
            assert_eq!(expected.draw(&mut a, text, 5, 5, 20, weight, Color::WHITE),
                actual.draw(&mut b, text, 5, 5, 20, weight, Color::WHITE));
            assert_eq!(a.pixels, b.pixels);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg_attr(panic = "abort", ignore = "recovers from a panic; a panic = \"abort\" build, as on the Q2, aborts")]
    fn early_drop_joins_font_worker_and_finish_propagates_panic() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        // A load whose result arrives through the same channel the pool job uses.
        fn load_from(work: impl FnOnce() -> Fonts + Send + 'static) -> FontLoad {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || { let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map(Ok)); });
            FontLoad { result: Some(rx) }
        }
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let load = load_from(move || {
            wait.recv().unwrap();
            let fonts = Fonts::bundled();
            done.store(true, Ordering::Release);
            fonts
        });
        release.send(()).unwrap();
        drop(load);
        assert!(finished.load(Ordering::Acquire));
        let load = load_from(|| panic!("generated font worker panic"));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load.finish())).is_err());
        // An already-failing startup must not double-panic in Drop.
        drop(load_from(|| panic!("generated font worker panic")));
        // The real start: a pool job.
        let fonts = FontLoad::start(Vec::new()).finish().unwrap();
        drop(fonts);
    }

    #[test]
    fn cached_clipping_matches_layout_for_utf8_kerning_and_whitespace() {
        let mut fonts = Fonts::bundled();
        for text in ["AVATAR Tomorrow's Music", "Café — 東京 🎵", "a  b   ", "", "WiiiiWWW"] {
            for weight in [Weight::Regular, Weight::Semibold, Weight::Bold] {
                for max_w in -2..300 {
                    let width = fonts.measure(text, 20, weight);
                    let ell = fonts.measure("…", 20, weight);
                    let expected = if width <= max_w { text.to_string() }
                    else if ell > max_w { String::new() }
                    else {
                        let mut ends = text.char_indices().map(|(i,c)|i+c.len_utf8());
                        let mut end = 0;
                        fonts.layout(text, 20, weight, |f,x,(font,index)| {
                            let next = ends.next().unwrap();
                            if (x+f.advance(font,index,20)).ceil() as i32 + ell <= max_w { end=next; }
                        });
                        format!("{}…", text[..end].trim_end())
                    };
                    assert_eq!(fonts.ellipsize(text, 20, weight, max_w), expected);
                }
            }
        }
    }

    #[test]
    fn measures_and_cuts() {
        let mut f = Fonts::bundled();
        let w = f.measure("Hello", 20, Weight::Regular);
        assert!(w > 30 && w < 70, "{w}");
        assert!(f.measure("Hello", 20, Weight::Semibold) >= w);
        let cut = f.ellipsize("A very long album title indeed", 20, Weight::Regular, 100);
        assert!(cut.ends_with('…'));
        assert!(f.measure(&cut, 20, Weight::Regular) <= 100);
        assert_eq!(f.ellipsize("Hi", 20, Weight::Regular, 100), "Hi");
    }

    #[test]
    fn emoji_render_from_the_emoji_fallback_and_joiners_take_no_space() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts/noto-emoji/NotoEmoji-Regular.ttf");
        let mut f = Fonts::load(&crate::rt::Blocking::dedicated_thread(), &[path.as_path()]).unwrap();
        // A test example: ice-cream emoji around a playlist name.
        for ch in ['🍦', '😀', '❤'] {
            let (font, idx) = f.pick(ch, Weight::Regular);
            assert!(idx != 0 && font >= 3, "{ch} from the emoji font");
            let glyph = f.glyph(font, idx, 20);
            assert!(glyph.mask.iter().any(|&p| p > 0), "{ch} renders ink");
            assert!(glyph.advance > 0.0 && glyph.advance <= 30.0, "{ch} spacing");
        }
        assert_eq!(f.pick('d', Weight::Regular).0, 0, "Latin stays Open Sans");
        let plain = f.measure("🍦 de la 🍦", 20, Weight::Regular);
        let joined = f.measure("🍦\u{FE0F} de la 🍦\u{200D}", 20, Weight::Regular);
        assert_eq!(plain, joined, "selectors and joiners add no width");
        // Without any fallback font, a joiner is still invisible, not a box.
        let mut bare = Fonts::bundled();
        assert_eq!(bare.measure("a\u{200D}b", 20, Weight::Regular), bare.measure("ab", 20, Weight::Regular));
    }

    /// The stock firmware's font fills in what Open Sans lacks: set
    /// Q2ND_STOCK_FONT to release/assets/default/raw/fonts/default.ttf.
    #[test]
    #[ignore = "needs the stock firmware font in Q2ND_STOCK_FONT"]
    fn stock_font_covers_cjk() {
        let path = std::env::var("Q2ND_STOCK_FONT").expect("Q2ND_STOCK_FONT");
        let mut f = Fonts::load(&crate::rt::Blocking::dedicated_thread(), &[Path::new(&path)]).unwrap();
        for ch in "坂本龍一 東京事変 사랑".chars().filter(|c| !c.is_whitespace()) {
            let (font, idx) = f.pick(ch, Weight::Bold);
            assert!(idx != 0 && font >= 3, "{ch} from the stock font");
            let glyph = f.glyph(font, idx, 20);
            assert!(glyph.mask.iter().any(|&p| p > 0), "{ch} renders ink");
            assert!(glyph.advance > 0.0 && glyph.advance <= 25.0, "{ch} has sensible spacing");
            assert!(glyph.width <= 30 && glyph.height <= 30, "{ch} fits a text row");
        }
        let (font, _) = f.pick('A', Weight::Bold);
        assert_eq!(font, 2, "Latin stays Open Sans");
        // Run this ignored hardware-font test on its own. The previous
        // eager parser peaked at 172 MiB here, above all the Q2's RAM.
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            let peak: u64 = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse().ok()).unwrap();
            assert!(peak < 64 * 1024, "font load/render peak {peak} KiB exceeds budget");
        }
    }

    #[test]
    fn lazy_fallback_preserves_scale_baseline_and_spacing() {
        let lazy = FontVec::try_from_vec(REGULAR.to_vec()).unwrap();
        let eager = Font::from_bytes(REGULAR, FontSettings::default()).unwrap();
        for ch in "Agjpé中 ".chars() {
            let idx = eager.lookup_glyph_index(ch);
            for size in [13, 17, 24, 40] {
                let old = eager.metrics_indexed(idx, size as f32);
                let new = rasterize_fallback(&lazy, idx, size);
                assert!((new.advance - old.advance_width).abs() < 0.01, "{ch} at {size}");
                // Rasterizers may round an outline edge differently by a pixel.
                assert!((new.xmin - old.xmin).abs() <= 1);
                assert!((new.ymin - old.ymin).abs() <= 1);
                assert!((new.width as i32 - old.width as i32).abs() <= 1);
                assert!((new.height as i32 - old.height as i32).abs() <= 1);
            }
        }
    }

    #[test]
    fn draws_ink() {
        let mut f = Fonts::bundled();
        let mut c = Canvas::new(100, 40);
        f.draw(&mut c, "Kid A", 2, 2, 20, Weight::Regular, Color::WHITE);
        assert!(c.pixels.iter().any(|&p| p != 0));
    }
    #[test]
    fn cached_layout_is_pixel_identical_and_bounded() {
        let mut f=Fonts::bundled();
        for text in ["AVATAR — Tomorrow's Music", "éàñ 012345", "", "jiffy"] {
            let mut a=Canvas::new(375,80);let mut b=Canvas::new(375,80);
            f.draw(&mut a,text,5,5,20,Weight::Bold,Color::WHITE);
            let baseline=5+f.ascent(20);
            f.layout(text,20,Weight::Bold,|fonts,x,(font,index)| {
                let g=fonts.glyph(font,index,20);
                b.draw_mask_rows(&g.mask,g.stride,false,g.width,g.height,5+x.round() as i32+g.xmin,baseline-g.height as i32-g.ymin,Color::WHITE);
            });
            assert_eq!(a.pixels,b.pixels);
        }
        for i in 0..512 {f.measure(&format!("Album {i}"),20,Weight::Bold);}
        assert!(f.layouts.values().map(|m|m.len()).sum::<usize>()<=128);
    }

}
