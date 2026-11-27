# Set up Q2 Nova

This guide connects an installed Q2 to Wi-Fi, a Navidrome music server, an
Audiobookshelf server and Bluetooth headphones. Each part is optional: files
on the microSD card play without any of them.

## Controls

Turn the wheel to move through a list and press Center to choose. Back
returns to the previous page. Holding Center opens actions for the selected
item, or the Sleep and Power Off menu on the home screen.

Text entry shows a grid of characters. Turn the wheel to a character and
press Center to type it. **ABC** and **#+=** switch character sets and
**Delete** removes the last character. Choose **OK** or hold Center to save.
Back closes the editor without saving. For long addresses and passwords,
[edit the settings on a computer](#edit-the-settings-on-a-computer).

With no Navidrome server set up, the player starts on the **Navidrome
Server** page. Press Back to reach the home screen.

## Join Wi-Fi

1. Open **Settings → Connections → Wi-Fi** and turn **Wi-Fi** on.
2. Choose **Scan**, then select your network. Enter its password if asked.
3. **Connected To** shows the network name and the player's address.

**Forget This Network…** on the same page removes the saved network.

## Sign in to a guest network

Some venue, airport and in-flight networks ask for a sign-in page before
they let traffic through. When the player finds one after joining, it shows
**Wi-Fi sign-in needed**.

1. Open **Settings → Connections → Wi-Fi → Wi-Fi Sign-In**. The row appears
   once the player has an address.
2. For a guest agreement page, the player shows **Terms & Conditions apply**
   with **Agree** selected. Press Center to accept.
3. **Internet Available** means the network let the player through.
   **This Wi-Fi sign-in needs a browser** means the page needs something the
   player does not do, such as an ad or a button that needs JavaScript.

The player accepts only a free option with visible agreement wording. It
also signs in to Delta Sync in-flight Wi-Fi with a SkyMiles account.
Downloaded music, books and podcasts play while the network waits for
sign-in. [Wi-Fi sign-in details and Delta Sync](portal-sign-in-test.md)

## Connect to Navidrome

The player streams and downloads music from a
[Navidrome](https://www.navidrome.org/) server.

1. Open **Set Up Server** on the home screen, or **Settings → Connections →
   Navidrome Server**. Until a server is set up, the player also opens this
   page at start.
2. Enter the address you use to open Navidrome in a browser under
   **Address**, for example `https://music.example.com`. Fill in **User** and
   **Password**.
3. Choose **Test Connection**. When it shows **Connected**, open
   **Music → Albums** and select a track.

The password is sent to the server only as a salted token. To use an
OpenSubsonic API key instead of a user and password, set `api_key` in the
`[server]` section of the settings file.

## Add Audiobookshelf

The player plays books and podcasts from an
[Audiobookshelf](https://www.audiobookshelf.org/) server.

1. In Audiobookshelf, create an API key for your account.
2. On the Q2, open **Settings → Connections → Audiobookshelf**. Enter the
   server's **Address** and the key under **API Token**.
3. Choose **Test Connection**. When it shows **Connected**, choose
   **Sync Now** to load your library.

**Books** and **Podcasts** then appear on the home screen. Books also lists
M4B files from the card under **On This Card**. [Audiobooks and
podcasts](audiobooks.md)

## Reach a server over Tailscale

If your server is on a Tailscale network, open **Settings → Connections →
Tailscale**, turn it on and choose **Sign In**. Open the link or QR code it
shows on a phone or computer, and approve the device in the Tailscale admin
console if your network requires it. Enter the server's Tailscale IP address
or full tailnet name as its Address. MagicDNS short names are not supported.

## Pair Bluetooth headphones

1. Open **Settings → Bluetooth** and turn **Bluetooth** on.
2. Put the headphones in pairing mode and choose **Pair New Device**.
3. Select the headphones from the list. The player shows **Connected to …**
   and moves playback to them.

Paired headphones are listed on the same page. Select one to connect,
disconnect or forget it. The next time Bluetooth starts, the player
reconnects the last headphones used. When the headphones disconnect,
playback pauses and the wired output takes over. **Preferred Codec**
picks LDAC, aptX HD, aptX, AAC or SBC. **Automatic**, the default, uses the
best codec both sides support. [Bluetooth headphones](bluetooth.md)

## Edit the settings on a computer

The player keeps its settings in `Q2Mainline/config.toml` on the microSD
card. It creates the file on first start with a writable card.

1. Power off the Q2 and put its card in a card reader.
2. Open `Q2Mainline/config.toml` in a text editor and change the entries.
3. Save the file, eject the card safely and put it back in the Q2 before
   powering on.

```toml
[wifi]
enabled = true

[[wifi.networks]]
ssid = "Your Wi-Fi name"
psk = "Your Wi-Fi password"

[server]
url = "https://music.example.com"
user = "Your Navidrome user name"
password = "Your Navidrome password"

[audiobookshelf]
url = "https://books.example.com"
token = "Your Audiobookshelf API key"
```

If the file contains `networks = []` under `[wifi]`, replace that line with
the `[[wifi.networks]]` block. [Every settings page and its
keys](settings.md)

## Radio and scrobbling

**Radios → Featured Stations** plays without a server, and Navidrome's web
radio stations appear under **Radios**. [Internet radio](radio.md) ·
[Scrobbling to ListenBrainz and Last.fm](settings.md#scrobbling)
