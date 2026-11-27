#![forbid(unsafe_code)]
//! Software drawing on a 32-bit canvas. Pixels are `0x00RRGGBB`, the layout of
//! the Q2's framebuffer words (red at bit 16) and of the simulator's window.

pub mod font;
pub mod image;
#[cfg(test)]
mod image_damage_tests;
pub mod qr;
pub(crate) mod resize;
pub mod span;
#[cfg(test)]
mod reference;
#[allow(dead_code)]
mod qrcodegen;

pub use self::image::Image;
pub use font::{Fonts, Weight};

pub const WIDTH: i32 = 375;
pub const HEIGHT: i32 = 320;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const BLACK: Color = Color(0x000000);
    pub const WHITE: Color = Color(0xFFFFFF);

    pub fn r(self) -> u32 {
        (self.0 >> 16) & 0xFF
    }
    pub fn g(self) -> u32 {
        (self.0 >> 8) & 0xFF
    }
    pub fn b(self) -> u32 {
        self.0 & 0xFF
    }

    /// `self` over `under` at `alpha` (0-255).
    pub fn over(self, under: Color, alpha: u32) -> Color {
        if alpha >= 255 {
            return self;
        }
        if alpha == 0 {
            return under;
        }
        let mix = |a: u32, b: u32| (a * alpha + b * (255 - alpha) + 127) / 255;
        Color(mix(self.r(), under.r()) << 16 | mix(self.g(), under.g()) << 8 | mix(self.b(), under.b()))
    }

    pub fn lerp(self, other: Color, t: f32) -> Color {
        other.over(self, (t.clamp(0.0, 1.0) * 255.0) as u32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect::new(x, y, (r - x).max(0), (b - y).max(0))
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn inset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w - 2 * dx, self.h - 2 * dy)
    }
}

/// The glass hides a circle of this radius at each corner of the panel
/// (Q2 Pod's `CORNER_R`, fitted to the device).
pub const CORNER_R: i32 = 80;

/// Pixels hidden at each end of screen row `y` by the rounded glass.
pub fn corner_inset(y: i32) -> i32 {
    let d = if y < CORNER_R {
        CORNER_R - y
    } else if y >= HEIGHT - CORNER_R {
        y - (HEIGHT - CORNER_R - 1)
    } else {
        return 0;
    };
    let d = d as f32;
    let r = CORNER_R as f32;
    (r - (r * r - d * d).max(0.0).sqrt()).ceil() as i32
}

/// Paints what the glass's rounded corners hide, for the simulator and
/// screenshots.
pub fn mask_corners(frame: &mut [u32]) {
    for y in 0..HEIGHT {
        let inset = corner_inset(y);
        for x in 0..inset.min(WIDTH) {
            frame[(y * WIDTH + x) as usize] = 0x2A2A2A;
            frame[(y * WIDTH + WIDTH - 1 - x) as usize] = 0x2A2A2A;
        }
    }
}

pub struct Canvas {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u32>,
    clip: Rect,
}

