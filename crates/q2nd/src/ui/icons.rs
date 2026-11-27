//! Small vector glyphs, drawn at any size with the canvas's anti-aliasing.

use crate::gfx::{Canvas, Color, Rect};

/// A right-pointing triangle in a `size` box at (x, y).
pub fn play(c: &mut Canvas, x: f32, y: f32, size: f32, col: Color) {
    c.fill_polygon(&[(x + size * 0.15, y), (x + size * 0.95, y + size * 0.5), (x + size * 0.15, y + size)], col);
}

pub fn pause(c: &mut Canvas, x: f32, y: f32, size: f32, col: Color) {
    let w = size * 0.3;
    c.fill_polygon(&[(x + size * 0.12, y), (x + size * 0.12 + w, y), (x + size * 0.12 + w, y + size), (x + size * 0.12, y + size)], col);
    let x2 = x + size * 0.58;
    c.fill_polygon(&[(x2, y), (x2 + w, y), (x2 + w, y + size), (x2, y + size)], col);
}

/// A speaker with sound waves: the playing song's mark (as the classic's).
pub fn speaker(c: &mut Canvas, x: f32, y: f32, size: f32, col: Color) {
    let s = size / 14.0;
    c.fill_polygon(
        &[(x, y + 4.5 * s), (x + 3.0 * s, y + 4.5 * s), (x + 7.0 * s, y + 1.0 * s), (x + 7.0 * s, y + 13.0 * s), (x + 3.0 * s, y + 9.5 * s), (x, y + 9.5 * s)],
        col,
    );
    use std::f32::consts::PI;
    for r in [3.0f32, 6.0] {
        let r = r * s;
        c.fill_arc(x + 7.5 * s, y + 7.0 * s, r + 1.2 * s, r, -PI / 3.0, PI / 3.0, col);
    }
}

/// A speaker without waves (the quiet end of the volume bar).
pub fn speaker_plain(c: &mut Canvas, x: f32, y: f32, size: f32, col: Color) {
    let s = size / 14.0;
    c.fill_polygon(
        &[(x, y + 4.5 * s), (x + 3.0 * s, y + 4.5 * s), (x + 7.0 * s, y + 1.0 * s), (x + 7.0 * s, y + 13.0 * s), (x + 3.0 * s, y + 9.5 * s), (x, y + 9.5 * s)],
        col,
    );
}

/// A star, filled or as a dot when empty (the classic's rating).
pub fn star(c: &mut Canvas, cx: f32, cy: f32, r: f32, filled: bool, col: Color) {
    if !filled {
        c.fill_circle(cx, cy, r * 0.22, 0.0, col);
        return;
    }
    let mut pts = Vec::with_capacity(10);
    for k in 0..10 {
        let a = -std::f32::consts::FRAC_PI_2 + k as f32 * std::f32::consts::PI / 5.0;
        let rr = if k % 2 == 0 { r } else { r * 0.45 };
        pts.push((cx + rr * a.cos(), cy + rr * a.sin()));
    }
    c.fill_polygon(&pts, col);
}

/// A check mark.
pub fn check(c: &mut Canvas, x: f32, y: f32, size: f32, col: Color) {
    let s = size / 14.0;
    c.fill_polygon(
        &[(x, y + 7.0 * s), (x + 2.0 * s, y + 5.0 * s), (x + 5.0 * s, y + 8.5 * s), (x + 12.0 * s, y), (x + 14.0 * s, y + 2.0 * s), (x + 5.0 * s, y + 12.5 * s)],
        col,
    );
}

/// A thin `>` for rows that open another page.
pub fn chevron(c: &mut Canvas, x: f32, cy: f32, size: f32, col: Color) {
    let h = size / 2.0;
    let t = size * 0.16;
    let w = size * 0.45;
    c.fill_polygon(&[(x, cy - h), (x + t, cy - h), (x + w + t, cy), (x + t, cy + h), (x, cy + h), (x + w, cy)], col);
}

