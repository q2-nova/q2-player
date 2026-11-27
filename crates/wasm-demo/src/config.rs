#[path = "../../q2nd/src/config/appearance.rs"]
mod appearance;
pub use appearance::{Accent, Look, RatingControls};
#[path = "../../q2nd/src/config/options.rs"]
pub(crate) mod options;

#[derive(Default)]
pub struct Config {
    pub display: Display,
    pub audiobookshelf: Shelf,
}

#[derive(Default)]
pub struct Display {
    pub rating_controls: RatingControls,
}

#[derive(Default)]
pub struct Shelf {
    pub url: String,
    pub token: String,
}
