//! Visual preferences shared by the device and browser.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RatingControls { #[default] Stars, Hearts, Both }
impl RatingControls {
    pub const ALL: [Self; 3] = [Self::Stars, Self::Hearts, Self::Both];
    pub fn label(self) -> &'static str { match self { Self::Stars => "Stars", Self::Hearts => "Hearts", Self::Both => "Both" } }
}

/// The selection bar's colour; the classic's is blue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    Blue,
    Graphite,
    Green,
    Orange,
    Pink,
    Purple,
    Red,
}

impl Accent {
    pub const ALL: [Accent; 7] =
        [Accent::Blue, Accent::Graphite, Accent::Green, Accent::Orange, Accent::Pink, Accent::Purple, Accent::Red];

    pub fn label(self) -> &'static str {
        match self {
            Accent::Blue => "Blue",
            Accent::Graphite => "Graphite",
            Accent::Green => "Green",
            Accent::Orange => "Orange",
            Accent::Pink => "Pink",
            Accent::Purple => "Purple",
            Accent::Red => "Red",
        }
    }
}

/// White lists as the iPod classic, or black ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Look {
    #[default]
    Classic,
    Dark,
}
