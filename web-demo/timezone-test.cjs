"use strict";

const fs = require("node:fs");
const vm = require("node:vm");
const assert = require("node:assert/strict");
const path = process.argv[2];
if (!path) throw new Error("Pass the built player.wasm path.");

async function main() {
  const { instance: { exports: demo } } = await WebAssembly.instantiate(fs.readFileSync(path));
  const decoder = new TextDecoder();
  const script = fs.readFileSync(require.resolve("./site/timezone.js"), "utf8");
  const WallClock = vm.runInNewContext(script + "\nPlayerWallClock");
  const clock = new WallClock(decoder);
  const read = (ptr, len) => decoder.decode(new Uint8Array(demo.memory.buffer, ptr(), len()));
  const label = () => read(demo.demo_label, demo.demo_label_len);
  const zone = () => read(demo.demo_timezone_ptr, demo.demo_timezone_len);
  let elapsed = 0;
  let epoch = Date.UTC(2026, 0, 15, 12);
  function frame() {
    clock.sync(demo, epoch);
    demo.demo_frame(elapsed += 150);
  }
  function input(code) {
    demo.demo_input(code);
    demo.demo_input(8);
    frame();
  }
  function pick(item) {
    for (const direction of [1, 0]) {
      for (let attempt = 0; attempt < 500; attempt++) {
        if (label().split(" · ")[1] === item) return;
        const previous = label();
        demo.demo_input(direction);
        demo.demo_input(8);
        if (label() === previous) break;
      }
    }
    assert.fail(`Unable to find ${item} from ${label()}`);
  }
  function open(item) {
    pick(item);
    input(2);
  }
  function shows(text) {
    assert.ok(label().includes(text), `Expected ${text} in ${label()}`);
  }

  function picker() {
    open("Settings"); open("System"); open("Date & Time"); open("Time Zone");
  }
  function apply(item, id) {
    open(item);
    shows("Change Time Zone? · Cancel");
    open("Apply & Restart");
    assert.match(label(), /^Q2 · /);
    assert.equal(zone(), id);
  }

  demo.demo_init();
  demo.demo_external_clock(1);
  frame();
  assert.equal(zone(), "Etc/UTC");
  open("Settings"); open("Display"); open("Time in Title");
  shows("12:00 PM");
  input(3); open("System"); open("Date & Time"); open("24-Hour Clock");
  shows("12:00");
  open("Time Zone");
  shows("Time Zone · GMT+0 UTC");
  input(2);
  shows("Date & Time · Time Zone");
  for (let i = 0; i < 3; i++) input(3);
  open("Music"); open("Cover Flow"); input(2);
  open("Shuffle");
  assert.equal(demo.demo_playing(), 1);
  assert.equal(demo.demo_media_kind(), 1);
  const playingGeneration = demo.demo_playback_generation();
  const mediaId = () => read(demo.demo_media_id, demo.demo_media_id_len);
  const playingTrack = mediaId();
  const position = 23456;
  demo.demo_media_progress(playingGeneration, demo.demo_seek_revision(), position, demo.demo_duration_ms());
  assert.equal(demo.demo_position_ms(), position);
  input(3); input(3); input(3); input(3);

  picker();
  open("GMT+5:45 Kathmandu");
  shows("Change Time Zone? · Cancel");
  input(2);
  shows("Time Zone · GMT+5:45 Kathmandu");
  assert.equal(zone(), "Etc/UTC");
  assert.equal(demo.demo_playing(), 1);
  input(2);
  input(1); input(3);
  shows("Time Zone · GMT+5:45 Kathmandu");
  assert.equal(zone(), "Etc/UTC");
  assert.equal(demo.demo_playing(), 1);
  apply("GMT+5:45 Kathmandu", "Asia/Kathmandu");
  assert.equal(demo.demo_playing(), 0);
  assert.equal(mediaId(), playingTrack);
  assert.equal(demo.demo_position_ms(), position);
  assert.notEqual(demo.demo_playback_generation(), playingGeneration);
  demo.demo_media_ended(playingGeneration);
  input(4);
  assert.equal(demo.demo_playing(), 1);
  assert.equal(mediaId(), playingTrack);
  assert.equal(demo.demo_position_ms(), position);
  input(4);
  shows("17:45");

  picker();
  shows("Time Zone · GMT+5:45 Kathmandu");
  apply("GMT+12:45 Chatham", "Pacific/Chatham");
  shows("01:45");
  epoch = Date.UTC(2026, 6, 15, 12);
  frame();
  shows("00:45");
  picker();
  apply("GMT-6 Chicago", "America/Chicago");
  shows("07:00");
  epoch = Date.UTC(2026, 0, 15, 12);
  frame();
  shows("06:00");
  picker();
  apply("GMT+3:30 Tehran (Fixed)", "<+0330>-3:30");
  shows("15:30");
  picker();
  shows("Time Zone · GMT+3:30 Tehran (Fixed)");
  apply("GMT+0 UTC", "Etc/UTC");
  shows("12:00");
  console.log("PASS flat GMT timezone picker, current choice, Cancel/Back, simulated restart and paused track restoration, legacy rule, fractional offsets, DST and rendered 12/24-hour clock");
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
