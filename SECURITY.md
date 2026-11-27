# Security policy

Report a vulnerability privately: open this repository's **Security** tab
and choose **Report a vulnerability**. Do not open a public issue or pull
request for it.

Include the affected release or commit, the steps to reproduce, and what an
attacker can do. Leave out your own credentials, server addresses and
device identifiers.

## Supported versions

Security fixes go to `main` and ship in the next release. Emulator
rehearsal prereleases are not for devices and receive no fixes.

## Scope

This repository covers the player (`q2nd`), the Wi-Fi sign-in portal
(`q2-portal`), their network clients and the update package they are
released in. Report problems in the Linux system, compiler or Tailscale
daemon to the repository that builds them.
