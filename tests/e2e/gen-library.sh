#!/bin/bash
# A test library for end-to-end runs: tagged sine tones with covers, in
# build/nd/music. Needs ffmpeg.
set -e
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$ROOT/build/nd/music"
cd "$ROOT/build/nd/music"
gen() { # artist album year track title fmt colour seconds genre
  d="$1/$2"; mkdir -p "$d"
  [ -f "$d/cover.jpg" ] || ffmpeg -loglevel error -f lavfi -i "color=c=$7:s=600x600" -frames:v 1 "$d/cover.jpg"
  f="$d/$(printf %02d "$4") $5.$6"
  ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=$((200 + $4 * 40)):duration=$8" -ac 2 -ar 44100 \
    -metadata artist="$1" -metadata album_artist="$1" -metadata album="$2" -metadata date="$3" \
    -metadata track="$4" -metadata title="$5" -metadata genre="$9" "$f"
}
gen "Aurora Lines" "Northern Static" 2019 1 "First Light" flac navy 40 Electronic
gen "Aurora Lines" "Northern Static" 2019 2 "Polar Drift" flac navy 35 Electronic
gen "Aurora Lines" "Northern Static" 2019 3 "The Long Night Is Over Now and Then Some" flac navy 30 Electronic
gen "Aurora Lines" "Coastal" 2022 1 "Tidewater" mp3 teal 30 Electronic
gen "Aurora Lines" "Coastal" 2022 2 "Harbour" mp3 teal 30 Electronic
gen "The Paper Kites" "Paper Moon" 2015 1 "Paper Moon" flac maroon 30 Folk
gen "The Paper Kites" "Paper Moon" 2015 2 "Arrows" flac maroon 30 Folk
gen "Sigur Test" "Ágætis" 1999 1 "Svefn-g-englar" flac olive 30 "Post-Rock"
gen "Sigur Test" "Ágætis" 1999 2 "Starálfur" flac olive 30 "Post-Rock"