impl Canvas {
    pub fn new(width: i32, height: i32) -> Canvas {
        Canvas { width, height, pixels: vec![0; (width * height) as usize], clip: Rect::new(0, 0, width, height) }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Restricts drawing to `r` (within the canvas); returns the old clip.
    pub fn set_clip(&mut self, r: Rect) -> Rect {
        let old = self.clip;
        self.clip = r.intersect(&self.bounds());
        old
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    pub fn clear(&mut self, c: Color) {
        self.pixels.fill(c.0);
    }

    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Color, alpha: u32) {
        let cl = &self.clip;
        if x < cl.x || y < cl.y || x >= cl.right() || y >= cl.bottom() || alpha == 0 {
            return;
        }
        let i = (y * self.width + x) as usize;
        self.pixels[i] = c.over(Color(self.pixels[i]), alpha).0;
    }

    #[cfg(test)]
    pub fn get(&self, x: i32, y: i32) -> Color {
        Color(self.pixels[(y * self.width + x) as usize])
    }

    pub fn fill_rect(&mut self, r: Rect, c: Color) {
        let r = r.intersect(&self.clip);
        if r.is_empty() {
            return;
        }
        for y in r.y..r.bottom() {
            let start = (y * self.width + r.x) as usize;
            self.pixels[start..start + r.w as usize].fill(c.0);
        }
    }

    pub fn fill_rect_alpha(&mut self, r: Rect, c: Color, alpha: u32) {
        let r = r.intersect(&self.clip);
        if r.is_empty() || alpha == 0 {
            return;
        }
        if r.w == self.width {
            // Whole rows: one span.
            let start = (r.y * self.width) as usize;
            return span::blend_color(&mut self.pixels[start..start + (r.w * r.h) as usize], c.0, alpha);
        }
        for y in r.y..r.bottom() {
            let start = (y * self.width + r.x) as usize;
            span::blend_color(&mut self.pixels[start..start + r.w as usize], c.0, alpha);
        }
    }

    pub fn vertical_gradient(&mut self, r: Rect, top: Color, bottom: Color) {
        for y in r.y..r.bottom() {
            let t = (y - r.y) as f32 / (r.h - 1).max(1) as f32;
            self.fill_rect(Rect::new(r.x, y, r.w, 1), top.lerp(bottom, t));
        }
    }

    /// Coverage (0-255) of pixel (px, py) by a rounded rectangle.
    fn round_coverage(r: &Rect, radius: f32, px: i32, py: i32) -> u32 {
        let cx = px as f32 + 0.5;
        let cy = py as f32 + 0.5;
        let (x0, y0) = (r.x as f32, r.y as f32);
        let (x1, y1) = (r.right() as f32, r.bottom() as f32);
        let qx = if cx < x0 + radius {
            x0 + radius
        } else if cx > x1 - radius {
            x1 - radius
        } else {
            return 255;
        };
        let qy = if cy < y0 + radius {
            y0 + radius
        } else if cy > y1 - radius {
            y1 - radius
        } else {
            return 255;
        };
        let d = ((cx - qx).powi(2) + (cy - qy).powi(2)).sqrt();
        ((radius - d + 0.5).clamp(0.0, 1.0) * 255.0) as u32
    }

    pub fn fill_round_rect(&mut self, r: Rect, radius: i32, c: Color) {
        self.fill_round_rect_alpha(r, radius, c, 255);
    }

    pub fn fill_round_rect_alpha(&mut self, r: Rect, radius: i32, c: Color, alpha: u32) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        if radius == 0 {
            return self.fill_rect_alpha(r, c, alpha);
        }
        let rad = radius as f32;
        let area = r.intersect(&self.clip);
        if area.is_empty() {
            return;
        }
        // Only the `radius` columns at each end of the `radius` rows at the
        // top and bottom have partial coverage; the rest is one span a row.
        let left_end = (r.x + radius).clamp(area.x, area.right());
        let right_start = (r.right() - radius).clamp(left_end, area.right());
        for y in area.y..area.bottom() {
            let row = (y * self.width) as usize;
            if y >= r.y + radius && y < r.bottom() - radius {
                span::blend_color(&mut self.pixels[row + area.x as usize..row + area.right() as usize], c.0, alpha);
                continue;
            }
            for x in (area.x..left_end).chain(right_start..area.right()) {
                let a = Self::round_coverage(&r, rad, x, y) * alpha / 255;
                if a != 0 {
                    let p = &mut self.pixels[row + x as usize];
                    *p = c.over(Color(*p), a).0;
                }
            }
            span::blend_color(&mut self.pixels[row + left_end as usize..row + right_start as usize], c.0, alpha);
        }
    }

