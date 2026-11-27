# Wheel and jogging

The Q2's touch wheel reports a ring position (200 units per turn) and finger
contact. The player turns positions into detents, moves lists by detents,
and seeks by detents on Now Playing. The tuning follows the iPod click wheel
and is chosen on the Q2; it is not Apple's algorithm. The requirements are
UX-011, BROWSE-002 and PLAYBACK-008 in the
[specification](specification.md). [UX norms](ux-norms.md) cover hierarchy
changes.

## Detents

[`input.rs`](../crates/q2nd/src/input.rs) `Wheel` turns positions into
detents of 20 ring units (36°), carrying the remainder so jitter at a
detent's edge does not tick back. The first detent after a reversal needs
three quarters of the remaining travel (rounded up to whole ring units);
later detents in either direction keep the normal spacing. Jitter does not
move the anchor, and lifting the finger clears it. Every event carries its
input timestamp, so a busy UI that handles several events together cannot
create speed.

## Lists

[`ui/list.rs`](../crates/q2nd/src/ui/list.rs) `ListView::wheel`:

- One detent moves one row. Lists of 16 rows or fewer never accelerate.
- After 200 ms of continuous movement in one direction, rows per detent are
  `(speed / 270°/s)²`, rounded, from 1 to 32. Speed is smoothed (half old,
  half new) and capped at 2160°/s. Two events with the same timestamp add no
  speed.
- A gap of more than 250 ms, a reversal, or a finger lift returns to one row
  per detent.
- Selection stops at either end. There is no momentum and no wrap.
- On settings pages the wheel skips information rows.

Alphabetical lists (Albums, Headphone EQ) never accelerate; they have a
letter mode instead (BROWSE-002):

- **Enter.** In a list of at least 100 rows with more than one initial
  letter, turn in one direction for at least 15 detents over at least
  500 ms, averaging at least 720°/s. A detent slower than 540°/s, a reversal
  or a lift clears the run. Albums decides eligibility from the whole
  catalog, not the loaded rows.
- **Choose.** The current letter appears and the selection stays in place.
  Three detents move one letter group. The thumb can slow down, reverse or
  rest on the wheel while the letter stays. Groups follow catalog order,
  including digits and non-Latin initials.
- **Finish.** Lifting the thumb dismisses the letter, and the next detent
  moves one row. Center also dismisses it and keeps the selected row.

## Reading pages

A page with no actionable rows (System Information, status pages) has no
selection: each detent scrolls the content 18 px (half a row). Short content
does not move, a refresh keeps the reading position within bounds, and
Center does nothing. A page that mixes information and actions keeps a
selection on its actionable rows.

## Seeking on Now Playing

Center on Now Playing enters seek mode (PLAYBACK-008). The time selector in
[`ui/screens/now_playing.rs`](../crates/q2nd/src/ui/screens/now_playing.rs)
(`ScrubWheel`) moves 1 s per detent. After 200 ms of continuous movement the
step is `(speed / 180°/s)²` seconds, capped at the track length / 120
(1 to 60 s): 2 s for a four-minute song, 30 s for an hour. Speed rises
smoothed and falls at once, so slowing down restores 1 s steps. A gap of
250 ms or more, a reversal, a lift, a new track or a mode change resets it.
The seek applies after 500 ms of rest, on Center, or when leaving the
selector.

The player keeps seeking safe:

- Every playback instance has a generation, including a repeat of the same
  song. Scrub commands carry it, so a stale command cannot seek the next
  track, and Now Playing drops a pending scrub or rating when it changes.
- Seeks stay inside the known duration. While a scrub gesture is active, the
  end of the file holds the track instead of advancing; the hold lasts 4 s
  after the last gesture, so leaving the screen cannot hold playback.
- Seeking while paused stays paused. A failed seek pauses on the same track
  with an error instead of skipping it. Decoded audio before the requested
  position is discarded.
- A streamed original file seeks beyond the downloaded prefix with 256 KiB
  range reads; transcoded streams read sequentially. A streamed Ogg Vorbis
  file seeks only after the whole file is downloaded, because Symphonia's
  Ogg seeker needs bounds from a seekable probe. MP4 files with their index
  at the end need the whole file before decoding starts.

## Center cycles and ratings

On Now Playing, Center cycles the wheel's target:

| Content | Cycle |
| --- | --- |
| Music, Song Ratings = Stars | Volume → Seek → Stars → Volume |
| Music, Song Ratings = Hearts | Volume → Seek → Heart → Volume |
| Music, Song Ratings = Both | Volume → Seek → Stars → Heart → Volume |
| Books and podcasts | Volume → Seek → Playback Speed → Volume (SPOKEN-001) |
| Web radio | Volume only |

For a heart, the wheel right loves and left unloves; Center or leaving the
control commits it. A rating shows at once on screen and is sent to the
server in the background. The control returns to Volume after 4 s idle.
Long-Center opens the Now Playing menu. Every menu has Brightness,
Equalizer and Sleep Timer; music adds Up Next, Shuffle and Repeat
(SETTINGS-002), books and podcasts put Playback Speed first, and radio adds
Stations.
