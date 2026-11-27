//! Scenes of the `ui` benchmark kind beyond the two lists: Cover Flow, the
//! night tint, the frame rotation of presentation, and single primitives.
//! Each reports its samples (`NAME_us`), their median (`NAME_median_us`) and
//! a checksum of every frame it drew. Generated input only.
use super::{icons, night, screens, theme::Theme};
use crate::gfx::{Canvas, Fonts, HEIGHT, Image, Rect, WIDTH, Weight};
use serde_json::{Map, Value, json};
use std::time::Instant;

pub(super) fn median(samples: &[u64]) -> u64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

pub(super) fn checksum(mut sum: u64, pixels: &[u32]) -> u64 {
    for p in pixels { sum = sum.wrapping_mul(31).wrapping_add(*p as u64); }
    sum
}

/// Deterministic pixels with every channel value and a clear top byte.
pub(crate) fn noise(count: usize, seed: u32) -> Vec<u32> {
    let mut value = seed;
    (0..count).map(|_| {
        value = value.wrapping_mul(1664525).wrapping_add(1013904223);
        (value >> 8) & 0xFF_FFFF
    }).collect()
}

/// The rotation `os/platform/display.c` does on presentation: the landscape
/// canvas clockwise onto the 320x375 panel, opaque alpha. `out` has 320
/// pixels per row here; the device's rows are `pitch` bytes.
pub(crate) fn rotate_reference(pixels: &[u32], out: &mut [u32]) {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    for x in 0..w {
        for column in 0..h {
            out[x * h + column] = pixels[(h - 1 - column) * w + x] | 0xFF00_0000;
        }
    }
}

fn micros(work: impl FnOnce()) -> u64 {
    let started = Instant::now();
    work();
    started.elapsed().as_micros() as u64
}

struct Scenes<'a> {
    canvas: &'a mut Canvas,
    iterations: usize,
    results: &'a mut Map<String, Value>,
    sums: Map<String, Value>,
}

impl Scenes<'_> {
    /// `step` draws iteration `i` and returns the microseconds of its timed
    /// part. The first `warm` iterations are drawn and summed, not sampled.
    fn run(&mut self, name: &str, warm: usize, mut step: impl FnMut(&mut Canvas, usize) -> u64) {
        let mut samples = Vec::with_capacity(self.iterations);
        let mut sum = 0u64;
        for i in 0..self.iterations + warm {
            let elapsed = step(self.canvas, i);
            std::hint::black_box(&self.canvas.pixels);
            sum = checksum(sum, &self.canvas.pixels);
            if i >= warm { samples.push(elapsed); }
        }
        self.results.insert(format!("{name}_median_us"), median(&samples).into());
        self.results.insert(format!("{name}_us"), json!(samples));
        self.sums.insert(name.into(), sum.into());
    }
}

