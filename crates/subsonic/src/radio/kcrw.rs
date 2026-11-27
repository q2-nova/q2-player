//! KCRW's public Eclectic24 playlist. Its stream advertises ICY metadata
//! but sends empty titles, so the playlist supplies the display metadata.
use super::france::Update;
use serde::Deserialize;
use std::{
    io,
    time::{Duration, UNIX_EPOCH},
};

const ENDPOINT: &str = "https://tracklist-api.kcrw.com/Music/date";
const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_AGE: u64 = 10 * 60;

pub fn matches_stream(stream: &str) -> bool {
    url::Url::parse(stream).is_ok_and(|url| {
        url.host_str() == Some("streams.kcrw.com")
            && matches!(url.path(), "/e24_mp3" | "/e24_aac" | "/e24")
    })
}

fn day_path(epoch: u64) -> String {
    let date = httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(epoch));
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| *month == &date[8..11])
    .unwrap()
        + 1;
    format!("{}/{month:02}/{}", &date[12..16], &date[5..7])
}

/// The playlist uses calendar dates in Los Angeles. For the hour when the
/// two possible UTC offsets yield different dates, offer both and choose the
/// newest eligible timestamp. Fall back to yesterday during midnight rollover.
fn days(now: u64) -> Vec<String> {
    let later = now.saturating_sub(7 * 3600);
    let earlier = now.saturating_sub(8 * 3600);
    let mut dates = vec![day_path(later)];
    if later / 86400 != earlier / 86400 {
        dates.push(day_path(earlier));
    } else if earlier % 86400 < MAX_AGE {
        dates.push(day_path(earlier.saturating_sub(86400)));
    }
    dates
}

#[derive(Deserialize)]
struct Entry {
    program_id: Option<String>,
    channel: Option<String>,
    datetime: Option<String>,
    artist: Option<String>,
    title: Option<String>,
}

fn timestamp(s: &str) -> Option<u64> {
    let s = s.as_bytes();
    if s.len() != 25 || [s[4], s[7], s[10], s[13], s[16], s[22]] != *b"--T:::" {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        let digits = s.get(range)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse::<u64>().ok()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let leap = |year: u64| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    let month_days = [
        31,
        28 + u64::from(leap(year)),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let index = usize::try_from(month.checked_sub(1)?).ok()?;
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let (offset_hour, offset_minute) = (number(20..22)?, number(23..25)?);
    if year < 1970
        || day == 0
        || day > *month_days.get(index)?
        || hour > 23
        || minute > 59
        || second > 59
        || offset_hour > 23
        || offset_minute > 59
    {
        return None;
    }
    let years = year - 1970;
    let leap_days = |y| y / 4 - y / 100 + y / 400;
    let days = years * 365 + leap_days(year - 1) - leap_days(1969)
        + month_days[..index].iter().sum::<u64>()
        + day
        - 1;
    let base = days * 86400 + hour * 3600 + minute * 60 + second;
    let offset = offset_hour * 3600 + offset_minute * 60;
    match s[19] {
        b'-' => Some(base + offset),
        b'+' => base.checked_sub(offset),
        _ => None,
    }
}

fn current(bytes: &[u8], now: u64) -> io::Result<Option<(u64, Option<String>)>> {
    let entries: Vec<Entry> =
        serde_json::from_slice(bytes).map_err(|_| io::Error::other("Invalid KCRW playlist"))?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            if entry.channel.as_deref() != Some("Music")
                || entry.program_id.as_deref() != Some("e24")
            {
                return None;
            }
            let at = timestamp(entry.datetime.as_deref()?)?;
            (at <= now && now - at < MAX_AGE).then_some((at, entry))
        })
        .max_by_key(|(at, _)| *at)
        .map(|(at, entry)| {
            let clean = |s: Option<String>| {
                s.unwrap_or_default()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect::<String>()
            };
            let artist = clean(entry.artist);
            let title = clean(entry.title);
            (
                at,
                (!artist.is_empty() && !title.is_empty()).then(|| format!("{artist} - {title}")),
            )
        }))
}

