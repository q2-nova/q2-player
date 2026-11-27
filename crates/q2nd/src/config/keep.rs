//! Settings tables the player does not model, kept as written.
//!
//! A save serializes the typed [`Config`](super::Config), which knows only
//! its own sections. Every other top-level key in the file being replaced
//! (`[portal]` with its accounts, or a table added by hand or by another
//! build) is appended after them. A table written under its own headers is
//! copied byte for byte, with the comments directly above each header. A key
//! that cannot be cut out that way (a root-level value, an inline or dotted
//! key) is written again from its parsed value. The result is checked by
//! parsing it again; if any kept value differs, every kept key is written
//! from its parsed value instead.

/// Top-level keys the typed `Config` writes. `portal` is read but never
/// written from the typed value, so its text is always kept.
pub(super) const MODELED: [&str; 14] = [
    "server", "audiobookshelf", "spoken", "playback", "listenbrainz", "lastfm", "storage",
    "display", "power", "wifi", "tailscale", "bluetooth", "telemetry", "diagnostics",
];

/// `serialized` (the typed settings) followed by the unmodeled keys of
/// `previous` (the file being replaced).
pub(super) fn with_unmodeled(serialized: Vec<u8>, previous: Option<&[u8]>) -> Vec<u8> {
    let Some(previous) = previous.and_then(|bytes| std::str::from_utf8(bytes).ok()) else { return serialized };
    let Ok(table) = previous.parse::<toml::Table>() else { return serialized };
    let unmodeled: Vec<&String> = table.keys().filter(|key| !MODELED.contains(&key.as_str())).collect();
    if unmodeled.is_empty() {
        return serialized;
    }
    let sections = sections(previous);
    let typed_all = assemble(&serialized, &table, &unmodeled, |_| None);
    let kept = assemble(&serialized, &table, &unmodeled, |key| {
        let text: String = sections.iter().filter(|(owner, _)| owner.as_deref() == Some(key))
            .map(|(_, text)| text.as_str()).collect();
        let text = without_edge_blank_lines(&text);
        text.parse::<toml::Table>().is_ok_and(|parsed| parsed.len() == 1 && parsed.get(key) == table.get(key))
            .then_some(text)
    });
    for candidate in [kept, typed_all] {
        if verified(&candidate, &serialized, &table, &unmodeled) {
            return candidate.into_bytes();
        }
    }
    log::warn!("Could not keep {} unmodeled settings keys", unmodeled.len());
    serialized
}

/// The typed text with each unmodeled key added: its exact text from
/// `exact`, or else its parsed value. Plain values go before the first table.
fn assemble(serialized: &[u8], table: &toml::Table, unmodeled: &[&String],
            exact: impl Fn(&str) -> Option<String>) -> String {
    let typed_text = String::from_utf8_lossy(serialized).into_owned();
    let mut root = String::new();
    let mut tables = String::new();
    for key in unmodeled {
        match exact(key) {
            Some(text) => push_section(&mut tables, &text),
            None => {
                let text = typed(table, &[key]);
                if text.trim_start().starts_with('[') { push_section(&mut tables, &text) } else { root.push_str(&text) }
            }
        }
    }
    let first_table = typed_text.match_indices('\n').map(|(at, _)| at + 1)
        .find(|&at| typed_text[at..].starts_with('[')).unwrap_or(typed_text.len());
    let mut out = String::with_capacity(typed_text.len() + root.len() + tables.len());
    out.push_str(&typed_text[..first_table]);
    out.push_str(&root);
    if !root.is_empty() { out.push('\n'); }
    out.push_str(&typed_text[first_table..]);
    push_section(&mut out, &tables);
    out
}

fn push_section(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
}

fn typed(table: &toml::Table, keys: &[&String]) -> String {
    let subset: toml::Table = keys.iter().filter_map(|key| table.get(*key).map(|v| ((*key).clone(), v.clone()))).collect();
    toml::to_string(&subset).unwrap_or_default()
}

