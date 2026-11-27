# Player release candidates

`release-lock.json` selects four component prereleases by tag. The OS release
contains its matching SDK and platform assets. The compiler, standalone TSRS
daemon and QEMU releases contain their architecture-specific archives. The
player workflow obtains the checksums from those same releases and verifies the
archive bytes, producer identities and source pins. Temporary source locks and
build receipts record exactly which revisions the build used.

A player tag such as `v0.1.0-rc.1` on a verified commit in main triggers **Player release
candidate**. It builds the paired player and portal, composes a keyless
installed OS fixture with the tagged OS builder, packs the firmware archive and
reads it back before booting those exact bytes under QEMU. The paired artifact
remains in Actions for reuse. The prerelease contains
`update.tar` labelled QEMU emulator rehearsal only, its boot result and their checksums. Keep this
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
`hardware-qualification.json`, `release-inputs.json`, and `image/` with
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
stock qualification covers the identical archive. It copies those bytes into a
draft only after the checks pass. The stock draft retains the package and its
verification summary. An existing tag or release at that version is left in
place. Publication is a separate maintainer action.