    /// Fills a polygon, anti-aliased by 4x4 supersampling. For icons.
    ///
    /// Each of a pixel row's four sample rows computes where the edges
    /// cross it once; a sample is inside when an odd number of crossings
    /// lie to its right (the even-odd rule, as `point_in_polygon`).
    pub fn fill_polygon(&mut self, pts: &[(f32, f32)], c: Color) {
        const S: i32 = 4;
        const EDGES: usize = 16;
        if pts.len() < 3 {
            return;
        }
        if pts.len() > EDGES {
            // More edges than a sample row's crossing table holds.
            return self.fill_polygon_sampled(pts, c);
        }
        let min_x = pts.iter().map(|p| p.0).fold(f32::MAX, f32::min).floor() as i32;
        let max_x = pts.iter().map(|p| p.0).fold(f32::MIN, f32::max).ceil() as i32;
        let min_y = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i32;
        let max_y = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i32;
        let cl = self.clip;
        let (x0, x1) = (min_x.max(cl.x), max_x.min(cl.right()));
        for py in min_y.max(cl.y)..max_y.min(cl.bottom()) {
            let mut crossings = [[0f32; EDGES]; S as usize];
            let mut counts = [0usize; S as usize];
            for sy in 0..S {
                let y = py as f32 + (sy as f32 + 0.5) / S as f32;
                let mut j = pts.len() - 1;
                for i in 0..pts.len() {
                    let (xi, yi) = pts[i];
                    let (xj, yj) = pts[j];
                    if (yi > y) != (yj > y) {
                        crossings[sy as usize][counts[sy as usize]] = (xj - xi) * (y - yi) / (yj - yi) + xi;
                        counts[sy as usize] += 1;
                    }
                    j = i;
                }
            }
            if counts == [0; S as usize] {
                continue;
            }
            for px in x0..x1 {
                let mut hits = 0;
                for sy in 0..S as usize {
                    let row = &crossings[sy][..counts[sy]];
                    for sx in 0..S {
                        let x = px as f32 + (sx as f32 + 0.5) / S as f32;
                        hits += row.iter().filter(|cross| x < **cross).count() as i32 & 1;
                    }
                }
                let alpha = (hits * 255 / (S * S)) as u32;
                if alpha != 0 {
                    let p = &mut self.pixels[(py * self.width + px) as usize];
                    *p = c.over(Color(*p), alpha).0;
                }
            }
        }
    }

    /// `fill_polygon` by testing every sample against every edge.
    fn fill_polygon_sampled(&mut self, pts: &[(f32, f32)], c: Color) {
        if pts.len() < 3 {
            return;
        }
        let min_x = pts.iter().map(|p| p.0).fold(f32::MAX, f32::min).floor() as i32;
        let max_x = pts.iter().map(|p| p.0).fold(f32::MIN, f32::max).ceil() as i32;
        let min_y = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i32;
        let max_y = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i32;
        const S: i32 = 4;
        for py in min_y..max_y {
            for px in min_x..max_x {
                let mut hits = 0;
                for sy in 0..S {
                    for sx in 0..S {
                        let x = px as f32 + (sx as f32 + 0.5) / S as f32;
                        let y = py as f32 + (sy as f32 + 0.5) / S as f32;
                        if point_in_polygon(pts, x, y) {
                            hits += 1;
                        }
                    }
                }
                self.blend(px, py, c, (hits * 255 / (S * S)) as u32);
            }
        }
    }