fn verified(text: &str, serialized: &[u8], previous: &toml::Table, unmodeled: &[&String]) -> bool {
    let (Ok(parsed), Some(modeled)) = (text.parse::<toml::Table>(),
        std::str::from_utf8(serialized).ok().and_then(|s| s.parse::<toml::Table>().ok())) else { return false };
    let mut expected = modeled;
    for key in unmodeled {
        expected.insert((*key).clone(), previous[key.as_str()].clone());
    }
    parsed == expected
}

/// The file split at table headers: each piece's first header key (none for
/// the root) and its text. Comment and blank lines directly above a header
/// belong to that header.
fn sections(text: &str) -> Vec<(Option<String>, String)> {
    let mut sections: Vec<(Option<String>, String)> = vec![(None, String::new())];
    let mut quote: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let header = if quote.is_none() { header_key(line) } else { None };
        quote = multiline_state(line, quote);
        match header {
            Some(owner) => {
                let previous = &mut sections.last_mut().expect("root section").1;
                let lead = leading_comments(previous);
                let moved = previous.split_off(previous.len() - lead);
                sections.push((Some(owner), moved + line));
            }
            None => sections.last_mut().expect("root section").1.push_str(line),
        }
    }
    sections
}

/// The first key of a `[table]` or `[[array]]` header line.
fn header_key(line: &str) -> Option<String> {
    if !line.trim_start().starts_with('[') {
        return None;
    }
    let parsed: toml::Table = format!("{}\n", line.trim_end()).parse().ok()?;
    let mut keys = parsed.keys();
    let key = keys.next()?.clone();
    keys.next().is_none().then_some(key)
}

/// Whether a multi-line string is open after `line`, tracking `"""` and `'''`.
fn multiline_state<'a>(line: &str, mut open: Option<&'a str>) -> Option<&'a str> {
    let mut rest = line;
    loop {
        match open {
            Some(delimiter) => match rest.find(delimiter) {
                Some(at) => { rest = &rest[at + 3..]; open = None; }
                None => return open,
            },
            None => {
                let next = [("\"\"\"", rest.find("\"\"\"")), ("'''", rest.find("'''"))].into_iter()
                    .filter_map(|(d, at)| at.map(|at| (at, d))).min();
                let comment = rest.find('#');
                match next {
                    Some((at, delimiter)) if comment.is_none_or(|c| c > at) => {
                        rest = &rest[at + 3..];
                        open = Some(delimiter);
                    }
                    _ => return None,
                }
            }
        }
    }
}

/// `text` without whole blank lines before its first or after its last
/// other line; the separator between kept sections is written once.
fn without_edge_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let first = lines.iter().position(|line| !line.trim().is_empty()).unwrap_or(lines.len());
    let last = lines.iter().rposition(|line| !line.trim().is_empty()).map_or(first, |at| at + 1);
    lines[first..last.max(first)].concat()
}

