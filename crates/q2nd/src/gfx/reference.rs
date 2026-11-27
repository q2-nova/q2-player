//! The per-pixel drawing code the span code replaced, kept as the reference
//! the tests compare against.
use super::{Canvas, Color, Image, Rect};

impl Canvas {
    pub fn reference_fill_rect_alpha(&mut self, r: Rect, c: Color, alpha: u32) {
        let r = r.intersect(&self.clip);
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                let i = (y * self.width + x) as usize;
                self.pixels[i] = c.over(Color(self.pixels[i]), alpha).0;
            }
        }
    }

    pub fn reference_fill_round_rect_alpha(&mut self, r: Rect, radius: i32, c: Color, alpha: u32) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        if radius == 0 {
            return self.reference_fill_rect_alpha(r, c, alpha);
        }
        let rad = radius as f32;
        let area = r.intersect(&self.clip);
        for y in area.y..area.bottom() {
            let in_band = y >= r.y + radius && y < r.bottom() - radius;
            if in_band && alpha == 255 {
                self.fill_rect(Rect::new(area.x, y, area.w, 1), c);
                continue;
            }
            for x in area.x..area.right() {
                let cov = Self::round_coverage(&r, rad, x, y);
                self.blend(x, y, c, cov * alpha / 255);
            }
        }
    }

    /// Draws an image at `alpha` (0-255) over what is there.
    pub fn reference_draw_image_alpha(&mut self, img: &Image, x: i32, y: i32, alpha: u32) {
        let dst = Rect::new(x, y, img.width as i32, img.height as i32);
        let area = dst.intersect(&self.clip);
        for py in area.y..area.bottom() {
            let sy = (py - y) as usize;
            for px in area.x..area.right() {
                let p = Color(img.pixels[sy * img.width as usize + (px - x) as usize]);
                let i = (py * self.width + px) as usize;
                self.pixels[i] = p.over(Color(self.pixels[i]), alpha).0;
            }
        }
    }

    /// Draws an image with its corners rounded at `radius`.
    pub fn reference_draw_image_rounded(&mut self, img: &Image, x: i32, y: i32, radius: i32) {
        let dst = Rect::new(x, y, img.width as i32, img.height as i32);
        let area = dst.intersect(&self.clip);
        let rad = radius.min(dst.w / 2).min(dst.h / 2) as f32;
        for py in area.y..area.bottom() {
            let sy = (py - y) as usize;
            let row = &img.pixels[sy * img.width as usize..(sy + 1) * img.width as usize];
            let corner_row = radius > 0 && (py < dst.y + radius || py >= dst.bottom() - radius);
            for px in area.x..area.right() {
                let p = Color(row[(px - x) as usize]);
                if corner_row {
                    let cov = Self::round_coverage(&dst, rad, px, py);
                    self.blend(px, py, p, cov);
                } else {
                    self.pixels[(py * self.width + px) as usize] = p.0;
                }
            }
        }
    }

    /// Draws a one-channel coverage mask (a glyph) in color `c`.
    pub fn reference_draw_mask(&mut self, mask: &[u8], w: usize, h: usize, x: i32, y: i32, c: Color) {
        let dst = Rect::new(x, y, w as i32, h as i32).intersect(&self.clip);
        for py in dst.y..dst.bottom() {
            let my = (py - y) as usize;
            for px in dst.x..dst.right() {
                let a = mask[my * w + (px - x) as usize] as u32;
                if a > 0 {
                    let i = (py * self.width + px) as usize;
                    self.pixels[i] = c.over(Color(self.pixels[i]), a).0;
                }
            }
        }
    }

}
