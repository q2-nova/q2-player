# Q2 Nova player

Q2 Nova turns the Shanling Q2 into a wheel-controlled music and audiobook
player. Listen to files on the microSD card, stream music from Navidrome, or
pick up a book from Audiobookshelf. You can try the controls in a browser
before installing anything. [Try the player](https://q2-nova.github.io/site/features.html)

For a Shanling Q2 running stock V1.32, read the [installation and recovery
guide](docs/install.md). Use the [v0.1.0 stable release](https://github.com/q2-nova/q2-player/releases/tag/v0.1.0)
when it is available. See the [release notes](docs/release-v0.1.0.md).

Fork this repository to change the menus, playback or library behavior. The
desktop simulator runs without a Q2.

## First listen

With the repository’s Rust toolchain, the simulator’s native development libraries, Python and FFmpeg installed, run:

```sh
python3 tests/e2e/first-listen.py --out build/my-first-listen
```

The check builds the native simulator, generates a short audiobook, plays it, pauses and repeats in Offline Mode. Use a fresh output name for each run. The SQLite compile options used by this workspace are tracked in `q2/toolchain/libsqlite3-flags`.

See [simulator prerequisites and checks](tests/e2e/README.md) and the [browser demo](web-demo/README.md). The [component lock](component-lock.json) selects exact revisions in the q2-nova repositories. The simulator uses generated media and needs no Q2 hardware.

## Accounts and stations

Settings live in the card's `Q2Mainline/config.toml`. For Last.fm
scrobbling, run `python3 tools/lastfm_authorize.py --out NEW_FILE --contact-url HTTPS_URL`
on a computer with the API key and secret of your Last.fm API account, and
merge the `[lastfm]` stanza it writes into `config.toml`.
`q2nd --radio-import MANIFEST CONFIG [--apply]` adds web radio stations to
the Navidrome server (RADIO-IMPORT-001).

## Device builds

The installed Q2 runs the paired player and portal with the q2-os firmware.
Use the [target build guide](docs/target-build.md) for local development and
the [release candidate guide](docs/release-candidate.md) to build and verify
the complete emulator package. See the [architecture](docs/architecture.md)
for the native player and browser demo boundaries.
The [specification](docs/specification.md) lists what the player must do,
and the [changelog](CHANGELOG.md) records completed work.

## Building a release candidate

The [release lock](release-lock.json) selects the OS (SDK and platform), compiler,
QEMU and network daemon by their release tags. The **Player release candidate**
workflow builds the paired player and portal when a player RC tag is pushed.
It composes, packs and boots the exact QEMU image before attaching the emulator
rehearsal to a prerelease. See the [release workflow](docs/release-candidate.md)
for the separate physical stock qualification path.

On an x86-64 Linux host with Python 3.12+ and GitHub CLI, the same lock can
fetch and verify the component assets for a local build:

```sh
python3 tools/fetch-build-tools.py --out build/rc-inputs
```

The command reads checksums from the selected releases, relocates the SDK, and
records the resolved source commits and asset hashes under its output directory.
Use a fresh output directory. App forks use the q2-nova component releases by
default. Set the Actions repository variable `Q2_COMPONENTS_OWNER` or pass
`--owner` locally to choose another component owner. Private component
repositories require read access.

## License

Original Q2 player code is available under the GNU General Public License,
version 3 or (at your option) any later version (`GPL-3.0-or-later`). See
[COPYING](COPYING). The codec packages, vendored libraries, fonts, datasets
and other third-party material carry their own licenses and notices.

Binary releases include libopenaptx source, notices and the LGPL license.
[How to rebuild with a modified libopenaptx](docs/relink.md)
