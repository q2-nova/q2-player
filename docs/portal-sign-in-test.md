# Test Wi-Fi sign-in on a real network

The player can get past three kinds of Wi-Fi sign-in page without a browser:
an airport or venue guest agreement, Delta Sync with SkyMiles details typed on
the wheel, and Delta Sync with SkyMiles details typed on a phone. This guide
shows how to start each one, what the player records, and how to report a
failure. These flows have passed generated tests only; a real network is the
test.

## Before you join the network

1. Insert a writable microSD card with at least 4 MiB free. The player writes
   its sign-in record to the card. Without a card, or with a full one, sign-in
   still works but leaves no record.
2. For Delta phone entry, prepare the account before joining the aircraft
   Wi-Fi (see [Delta with a phone](#delta-with-a-phone)).
3. Have one downloaded track and one track that is not downloaded. Downloaded
   music, books and podcasts play while the network waits for sign-in.

## Start sign-in

1. Join the network in **Settings → Connections → Wi-Fi**.
2. When the player gets an address it checks for a sign-in page once. If it
   finds one it shows **Wi-Fi sign-in needed**.
3. Open **Settings → Connections → Wi-Fi → Wi-Fi Sign-In**. The row appears
   once the player has an address. Its detail reads **Sign-in required** for a
   page the player can handle and **A browser is required** for one it cannot.
   Opening the row checks again.

Back leaves any flow without sending anything further.

## Airport or venue guest agreement

1. Open **Wi-Fi Sign-In**. The player shows **Terms & Conditions apply** with
   **Agree** selected. Read the full terms on a phone if you want to; the player
   does not show them.
2. Press Center on **Agree**. The player submits the free-access form once.
3. Expected results:
   - **Internet Available**: the network let the player through.
   - **Wi-Fi still needs sign-in** with **Retry**: the form went through but the
     network still blocks the player.
   - **This Wi-Fi sign-in needs a browser**: the page needs something the player
     does not do, such as watching an ad or a JavaScript-only button. A free
     choice followed by an ad page ends here.
4. Play the track that is not downloaded.

The player picks only a free choice with visible agreement wording. It never
opens the terms link, an account login or a paid option.

## Delta on the wheel

1. Join **DeltaWiFi.com** and open **Wi-Fi Sign-In**. The screen is titled
   **Delta Wi-Fi**.
2. Choose **Enter SkyMiles Details**. The player first contacts Delta. When
   Delta asks for the account, **SkyMiles Account** opens.
3. Type with the wheel: turn to a character and press Center to add it. Hold
   Center, or choose OK, to finish the field. Back cancels. **SkyMiles
   Password** opens next.
4. Expected results:
   - **Internet Available**.
   - **Delta showed a video. This player still needs Wi-Fi access.** Delta
     answered with a video page and did not admit the player within about
     30 seconds. The player does not play the video or press Skip.
   - Another Delta message, for example a password or account challenge.
     Nothing further is sent.
5. Play the track that is not downloaded.

## Delta with a phone

The player starts a private Wi-Fi network for one phone and shows two QR
codes. The phone sends the SkyMiles details to the player; the player signs in
to Delta itself.

1. Open **Settings → Connections → Wi-Fi → Prepare Delta Sign-In** and choose
   **Start Phone Setup**. Do this before joining the aircraft Wi-Fi, or while
   connected on a 2.4 GHz channel. The private network shares the radio's
   current channel and cannot start on the 5 GHz channels that need radar
   checks.
2. Scan the first QR code to join the player's Wi-Fi. Press Center to show the
   second code, then scan it to open the setup page. Enter the SkyMiles account
   and password there and send them.
3. The player shows **Account Ready**, available for 30 minutes on this player.
   Choose **Done**. The private network and the phone connection close.
4. Join **DeltaWiFi.com**, open **Wi-Fi Sign-In** and choose **Use Prepared
   Account**. On the Delta Wi-Fi screen, **Enter Details on Phone** starts the
   same setup if no account is ready.
5. Expected results are the same as for the wheel. After a temporary failure
   before the password step, **Retry** keeps the prepared account. A submitted
   account is used once.
6. Play the track that is not downloaded.

**Clear Prepared Account** removes the details from the player.

## What the player records

Each attempt gets its own folder on the card, `portal-debug/<ID>/`. Detection
at join, consent, phone setup and each retry are separate attempts.

- `events.jsonl`: the attempt in order. Each HTTP request and its response
  share a number `n`. Records include timing, status, redirect targets, cookie
  counts, an outline of each page (visible text, form fields and buttons, link
  and script address shapes) and the helper's decision about the page.
- `player-outcome.json`: what the player saw at the end: completion,
  cancellation, timeout or a network change.

Passwords, cookies, authorization values and the QR secret are left out. Raw
scripts and media are left out. The record can identify the venue, network and
nearby access point. Each attempt is capped at 2 MiB and 256 events. A missing
final `close` line means the attempt was interrupted; keep partial folders.

## Report a failure

1. Leave the card in until the player shows its final message.
2. Write down the network name, the screen and message the player showed, the
   approximate time, and whether other devices on the same network had
   internet.
3. Shut down normally or eject the card safely, then copy the whole
   `portal-debug/` folder. Do not edit or rename the folders.
4. Send the folder and the notes privately. Do not attach them to a public
   issue: they can identify the venue and network.

One attempt per network is enough. An unfamiliar page or an error is a useful
result.
