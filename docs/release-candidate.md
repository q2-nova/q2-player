# Player release candidates

`release-lock.json` selects four component prereleases by tag. The OS release
contains its matching SDK, platform, and prebuilt kernel, rescue, device tree and modules. The compiler, standalone TSRS
daemon and QEMU releases contain their architecture-specific archives. The
player workflow obtains the checksums from those same releases and verifies the
archive bytes, producer identities and source pins. Temporary source locks and
build receipts record exactly which revisions the build used.

A player tag such as `v0.1.0-rc.1` on a verified commit in main triggers **Player release
candidate**. It builds the paired player and portal, composes a keyless
installed OS fixture with the tagged OS builder and prebuilt kernel, packs the firmware archive and
reads it back before booting those exact bytes under QEMU. Before the
release build it profiles q2nd in the emulator (`tools/pgo.py profile`, see
[the target build](target-build.md#profile-guided-release-build)) and builds
with that profile. `tools/release-candidate.py check-profile` refuses a
receipt from another source commit or compiler. The boot result records the
q2nd receipt and the downloaded TSRS daemon's receipt under `pgo`. It then
records the startup read-ahead blocks of the release q2nd
(`tools/startup-profile.py`, see
[the target build](target-build.md#startup-read-ahead)) beside it, and
checks that the composed image's `artifact.json` holds the read-ahead
ranges for that q2nd. The paired artifact, with
`startup-read-ahead/profile.json`, remains in Actions for reuse. The prerelease contains
`update.tar` labelled QEMU emulator rehearsal only, its boot result, corresponding player source,
`q2nd-symbols.tar.gz` and their checksums. Keep this
emulator rehearsal off physical devices. Manual dispatch runs the generated
admission controls without producing release assets.

For private component repositories the workflow can use
`Q2_COMPONENTS_READ_TOKEN` with read access. Public component releases do not
require a private repository credential.

## Draft a physically qualified stock candidate

The **Q2 release candidate** workflow independently promotes a stock package
already tested on a physical Shanling Q2. It does not regenerate that package.
A stock qualification prerelease such as `stock-inputs-v0.1.0-rc.1` points to
the exact tested player commit. It supplies
`q2-stock-qualified-inputs.tar.gz` and
`q2-stock-qualified-inputs.tar.gz.sha256`. Dispatch using only that
qualification tag.

The tar archive contains the hardware-tested `update.tar`,
`hardware-qualification.json`, `release-inputs.json`, `q2nd-symbols.tar.gz`
from the stock qualification draft, and `image/` with
`artifact.json`, `xImage`, `rootfs.squashfs`, `rescue.cpio`, and
`shanling-q2.dtb`. The input manifest records schema `1`, mode `stock-draft`,
`player_commit`, its complete source lock as `components`, `version`,
`installed_version`, and each payload’s SHA-256 in `files`.

The hardware report records schema `1`, domain `physical-stock-q2`, device
model `Shanling Q2`, the exact `player_commit` and `components`, the
`update_tar_sha256` actually installed, `version`, `starting_version`,
`stock_install_passed`, `generated_local_playback_passed`, `recovery_passed`,
and the measured `conditions` and `procedure`. Use generated local media.
Keep credentials, identifiers and private logs out of this report.

The workflow checks the candidate’s source locks and packaged filesystem,
validates the stock update format and keyless contents, and verifies that the
stock qualification covers the identical archive and that the symbols belong
to the q2nd in `image/artifact.json`. It copies those bytes into a
draft only after the checks pass. The stock draft retains the package and its
verification summary. An existing tag or release at that version is left in
place. Publication is a separate maintainer action.

Release workflows attach `q2nd-symbols.tar.gz` and include it in
`SHA256SUMS`. It holds the unstripped and debug companions of q2nd and
q2-portal from `symbols/` and the paired build's `result.json` as
`build-result.json`. It is a separate asset; `update.tar` does not contain
it. `tools/release-symbols.py` packs it only from a release build with no
stall sampler and checks each companion against its symbols receipt.

Both release workflows attach `player-source.tar.gz` and include it in
`SHA256SUMS`. The source bundle records the exact firmware digest and player
revision. It includes the static libopenaptx notice, license and
[rebuild instructions](relink.md).

### Prepare the archive for physical qualification

These commands require a stock-capable q2-os tag that includes
`os/compose_stock.py` and publishes the matching stock inputs. The OS tag
in the current `release-lock.json` supplies the emulator candidate path.
Once a stock-capable OS tag is available, use its checkout and the resolved
component lock from the admitted release assets. Compose the keyless normal installed image with the tagged OS
stock composer and the paired player, portal, and daemon. The stock packer
checks the normal service hook, release metadata, executable hashes, and
keyless root and rescue contents before using the stock update format.

```sh
python3 "$OS/os/compose_stock.py" prepare --assets "$INPUTS/q2-os-assets" \
  --output "$OS/build/stock-inputs"
python3 "$OS/os/compose_stock.py" image --assets "$INPUTS/q2-os-assets" \
  --stock-inputs "$OS/build/stock-inputs" --player-source "$PWD" \
  --app "$PWD/build/paired-player/q2nd" --tsrsd "$INPUTS/tsrs" \
  --tsrs-source "$INPUTS/tsrs-source" --release-tag "$CANDIDATE_VERSION" \
  --qemu-user "$INPUTS/qemu/bin/qemu-mipsel" --output "$STOCK_IMAGE"
python3 tools/verify-package.py pack-stock --os-repo "$OS" \
  --component-lock "$INPUTS/component-lock.json" --image "$STOCK_IMAGE" \
  --version "$CANDIDATE_VERSION" --installed-version "$STARTING_VERSION" \
  --out build/stock-package
python3 tools/verify-package.py boot-stock --os-repo "$OS" \
  --component-lock "$INPUTS/component-lock.json" \
  --package build/stock-package/update.tar --pack-receipt build/stock-package/result.json \
  --qemu "$INPUTS/qemu/bin/qemu-system-mipsel" --qemu-data "$INPUTS/qemu/share/qemu" \
  --native-qemu --out build/stock-root-smoke
```

For an image built from an earlier player commit in this checkout’s history,
pass that commit with `--player-commit` and its exact build receipt with
`--paired-build-receipt`. The receipt distinguishes the OS checkout used
to cross-build the app from the OS source used to compose the stock image.

The direct-kernel smoke reads the exact archive and checks normal installed
root readiness with disposable guest storage. The normal image has no
simulator player probe, so the result leaves player readiness unset. Keep
the exact `update.tar` for the physical stock installation, local playback
and recovery checks. Only those checks qualify it for the stock draft gate.

A signed annotated `stock-vX.Y.Z-rc.N` tag on a verified `main` commit can
run the stock qualification workflow once the flat release lock selects a
published stock-capable OS version. The workflow builds one paired app and
portal. The provisional physical trial versions live in
`stock-release.json`. The selected candidate is `V0.02`, with factory
`V1.32` as the intended starting version. The trial is pending.
The workflow attaches `update.tar`, its boot and package evidence, corresponding
player source, `q2nd-symbols.tar.gz` and `SHA256SUMS` directly to a newly
created GitHub draft.
Keep that draft private during physical qualification. The workflow refuses
to reuse an existing draft, including after an interrupted upload.