pub(super) fn scenes(canvas: &mut Canvas, fonts: &mut Fonts, theme: Theme, iterations: usize, results: &mut Map<String, Value>) {
    let full = Rect::new(0, 0, WIDTH, HEIGHT);
    canvas.set_clip(full);
    let frame = noise((WIDTH * HEIGHT) as usize, 0x51324f53);
    let art = Image::placeholder(150);
    let cover = Image { width: 150, height: 150, pixels: noise(150 * 150, 7) };
    let thumb = Image { width: 52, height: 52, pixels: noise(52 * 52, 11) };
    let mut s = Scenes { canvas, iterations, results, sums: Map::new() };

    s.run("coverflow", 10, |c, i| micros(|| screens::coverflow::benchmark_frame(c, &art, (i % 20) as f32 / 20.0)));
    s.run("coverflow_noise", 0, |c, i| micros(|| screens::coverflow::benchmark_frame(c, &cover, (i % 20) as f32 / 20.0)));

    // Every warmth but neutral over the iterations; the cost does not depend on it.
    let mut tint = night::Night::new();
    s.run("night_tint", 0, |c, i| {
        c.pixels.copy_from_slice(&frame);
        tint.set_quantized(255 - (i * 37 % 255) as u8);
        micros(|| tint.apply(&mut c.pixels))
    });

    // Presentation's rotation, into ordinary memory (the device writes a
    // write-combined dumb buffer). The rotated frame is copied back so the
    // scene's checksum covers it.
    let mut rotated = vec![0u32; (WIDTH * HEIGHT) as usize];
    s.run("present", 0, |c, i| {
        c.pixels.copy_from_slice(&frame);
        c.pixels[i] ^= 0xFF;
        let elapsed = micros(|| rotate_reference(&c.pixels, &mut rotated));
        c.pixels.copy_from_slice(&rotated);
        elapsed
    });

    // Single primitives, each over the theme background.
    s.run("prim_clear", 0, |c, _| micros(|| for _ in 0..4 { c.clear(theme.bg); }));
    s.run("prim_dim", 0, |c, i| {
        c.pixels.copy_from_slice(&frame);
        micros(|| c.dim(40 + (i % 100) as u32))
    });
    s.run("prim_text", 0, |c, i| {
        c.clear(theme.bg);
        micros(|| for line in 0..8 {
            let y = 30 + line * 36;
            fonts.draw(c, "Track 0123 — Tomorrow's Music", 12 + (i % 5) as i32, y, 17, Weight::Bold, theme.text);
            fonts.draw(c, "A synthetic artist · 2026", 12, y + 19, 13, Weight::Regular, theme.muted);
        })
    });
    s.run("prim_round_rect", 0, |c, i| {
        c.pixels.copy_from_slice(&frame);
        micros(|| {
            for k in 0..8 { c.fill_round_rect_alpha(Rect::new(10 + k * 44, 20 + (i % 7) as i32, 80, 80), 14, theme.hud, theme.hud_alpha); }
            for k in 0..4 { c.fill_round_rect(Rect::new(30, 110 + k * 50, 300, 44), 12, theme.text); }
        })
    });
    s.run("prim_image_rounded", 0, |c, i| {
        c.clear(theme.bg);
        micros(|| for k in 0..7 { c.draw_image_rounded(&thumb, 8 + (i % 3) as i32, -20 + k * 52, 2); })
    });
    s.run("prim_image_alpha", 0, |c, i| {
        c.pixels.copy_from_slice(&frame);
        micros(|| for k in 0..2 { c.draw_image_alpha(&cover, 20 + k * 170, 40, 30 + (i * 13 % 200) as u32); })
    });
    s.run("prim_selection", 0, |c, i| {
        c.clear(theme.bg);
        micros(|| for k in 0..7 { theme.fill_selection(c, Rect::new(0, 30 + k * 44 + (i % 3) as i32, WIDTH - 10, 44)); })
    });
    s.run("prim_icons", 0, |c, _| {
        c.clear(theme.bg);
        micros(|| for k in 0..7 {
            icons::chevron(c, 340.0, 52.0 + k as f32 * 44.0, 12.0, theme.chevron);
            icons::speaker(c, 300.0, 46.0 + k as f32 * 44.0, 13.0, theme.sel_mid);
        })
    });
    let sums = std::mem::take(&mut s.sums);
    s.results.insert("checksums".into(), sums.into());
}

