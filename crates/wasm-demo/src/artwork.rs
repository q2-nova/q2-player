//! Original placeholders for releases whose sleeve artwork is not bundled.
use crate::gfx::Image;

pub fn cover_image(id: &str, size: u32) -> Image {
    let bytes: &[u8] = match id {
        "tidal" => include_bytes!("../art/tidal.jpg"),
        "northline" => include_bytes!("../art/northline.jpg"),
        "sundial" => include_bytes!("../art/sundial.jpg"),
        "copper" => include_bytes!("../art/copper.jpg"),
        _ => return Image::placeholder(size),
    };
    Image::decode_square(bytes, size).unwrap_or_else(|| Image::placeholder(size))
}
