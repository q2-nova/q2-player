#![forbid(unsafe_code)]
//! Picks the AutoEQ catalog model for a Bluetooth device from its name, its
//! class of device and its Device ID record (vendor and product).
//!
//! The name is what the headphones call themselves, often with the owner's
//! name in front ("Alex's AirPods Pro") or a "LE_" prefix. A match needs the
//! model to agree exactly once spaces, punctuation and filler words are gone
//! ("WH-1000XM5" is "Sony WH-1000XM5"), or every word of the model to appear
//! in the name with no model number missing. A brand known from the vendor ID
//! or the name excludes other brands. Speakers, cars and computers get none:
//! AutoEQ measures headphones.

use crate::bluetooth::DeviceId;

/// What is known about the device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hint { pub name: String, pub class: u32, pub id: Option<DeviceId> }

/// Brands by Bluetooth SIG company identifier (Device ID source 1) and USB
/// vendor ID (source 2), spelled as the catalog's model names start.
const BRANDS: [(u16, u16, &str); 9] = [
    (1, 0x004c, "apple"), (1, 0x012d, "sony"), (1, 0x0075, "samsung"), (1, 0x00e0, "google"), (1, 0x009e, "bose"),
    (2, 0x05ac, "apple"), (2, 0x054c, "sony"), (2, 0x04e8, "samsung"), (2, 0x18d1, "google"),
];

/// Apple's product IDs for AirPods, as the catalog names the models. From public
/// device reports; not verified on a Q2. A renamed pair is matched by these alone.
const APPLE_PRODUCTS: [(u16, &str); 9] = [
    (0x2002, "AirPods (1st generation)"), (0x200f, "AirPods (2nd generation)"), (0x2013, "AirPods (3rd generation)"),
    (0x200e, "AirPods Pro"), (0x2014, "AirPods Pro 2"), (0x2024, "AirPods Pro 2"), (0x200a, "AirPods Max"),
    (0x2019, "AirPods 4"), (0x201b, "AirPods 4 (ANC on)"),
];

/// Words that say what the device is rather than which model it is.
const FILLER: [&str; 17] = ["the", "my", "new", "and", "headphones", "headphone", "headset", "earbuds", "earphones", "buds",
    "wireless", "bluetooth", "bt", "le", "tws", "stereo", "audio"];

/// Measurement sources in the order AutoEQ recommends them.
const SOURCES: [&str; 4] = ["oratory1990", "crinacle", "Rtings", "Innerfidelity"];

fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}
fn significant(words: &[String]) -> Vec<String> { words.iter().filter(|w| !FILLER.contains(&w.as_str())).cloned().collect() }
fn has_digit(word: &str) -> bool { word.chars().any(|c| c.is_ascii_digit()) }

/// The device's own name without an owner's possessive or a Bluetooth LE prefix.
fn model_part(name: &str) -> &str {
    let name = name.trim();
    let name = ["LE_", "LE-", "LE "].iter().find_map(|p| name.strip_prefix(p)).unwrap_or(name);
    for marker in ["'s ", "\u{2019}s ", "s' "] {
        if let Some(at) = name.find(marker) { return name[at + marker.len()..].trim() }
    }
    name
}

/// Headphones of any kind: the audio/video major class, minor class not a
/// loudspeaker, car, hi-fi or video device. A class of 0 (unknown) passes.
fn headphone_class(class: u32) -> bool {
    if class == 0 { return true }
    let (major, minor) = (class >> 8 & 0x1f, class >> 2 & 0x3f);
    major == 4 && matches!(minor, 0x00 | 0x01 | 0x02 | 0x06 | 0x07)
}

struct Candidate { brand: String, model: Vec<String>, qualifier: Vec<String> }
fn candidate(name: &str) -> Candidate {
    let (base, qualifier) = match name.find(" (") { Some(at) => (&name[..at], &name[at..]), None => (name, "") };
    let mut model = words(base);
    let brand = if model.is_empty() { String::new() } else { model.remove(0) };
    Candidate { brand, model, qualifier: words(qualifier) }
}