/// The `ui-parts` kind: parts of a Cover Flow frame and of text drawing, to
/// see where a frame's time goes. Medians and checksums as in `scenes`.
pub(super) fn parts(blocking: &crate::rt::Blocking, iterations: usize) -> Result<Value, String> {
    let mut fonts = Fonts::load(blocking, &[])?;
    let config = crate::config::Config::default();
    let theme = Theme::new(config.display.look, config.display.accent);
    let mut canvas = Canvas::new(WIDTH, HEIGHT);
    let mut results = Map::new();
    let albums: Vec<Image> = (0..13).map(|i| Image { width: 150, height: 150, pixels: noise(150 * 150, 100 + i) }).collect();
    let mut s = Scenes { canvas: &mut canvas, iterations, results: &mut results, sums: Map::new() };
    use screens::coverflow::benchmark_part;
    s.run("parts_black", 0, |c, _| micros(|| benchmark_part(c, &albums[..1], 0.0, 1, 0)));
    // The resting middle cover; three turned covers at the left; the two
    // covers between places; all thirteen with an image each.
    s.run("parts_middle", 0, |c, _| micros(|| benchmark_part(c, &albums[..1], 0.0, 0, 0)));
    s.run("parts_left3", 0, |c, _| micros(|| benchmark_part(c, &albums[..1], 0.0, -3, -1)));
    s.run("parts_left3_albums", 0, |c, _| micros(|| benchmark_part(c, &albums, 0.0, -3, -1)));
    s.run("parts_between", 0, |c, _| micros(|| benchmark_part(c, &albums[..1], 0.5, 0, 1)));
    s.run("parts_albums", 0, |c, i| micros(|| benchmark_part(c, &albums, (i % 20) as f32 / 20.0, -6, 6)));
    // Text: shaping and measuring alone, then drawing over a clear canvas.
    s.run("parts_text_measure", 0, |_, i| micros(|| for line in 0..8 {
        std::hint::black_box(fonts.measure("Track 0123 — Tomorrow's Music", 17, Weight::Bold) + line + i as i32);
        std::hint::black_box(fonts.measure("A synthetic artist · 2026", 13, Weight::Regular));
    }));
    s.run("parts_text_one_row", 0, |c, _| {
        c.clear(theme.bg);
        micros(|| for _ in 0..8 {
            fonts.draw(c, "Track 0123 — Tomorrow's Music", 12, 30, 17, Weight::Bold, theme.text);
            fonts.draw(c, "A synthetic artist · 2026", 12, 49, 13, Weight::Regular, theme.muted);
        })
    });
    let sums = std::mem::take(&mut s.sums);
    s.results.insert("checksums".into(), sums.into());
    Ok(results.into())
}