/// The classic's battery: a grey outlined cell, its charge in green (red
/// when low), a bolt while charging.
pub fn battery(c: &mut Canvas, x: i32, y: i32, level: Option<u8>, charging: bool, outline: Color, inside: Color) {
    let body = Rect::new(x, y, 22, 12);
    c.fill_round_rect(body, 3, outline);
    c.fill_round_rect(body.inset(1, 1), 2, inside);
    c.fill_round_rect(Rect::new(x + 22, y + 3, 2, 6), 1, outline);
    let level = level.unwrap_or(0).min(100) as i32;
    let (top, bottom) = if level <= 15 && !charging {
        (Color(0xF2878A), Color(0xC9282E))
    } else {
        (Color(0xA9E58F), Color(0x3E9E22))
    };
    let w = (18 * level + 50) / 100;
    if w > 0 {
        c.vertical_gradient(Rect::new(x + 2, y + 2, w, 8), top, bottom);
    }
    if charging {
        // A bolt across the body.
        let bx = x as f32 + 6.0;
        let by = y as f32 - 1.0;
        c.fill_polygon(&[(bx + 5.0, by), (bx + 1.0, by + 7.0), (bx + 4.5, by + 7.0), (bx + 3.0, by + 13.0), (bx + 8.0, by + 5.0), (bx + 4.5, by + 5.0)], Color::WHITE);
    }
}

/// Wi-Fi: a dot and arcs, `bars` (0-3) of them lit.
pub fn wifi(c: &mut Canvas, cx: f32, bottom: f32, bars: u8, col: Color, dim: Color) {
    use std::f32::consts::PI;
    let a0 = -PI * 0.75;
    let a1 = -PI * 0.25;
    c.fill_circle(cx, bottom - 1.5, 1.8, 0.0, col);
    for i in 1..=3u8 {
        let r = 3.5 + i as f32 * 3.4;
        let color = if i <= bars { col } else { dim };
        c.fill_arc(cx, bottom, r, r - 2.0, a0, a1, color);
    }
}

/// A cloud with a slash: offline.
pub fn offline(c: &mut Canvas, x: f32, y: f32, col: Color) {
    c.fill_circle(x + 6.0, y + 8.0, 4.0, 0.0, col);
    c.fill_circle(x + 11.0, y + 6.0, 5.5, 0.0, col);
    c.fill_circle(x + 16.0, y + 8.5, 3.5, 0.0, col);
    c.fill_rect(Rect::new(x as i32 + 6, y as i32 + 8, 11, 4), col);
    c.fill_polygon(&[(x + 2.0, y), (x + 4.0, y), (x + 20.0, y + 14.0), (x + 18.0, y + 14.0)], Color::BLACK);
    c.fill_polygon(&[(x + 3.0, y - 1.0), (x + 4.2, y - 1.0), (x + 20.0, y + 13.0), (x + 18.8, y + 13.0)], col);
}

/// Deliberate Offline Mode; this indicator does not change radio settings.
pub fn airplane(c: &mut Canvas, x: f32, y: f32, col: Color) {
    c.fill_polygon(&[(x+9.0,y), (x+11.0,y), (x+11.0,y+5.0), (x+20.0,y+10.0),
        (x+20.0,y+12.0), (x+11.0,y+9.0), (x+11.0,y+13.0), (x+14.0,y+15.0),
        (x+14.0,y+16.0), (x+10.0,y+15.0), (x+6.0,y+16.0), (x+6.0,y+15.0),
        (x+9.0,y+13.0), (x+9.0,y+9.0), (x,y+12.0), (x,y+10.0), (x+9.0,y+5.0)], col);
}

fn unavailable_slash(c: &mut Canvas, x: f32, y: f32, background: Color) {
    c.fill_polygon(&[(x,y-1.0),(x+3.5,y-1.0),(x+22.0,y+15.0),(x+18.5,y+15.0)], background);
    c.fill_polygon(&[(x+1.0,y-1.0),(x+2.5,y-1.0),(x+21.0,y+15.0),(x+19.5,y+15.0)], Color(0x9C302F));
}

