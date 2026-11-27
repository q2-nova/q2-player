//! A font-independent first image while the player is still starting.
//!
//! This is neither a screen nor a readiness signal: it has no input handler,
//! timer, minimum display time or owner of its own.
use crate::config::{Accent, Look};
use crate::gfx::{Canvas, Rect, WIDTH, HEIGHT};
use super::theme::Theme;

pub fn frame(look: Look, accent: Accent) -> Canvas {
    let theme = Theme::new(look, accent);
    let mut canvas = Canvas::new(WIDTH, HEIGHT);
    canvas.clear(theme.bg);
    // Original paired-note mark, drawn directly rather than loading an asset
    // or waiting for the font reader. Stay clear of the rounded glass.
    let x = WIDTH / 2 - 24;
    let y = HEIGHT / 2 - 46;
    for (dx, dy) in [(0, 10), (48, 0)] {
        canvas.fill_circle((x + dx) as f32, (y + dy + 68) as f32, 13.0, 0.0, theme.sel_bottom);
        canvas.fill_rect(Rect::new(x + dx + 6, y + dy, 7, 68), theme.sel_bottom);
    }
    canvas.fill_polygon(&[
        ((x + 6) as f32, (y + 10) as f32),
        ((x + 61) as f32, y as f32),
        ((x + 61) as f32, (y + 13) as f32),
        ((x + 6) as f32, (y + 23) as f32),
    ], theme.sel_bottom);
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_image_needs_no_fonts_and_respects_theme_and_glass() {
        #[cfg(not(any(target_arch = "mips", target_arch = "mips64")))]
        let previews = {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../build/perf/startup-image-preview-{}", fastrand::u64(..)));
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        for look in [Look::Classic, Look::Dark] {
            for accent in [Accent::Blue, Accent::Graphite, Accent::Green, Accent::Orange,
                Accent::Pink, Accent::Purple, Accent::Red] {
                let theme = Theme::new(look, accent);
                let canvas = frame(look, accent);
                assert_eq!(canvas.pixels.len(), (WIDTH * HEIGHT) as usize);
                let mut mark_pixels = 0;
                for y in 0..HEIGHT {
                    for x in 0..WIDTH {
                        if canvas.get(x, y) != theme.bg {
                            mark_pixels += 1;
                            assert!((WIDTH / 2 - 64..WIDTH / 2 + 64).contains(&x));
                            assert!((HEIGHT / 2 - 64..HEIGHT / 2 + 64).contains(&y));
                        }
                    }
                }
                assert!(mark_pixels > 1500);
                assert!(canvas.pixels.contains(&theme.sel_bottom.0));
                #[cfg(not(any(target_arch = "mips", target_arch = "mips64")))]
                if matches!(accent, Accent::Blue) {
                    let image = ::image::RgbImage::from_fn(WIDTH as u32, HEIGHT as u32, |x, y| {
                        let color = canvas.get(x as i32, y as i32);
                        ::image::Rgb([color.r() as u8, color.g() as u8, color.b() as u8])
                    });
                    image.save(previews.join(if theme.dark { "dark.png" } else { "classic.png" })).unwrap();
                }
            }
        }
    }
}
