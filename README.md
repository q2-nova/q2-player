# Q2 Nova player

Q2 Nova turns the Shanling Q2 into a wheel-controlled music and audiobook
player. Listen to files on the microSD card, stream music from Navidrome, or
pick up a book from Audiobookshelf. You can try the controls in a browser
before installing anything. [Try the player](https://q2-nova.github.io/site/features.html)

Fork this repository to change the menus, playback or library behavior. The
desktop simulator runs without a Q2. For a physical player, choose a release
explicitly qualified for installation on stock Shanling Q2 hardware.
[Player releases](https://github.com/q2-nova/q2-player/releases)

## Set up the player

Once Q2 Nova is installed, [join Wi-Fi, connect Navidrome or Audiobookshelf,
and pair Bluetooth headphones](docs/setup.md). Settings live in the card's
`Q2Mainline/config.toml` and can also be edited on a computer.

## Try the player without a Q2

With the repository’s Rust toolchain, the simulator’s native development
libraries, Python and FFmpeg installed, run:

```sh
python3 tests/e2e/first-listen.py --out build/my-first-listen
```

The check builds the native simulator, generates a short audiobook, plays
it, pauses and repeats in Offline Mode. Use a fresh output name for each
run. [Simulator prerequisites and checks](tests/e2e/README.md) · [Browser
demo](web-demo/README.md)

## Build and test

[Tests](docs/testing.md) describes the host tests, the Rust tests built for
the Q2 and run under QEMU, and the CI that runs them. The [target build
guide](docs/target-build.md) builds the player and portal for the Q2 from an
OS SDK and the X2000 compiler. The [component lock](component-lock.json)
selects exact revisions in the q2-nova repositories. See the
[architecture](docs/architecture.md) for how the native player and browser
demo are divided, and the [specification](docs/specification.md) for what
the player must do. The [changelog](CHANGELOG.md) records completed work.

## Releases

The [release lock](release-lock.json) selects the OS (SDK and platform),
compiler, QEMU and network daemon by their release tags. A player tag builds
the paired player and portal, composes the OS image, and boots that exact
image under QEMU before attaching the emulator rehearsal to a prerelease. A
stock release adds a physical test of the exact `update.tar` on a Q2 running
stock firmware. [Release candidates](docs/release-candidate.md)

On an x86-64 Linux host with Python 3.12+ and GitHub CLI, the same lock
fetches and verifies the component assets for a local build:

```sh
python3 tools/fetch-build-tools.py --out build/rc-inputs
```

The command reads checksums from the selected releases, relocates the SDK,
and records the resolved source commits and asset hashes under its output
directory. Use a fresh output directory. Forks of this repository use the
q2-nova component releases by default; set the Actions repository variable
`Q2_COMPONENTS_OWNER` or pass `--owner` to use another owner's releases.

## Related repositories

- [q2-os](https://github.com/q2-nova/q2-os): the Linux system, recovery environment and SDK.
- [q2-toolchain](https://github.com/q2-nova/q2-toolchain): the Rust and Clang cross-compilers and the Q2 QEMU emulator.
- [tsrs](https://github.com/q2-nova/tsrs): the Tailscale network daemon.
- Fork used as a build input: [tokio](https://github.com/q2-nova/tokio) (branch `q2/1.54.1`).

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and what a pull
request must pass. Report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md). Participation follows the [code of
conduct](CODE_OF_CONDUCT.md).

## License

Original Q2 player code is available under the GNU General Public License,
version 3 or (at your option) any later version (`GPL-3.0-or-later`). See
[COPYING](COPYING). The codec packages, vendored libraries, fonts, datasets
and other third-party material carry their own licenses and notices.

Binary releases include libopenaptx source, notices and the LGPL license.
[How to rebuild with a modified libopenaptx](docs/relink.md)
