# Audiobooks and podcasts

Books play from M4B files on the card or from Audiobookshelf; podcasts come
only from Audiobookshelf. Every book and episode keeps a durable listening
position on the card, and Audiobookshelf positions sync both ways with the
server's newer-update-wins rule. The menus, settings and controls are
requirements BOOK-001 to SPOKEN-004 in the
[specification](specification.md#books-and-podcasts). This page covers how
the player reads local books, the Audiobookshelf API it relies on, and how
positions are stored and synced.

## Local books

[`library/spoken.rs`](../crates/q2nd/src/library/spoken.rs) scans the
`audiobooks` folder of the media tree (`Q2Mainline/media/audiobooks` on the
card) within the BOOK-001 limits. A book's identity is `local-m4b:` and its
path under that folder, so it survives card mount paths and restarts, and a
moved or renamed file is a new book.

The reader skips the `mdat` audio payload and reads at most 16 MiB of
`moov`. Chapters come from a Nero `chpl` table or a QuickTime chapter text
track referenced by `tref/chap`
([`spoken/quicktime.rs`](../crates/q2nd/src/library/spoken/quicktime.rs)),
with 32- or 64-bit chunk offsets, up to 16,384 chapters and 16 MiB of
chapter text. Tags:

| Atom | Use |
| --- | --- |
| `©nam`, `©ART` | Title and author |
| `ldes`, else `desc` or `©cmt` | Summary |
| `covr` (up to 4 MiB) | Cover |
| Freeform `----` named `narrator`, `narrated_by`, `narratedby` or `series` | Narrator, series |

Composer and album tags are not read as narrator or series. Input past
these limits is rejected.

## Audiobookshelf client

[`subsonic/src/audiobookshelf.rs`](../crates/subsonic/src/audiobookshelf.rs)
and [`audiobookshelf_browse.rs`](../crates/subsonic/src/audiobookshelf_browse.rs)
follow the Audiobookshelf 2.37.2 server source at commit
[`ce41ce48`](https://github.com/advplyr/audiobookshelf/tree/ce41ce489cebaf00e008919f6f2901b428acfd56).
The [API reference](https://api.audiobookshelf.org/) is unmaintained; where
it disagrees with that source, the source is the contract.

The client authenticates with an Audiobookshelf API key as a bearer token
(ABS-001). It has no username and password login and no access-token
refresh. It uses the Navidrome route (ABS-005), follows no redirects, and
accepts only URLs under the configured address and path, so a media URL that
points elsewhere is refused. Connect times out after 8 s, reads and writes
after 15 s. Errors carry no URL, token or response body:

| Response | Error |
| --- | --- |
| No connection, name or proxy failure | Network: "Audiobookshelf unavailable" |
| 401, 403 | Auth: "Audiobookshelf token rejected or expired" |
| 404 | Missing |
| 408, 429, 5xx | Server: "Audiobookshelf had a problem" |
| Any other status, malformed body | Protocol |

`GET /api/me` identifies the account. The namespace that keys cached pages,
covers, downloads and positions is a hash of the server address and the user
ID, not the token: a new API key for the same user keeps everything, and
another user starts empty. A request that completes after the account
changed is discarded ("Audiobookshelf settings changed; select the item
again").

| Purpose | Request | Checks |
| --- | --- | --- |
| Account and progress snapshot | `GET /api/me` | `mediaProgress` read as it streams; `bookmarks` skipped |
| Libraries | `GET /api/libraries` | |
| Item and Book Info | `GET /api/items/:id?expanded=1` | File, track and episode lists dropped while streaming, except the requested episode |
| Play session | `POST /api/items/:id/play[/:episode]` with `forceDirectPlay: true` | Session supplies duration, chapters and the direct audio tracks |
| Audio size | `GET` track URL, `Range: bytes=0-0` | 206 with `Content-Range: bytes 0-0/N` |
| Streamed audio | `GET` track URL (`/api/items/:id/file/:ino`) with a range of at most 256 KiB | 206 and the exact requested `Content-Range` |
| Download | `GET` track URL | 200 with the expected `Content-Length`; a resume sends `Range` with `If-Range` (ETag) or `If-Unmodified-Since`, and 412 or 416 restarts |
| Cover | `GET /api/items/:id/cover?width=N` | Width 1 to 1024, body at most 4 MiB |
| Listening session | `POST /api/session/local` | Plain 200 |
| Close session | `POST /api/session/:id/close` | |
| Read a position | `GET /api/me/progress/:item[/:episode]` | |
| Position without listening, Mark Played | `PATCH /api/me/progress/:item[/:episode]` | |

Whole JSON bodies are limited to 8 MiB. Large documents are parsed as they
arrive in 32 KiB pieces on short blocking jobs, with their unused lists
skipped, up to 64 MiB. Streamed audio range requests
([`audio/source/audiobook_ranges.rs`](../crates/q2nd/src/audio/source/audiobook_ranges.rs))
retry for 60 s, backing off from 0.5 s to 8 s, before playback reports the
failure.

## Browsing

Pages hold at most 50 entries (BOOK-004). Each section maps to one request
(`plan` in `audiobookshelf_browse.rs`):

| Section | Request | Paging |
| --- | --- | --- |
| All Books, podcast Library | `libraries/:id/items`, `sort=media.metadata.title`, `minified=1` | Server |
| Recently Added | items with `filter=recent`, `sort=addedAt`, `desc=1` | Server |
| Series, Authors | `libraries/:id/series`, `/authors`, `sort=name` | Server |
| Collections, Playlists | `libraries/:id/collections`, `/playlists` | Server |
| Latest Episodes | `libraries/:id/recent-episodes` | Server |
| By genre, narrator, author or series | items with `filter=GROUP.BASE64(value)` | Server |
| Continue Listening, Continue Series, Recent Series, Discover, Listen Again, Newest Episodes | `libraries/:id/personalized` with a shelf limit | Preview |
| Search Books | `libraries/:id/search?q=` | Preview |
| Genres, Narrators | `libraries/:id/filterdata`, `/narrators` | Snapshot |
| Collection or playlist members | `collections/:id`, `playlists/:id` | Snapshot |
| A show's episodes | `items/:id` `media.episodes`, only episodes with an audio file | Snapshot |

Server paging passes `limit` and `page`. Preview keeps the server's
selection and offers no next page, because those routes take no offset.
Snapshot routes have no paging, so the whole response streams through the
parser and only the requested window of rows is kept. Filter values are
standard base64 of the UTF-8 value, escaped once by the HTTP client. A
stored page is revalidated with `If-None-Match` when the server sent an
ETag. The background sync of list metadata is BOOK-009.

## Book Info

Book Info ([`library/book_info.rs`](../crates/q2nd/src/library/book_info.rs))
reads `GET /api/items/:id?expanded=1` and keeps the fields BOOK-005 lists.
The summary is `descriptionPlain` when present, else `description` with
markup and active content removed; no HTML is rendered. Local books use the
tags above. The record is stored per account namespace and item in the
card's library database; a failed refresh or write keeps the stored copy.
Prefetch on a resting selection is UX-007.

## Listening positions

Spoken items have card tables separate from Navidrome's
([`library/db.rs`](../crates/q2nd/src/library/db.rs)). An Audiobookshelf
item's logical ID is `abs:NAMESPACE:ITEM:EPISODE`.

- `spoken_positions` holds position, update time, a revision and a dirty
  flag. Each checkpoint (the BOOK-002 events) runs in one transaction with
  SQLite `synchronous=FULL`, then restores the connection's setting. It
  raises the revision and marks the row dirty; a failed write leaves the
  previous position and sync state.
- [`audio/listening.rs`](../crates/q2nd/src/audio/listening.rs) counts
  listened time from cursor advance at the output tempo. A seek or reopened
  decoder starts a new observation and a paused cursor adds nothing. A
  checkpoint with listened time also queues a session upload in
  `spoken_sessions`.

The sync pass ([`library/shelf.rs`](../crates/q2nd/src/library/shelf.rs))
handles one account at a time, in bounded batches:

1. Each pending session goes to `POST /api/session/local` with `currentTime`,
   `startTime`, `timeListening`, `startedAt`, `updatedAt` and the device
   identity. The server keeps whichever update is newer by timestamp, so a
   later rewind on another client wins over an older Q2 position. After the
   acknowledgement the client reads the server position back.
2. A position with no listened time (a seek before any audio) is first
   reconciled with the server, where a newer server position replaces it,
   then sent with `PATCH /api/me/progress`. The player never invents a
   session for it.
3. Mark Played sends `isFinished: true`; Mark Unplayed resets
   `isFinished`, `currentTime` and `progress`.

An acknowledgement clears the dirty flag only for the revision it carried,
so a pause made while an upload is in flight stays pending. A server
position read at other times replaces the local one only when it is newer,
the local row is not dirty, and no Mark Played or Unplayed is waiting. Conflicts are decided by
timestamps and depend on a correct clock. When the player fetches the server
position before playing is PLAYBACK-006 and PLAYBACK-007.
