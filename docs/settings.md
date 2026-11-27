# Settings

Settings is grouped by task: Sound, Playback, Display, Connections,
Bluetooth, Storage and System (SETTINGS-003). The generated
[UI map](../tests/ui/ui-hierarchy.md) lists every row, value, choice,
confirmation and Back target; the [specification](specification.md) defines
what each page must hold. This page covers how the pages behave and where
their requirements are.

## Pages

| Page | Rows | Saved in | Requirements |
| --- | --- | --- | --- |
| Sound | Equalizer, Volume Normalization, Volume Limit, Headphone Power, DAC Filter | `[playback]`, `[playback.eq]` | AUDIO-006, BLUETOOTH-002, EQ-OUTPUT-001; [Equalizer](equalizer.md), [ReplayGain](replaygain.md), [Headphone output](audio.md) |
| Playback | Auto Sleep Timer, Gapless, Pause on Unplug, Song Ratings; spoken defaults; quality and scrobbling | `[playback]`, `[spoken]`, `[listenbrainz]`, `[lastfm]` | SETTINGS-001, SETTINGS-002, SCROBBLE-002 |
| Display | Brightness, Backlight Timeout, Night Warmth, Theme, Selection Color, Time in Title | `[display]` | DISPLAY-001, DISPLAY-004 |
| Connections | Wi-Fi, Navidrome Server, Audiobookshelf, Tailscale, Offline Mode, USB Drive when available | `[wifi]`, `[server]`, `[audiobookshelf]`, `[tailscale]` | ABS-001, ABS-004, ABOUT-002, MUSIC-SYNC-001, NETWORK-001, NETWORK-003 |
| Bluetooth | switch, paired devices, Pair New Device, Preferred Codec | `[bluetooth]` | BLUETOOTH-003; [Bluetooth headphones](bluetooth.md#settings) |
| Storage | Storage Usage, Offline Mix, Mix Size (while Offline Mix is on), Song Cache Limit, Fetch Ahead, Clear Song Cache… | `[storage]` | STORAGE-002, STORAGE-005, DOWNLOAD-004, UX-010 |
| System | Clicker, Touchscreen (when the panel exists), Sleep When Idle, Date & Time, About | `[display]`, `[power]` | SPEAKER-001, TOUCH-001, TIME-001, ABOUT-001 |

Shuffle and Repeat are not in Settings: they are rows of the music Now
Playing menu (SETTINGS-002). Brightness and Equalizer in the Now Playing
menu open the same pages as Settings.

Settings are written to the card's `Q2Mainline/config.toml` and the
internal copy (STORAGE-003) 3 s after the last change, by one ordered writer
([`config/save.rs`](../crates/q2nd/src/config/save.rs)).

## Controls

- **Switches.** Center toggles an On/Off row in place.
- **Choices.** A row with several values opens a checked list with the saved
  value selected. Turning the wheel changes only the selection; Center
  applies the choice and returns, and Back returns without a change
  ([`ui/screens/controls.rs`](../crates/q2nd/src/ui/screens/controls.rs)).
- **Sliders.** Brightness and Volume Limit apply as the wheel turns, from 1
  to 100. Center or Back closes the slider. Lowering the limit below the
  current volume lowers the volume; raising it leaves the volume as it is.
  Both are relative levels, not measured output.
- **Confirmations.** Reset Bands…, Clear Song Cache…, Wi-Fi's Forget This
  Network…, Tailscale's Forget This Device and a Bluetooth device's Forget
  This Device… state the consequence and open with Cancel selected.
  Choosing a different time zone asks "Change Time Zone?" with Apply &
  Restart, because the app restarts; choosing the current zone returns
  without asking.

## Layout

- Rows that open a page, an editor or a confirmation have an arrow. Values
  are at the right; a long value is shortened with an ellipsis instead of
  covering the label. Chosen values in lists have a check.
- Information rows are muted, have no highlight and are skipped by the
  wheel. A page of information only scrolls its content
  ([Reading pages](interaction.md#reading-pages)).
- Selection and scroll position are kept across value refreshes and when
  returning from a sub-page; Back returns to the parent row.
- Summary values show state on the parent row: Sound shows the EQ state,
  Display the brightness, Connections the Wi-Fi state, and Bluetooth the
  controller state.
- An option that has no use while its parent switch is off appears only
  while the switch is on and keeps its value while hidden
  ([UX norms](ux-norms.md#hierarchy-and-interaction)).

## Scrobbling

Navidrome Scrobbles (default On) records library song plays on the
Navidrome server. ListenBrainz and Last.fm (default Off) send library songs
and identified web-radio songs directly to those accounts, each with its own
queue and status, and retry delivery after an offline period (SCROBBLE-001,
SCROBBLE-002). If Navidrome also forwards plays to the same external
account, turn that forwarding off on the server to avoid duplicate listens.

- **ListenBrainz**: Settings → Playback → ListenBrainz takes the API token
  and a Client Contact URL (HTTPS); `[listenbrainz]` holds `enabled`,
  `token` and `contact_url`. The page shows only whether a token is saved.
- **Last.fm**: run `python3 tools/lastfm_authorize.py --out NEW_FILE
  --contact-url HTTPS_URL` on a computer with your Last.fm API key and
  secret, and merge the `[lastfm]` section it writes into `config.toml`.
  Settings → Playback → Last.fm then shows the Scrobbling switch and the
  delivery status.