/// The index of the catalog entry (name, path) for the device, if one matches.
pub fn best(hint: &Hint, entries: &[(&str, &str)]) -> Option<usize> {
    if !headphone_class(hint.class) { return None }
    let id = hint.id.filter(|id| id.source == 1 || id.source == 2);
    let mut brand = id.and_then(|id| BRANDS.iter().find(|b| b.0 == id.source && b.1 == id.vendor)).map(|b| b.2.to_owned());
    let product = id.filter(|id| brand.as_deref() == Some("apple") && id.source == 1)
        .and_then(|id| APPLE_PRODUCTS.iter().find(|p| p.0 == id.product)).map(|p| p.1);
    let (mut device, device_qualifier) = match product {
        Some(model) => { let c = candidate(&format!("x {model}")); (c.model, c.qualifier) }
        None => (significant(&words(model_part(&hint.name))), Vec::new()),
    };
    let candidates: Vec<Candidate> = entries.iter().map(|(name, _)| candidate(name)).collect();
    // A brand named first in the device's name.
    if let Some(first) = device.first().cloned() {
        if candidates.iter().any(|c| c.brand == first) {
            if brand.is_none() { brand = Some(first.clone()); }
            if brand.as_deref() == Some(first.as_str()) { device.remove(0); }
        }
    }
    if device.is_empty() { return None }
    let device_compact: String = device.concat();
    let mut ranked: Vec<(u32, usize)> = Vec::new();
    for (index, c) in candidates.iter().enumerate() {
        if brand.as_deref().is_some_and(|b| b != c.brand) || c.model.is_empty() { continue }
        let model = significant(&c.model);
        let score = if model.concat() == device_compact || c.model.concat() == device_compact { 100 }
            // Every word of the model in the name, and no model number of the name missing from it.
            else if model.iter().all(|w| device.contains(w)) && device.iter().filter(|w| has_digit(w)).all(|w| model.contains(w)) && model.iter().any(|w| has_digit(w)) {
                60 + model.len() as u32
            } else { continue };
        // The plain measurement before variants (ANC modes, ear tips, presets), unless the device names the variant.
        let qualifier = if c.qualifier == device_qualifier { 20 } else if c.qualifier.is_empty() { 10 } else { 0 };
        let path = entries[index].1;
        let source = SOURCES.iter().position(|s| path.starts_with(&format!("{s}/"))).map_or(0, |p| (SOURCES.len() - p) as u32);
        // Headphones over the ear for the headphones class, in the ear for a wearable headset.
        let minor = hint.class >> 2 & 0x3f;
        let shape = u32::from((minor == 0x06 && path.contains("over-ear")) || (minor == 0x01 && path.contains("in-ear")));
        ranked.push((score * 1000 + qualifier * 20 + source * 2 + shape, index));
    }
    // Highest rank; among equals, the first in the catalog.
    ranked.iter().max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1))).map(|r| r.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic catalog in the index's shape (model name, source path).
    const CATALOG: [(&str, &str); 16] = [
        ("Apple AirPods (1st generation)", "Rtings/in-ear/Apple%20AirPods%20(1st%20generation)"),
        ("Apple AirPods (2nd generation)", "Rtings/in-ear/Apple%20AirPods%20(2nd%20generation)"),
        ("Apple AirPods Max", "oratory1990/over-ear/Apple%20AirPods%20Max"),
        ("Apple AirPods Pro", "crinacle/711%20in-ear/Apple%20AirPods%20Pro"),
        ("Apple AirPods Pro 2", "Generated/in-ear/Apple%20AirPods%20Pro%202"),
        ("Apple AirPods Pro 2 (ANC mode)", "Generated/in-ear/Apple%20AirPods%20Pro%202%20(ANC%20mode)"),
        ("Generated Model 7", "Generated/over-ear/Generated%20Model%207"),
        ("Samsung Galaxy Buds2 Pro", "Generated/in-ear/Samsung%20Galaxy%20Buds2%20Pro"),
        ("Sony WF-1000XM4", "oratory1990/in-ear/Sony%20WF-1000XM4"),
        ("Sony WF-1000XM4 (ANC on)", "Super%20Review/in-ear/Sony%20WF-1000XM4%20(ANC%20on)"),
        ("Sony WH-1000XM4", "oratory1990/over-ear/Sony%20WH-1000XM4"),
        ("Sony WH-1000XM4 (ANC Off)", "HypetheSonics/over-ear/Sony%20WH-1000XM4%20(ANC%20Off)"),
        ("Sony WH-1000XM5", "Generated/over-ear/Sony%20WH-1000XM5"),
        ("Sony WH-1000XM5", "oratory1990/over-ear/Sony%20WH-1000XM5"),
        ("Sony WH-1000XM6", "Kuulokenurkka/over-ear/Sony%20WH-1000XM6"),
        ("Google Pixel Buds Pro", "Generated/in-ear/Google%20Pixel%20Buds%20Pro"),
    ];
    const HEADPHONES: u32 = 0x24_0418;
    const HEADSET: u32 = 0x24_0404;

    fn pick(name: &str, class: u32, id: Option<DeviceId>) -> Option<&'static str> {
        best(&Hint { name: name.into(), class, id }, &CATALOG).map(|i| CATALOG[i].1)
    }
    fn apple(product: u16) -> Option<DeviceId> { Some(DeviceId { source: 1, vendor: 0x004c, product, version: 0 }) }
    fn sony() -> Option<DeviceId> { Some(DeviceId { source: 1, vendor: 0x012d, product: 0x0d58, version: 0 }) }

    #[test]
    fn model_names_match_with_or_without_brand_punctuation_and_prefixes() {
        let xm5 = Some("oratory1990/over-ear/Sony%20WH-1000XM5");
        for name in ["WH-1000XM5", "Sony WH-1000XM5", "LE_WH-1000XM5", "wh1000xm5", "WH-1000XM5 Wireless Headphones", "Alex's WH-1000XM5"] {
            assert_eq!(pick(name, HEADPHONES, None), xm5, "{name}");
        }
        // The vendor agrees; a number of its own decides between neighbours.
        assert_eq!(pick("WH-1000XM5", HEADPHONES, sony()), xm5);
        assert_eq!(pick("WH-1000XM4", HEADPHONES, None), Some("oratory1990/over-ear/Sony%20WH-1000XM4"), "the plain measurement before ANC variants");
        assert_eq!(pick("WF-1000XM4", HEADSET, None), Some("oratory1990/in-ear/Sony%20WF-1000XM4"));
        assert_eq!(pick("Galaxy Buds2 Pro", HEADSET, None), Some("Generated/in-ear/Samsung%20Galaxy%20Buds2%20Pro"));
        assert_eq!(pick("Pixel Buds Pro", HEADSET, None), Some("Generated/in-ear/Google%20Pixel%20Buds%20Pro"), "filler words are dropped on both sides");
    }

    #[test]
    fn owners_names_are_stripped_and_numbers_must_agree() {
        assert_eq!(pick("Alex's AirPods Pro", HEADPHONES, None), Some("crinacle/711%20in-ear/Apple%20AirPods%20Pro"));
        assert_eq!(pick("Sam\u{2019}s AirPods Pro 2", HEADPHONES, None), Some("Generated/in-ear/Apple%20AirPods%20Pro%202"));
        assert_eq!(pick("AirPods Max", HEADPHONES, None), Some("oratory1990/over-ear/Apple%20AirPods%20Max"));
        // A model number in the name that no model has: no guess.
        assert_eq!(pick("WH-1000XM9", HEADPHONES, None), None);
        assert_eq!(pick("Generated Model 8", HEADPHONES, None), None);
        assert_eq!(pick("Generated Model 7 Studio", HEADPHONES, None), Some("Generated/over-ear/Generated%20Model%207"), "every model word present, numbers agree");
    }

    #[test]
    fn device_ids_name_the_brand_and_renamed_airpods() {
        assert_eq!(pick("Kitchen", HEADPHONES, apple(0x200e)), Some("crinacle/711%20in-ear/Apple%20AirPods%20Pro"));
        assert_eq!(pick("Kitchen", HEADPHONES, apple(0x2014)), Some("Generated/in-ear/Apple%20AirPods%20Pro%202"));
        assert_eq!(pick("Alex's AirPods", HEADPHONES, apple(0x200f)), Some("Rtings/in-ear/Apple%20AirPods%20(2nd%20generation)"));
        assert_eq!(pick("Kitchen", HEADPHONES, apple(0x7777)), None, "an unknown product and a name that is no model");
        // A known vendor excludes other brands' models of the same name.
        let other = Some(DeviceId { source: 1, vendor: 0x0075, product: 1, version: 0 });
        assert_eq!(pick("WH-1000XM5", HEADPHONES, other), None);
        // An unknown vendor is no constraint.
        assert_eq!(pick("WH-1000XM5", HEADPHONES, Some(DeviceId { source: 1, vendor: 0x7fff, product: 1, version: 0 })), Some("oratory1990/over-ear/Sony%20WH-1000XM5"));
    }

    #[test]
    fn speakers_computers_and_generic_names_get_no_profile() {
        for (name, class) in [("WH-1000XM5", 0x24_0414), ("WH-1000XM5", 0x7c_0104), ("WH-1000XM5", 0x20_0420)] {
            assert_eq!(pick(name, class, None), None, "{name} {class:#08x}");
        }
        for name in ["Headphones", "Fixture Headphones", "Wireless Earbuds", "", "  ", "Alex's", "LE_"] {
            assert_eq!(pick(name, HEADPHONES, None), None, "{name:?}");
        }
        assert_eq!(pick("WH-1000XM5", 0, None), Some("oratory1990/over-ear/Sony%20WH-1000XM5"), "an unknown class passes");
    }

    #[test]
    fn the_packaged_catalog_has_the_common_models() {
        let entries = crate::audio::autoeq::search("");
        let pairs: Vec<(&str, &str)> = entries.iter().map(|e| (e.name(), e.path())).collect();
        let name = |hint: &str, id: Option<DeviceId>| best(&Hint { name: hint.into(), class: HEADPHONES, id }, &pairs).map(|i| pairs[i].0);
        assert_eq!(name("WH-1000XM5", None), Some("Sony WH-1000XM5"));
        assert_eq!(name("LE_WH-1000XM4", sony()), Some("Sony WH-1000XM4"));
        assert_eq!(name("Alex's AirPods Pro", None), Some("Apple AirPods Pro"));
        assert_eq!(name("AirPods Max", apple(0x200a)), Some("Apple AirPods Max"));
        assert_eq!(name("Kitchen", apple(0x2014)).map(|n| n.to_lowercase().replace(' ', "")), Some("appleairpodspro2".into()));
        assert_eq!(name("Fixture Headphones", None), None);
    }
}