/// Tailscale's nine-dot VPN mark with an unavailable indicator.
pub fn vpn_unavailable(c: &mut Canvas, x: f32, y: f32, col: Color, background: Color) {
    for row in 0..3 { for column in 0..3 {
        c.fill_circle(x+4.0+column as f32*6.0, y+1.5+row as f32*5.5, 1.9, 0.0, col);
    } }
    unavailable_slash(c, x, y, background);
}

/// Small disc mark from https://www.navidrome.org/brand/navidrome-logo.svg.
/// The server badge retains the disc's blue/white/black and proportions.
pub fn navidrome_unavailable(c: &mut Canvas, x: f32, y: f32, background: Color) {
    let (cx, cy) = (x+10.0, y+7.0);
    c.fill_circle(cx, cy, 7.8, 0.0, Color::BLACK);
    c.fill_circle(cx, cy, 7.2, 0.0, Color(0x0084FF));
    c.fill_circle(cx, cy, 3.0, 0.0, Color::BLACK);
    c.fill_circle(cx, cy, 2.4, 0.0, Color::WHITE);
    c.fill_circle(cx, cy, 0.9, 0.0, Color::BLACK);
    use std::f32::consts::{PI, FRAC_PI_2};
    for radius in [4.8, 6.2] {
        c.fill_arc(cx, cy, radius, radius-0.6, PI, PI+FRAC_PI_2, Color::BLACK);
        c.fill_arc(cx, cy, radius, radius-0.6, 0.0, FRAC_PI_2, Color::BLACK);
    }
    unavailable_slash(c, x, y, background);
}

/// A down arrow into a tray: downloaded, or downloading.
pub fn download(c: &mut Canvas, x: f32, y: f32, size: f32, col: Color) {
    let s = size / 14.0;
    c.fill_polygon(&[(x + 6.0 * s, y), (x + 8.0 * s, y), (x + 8.0 * s, y + 7.0 * s), (x + 6.0 * s, y + 7.0 * s)], col);
    c.fill_polygon(&[(x + 2.5 * s, y + 5.0 * s), (x + 11.5 * s, y + 5.0 * s), (x + 7.0 * s, y + 10.0 * s)], col);
    c.fill_polygon(&[(x, y + 12.0 * s), (x + 14.0 * s, y + 12.0 * s), (x + 14.0 * s, y + 14.0 * s), (x, y + 14.0 * s)], col);
}

/// A heart, filled or outlined.
pub fn heart(c: &mut Canvas, cx: f32, cy: f32, size: f32, filled: bool, col: Color, bg: Color) {
    let r = size * 0.27;
    let draw = |c: &mut Canvas, grow: f32, colour: Color| {
        c.fill_circle(cx - r * 0.95, cy - r * 0.55, r + grow, 0.0, colour);
        c.fill_circle(cx + r * 0.95, cy - r * 0.55, r + grow, 0.0, colour);
        c.fill_polygon(
            &[
                (cx - r * 1.9 - grow, cy - r * 0.35),
                (cx + r * 1.9 + grow, cy - r * 0.35),
                (cx, cy + size * 0.48 + grow),
            ],
            colour,
        );
    };
    draw(c, 0.0, col);
    if !filled {
        draw(c, -size * 0.11, bg);
    }
}