pub async fn fetch_async(proxy: Option<&str>, now: u64) -> io::Result<Update> {
    if !(1_000_000_000..=4_102_444_800).contains(&now) {
        return Err(io::Error::other("KCRW needs current time"));
    }
    refresh(ENDPOINT, proxy, now).await
}

async fn refresh(endpoint: &str, proxy: Option<&str>, now: u64) -> io::Result<Update> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut builder = crate::http::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(8));
        if let Some(proxy) = proxy {
            builder = builder.proxy(
                crate::http::Proxy::new(proxy)
                    .map_err(|_| io::Error::other("Invalid radio proxy"))?,
            );
        }
        let agent = builder.build();
        let mut latest = None;
        for date in days(now) {
            let response = agent
                .get(&format!("{endpoint}/{date}"))
                .send(Default::default())
                .await
                .map_err(|_| io::Error::other("KCRW playlist unavailable"))?;
            let mut body = response.into_decoded();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.chunk().await {
                let chunk = chunk?;
                if chunk.len() > MAX_RESPONSE.saturating_sub(bytes.len()) {
                    return Err(io::Error::other("KCRW playlist too large"));
                }
                bytes.extend_from_slice(&chunk);
            }
            let item = tokio::task::spawn_blocking(move || current(&bytes, now))
                .await
                .map_err(io::Error::other)??;
            if item
                .as_ref()
                .is_some_and(|(at, _)| latest.as_ref().is_none_or(|(old, _)| at > old))
            {
                latest = item;
            }
        }
        let (expires, title) = latest.map_or((now, None), |(at, title)| (at + MAX_AGE, title));
        Ok(Update {
            title,
            expires,
            refresh: Duration::from_secs(30),
        })
    })
    .await
    .map_err(|_| io::Error::other("KCRW playlist timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_uses_a_public_unauthenticated_request() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/Music/date", listener.local_addr().unwrap());
        let wire = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
            assert!(headers.starts_with("get /music/date/2026/11/23 http/1.1"));
            assert!(!headers.contains("authorization:"));
            let body = r#"[{"program_id":"e24","channel":"Music","datetime":"2026-11-23T18:21:18-08:00","artist":"Generated Artist","title":"Generated Song"}]"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let now = timestamp("2026-11-23T18:26:47-08:00").unwrap();
        let update = crate::http::block_on(async move { refresh(&endpoint, None, now).await })
            .unwrap()
            .unwrap();
        assert_eq!(
            update.title.as_deref(),
            Some("Generated Artist - Generated Song")
        );
        wire.join().unwrap();
    }

    #[test]
    fn playlist_uses_latest_eligible_entry_and_discards_stale_metadata() {
        let now = timestamp("2026-11-23T18:26:47-08:00").unwrap();
        assert_eq!(now, timestamp("2026-11-24T02:26:47+00:00").unwrap());
        assert_eq!(days(now), ["2026/11/23"]);
        assert_eq!(
            days(timestamp("2026-11-24T07:35:00+00:00").unwrap()),
            ["2026/11/24", "2026/11/23"]
        );
        let response = br#"[
            {"program_id":"e24","channel":"Music","datetime":"2026-11-23T18:21:18-08:00","artist":"Fixture","title":"Current"},
            {"program_id":"e24","channel":"Simulcast","datetime":"2026-11-23T18:25:20-08:00","artist":"Future Or Other","title":"Other"},
            {"program_id":"e24","channel":"Music","datetime":"2026-11-23T18:30:20-08:00","artist":"Future","title":"Future"}
        ]"#;
        // Other-channel and future entries cannot override the current title.
        assert_eq!(
            current(response, now).unwrap().unwrap().1.as_deref(),
            Some("Fixture - Current")
        );
        assert!(current(response, now + 20 * 60).unwrap().is_none());
        assert_eq!(timestamp("2026-02-29T00:00:00+00:00"), None);
    }
}