    /// A filled circle, or a ring when `inner` > 0.
    pub fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, inner: f32, c: Color) {
        let x0 = (cx - radius).floor() as i32;
        let x1 = (cx + radius).ceil() as i32;
        let y0 = (cy - radius).floor() as i32;
        let y1 = (cy + radius).ceil() as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let mut cov = (radius - d + 0.5).clamp(0.0, 1.0);
                if inner > 0.0 {
                    cov = cov.min((d - inner + 0.5).clamp(0.0, 1.0));
                }
                self.blend(x, y, c, (cov * 255.0) as u32);
            }
        }
    }

    /// An arc of a ring between two angles (radians, 0 = right, clockwise
    /// with y down). For the Wi-Fi glyph.
    pub fn fill_arc(&mut self, cx: f32, cy: f32, radius: f32, inner: f32, a0: f32, a1: f32, c: Color) {
        let x0 = (cx - radius).floor() as i32;
        let x1 = (cx + radius).ceil() as i32;
        let y0 = (cy - radius).floor() as i32;
        let y1 = (cy + radius).ceil() as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let cov = (radius - d + 0.5).clamp(0.0, 1.0).min((d - inner + 0.5).clamp(0.0, 1.0));
                let alpha = (cov * 255.0) as u32;
                // The angle only for pixels the ring covers.
                if alpha == 0 {
                    continue;
                }
                let a = dy.atan2(dx);
                if a < a0 || a > a1 {
                    continue;
                }
                self.blend(x, y, c, alpha);
            }
        }
    }

    pub fn draw_image(&mut self, img: &Image, x: i32, y: i32) {
        self.draw_image_rounded(img, x, y, 0);
    }

    /// Draws an image at `alpha` (0-255) over what is there.
    pub fn draw_image_alpha(&mut self, img: &Image, x: i32, y: i32, alpha: u32) {
        let dst = Rect::new(x, y, img.width as i32, img.height as i32);
        let area = dst.intersect(&self.clip);
        if area.is_empty() {
            return;
        }
        for py in area.y..area.bottom() {
            let from = (py - y) as usize * img.width as usize + (area.x - x) as usize;
            let to = (py * self.width + area.x) as usize;
            span::blend_pixels(&mut self.pixels[to..to + area.w as usize], &img.pixels[from..from + area.w as usize], alpha);
        }
    }

    /// Draws an image with its corners rounded at `radius`.
    pub fn draw_image_rounded(&mut self, img: &Image, x: i32, y: i32, radius: i32) {
        let dst = Rect::new(x, y, img.width as i32, img.height as i32);
        let area = dst.intersect(&self.clip);
        if area.is_empty() {
            return;
        }
        let rounded = radius.min(dst.w / 2).min(dst.h / 2);
        let rad = rounded as f32;
        // Columns that can have partial coverage in a corner row.
        let left_end = (dst.x + rounded.max(0)).clamp(area.x, area.right());
        let right_start = (dst.right() - rounded.max(0)).clamp(left_end, area.right());
        for py in area.y..area.bottom() {
            let from = (py - y) as usize * img.width as usize;
            let source = &img.pixels[from..from + img.width as usize];
            let row = (py * self.width) as usize;
            let corner_row = radius > 0 && (py < dst.y + radius || py >= dst.bottom() - radius);
            let (copy_from, copy_to) = if corner_row { (left_end, right_start) } else { (area.x, area.right()) };
            if corner_row {
                for px in (area.x..left_end).chain(right_start..area.right()) {
                    let cov = Self::round_coverage(&dst, rad, px, py);
                    if cov != 0 {
                        let p = &mut self.pixels[row + px as usize];
                        *p = Color(source[(px - x) as usize]).over(Color(*p), cov).0;
                    }
                }
            }
            self.pixels[row + copy_from as usize..row + copy_to as usize]
                .copy_from_slice(&source[(copy_from - x) as usize..(copy_to - x) as usize]);
        }
    }

    /// Draws a one-channel coverage mask (a glyph) in color `c`.
    pub fn draw_mask(&mut self, mask: &[u8], w: usize, h: usize, x: i32, y: i32, c: Color) {
        self.draw_mask_rows(mask, w, false, w, h, x, y, c);
    }

    /// Draws a coverage mask whose rows are `stride` bytes apart. `padded`
    /// promises a `stride` that is a multiple of four with zero bytes from
    /// `w` up to it in every row (see `span::blend_color_mask_rect`).
    pub fn draw_mask_rows(&mut self, mask: &[u8], stride: usize, padded: bool, w: usize, h: usize, x: i32, y: i32, c: Color) {
        let dst = Rect::new(x, y, w as i32, h as i32).intersect(&self.clip);
        if dst.is_empty() {
            return;
        }
        let first = (dst.y - y) as usize * stride + (dst.x - x) as usize;
        let at = (dst.y * self.width + dst.x) as usize;
        // A horizontally clipped mask has coverage after the columns drawn.
        let padded = padded && dst.w as usize == w;
        span::blend_color_mask_rect(&mut self.pixels, self.width as usize, at, &mask[first..], stride, dst.w as usize, dst.h as usize, c.0, padded);
    }

    /// Darkens everything by `alpha` (0-255), for pop-up backdrops.
    pub fn dim(&mut self, alpha: u32) {
        let r = self.clip;
        self.fill_rect_alpha(r, Color::BLACK, alpha);
    }
}