/// Two crossing arrows.
pub fn shuffle(c: &mut Canvas, x: f32, y: f32, size: f32, col: Color) {
    let s = size / 20.0;
    let t = 1.0 * s;
    let line = |c: &mut Canvas, x0: f32, y0: f32, x1: f32, y1: f32| {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt();
        let (nx, ny) = (-dy / len * t, dx / len * t);
        c.fill_polygon(&[(x0 + nx, y0 + ny), (x1 + nx, y1 + ny), (x1 - nx, y1 - ny), (x0 - nx, y0 - ny)], col);
    };
    line(c, x, y + 5.0 * s, x + 5.0 * s, y + 5.0 * s);
    line(c, x + 5.0 * s, y + 5.0 * s, x + 12.0 * s, y + 15.0 * s);
    line(c, x + 12.0 * s, y + 15.0 * s, x + 16.0 * s, y + 15.0 * s);
    line(c, x, y + 15.0 * s, x + 5.0 * s, y + 15.0 * s);
    line(c, x + 5.0 * s, y + 15.0 * s, x + 12.0 * s, y + 5.0 * s);
    line(c, x + 12.0 * s, y + 5.0 * s, x + 16.0 * s, y + 5.0 * s);
    for ay in [5.0, 15.0] {
        c.fill_polygon(&[(x + 16.0 * s, y + (ay - 3.5) * s), (x + 20.0 * s, y + ay * s), (x + 16.0 * s, y + (ay + 3.5) * s)], col);
    }
}

/// A loop of arrows; `one` adds a small "1".
pub fn repeat(c: &mut Canvas, x: f32, y: f32, size: f32, one: bool, col: Color) {
    let s = size / 20.0;
    let r = Rect::new((x + 1.0 * s) as i32, (y + 4.0 * s) as i32, (18.0 * s) as i32, (12.0 * s) as i32);
    c.fill_round_rect(r, (4.0 * s) as i32, col);
    c.fill_round_rect(r.inset((2.0 * s).max(1.0) as i32, (2.0 * s).max(1.0) as i32), (3.0 * s) as i32, Color::BLACK);
    c.fill_rect(Rect::new((x + 8.0 * s) as i32, (y + 3.0 * s) as i32, (4.0 * s) as i32, (4.0 * s) as i32), Color::BLACK);
    c.fill_polygon(&[(x + 8.0 * s, y + 1.5 * s), (x + 12.0 * s, y + 5.0 * s), (x + 8.0 * s, y + 8.5 * s)], col);
    if one {
        c.fill_rect(Rect::new((x + 9.0 * s) as i32, (y + 8.0 * s) as i32, (2.0 * s).max(1.0) as i32, (6.0 * s) as i32), col);
    }
}

/// A clockwise download pie fitted to the cover. Both sectors show the art
/// underneath; queued and unknown-length downloads show only the outline.
pub fn download_pie(c: &mut Canvas, cover: Rect, fraction: Option<f32>) {
    let radius = cover.w.min(cover.h) as f32 / 2.0;
    let cx = cover.x as f32 + cover.w as f32 / 2.0;
    let cy = cover.y as f32 + cover.h as f32 / 2.0;
    c.fill_round_rect_alpha(cover, radius as i32, Color::BLACK, 95);
    c.fill_circle(cx, cy, radius - 1.0, radius - 3.0, Color::WHITE);

    let fraction = fraction.unwrap_or(0.0).clamp(0.0, 1.0);
    if fraction == 0.0 { return; }
    // Test the moving radial edge with a cross product. One sin/cos pair
    // serves the whole thumbnail, including sectors larger than half a turn.
    let (sin, cos) = (std::f32::consts::TAU * fraction).sin_cos();
    let inner_radius_squared = (radius - 2.5).powi(2);
    let area = cover.intersect(&c.clip());
    for y in area.y..area.bottom() {
        let dy = y as f32 + 0.5 - cy;
        for x in area.x..area.right() {
            let dx = x as f32 + 0.5 - cx;
            if dx * dx + dy * dy > inner_radius_squared { continue; }
            let before_edge = -dx * cos - dy * sin >= 0.0;
            let filled = if fraction <= 0.5 { dx >= 0.0 && before_edge } else { dx >= 0.0 || before_edge };
            if filled { c.blend(x, y, Color::WHITE, 158); }
        }
    }
}

