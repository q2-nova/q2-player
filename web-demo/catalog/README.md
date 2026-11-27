# Browser demo music catalog

[music.json](music.json) credits every recording by song, artist, album, source and license. The hosted demo serves all tracks from its own origin so the browser can apply the player's volume. It serves the original Chris Zabriskie files and full-length 224 kbps MP3 re-encodes of the Disco Ultralounge tracks, which the player marks in the credits. The standalone HTML uses the artists' URLs for online playback and includes short samples for offline playback. All recordings in this catalog are licensed under [Creative Commons Attribution 4.0 International](https://creativecommons.org/licenses/by/4.0/).

| Album | Artist | Included |
| --- | --- | --- |
| [Cylinders](https://chriszabriskie.com/cylinders/) | Chris Zabriskie | 9 tracks |
| [Soft Rock Champion](https://chriszabriskie.com/src/) | Chris Zabriskie | 10 tracks |
| [Undercover Vampire Policeman](https://chriszabriskie.com/uvp/) | Chris Zabriskie | 7 tracks |
| [Disco Ultralounge](https://open.spotify.com/album/4R88Ml4Ecn7I94c4SyShcd) | Kevin MacLeod | 20 tracks |

Chris Zabriskie publishes his [licensing and attribution terms](https://chriszabriskie.com/use/). Individual Incompetech track pages are the primary source for Kevin MacLeod's license and download URLs. Track positions in the demo follow each album's published sequence.

When adding a recording, obtain its URL from the artist's own player or download link. Check that an HTTP range request returns 206 and that the returned slice matches a full download. Decode the full file and record its exact byte count and SHA-256 digest. The track's duration is the rounded decoded media duration in seconds. The packager verifies each original against the pinned byte count and SHA-256 before using it. Remove an invalid cached copy to let the packager fetch it again. If the publisher has changed the source bytes, verify the new file and update its catalog entry before packaging it.

The media for Soft Rock Champion matches the artist's [Bandcamp track timings](https://chriszabriskie.bandcamp.com/album/soft-rock-champion) in release order. Some timings on the artist's own album page are assigned to the wrong titles. Disco Ultralounge's track 17 is omitted because the artist's source file by that name is a different length from the album release. The remaining tracks keep their original track numbers.

The demo uses its own illustrated sleeves as placeholders. [Artwork provenance](../../crates/wasm-demo/art/README.md).
