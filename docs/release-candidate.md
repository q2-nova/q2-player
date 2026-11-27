# Q2 release candidate workflow

The **Q2 release candidate** workflow separates generated checks, QEMU rehearsal,
and a physical stock candidate. Run it from the Actions tab on the default
branch. Ordinary PRs run the generated admission controls only.

## Draft a stock candidate

Build the keyless installed image with the exact player default-branch commit
and the components selected by `component-lock.json`. Use the OS package
tool to produce the proposed `update.tar`. Check those exact package bytes on
a physical Shanling Q2 starting with supported stock firmware, including stock
installation, generated local playback and recovery. Record the conditions,
starting firmware, commands and checks in a manual `hardware-qualification.json`.
Use a generated music card. Keep device identifiers, credentials, and
private logs out of the report.

The qualification record has schema `1` and these fields:

| Field | Value |
| --- | --- |
| `domain`, `device_model` | `physical-stock-q2`, `Shanling Q2` |
| `player_commit`, `components` | Exact player commit and complete component lock |
| `update_tar_sha256` | SHA-256 of the package actually installed in the stock test |
| `version`, `starting_version` | Package release and starting stock firmware version |
| `stock_install_passed`, `generated_local_playback_passed`, `recovery_passed` | Measured outcomes |
| `conditions`, `procedure` | Conditions and commands needed to understand the measurements |

A successful producer run in this repository uploads a GitHub Actions artifact
named `q2-stock-draft-inputs`. Its root contains `hardware-qualification.json`,
`release-inputs.json`, and an `image/` directory with `artifact.json`, `xImage`,
`rootfs.squashfs`, `rescue.cpio`, and `shanling-q2.dtb`. The release input
manifest has schema `1`, `mode` set to `stock-draft`, `player_commit`,
`components`, `version`, `installed_version`, and a `files` object mapping
all six payload paths to their SHA-256. The starting firmware must match the
qualification record. Preserve the input artifact while reviewing the draft.

Dispatch `stock-draft` with that producer run ID and the SHA-256 of
`release-inputs.json`. The workflow reads the locked OS package format, checks
the image provenance and keyless contents, recreates and reads back the
stock package, and requires its SHA-256 to match the actual stock test.
The draft job rechecks the verified bytes before attaching them. An existing
release or draft at the version is left in place. Publication takes a separate
maintainer action. A private OS repository requires a scoped read token stored
in `Q2_OS_READ_TOKEN`. Public OS sources can be read using the workflow token.

## Rehearse in QEMU

Dispatch `qemu-rehearsal` with an artifact named `q2-qemu-rehearsal-inputs`.
Use the same manifest layout with `mode` set to `qemu-rehearsal` and a
host-runnable `qemu-system-mipsel` executable for Ubuntu 24.04 in place of
the hardware qualification record. The workflow verifies the image, packs and
reads back its update archive, then boots that archive to root and player
readiness. The result stays in Actions as an emulator rehearsal; the job
has no release-writing permission.