fn point_in_polygon(pts: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(count: usize, seed: u32) -> Vec<u32> {
        let mut value = seed;
        (0..count).map(|_| { value = value.wrapping_mul(1664525).wrapping_add(1013904223); value }).collect()
    }

    /// Both canvases start from the same noise (top bytes included) and clip.
    fn pair(seed: u32, clip: Rect) -> (Canvas, Canvas) {
        let mut a = Canvas::new(97, 61);
        a.pixels = noise(97 * 61, seed);
        a.set_clip(clip);
        let mut b = Canvas::new(97, 61);
        b.pixels = a.pixels.clone();
        b.set_clip(clip);
        (a, b)
    }

    #[test]
    fn span_drawing_equals_the_per_pixel_reference() {
        let mut seed = 5u32;
        let mut next = |range: i32| { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); ((seed >> 8) % range as u32) as i32 };
        let img = Image { width: 40, height: 33, pixels: noise(40 * 33, 8) };
        let small = Image { width: 3, height: 2, pixels: noise(6, 9) };
        let mask: Vec<u8> = noise(23 * 17, 4).iter().enumerate().map(|(i, v)| match i % 4 { 0 => 0, 1 => 255, _ => (v >> 9) as u8 }).collect();
        for case in 0..600 {
            let clip = match case % 3 {
                0 => Rect::new(0, 0, 97, 61),
                1 => Rect::new(next(60), next(40), next(60), next(40)),
                _ => Rect::new(10, 5, 70, 50),
            };
            let r = Rect::new(next(110) - 10, next(70) - 8, next(120) - 5, next(80) - 5);
            let radius = next(30) - 2;
            let alpha = match case % 7 { 0 => 255, 1 => 0, 2 => 300, _ => next(256) as u32 };
            let color = Color(noise(1, case as u32)[0]);
            let (mut a, mut b) = pair(case as u32, clip);
            a.fill_rect_alpha(r, color, alpha);
            b.reference_fill_rect_alpha(r, color, alpha);
            assert!(a.pixels == b.pixels, "fill_rect_alpha {case} {r:?} {alpha} {clip:?}");
            a.fill_round_rect_alpha(r, radius, color, alpha);
            b.reference_fill_round_rect_alpha(r, radius, color, alpha);
            assert!(a.pixels == b.pixels, "fill_round_rect_alpha {case} {r:?} {radius} {alpha} {clip:?}");
            let picture = if case % 5 == 0 { &small } else { &img };
            a.draw_image_alpha(picture, r.x, r.y, alpha);
            b.reference_draw_image_alpha(picture, r.x, r.y, alpha);
            assert!(a.pixels == b.pixels, "draw_image_alpha {case}");
            a.draw_image_rounded(picture, r.y, r.x / 2, radius);
            b.reference_draw_image_rounded(picture, r.y, r.x / 2, radius);
            assert!(a.pixels == b.pixels, "draw_image_rounded {case} {radius}");
            a.draw_mask(&mask, 23, 17, r.x, r.y, color);
            b.reference_draw_mask(&mask, 23, 17, r.x, r.y, color);
            assert!(a.pixels == b.pixels, "draw_mask {case}");
            // A padded mask: 23 columns in rows 24 bytes apart.
            let padded: Vec<u8> = mask.chunks(23).flat_map(|row| row.iter().copied().chain([0])).collect();
            a.draw_mask_rows(&padded, 24, true, 23, 17, r.y, r.x / 2, color);
            b.reference_draw_mask(&mask, 23, 17, r.y, r.x / 2, color);
            assert!(a.pixels == b.pixels, "draw_mask_rows {case}");
            a.dim(alpha);
            let all = b.clip();
            b.reference_fill_rect_alpha(all, Color::BLACK, alpha);
            assert!(a.pixels == b.pixels, "dim {case}");
        }
    }

    #[test]
    fn polygons_and_arcs_equal_the_per_sample_code() {
        let mut seed = 21u32;
        let mut next = |range: f32| { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); (seed >> 8) as f32 / 16777216.0 * range };
        for case in 0..300 {
            let clip = if case % 3 == 0 { Rect::new(20, 10, 50, 30) } else { Rect::new(0, 0, 97, 61) };
            let (mut a, mut b) = pair(case, clip);
            // Up to 20 points: above 16 the crossing table gives way to sampling.
            let count = 3 + (case as usize * 7) % 18;
            let pts: Vec<(f32, f32)> = (0..count).map(|i| if case % 4 == 0 && i % 2 == 0 {
                ((next(110.0) - 6.0).round(), (next(70.0) - 4.0).round() + 0.125)
            } else { (next(110.0) - 6.0, next(70.0) - 4.0) }).collect();
            let color = Color(0x00C0FFEE ^ case.wrapping_mul(977));
            a.fill_polygon(&pts, color);
            b.fill_polygon_sampled(&pts, color);
            assert!(a.pixels == b.pixels, "polygon {case} {pts:?}");
            let (cx, cy, radius) = (next(97.0), next(61.0), next(20.0));
            let (inner, a0, a1) = (radius - next(4.0), next(6.4) - 3.2, next(6.4) - 3.2);
            a.fill_arc(cx, cy, radius, inner, a0, a1, color);
            // The former order: angle first.
            let cl = b.clip();
            for y in (cy - radius).floor() as i32..(cy + radius).ceil() as i32 {
                for x in (cx - radius).floor() as i32..(cx + radius).ceil() as i32 {
                    let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                    let angle = dy.atan2(dx);
                    if angle < a0 || angle > a1 { continue; }
                    let d = (dx * dx + dy * dy).sqrt();
                    let cov = (radius - d + 0.5).clamp(0.0, 1.0).min((d - inner + 0.5).clamp(0.0, 1.0));
                    if x >= cl.x && y >= cl.y && x < cl.right() && y < cl.bottom() { b.blend(x, y, color, (cov * 255.0) as u32); }
                }
            }
            assert!(a.pixels == b.pixels, "arc {case}");
        }
    }

    #[test]
    fn corners() {
        assert_eq!(corner_inset(160), 0);
        assert!(corner_inset(0) >= 79);
        assert_eq!(corner_inset(0), corner_inset(HEIGHT - 1));
        assert!(corner_inset(30) > corner_inset(50));
    }

    #[test]
    fn round_rect_fills_centre_not_corner() {
        let mut c = Canvas::new(40, 40);
        c.fill_round_rect(Rect::new(0, 0, 40, 40), 10, Color::WHITE);
        assert_eq!(c.get(20, 20), Color::WHITE);
        assert_eq!(c.get(0, 0), Color::BLACK);
        assert_eq!(c.get(20, 0), Color::WHITE);
    }

    #[test]
    fn clip_limits_drawing() {
        let mut c = Canvas::new(10, 10);
        c.set_clip(Rect::new(0, 0, 5, 10));
        c.fill_rect(Rect::new(0, 0, 10, 10), Color::WHITE);
        assert_eq!(c.get(4, 4), Color::WHITE);
        assert_eq!(c.get(5, 4), Color::BLACK);
    }

    #[test]
    fn blending() {
        assert_eq!(Color::WHITE.over(Color::BLACK, 255), Color::WHITE);
        assert_eq!(Color::WHITE.over(Color::BLACK, 0), Color::BLACK);
        assert_eq!(Color::WHITE.over(Color::BLACK, 128).r(), 128);
    }
}
