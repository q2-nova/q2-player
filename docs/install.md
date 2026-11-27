# Install Q2 Nova on a Shanling Q2

These instructions cover the Q2 running stock **V1.32** and the
[Q2 Nova v0.1.0 release](https://github.com/q2-nova/q2-player/releases/tag/v0.1.0),
which appears on the device as **V0.10**. If the stable download is unavailable,
wait for publication. Check the stock version in **System settings → About**.

## Before you install

Fully charge the Q2. Use a healthy exFAT or FAT32 microSD card, a card reader,
and a backup of your card and settings. Save your original Shanling V1.32
update ZIP and prepare the two recovery files described below while you have
a working computer and card reader. Shanling's current download page lists
V1.36, so keep your V1.32 ZIP for this recovery procedure.
[Shanling downloads](https://en.shanling.com/download/150)

## Install V0.10

1. Download `update.tar` and `SHA256SUMS` from the stable release. Check that
   the SHA-256 of `update.tar` matches its entry in `SHA256SUMS`. For example,
   run `sha256sum update.tar` on Linux or `shasum -a 256 update.tar` on macOS.
2. With the Q2 off, use the reader to copy the unexpanded `update.tar` to
   the card root. Leave any rollback files in a separate folder off the
   card root. The card root must have no `recovery-update/` directory.
   Eject the card from your computer and insert it in the Q2.
3. In the stock player, select **System settings → System Update → TF card
   update** and confirm. Leave the card in and keep the Q2 powered on until
   the update finishes and the player restarts.
4. Unplug the USB cable if first boot opens **USB Drive**. If the computer
   mounted the card, eject it in the computer's operating system first.
   Confirm **Settings → About → OS Version** shows **V0.10**.

## First listen from the card

You can try local playback without setting up Wi-Fi or a server. Use an
existing M4B audiobook, or make a short generated sample on a computer with
FFmpeg:

```sh
ffmpeg -v error -n -f lavfi -i 'sine=frequency=550:duration=15' \
  -metadata 'title=Generated First Listen' -c:a aac -f mp4 first-listen.m4b
```

Copy the file to `Q2Mainline/media/audiobooks/` on the card, creating the
folders if needed. Safely eject the card and boot the Q2. With no Navidrome
server set up, the Q2 starts on the **Navidrome Server** page; press Back to
reach the home screen. From the home screen, open **Books → On This Card**, select **Generated First Listen** (or
your book's title), and press Center on **Book Info**. Use headphones at low
volume and check that Now Playing advances and pauses.

## Restore stock V1.32

1. On the computer, open the original Shanling V1.32 ZIP, then open the
   `update.tar` inside. Save its `recovery-update/xImage` and
   `recovery-update/rootfs.squashfs` together. Keep these recovery files off
   the card root while installing Q2 Nova.
2. To restore stock, power off the Q2 completely. With a card reader, copy
   the **stock** files to `recovery-update/xImage` and
   `recovery-update/rootfs.squashfs` at the card root. Eject the card safely
   and put it in the Q2.
3. Hold **Previous** and press **Center** to power on. Release the buttons
   once recovery starts. Leave the card in until recovery finishes and the
   Q2 restarts. Check **System settings → About** for **V1.32**.
4. Put the candidate `update.tar` away before using the stock update menu
   again. Recovery consumes the card-root `recovery-update/` directory,
   so keep your backup copy.

If the Q2 boots normally without restoring stock, power it off and check the
card layout before trying again. Recovery repairs the installed system. It may
reset internal settings if the device cannot mount its internal data.
