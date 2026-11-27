//! Group pages built from the Books list the sync already stores, so series
//! and author pages need no requests of their own. A minified Audiobookshelf item names
//! its series (`Name #sequence`), authors and narrators joined with ", ", and
//! lists its genres; it carries no series or author ids. The ids come from
//! the Series and Authors lists the sync fetches anyway (one page per 50
//! groups, usually a 304). A group whose derived book count differs from the
//! count its list row gives (a name containing ", ", two groups with one
//! name, a series whose order the rows cannot settle) is fetched instead.
use super::{Entry, EntryKind, Facts, Section};
use std::collections::HashMap;

/// The kinds of group page this module can build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind { Series, Author, Narrator, Genre }
impl Kind {
    pub(super) fn of(section: &Section) -> Option<Kind> {
        match section {
            Section::BySeries(_) => Some(Kind::Series),
            Section::ByAuthor(_) => Some(Kind::Author),
            Section::ByNarrator(_) => Some(Kind::Narrator),
            Section::ByGenre(_) => Some(Kind::Genre),
            _ => None,
        }
    }
    /// The list section whose rows name this kind's groups.
    pub(super) fn list(self) -> Section {
        match self { Kind::Series => Section::Series, Kind::Author => Section::Authors, Kind::Narrator => Section::Narrators, Kind::Genre => Section::Genres }
    }
}

/// `Name #1, Other #2.5` as Audiobookshelf writes `seriesName`: each series
/// with its sequence, or none.
fn series_of(text: &str) -> Vec<(String, Option<String>)> {
    split(text).into_iter().map(|token| match token.rsplit_once(" #") {
        Some((name, sequence)) if !sequence.is_empty() => (name.to_owned(), Some(sequence.to_owned())),
        _ => (token, None),
    }).collect()
}
fn split(text: &str) -> Vec<String> {
    text.split(", ").map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}
/// A sequence as Audiobookshelf orders it: numerically where it is a number.
fn sequence_key(sequence: &Option<String>) -> (u8, f64, String) {
    match sequence.as_deref().map(str::trim) {
        Some(s) => match s.parse::<f64>() { Ok(n) if n.is_finite() => (0, n, String::new()), _ => (1, 0.0, s.to_lowercase()) },
        None => (2, 0.0, String::new()),
    }
}

/// A group from its list: the section it opens, its name, its book count.
pub(super) struct Group { pub section: Section, pub name: String, pub count: Option<u64> }
pub(super) fn groups(entries: Vec<Entry>, facts: Vec<Facts>) -> Vec<Group> {
    let mut facts = facts.into_iter();
    entries.into_iter().filter_map(|entry| {
        let fact = facts.next().unwrap_or_default();
        match entry.kind { EntryKind::Group { section } => Some(Group { section, name: entry.title, count: fact.count }), _ => None }
    }).collect()
}

/// The pages of each group of `kind` from the library's book rows, in the
/// order the server pages them: a series by sequence, an author, narrator
/// or genre by when the book was added. Returns the derived groups (section,
/// its entries with their facts) and the sections that must be fetched.
pub(super) fn derive(kind: Kind, books: &[(Entry, Facts)], groups: &[Group]) -> (Vec<(Section, Vec<(Entry, Facts)>)>, Vec<Section>) {
    // The group names each book belongs to, with a series' sequence.
    let mut members: HashMap<String, Vec<(usize, Option<String>)>> = HashMap::new();
    for (index, (_, facts)) in books.iter().enumerate() {
        let names: Vec<(String, Option<String>)> = match kind {
            Kind::Series => facts.series.as_deref().map(series_of).unwrap_or_default(),
            Kind::Author => facts.authors.as_deref().map(split).unwrap_or_default().into_iter().map(|n| (n, None)).collect(),
            Kind::Narrator => facts.narrators.as_deref().map(split).unwrap_or_default().into_iter().map(|n| (n, None)).collect(),
            Kind::Genre => facts.genres.iter().cloned().map(|n| (n, None)).collect(),
        };
        for (name, sequence) in names { members.entry(name).or_default().push((index, sequence)); }
    }
    let mut named: HashMap<&str, usize> = HashMap::new();
    for group in groups { *named.entry(group.name.as_str()).or_default() += 1; }
    let (mut derived, mut fetch) = (Vec::new(), Vec::new());
    for group in groups {
        let found = members.get(&group.name).map(Vec::as_slice).unwrap_or(&[]);
        let unique = named.get(group.name.as_str()) == Some(&1);
        // Genres carry no count; the others must agree with their row.
        let agrees = match kind { Kind::Genre => !found.is_empty(), _ => group.count == Some(found.len() as u64) };
        // A series pages by sequence and breaks ties by an order the rows do
        // not show (15 of 52 series pages on one real server): one with a
        // missing, repeated or non-numeric sequence is fetched.
        let ordered = kind != Kind::Series || {
            let mut numbers: Vec<f64> = Vec::new();
            found.iter().all(|(_, s)| s.as_deref().and_then(|s| s.trim().parse::<f64>().ok()).filter(|n| n.is_finite()).map(|n| numbers.push(n)).is_some())
                && { numbers.sort_by(f64::total_cmp); numbers.windows(2).all(|w| w[0] < w[1]) }
        };
        let agrees = agrees && ordered;
        if !unique || !agrees { fetch.push(group.section.clone()); continue; }
        let mut rows: Vec<&(usize, Option<String>)> = found.iter().collect();
        match kind {
            Kind::Series => rows.sort_by(|a, b| sequence_key(&a.1).partial_cmp(&sequence_key(&b.1)).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| books[a.0].1.added.cmp(&books[b.0].1.added))),
            _ => rows.sort_by(|a, b| books[a.0].1.added.cmp(&books[b.0].1.added).then_with(|| books[a.0].0.id.cmp(&books[b.0].0.id))),
        }
        derived.push((group.section.clone(), rows.into_iter().map(|(index, _)| books[*index].clone()).collect()));
    }
    (derived, fetch)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn series_names_and_sequences_read_as_audiobookshelf_writes_them() {
        assert_eq!(series_of("Saga #1, Other #2.5, Plain"), vec![("Saga".into(), Some("1".into())), ("Other".into(), Some("2.5".into())), ("Plain".into(), None)]);
        let mut keys = [Some("10".to_string()), Some("2".into()), None, Some("1.5".into())].map(|s| sequence_key(&s));
        keys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(keys.iter().map(|k| k.1).collect::<Vec<_>>(), [1.5, 2.0, 10.0, 0.0]);
    }
}
