# Internet radio

Web radio plays live HTTP(S) streams from RAM: nothing is written to the
card, the stream cannot seek, and a pause closes the connection. Station
lists come from Navidrome or from the built-in Featured Stations. Song
titles come from the stream's ICY metadata, or for two stations from the
broadcaster's public playlist feed. Played songs can go to ListenBrainz and
Last.fm (SCROBBLE-001, SCROBBLE-002 in the
[specification](specification.md#network)).

## Stations

Home → Radios loads `getInternetRadioStations` from Navidrome
([OpenSubsonic](https://opensubsonic.netlify.app/docs/endpoints/getinternetradiostations/))
and caches the list like other library pages. It reloads when the server
comes back online or a station is added. Featured Stations
([`ui/screens/sources.rs`](../crates/q2nd/src/ui/screens/sources.rs)) is a
fixed list that needs no server: FIP, KEXP and three SomaFM playlists. Hold
Center on a featured station and choose Add to Navidrome to save it with
`createInternetRadioStation`; the player skips the request when a station
with the same stream URL exists. Creating stations needs a Navidrome account
with that permission, and a refusal shows "Server denied access; radio
editing may need an admin".

A station's identity is its stream URL. Selecting the station that is
already loaded returns to Now Playing without reconnecting or changing the
pause state, even when Featured Stations and Navidrome list it under
different names. A different URL starts a new stream. In Offline Mode
selecting a station shows "Radio needs a connection".

`q2nd --radio-import MANIFEST CONFIG [--apply]` adds stations from a JSON
manifest to the server (RADIO-IMPORT-001,
[`radio_import.rs`](../crates/q2nd/src/radio_import.rs)).
[`q2/radio-stations.json`](../q2/radio-stations.json) is a manifest of five
public stations.

## Stream transport

[`subsonic/src/radio.rs`](../crates/subsonic/src/radio.rs) opens the stream
with `Icy-MetaData: 1`, a 5 s connect timeout and a 15 s read timeout. Radio
requests carry no Navidrome credentials and connect directly with certificate
checks, also when Tailscale is on; the Tailscale proxy is for the library
servers.

- A URL ending in `.pls`, `.m3u` or `.m3u8`, or served as an MPEG URL or PLS
  type, is a playlist. Its body may be up to 64 KiB, and the player follows
  its first entry (`File1` in PLS). The stream must be reached within four
  requests.
- HLS playlists (`#EXT-X-` lines) are refused with "HLS radio is not
  supported; choose an MP3 or AAC stream".
- Supported codecs are MP3 and AAC-LC in ADTS. Symphonia's ADTS reader
  accepts only MPEG-4 headers without CRC, so
  [`audio/radio.rs`](../crates/q2nd/src/audio/radio.rs) rewrites MPEG-2 and
  CRC-bearing single-block frame headers and leaves the AAC payload
  unchanged. HE-AAC and multi-block frames fail with "Radio needs AAC-LC with
  one block per frame".

## Buffer

The compressed-byte queue holds ten seconds at the `icy-br` bitrate,
256 kb/s when the header is absent, clamped to 64–512 KiB. A producer task on
the I/O runtime fills it in reads of up to 16 KiB; the player thread drains
it. Playback starts when the response headers arrive, not when the queue is
full, so the queue absorbs bursts from a server that sends faster than real
time but gives no fixed outage margin. A read that waits longer than 15 s
fails the stream.

When a stream ends or fails, playback pauses on the same station; it never
repeats or advances to the next queue entry. Pause closes the track, the
connection and the PCM output, and Play reconnects to live audio. Live
stations are never downloaded, cached, prefetched, scrobbled to Navidrome or
saved in the server play queue.

A stream error keeps its text only when it is one of the transport's own
URL-free messages, such as "Radio server returned HTTP 404" or "Radio
connection failed (…)". Any other error becomes "Radio stream disconnected",
so a stream URL or server error body never reaches the player status.

## Song titles

ICY metadata blocks travel in the queue with the audio and are removed as
the decoder reads, so a title changes when its audio plays rather than when
the network delivers it. Decoder read-ahead and output latency remain, so
the change is close to the audio, not sample-exact. Rules for `StreamTitle`:

- A block without `StreamTitle` keeps the previous title; an empty
  `StreamTitle` clears it.
- Text is UTF-8, or Latin-1 when it is not valid UTF-8. Control characters
  become spaces and the title is cut at 512 characters.
- Now Playing splits `Artist - Title` into title and artist lines with the
  station name below; other text is shown as the title. Pause, reconnect and
  station changes clear it. Titles are never stored in the library or the
  queue.

Two stations take titles from public feeds that need no account
([`radio/france.rs`](../crates/subsonic/src/radio/france.rs),
[`radio/kcrw.rs`](../crates/subsonic/src/radio/kcrw.rs)). Each feed is
polled by its own task, independent of the audio: a feed failure leaves the
audio playing and the station name shown once the last title expires.

| Station | Streams | Feed | Title chosen | Refresh |
| --- | --- | --- | --- | --- |
| FIP | `icecast.radiofrance.fr` `/fip-midfi.mp3`, `/fip-lofi.mp3`, `/fip-hifi.aac`, when the stream has no ICY metadata | Radio France LiveMeta (`api.radiofrance.fr/livemeta/pull/7`), at most 128 KiB | The latest-starting song whose start and end contain the current time; never a future entry or a programme | At the song's end, 5–60 s; 15 s when no song is current |
| KCRW Eclectic24 (RADIO-META-001) | `streams.kcrw.com` `/e24_mp3`, `/e24_aac`, `/e24` | KCRW Music playlist for the current Los Angeles date, at most 1 MiB | The newest Eclectic24 entry aired less than 10 minutes ago; the stream's empty ICY titles are ignored | 30 s |

A feed request times out after 8 s. Failures retry after 15 s, doubling to
60 s. Only the listed mounts use a feed; other FIP channels show ICY titles
or the station name.

## Played songs

[`audio/player/radio_listen.rs`](../crates/q2nd/src/audio/player/radio_listen.rs)
counts a radio song from PCM written to the output, so buffering and pause
do not count as listening. A title is a song only in the form
`Artist - Title` where neither part is empty, the station name or a
placeholder such as "Unknown" or "Advertisement". Feed titles can lead the
audio and a first ICY title may already be in progress, so a duration
estimate is made only from two observed ICY boundaries; SCROBBLE-001 gives
the thresholds.

[`library/direct_listens.rs`](../crates/q2nd/src/library/direct_listens.rs)
keeps qualified listens in a SQLite outbox bound to each service account,
sent by one async owner. The clients
([`listenbrainz.rs`](../crates/subsonic/src/listenbrainz.rs),
[`lastfm.rs`](../crates/subsonic/src/lastfm.rs)) follow the services' rules:
ListenBrainz
([submit-listens](https://listenbrainz.readthedocs.io/en/latest/users/api/index.html))
gets at most one request per second across account changes and honours its
rate headers; Last.fm
([track.scrobble](https://www.last.fm/api/show/track.scrobble)) gets signed
batches of up to 50 with `chosenByUser=0` for radio, and each item's
acknowledgement is recorded separately. Neither client follows redirects or
puts tokens, session keys or response bodies in errors. Navidrome's scrobble
endpoint takes only server media IDs, so radio songs go to the services
directly and never through Navidrome.

`[listenbrainz]` takes `enabled`, `token` and `contact_url`; `[lastfm]`
takes `enabled`, `api_key`, `shared_secret`, `session` and `contact_url`.
[`tools/lastfm_authorize.py`](../tools/lastfm_authorize.py) obtains a
Last.fm session key and writes the `[lastfm]` stanza.
