# First listen in the simulator

On Linux, install Rustup, Python 3.11 or newer, a C compiler and linker with
the platform development headers, and FFmpeg with its AAC encoder. Rustup
selects the repository's pinned toolchain. The first build may download it
and the locked Cargo dependencies.

From the repository root, run:

```sh
python3 tests/e2e/first-listen.py --out build/first-listen
```

The command builds the native player with two jobs, generates a short AAC
audiobook and drives **Books → On This Card** in the headless simulator. It
checks at least 750 ms of playback progress with output open, a stable pause,
and a second listen to the same file in **Offline Mode** with a fresh data
directory. No server or login is needed. Headless output measures the player's
simulated audio path. Use the windowed simulator to hear the file.

Check `build/first-listen/result.json` for the measured positions and artifact
hashes, or the retained scripts, logs and numeric observations for failures.
Use a fresh `--out` for each run. The compiled player is reused across runs in
`build/first-listen-target`.

On a desktop with X11 or XWayland and ALSA support, open the first run's
generated library with:

```sh
build/first-listen-target/debug/q2nd --sim --data build/first-listen/first/card
```

Press Return at **Navidrome Server**, then open **Books → On This Card**.
Select **Generated First Listen** and press Center on **Book Info**.

## End-to-end runs against a local Navidrome

```sh
tests/e2e/gen-library.sh                     # build/nd/music
podman run -d --name q2nd-navidrome -p 127.0.0.1:4533:4533 \
  -v "$PWD/build/nd/music:/music:ro,Z" -v "$PWD/build/nd/data:/data:Z" \
  -e ND_SCANSCHEDULE=0 docker.io/deluan/navidrome:latest
curl -X POST http://127.0.0.1:4533/auth/createAdmin -H 'Content-Type: application/json' \
  -d '{"username":"q2","password":"q2test"}'  # first run only
curl "http://127.0.0.1:4533/rest/startScan?u=q2&p=q2test&v=1.16.1&c=t&f=json"
```

Then a data folder with a `config.toml` pointing at it:

```toml
[server]
url = "http://127.0.0.1:4533"
user = "q2"
password = "q2test"
```

and `q2nd --headless --data build/run --script FILE` (see `q2nd --help` for
the script's steps), or the simulator window without `--headless`. Stopping
the container (`podman stop q2nd-navidrome`) mid-way tests offline use.

## Wi-Fi sign-in journeys

```sh
python3 tests/portal/run.py --out build/portal-journeys --os-repo ../q2-os
```

The command builds `q2-portal` and the `q2nd` tests with two jobs, generates a
TLS root and venue certificate, and runs nine journeys on generated guest and
Delta venues inside an unprivileged bubblewrap sandbox with its own network.
They cover helper detection and consent, Delta sign-in with trusted and
untrusted certificates, video-page admission, the player supervisor, and the
App's Wi-Fi screen with wheel, saved-account and phone entry followed by an uncached stream.
The phone journeys use the access-point watcher and stand-in radio tools from a
q2-os checkout; without `--os-repo` they are skipped. The Delta journeys use a
copy of the helper whose only source change trusts the generated root; the
build refuses any other difference. Results are in
`build/portal-journeys/result.json`. The script's help lists each journey;
`--only NAME` runs one.

Bubblewrap needs unprivileged user namespaces. On Ubuntu 24.04, allow them with
`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.

On the Ubuntu 24.04 GitHub runner, `app-phone` stalls before the first QR
code: the App test times out waiting for the Wi-Fi QR, and the access-point
watcher never creates `/run/q2-portal-phone`. The same journey passes on a
Fedora host, and `phone-ap` (the watcher with the same stand-in radio) passes
on the runner. The workflow passes `--known-issue app-phone`, which reports the
result without failing the job; the other journeys are required.