/// A three-pixel baseline on spoken covers. Unheard artwork stays visible.
pub fn listening_progress(c: &mut Canvas, cover: Rect, percent: u8) {
    if percent == 0 { return; }
    let (width, color) = if percent >= 100 {
        (cover.w, Color(0x44A83B))
    } else {
        let width = ((cover.w * i32::from(percent) + 50) / 100).clamp(1, cover.w - 1);
        (width, Color(0x3D8DE6))
    };
    c.fill_rect(Rect::new(cover.x, cover.bottom() - 3, width, 3), color);
}

/// A round progress indicator: a faint track and a clockwise arc from the
/// top for `fraction`; without one, a quarter arc (size unknown).
pub fn progress_ring(c: &mut Canvas, cx: f32, cy: f32, r: f32, fraction: Option<f32>, col: Color, track: Color) {
    use std::f32::consts::{FRAC_PI_2, PI};
    let inner = r - 2.2;
    c.fill_circle(cx, cy, r, inner, track);
    let span = fraction.map_or(FRAC_PI_2, |f| f.clamp(0.0, 1.0) * 2.0 * PI);
    if span <= 0.0 { return; }
    let (a0, a1) = (-FRAC_PI_2, -FRAC_PI_2 + span);
    // fill_arc takes angles in -PI..PI: split an arc that wraps past PI.
    if a1 <= PI { c.fill_arc(cx, cy, r, inner, a0, a1, col); }
    else { c.fill_arc(cx, cy, r, inner, a0, PI, col); c.fill_arc(cx, cy, r, inner, -PI, a1 - 2.0 * PI, col); }
}

#[cfg(test)]
mod download_pie_tests {
    use super::*;
    #[test]
    fn unknown_stays_hollow_and_known_fraction_fills_only_its_sector() {
        let cover = Rect::new(4, 3, 50, 50);
        let art = Color(0x406080);
        let mut canvas = Canvas::new(60, 60);
        canvas.clear(art);
        download_pie(&mut canvas, cover, None);
        let shade = Color::BLACK.over(art, 95);
        assert_eq!(canvas.get(29, 5), Color::WHITE);
        assert_eq!(canvas.get(6, 28), Color::WHITE);
        assert_eq!(canvas.get(4, 3), art);
        assert_eq!(canvas.get(39, 28), shade);
        for (fraction, filled) in [
            (0.0, [false, false, false, false]),
            (0.25, [true, false, false, false]),
            (0.5, [true, true, false, false]),
            (0.75, [true, true, true, false]),
            (1.0, [true; 4]),
        ] {
            canvas.clear(art);
            download_pie(&mut canvas, cover, Some(fraction));
            for ((x, y), filled) in [(39, 18), (39, 38), (19, 38), (19, 18)].into_iter().zip(filled) {
                assert_eq!(canvas.get(x, y), if filled { Color::WHITE.over(shade, 158) } else { shade });
            }
        }
    }
}

#[cfg(test)]
mod listening_progress_tests {
    use super::*;

    #[test]
    fn line_covers_only_the_listened_pixels_even_on_a_downloading_cover() {
        let cover = Rect::new(3, 5, 50, 50);
        let mut canvas = Canvas::new(60, 60);
        canvas.clear(Color(0x42687A));
        download_pie(&mut canvas, cover, Some(0.75));
        let before = canvas.pixels.clone();
        for (percent, width, color) in [
            (0, 0, 0x3D8DE6), (1, 1, 0x3D8DE6), (50, 25, 0x3D8DE6),
            (99, 49, 0x3D8DE6), (100, 50, 0x44A83B),
        ] {
            canvas.pixels.copy_from_slice(&before);
            listening_progress(&mut canvas, cover, percent);
            for y in 0..60 {
                for x in 0..60 {
                    let listened = (52..55).contains(&y) && (3..3 + width).contains(&x);
                    let expected = if listened { color } else { before[(y * 60 + x) as usize] };
                    assert_eq!(canvas.get(x, y), Color(expected), "progress {percent} at ({x}, {y})");
                }
            }
        }
    }
}
