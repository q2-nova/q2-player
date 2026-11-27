//! Bounded login QR rendering; encoder provenance is in QRCODEGEN.md.
use super::{Canvas, Color, Rect};
use super::qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};

pub struct Code(QrCode);
impl Code {
    pub fn for_url(url: &str) -> Option<Self> {
        // Bound allocation/work before encoding. Oversized links retain text UI.
        if url.is_empty() || url.len() > 213 { return None; }
        let segments = [QrSegment::make_bytes(url.as_bytes())];
        QrCode::encode_segments_advanced(&segments, QrCodeEcc::Medium,
            Version::MIN, Version::new(10), None, false).ok().map(Self)
    }

    /// Includes the four-module white quiet zone; never blur/resample modules.
    pub fn side(&self, maximum: i32) -> i32 {
        let modules = self.0.size() + 8;
        let scale = maximum / modules;
        if scale < 2 { 0 } else { modules * scale }
    }

    pub fn draw(&self, canvas: &mut Canvas, x: i32, y: i32, maximum: i32) -> i32 {
        let side = self.side(maximum);
        if side == 0 { return 0; }
        let scale = side / (self.0.size() + 8);
        canvas.fill_rect(Rect::new(x, y, side, side), Color::WHITE);
        for row in 0..self.0.size() {
            for col in 0..self.0.size() {
                if self.0.get_module(col, row) {
                    canvas.fill_rect(Rect::new(x + (col + 4) * scale,
                        y + (row + 4) * scale, scale, scale), Color::BLACK);
                }
            }
        }
        side
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use md5::{Digest, Md5};
    const URL: &str = "https://login.tailscale.com/a/example";

    #[test]
    fn approval_url_matches_independent_libqrencode_reference() {
        // libqrencode.so.4: QRinput_new2(0, QR_ECLEVEL_M), byte segment,
        // QRcode_encodeMask(input, mask). Hash one 0/1 byte per module, row-major.
        let expected = ["e842a009ecf8b775b0c87fee22156aa8", "a0d91d1cd44f7b88dd7587e03e3451ec",
            "fe4f9737a2d7b8d57386c4be67a87713", "93f77114e881bd34c0faf37d8f943595",
            "83e09327569ee34d6108300dddc3955e", "ddd7cebec8a4bb7e3b39f90daafb338f",
            "ca9946bc35aacbdfd41f3f371b169bf6", "0715a01fe160554116875ddffc7a3c4b"];
        let code = Code::for_url(URL).unwrap();
        assert_eq!(code.0.size(), 29);
        let mut digest = Md5::new();
        for y in 0..29 { for x in 0..29 { digest.update([u8::from(code.0.get_module(x, y))]); } }
        assert_eq!(format!("{:x}", digest.finalize()), expected[usize::from(code.0.mask().value())]);
    }

    #[test]
    fn render_has_integer_modules_and_complete_quiet_zone() {
        let code = Code::for_url(URL).unwrap();
        let mut canvas = Canvas::new(180, 180);
        canvas.clear(Color(0x123456));
        assert_eq!(code.draw(&mut canvas, 8, 8, 160), 148);
        for y in 0..148 { for x in 0..148 {
            let col = x / 4 - 4;
            let row = y / 4 - 4;
            let expected = if code.0.get_module(col, row) { Color::BLACK } else { Color::WHITE };
            assert_eq!(canvas.pixels[((y+8)*180+x+8) as usize], expected.0);
        } }
        assert_eq!(canvas.pixels[0], 0x123456);
        assert!(Code::for_url(&"x".repeat(214)).is_none());
        assert_eq!(code.side(60), 0, "do not display a tiny/unscannable QR");
    }
}