/// Bytes of trailing comment and blank lines at the end of `text`.
fn leading_comments(text: &str) -> usize {
    let mut keep = 0;
    for line in text.split_inclusive('\n').rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            keep += line.len();
        } else {
            break;
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    // Generated values only.
    const HAND: &str = "# Q2 settings\n[server]\nurl = \"http://192.0.2.1\"\nuser = \"generated\"\n\n\
        # Delta account, typed on the host\n[portal.delta]\nskymiles_number = 9000000001   # generated\n\
        password = 'generated-password'\n\n[playback]\nvolume = 40\n\n[future_feature]\nmode = \"\"\"\n[not.a.header]\n\"\"\"\n\
        list = [1, 2]\n\n[[future_feature.rows]]\nname = \"row\"\n";

    fn save(config: &Config, previous: &str) -> String {
        let serialized = config.serialized().unwrap();
        String::from_utf8(with_unmodeled(serialized, Some(previous.as_bytes()))).unwrap()
    }

    #[test]
    fn modeled_keys_cover_every_written_section() {
        let mut config = Config::default();
        config.listenbrainz.enabled = true;
        config.lastfm = toml::from_str("enabled = true").unwrap();
        config.diagnostics.playtrace = true;
        let written: toml::Table = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        for key in written.keys() {
            assert!(MODELED.contains(&key.as_str()), "{key} is written but not listed");
        }
        assert!(!written.contains_key("portal"));
    }

    #[test]
    fn a_settings_save_keeps_unmodeled_tables_byte_for_byte() {
        let mut config = Config::parse(HAND, "generated");
        assert!(config.portal.delta.as_ref().unwrap().sign_in().is_some());
        config.playback.volume = 55;
        let saved = save(&config, HAND);
        let portal = "# Delta account, typed on the host\n[portal.delta]\nskymiles_number = 9000000001   # generated\n\
            password = 'generated-password'\n";
        let future = "[future_feature]\nmode = \"\"\"\n[not.a.header]\n\"\"\"\nlist = [1, 2]\n\n[[future_feature.rows]]\nname = \"row\"\n";
        assert!(saved.contains(portal), "{saved}");
        assert!(saved.contains(future), "{saved}");
        let reloaded = Config::parse(&saved, "generated");
        assert_eq!(reloaded.playback.volume, 55);
        assert_eq!(reloaded.portal, config.portal);
        // A second save is stable.
        assert_eq!(save(&reloaded, &saved), saved);
    }

    #[test]
    fn inline_and_root_level_unmodeled_keys_keep_their_values() {
        let previous = "note = \"generated\"\nportal = { delta = { username = \"generated-user\", password = \"generated-pass\" } }\n\
            [server]\nurl = \"http://192.0.2.1\"\n";
        let config = Config::parse(previous, "generated");
        let saved: toml::Table = save(&config, previous).parse().unwrap();
        let original: toml::Table = previous.parse().unwrap();
        assert_eq!(saved["note"], original["note"]);
        assert_eq!(saved["portal"], original["portal"]);
    }

    #[test]
    fn account_fields_prefer_username_and_never_reach_debug_output() {
        let both: Config = Config::parse("[portal.delta]\nusername = \"generated-name\"\n\
            skymiles_number = \"9000000002\"\npassword = \"generated-secret\"\n", "generated");
        let (user, password) = both.portal.delta.as_ref().unwrap().sign_in().unwrap();
        assert_eq!((user.as_str(), password.as_str()), ("generated-name", "generated-secret"));
        let number: Config = Config::parse("[portal.delta]\nskymiles_number = 9000000003\npassword = \"p\"\n", "generated");
        assert_eq!(number.portal.delta.as_ref().unwrap().sign_in().unwrap().0.as_str(), "9000000003");
        let missing: Config = Config::parse("[portal.delta]\nusername = \"generated-name\"\n", "generated");
        assert!(missing.portal.delta.as_ref().unwrap().sign_in().is_none());
        let shown = format!("{both:?} {:?}", both.portal);
        for secret in ["generated-name", "9000000002", "generated-secret"] {
            assert!(!shown.contains(secret), "{secret} in Debug output");
        }
        assert!(shown.contains("configured: true"));
        // The typed settings and the internal copy never carry the account.
        let typed = String::from_utf8(both.serialized().unwrap()).unwrap();
        assert!(!typed.contains("portal") && !typed.contains("generated-secret"));
    }

    #[test]
    fn nothing_to_keep_leaves_the_typed_bytes() {
        let config = Config::default();
        let serialized = config.serialized().unwrap();
        assert_eq!(with_unmodeled(serialized.clone(), None), serialized);
        assert_eq!(with_unmodeled(serialized.clone(), Some(b"[server]\nurl = \"x\"\n")), serialized);
        assert_eq!(with_unmodeled(serialized.clone(), Some(b"not toml [")), serialized);
    }
}
