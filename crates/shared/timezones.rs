//! Time-zone labels, rows and confirmation shared by the device and browser.
use crate::ui::{Action, Ctx, list::{Badge, Row, RowAction}, screens::controls::Confirm};
use std::sync::OnceLock;

pub const UTC: &str = "Etc/UTC";
pub const ZONE_COMMAND: u32 = 1000;

// Keep the exact stored POSIX rules from the stock picker.
const FIXED: &[(&str, &str, i16)] = &[
    ("<-12>12", "Baker Island", -720),
    ("CST6", "Mexico City (Fixed)", -360),
    ("<+0330>-3:30", "Tehran (Fixed)", 210),
    ("<+12>-12", "Fiji (Fixed)", 720),
];

pub struct Zone {
    pub id: &'static str,
    pub label: String,
    standard_offset: i16,
}

fn zone(id: &'static str, city: &str, standard_offset: i16) -> Zone {
    let sign = if standard_offset < 0 { '-' } else { '+' };
    let minutes = standard_offset.unsigned_abs();
    let (hours, minutes) = (minutes / 60, minutes % 60);
    let offset = if minutes == 0 {
        format!("GMT{sign}{hours}")
    } else {
        format!("GMT{sign}{hours}:{minutes:02}")
    };
    Zone { id, label: format!("{offset} {city}"), standard_offset }
}

fn zones() -> &'static [Zone] {
    static ZONES: OnceLock<Vec<Zone>> = OnceLock::new();
    ZONES.get_or_init(|| {
        let mut zones = vec![zone(UTC, "UTC", 0)];
        let ids = include_str!("zone.tab").lines().filter_map(|line| {
            if line.starts_with('#') { None } else { line.split('\t').nth(2) }
        });
        let offsets = include_str!("standard-offsets.tsv").lines().filter_map(|line| line.split_once('\t'));
        assert_eq!(ids.clone().count(), offsets.clone().count(), "incomplete standard offsets");
        for (id, (offset_id, offset)) in ids.zip(offsets) {
            assert_eq!(id, offset_id, "offset table must match its IANA source");
            let (_, path) = id.split_once('/').expect("IANA geographical zone");
            let (subregion, city) = path.rsplit_once('/').unwrap_or(("", path));
            let mut city = city.replace('_', " ");
            if !subregion.is_empty() {
                city.push_str(&format!(" ({})", subregion.replace('_', " ").replace('/', ", ")));
            }
            match id {
                "America/St_Johns" => city = "St. John's".into(),
                "Australia/Lord_Howe" => city = "L. Howe".into(),
                _ => {}
            }
            zones.push(zone(id, &city, offset.parse().expect("minute offset")));
        }
        zones.extend(FIXED.iter().map(|&(id, city, offset)| zone(id, city, offset)));
        zones.sort_unstable_by(|a, b| (a.standard_offset, &a.label, a.id).cmp(&(b.standard_offset, &b.label, b.id)));
        zones
    })
}

fn selected(id: &str) -> Option<&'static Zone> {
    let id = if matches!(id, "" | "UTC") { UTC } else { id };
    zones().iter().find(|zone| zone.id == id)
}

pub fn label(id: &str) -> &str {
    match selected(id) {
        Some(zone) if zone.id == UTC => "UTC",
        Some(zone) => &zone.label,
        None => id,
    }
}

pub fn rows(configured: &str) -> Vec<Row> {
    let current = selected(configured);
    let mut rows: Vec<_> = zones().iter().enumerate().map(|(index, zone)| {
        let row = Row::new(zone.label.as_str(), RowAction::Cmd(ZONE_COMMAND + index as u32)).key(zone.id);
        if current.is_some_and(|current| current.id == zone.id) { row.badge(Badge::Check) } else { row }
    }).collect();
    if current.is_none() {
        rows.insert(0, Row::new(configured, RowAction::None).badge(Badge::Check));
    }
    rows
}

pub fn change(command: u32, configured: &str, apply: fn(&mut Ctx, &'static str) -> Action) -> Action {
    let Some(zone) = command.checked_sub(ZONE_COMMAND).and_then(|index| zones().get(index as usize)) else {
        return Action::None;
    };
    if selected(configured).is_some_and(|current| current.id == zone.id) {
        return Action::Pop;
    }
    Action::Push(Box::new(Confirm::new(
        "Change Time Zone?",
        "Applying this change restarts\nthe app and interrupts playback.",
        "Apply & Restart",
        move |ctx| apply(ctx, zone.id),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_order_offsets_and_legacy_ids() {
        let all = zones();
        let ids: std::collections::BTreeSet<_> = all.iter().map(|zone| zone.id).collect();
        assert_eq!(ids.len(), all.len());
        assert_eq!(all.len(), 423);
        assert!(all.windows(2).all(|pair| pair[0].standard_offset <= pair[1].standard_offset));
        assert_eq!(all.first().unwrap().id, "<-12>12");
        assert_eq!(all.last().unwrap().id, "Pacific/Kiritimati");
        for (id, offset, expected) in [
            ("America/Chicago", -360, "GMT-6 Chicago"),
            ("America/St_Johns", -210, "GMT-3:30 St. John's"),
            ("Asia/Kathmandu", 345, "GMT+5:45 Kathmandu"),
            ("Australia/Eucla", 525, "GMT+8:45 Eucla"),
            ("Pacific/Chatham", 765, "GMT+12:45 Chatham"),
            ("CST6", -360, "GMT-6 Mexico City (Fixed)"),
            ("<+12>-12", 720, "GMT+12 Fiji (Fixed)"),
        ] {
            let zone = selected(id).unwrap();
            assert_eq!((zone.standard_offset, zone.label.as_str()), (offset, expected));
        }
        assert_eq!(selected("UTC").unwrap().id, UTC);
        assert_eq!(label("US/Eastern"), "US/Eastern");
    }
}