/// The `ui-check` kind: every span entry against the scalar code on
/// generated pixels, for all lengths up to 40 and three long ones, alpha
/// extremes and top bytes included. On the Q2 the entries run the MSA
/// kernels (`msa` in the result); an `Err` names the first difference.
pub(super) fn check() -> Result<Value, String> {
    use crate::gfx::span;
    let raw = |count: usize, seed: u32| -> Vec<u32> {
        let mut value = seed;
        (0..count).map(|_| { value = value.wrapping_mul(1664525).wrapping_add(1013904223); value }).collect()
    };
    let mut sum = 0u64;
    let mut compared = 0u64;
    let mut same = |name: &str, n: usize, got: &[u32], expected: &[u32], sum: &mut u64| -> Result<(), String> {
        if got != expected { return Err(format!("{name}: length {n} differs from the scalar code")); }
        *sum = checksum(*sum, got);
        compared += 1;
        Ok(())
    };
    for n in (0..=40).chain([375, 1501, 120_000]) {
        let (src, under) = (raw(n, 1 + n as u32), raw(n, 77 + n as u32));
        let mut alphas: Vec<u8> = raw(n, 5).iter().map(|v| (v >> 11) as u8).collect();
        for (i, a) in alphas.iter_mut().enumerate() {
            match (i / 4) % 5 { 0 => *a = 0, 1 => *a = 255, 2 if i % 4 == 1 => *a = 0, 2 if i % 4 == 2 => *a = 255, _ => {} }
        }
        for alpha in [0u32, 1, 2, 70, 127, 128, 200, 254, 255, 256] {
            let color = 0x9C00_0000 | (alpha * 0x010305) ^ 0x35A1C7;
            let (mut a, mut b) = (under.clone(), under.clone());
            span::blend_color(&mut a, color, alpha);
            span::scalar_blend_color(&mut b, color, alpha);
            same("blend_color", n, &a, &b, &mut sum)?;
            let (mut a, mut b) = (vec![0; n], vec![0; n]);
            span::blend_color_from(&mut a, &under, color, alpha);
            span::scalar_blend_color_from(&mut b, &under, color, alpha);
            same("blend_color_from", n, &a, &b, &mut sum)?;
            let (mut a, mut b) = (under.clone(), under.clone());
            span::blend_pixels(&mut a, &src, alpha);
            span::scalar_blend_pixels(&mut b, &src, alpha);
            same("blend_pixels", n, &a, &b, &mut sum)?;
        }
        let (mut a, mut b) = (under.clone(), under.clone());
        span::blend_pixels_mask(&mut a, &src, &alphas);
        span::scalar_blend_pixels_mask(&mut b, &src, &alphas);
        same("blend_pixels_mask", n, &a, &b, &mut sum)?;
        let (mut a, mut b) = (under.clone(), under.clone());
        span::blend_color_mask(&mut a, &alphas, 0xC3FE_DCBA);
        span::scalar_blend_color_mask(&mut b, &alphas, 0xC3FE_DCBA);
        same("blend_color_mask", n, &a, &b, &mut sum)?;
        for q in [1u32, 100, 128, 255] {
            let (mg, mb) = (span::tint_multiplier(25500 - q * 18), span::tint_multiplier(25500 - q * 45));
            let (mut a, mut b) = (under.clone(), under.clone());
            span::tint(&mut a, mg, mb);
            span::scalar_tint(&mut b, mg, mb);
            same("tint", n, &a, &b, &mut sum)?;
        }
    }
    // Padded masks of every width up to 21 at every position near the end
    // of a small canvas, where the last group of four may not fit.
    let (cw, ch) = (41usize, 9usize);
    let under = raw(cw * ch, 3);
    for w in 1..=21usize {
        let (stride, h) = ((w + 3) & !3, 5usize);
        let mut mask = vec![0u8; stride * h];
        for (i, v) in raw(w * h, w as u32).iter().enumerate() {
            mask[i / w * stride + i % w] = match v >> 29 { 0 => 0, 1 => 255, _ => (v >> 8) as u8 };
        }
        for at in [0, 7, cw - w, cw * (ch - h) + cw - w, cw * (ch - h) + cw - w - 2] {
            let (mut a, mut b) = (under.clone(), under.clone());
            span::blend_color_mask_rect(&mut a, cw, at, &mask, stride, w, h, 0x00E1_2D4B, true);
            for row in 0..h {
                span::scalar_blend_color_mask(&mut b[at + row * cw..at + row * cw + w], &mask[row * stride..row * stride + w], 0x00E1_2D4B);
            }
            same("blend_color_mask_rect", w, &a, &b, &mut sum)?;
        }
    }
    Ok(json!({"msa": span::vector_active(), "identical": true, "compared": compared, "checksum": sum}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_reference_turns_clockwise_and_sets_alpha() {
        let mut pixels = vec![0u32; (WIDTH * HEIGHT) as usize];
        pixels[0] = 0x112233; // top left of the landscape canvas
        pixels[(WIDTH - 1) as usize] = 0x445566; // top right
        let mut out = vec![0u32; pixels.len()];
        rotate_reference(&pixels, &mut out);
        assert_eq!(out[(HEIGHT - 1) as usize], 0xFF112233);
        assert_eq!(out[((WIDTH - 1) * HEIGHT + HEIGHT - 1) as usize], 0xFF445566);
        assert!(out.iter().all(|p| p >> 24 == 0xFF));
    }

    #[test]
    fn span_check_passes_on_the_portable_code() {
        let result = check().unwrap();
        assert_eq!(result["identical"], true);
        assert!(result["compared"].as_u64().unwrap() > 1500);
    }
}
