# Q2 browser demo

Build and serve the demo locally using the steps below. To share an offline copy, choose **Download offline demo** and send them `q2-player.html`. They can open it directly in a browser. The standalone file includes the WebAssembly module, page, styles, fonts, font license and the library artwork and audio previews.

## Try the player

Open **Settings → System → Date & Time → Time Zone** to browse one list ordered by standard GMT offset. The picker opens on the checked location. Selecting a different city opens the same **Cancel** or **Apply & Restart** prompt as the native player. Cancel keeps music playing. Applying the change pauses playback and returns to Home. Enable **Time in Title**
under **Settings → Display** to see the selected city's current time. The
**24-Hour Clock** setting controls its format. Both browser distributions share
the Q2 city catalog and use the browser's current time and time-zone rules.

**Settings → System → About** shows the version of Q2 Navidrome used in the build. The OS is listed as “Web Demo”. System Information and Report a Problem are disabled in the browser.

Settings pickers open on the saved value. Turn the wheel to highlight a choice, press Center to apply it, or use MENU to leave the value as it was. Brightness, Volume Limit and playback speed take effect while you adjust them.

**Settings → Playback → Default Spoken Speed** sets the starting speed for books and podcasts. While listening, hold Center, open **Playback Speed**, and adjust in 5% steps. A book keeps its own speed and episodes in the same podcast series share one speed. Hold Center on the speed screen to return to the default. These preferences last for the browser session.

Click or tap **Music → Cover Flow**. Click a side cover to center it, click the centered cover to open its album, then click a track to play it. Tap the artwork on Now Playing to pause or resume. The wheel and center button work throughout the same journey. On Now Playing, use ↑ or turn the wheel clockwise to raise the volume. Use ↓ or turn counterclockwise to lower it. Press the center to change what the wheel adjusts. MENU returns to the previous selection.

The locally served page starts in **Public streams**. Open **Radios → Featured Stations → FIP** to listen live. KEXP, FIP Jazz and FIP Groove also play inside the Q2. Groove Salad, Drone Zone and Secret Agent link to their official SomaFM players. [SomaFM’s listening terms](https://somafm.com/legal/tos.html).

Browse **Music → Albums** for 46 CC BY recordings by Chris Zabriskie and Kevin MacLeod. The podcast shelves have three episodes from NASA’s *Curious Universe* and *Houston We Have a Podcast*. Books includes three LibriVox recordings, with chapters and seeking. Each selection has a publisher link. [Music and credits](catalog/README.md) · [Episodes and audiobooks](catalog/spoken.json) · [Stations](catalog/radio.json).

The standalone file starts in **Offline demo samples**. This setting plays original looping music and narration as stand-ins for the public recordings. Select **Public streams** to hear the publishers’ versions when online. If a provider is unavailable, the page offers Retry and an explicit switch to the bundled sample. [Offline audio credits](audio/README.md).

| Browser control | Player action |
| --- | --- |
| Drag around the wheel, scroll over it, click the two scroll buttons, or use ↑ and ↓ | Turn the wheel |
| Click or tap an item on the screen | Open that item |
| Center or Enter | Select |
| Hold Center or Enter | Context menu on supported screens |
| MENU, Escape or Backspace | Back |
| Hold MENU or Escape | Jump to Now Playing and back |
| Play/pause button or Space | Toggle playback |
| Side buttons or ← and → | Previous or next |

The shared Rust renderer draws the 375 × 320 framebuffer and the Rust session owns queue and transport decisions. A browser media element buffers the selected recording or live station and reports its position and end to Rust. The hosted player routes the album tracks, in-player stations, NASA podcasts and bundled samples through Web Audio. The volume follows the Q2’s half-decibel steps and mutes at zero. LibriVox and the standalone file’s direct-from-publisher music use the browser’s media volume. On platforms that reserve that control for the system, use the device volume buttons for those sources. Downloads, connectivity settings and session history are simulated in memory. Reload starts a fresh session.

## Build and share

From the repository root with its pinned Rust toolchain:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --locked --target wasm32-unknown-unknown --manifest-path crates/wasm-demo/Cargo.toml --target-dir build/wasm-target -j 2
python3 web-demo/package.py build/wasm-target/wasm32-unknown-unknown/release/q2_wasm_demo.wasm --out build/web-demo
python3 -m http.server 8080 --bind 127.0.0.1 --directory build/web-demo
```

Visit [the local demo](http://127.0.0.1:8080). To host it elsewhere, copy the
contents of `build/web-demo` together. The page’s download link gives visitors the offline
version.

The packager stages 46 full music tracks alongside the hosted player. It reuses
the verified originals and fetches missing files from the attributed sources. A full
cache supports repeat builds without network access. Hosted packaging requires
FFmpeg with the libmp3lame encoder to re-encode the Disco Ultralounge tracks at
224 kbps and fit the upload limit. Use `--music-cache PATH` to select a
different cache. For a standalone HTML build without a populated cache or
FFmpeg, pass `--without-hosted-music` and use a fresh output directory. The
hosted page in that package uses the original URLs for music.

With Playwright and its Chromium browser installed, the browser journey runs against either distribution:

```sh
node web-demo/smoke.cjs http://127.0.0.1:8080/
node web-demo/smoke.cjs "file://$PWD/build/web-demo/q2-player.html"
# Also install Playwright Firefox for output and volume checks in both engines.
node web-demo/audio-test.cjs http://127.0.0.1:8080/ "file://$PWD/build/web-demo/q2-player.html"
node web-demo/click-test.cjs http://127.0.0.1:8080/ "file://$PWD/build/web-demo/q2-player.html"
```

The module-level checks exercise queue transitions, offline availability, downloads, spoken media and settings against the same built artifact.

```sh
node web-demo/transport.cjs build/web-demo/player.wasm
node web-demo/destinations.mjs build/web-demo/player.wasm
node web-demo/timezone-test.cjs build/web-demo/player.wasm
node web-demo/choice-test.cjs build/web-demo/player.wasm
```

### Checking native and demo settings together

With a native q2nd build, Python with Pillow, and Node, run the same settings journey in both players. Give each run a fresh output directory. The check records artifact hashes, synthetic native preferences, screen captures, and pixel differences for each compared region.

```sh
python3 web-demo/settings-parity.py --native build/native-test-target/debug/q2nd --wasm build/web-demo/player.wasm --out build/settings-parity-run
```

The player and demo compile the same Rust settings choices, Sound rows, slider rendering, confirmation controls and spoken speed display. The browser settings adapter maps input to tab-local state. The native handlers own hardware commands, persistent configuration and asynchronous playback requests. [Shared choices](../crates/q2nd/src/config/options.rs) · [Shared settings UI](../crates/q2nd/src/ui/screens/settings/pages.rs) · [Browser settings adapter](../crates/wasm-demo/src/destinations/settings.rs)
