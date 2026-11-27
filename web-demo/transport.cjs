// Exercise the Wasm ABI deterministically without a browser or native fixtures.
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { createHash } = require("node:crypto");

const file = process.argv[2];
if (!file) throw new Error("Pass the built q2_wasm_demo.wasm path.");

(async () => {
  const module = await WebAssembly.compile(readFileSync(file));
  assert.deepEqual(WebAssembly.Module.imports(module), []);
  const { exports: wasm } = await WebAssembly.instantiate(module);
  const decoder = new TextDecoder();
  const label = () => decoder.decode(
    new Uint8Array(wasm.memory.buffer, wasm.demo_label(), wasm.demo_label_len()),
  );
  const mediaId = () => decoder.decode(
    new Uint8Array(wasm.memory.buffer, wasm.demo_media_id(), wasm.demo_media_id_len()),
  );
  let now;
  const frame = () => {
    const ptr = wasm.demo_frame(now);
    const pixels = new Uint8Array(wasm.memory.buffer, ptr, 375 * 320 * 4);
    assert.equal(pixels.at(-1), 255);
    return createHash("sha256").update(pixels).digest("hex");
  };
  const input = code => {
    wasm.demo_input(code);
    return frame();
  };
  const reset = () => {
    now = 0;
    wasm.demo_init();
    frame();
    assert.equal(label(), "Q2 · Music");
    assert.equal(mediaId(), "");
    assert.equal(wasm.demo_media_kind(), 0);
    assert.equal(wasm.demo_playing(), 0);
    assert.equal(wasm.demo_volume(), 44);
    assert.equal(wasm.demo_wheel_is_volume(), 0);
  };
  const select = text => {
    for (const direction of [1, 0]) {
      for (let i = 0; i < 32; i++) {
        const previous = label();
        const selection = previous.slice(previous.indexOf(" · ") + 3);
        if (selection === text || selection.startsWith(text + " · ")) return input(2);
        input(direction);
        input(8);
        if (label() === previous) break;
      }
    }
    assert.fail(`Cannot reach ${text} from ${label()}`);
  };
  const step = ms => {
    now += ms;
    return frame();
  };
  const openAlbum = name => {
    select("Music");
    select("Albums");
    select(name);
  };
  const home = () => {
    for (let i = 0; i < 7 && !label().startsWith("Q2 · "); i++) input(3);
    assert.match(label(), /^Q2 · /);
  };

  reset();
  openAlbum("Soft Rock Champion");
  const atStart = select("1. She Can Live Without Me");
  assert.match(label(), /^Now Playing · Playing She Can Live Without Me/);
  assert.equal(mediaId(), "northline-1");
  assert.equal(wasm.demo_media_kind(), 1);
  assert.equal(wasm.demo_speed_percent(), 100);
  assert.equal(wasm.demo_duration_ms(), 110000);
  const firstGeneration = wasm.demo_playback_generation();
  assert.notEqual(step(2300), atStart, "track progress updates the framebuffer");
  const paused = input(4);
  assert.match(label(), /Paused She Can Live Without Me/);
  assert.equal(step(3500), paused, "pause freezes the framebuffer");
  assert.equal(wasm.demo_playing(), 0);
  assert.equal(wasm.demo_position_ms(), 2300);
  input(4);
  assert.match(label(), /Playing She Can Live Without Me/);
  assert.equal(wasm.demo_playing(), 1);
  assert.equal(wasm.demo_playback_generation(), firstGeneration);
  step(108000);
  assert.match(label(), /Playing Julie Dream/, "natural completion advances and resets the track");
  assert.equal(mediaId(), "northline-2");
  assert.equal(wasm.demo_playback_generation(), firstGeneration + 1);
  assert.equal(wasm.demo_position_ms(), 0);
  input(5);
  assert.match(label(), /Playing She Can Live Without Me/, "previous before three seconds selects the previous track");
  assert.equal(mediaId(), "northline-1");
  step(4000);
  input(5);
  assert.equal(mediaId(), "northline-1");
  assert.equal(wasm.demo_position_ms(), 0);
  assert.equal(wasm.demo_playback_generation(), firstGeneration + 3);
  input(7);
  select("Up Next");
  assert.match(label(), /^Up Next · She Can Live Without Me/);

  reset();
  input(1);
  input(8);
  assert.notEqual(label(), "Q2 · Music", "Down still moves the home selection down");
  input(0);
  input(8);
  assert.equal(label(), "Q2 · Music", "Up still moves the home selection up");
  openAlbum("Soft Rock Champion");
  select("10. Your Skin's Not in the Carpet Anymore");
  assert.equal(wasm.demo_wheel_is_volume(), 1);
  input(0);
  assert.equal(wasm.demo_volume(), 46);
  input(1);
  assert.equal(wasm.demo_volume(), 44);
  input(4);
  input(2);
  assert.equal(wasm.demo_wheel_is_volume(), 0);
  input(1);
  assert.equal(wasm.demo_seek_revision(), 0, "scrub movement previews before commit");
  input(3);
  assert.equal(wasm.demo_seek_revision(), 1);
  assert.ok(wasm.demo_position_ms() > 0);
  assert.equal(wasm.demo_playing(), 0, "a paused scrub stays paused");
  assert.equal(wasm.demo_volume(), 44);
  assert.equal(wasm.demo_wheel_is_volume(), 1);
  input(4);
  step(wasm.demo_duration_ms());
  assert.equal(wasm.demo_playing(), 0, "end of the last entry stops playback");
  assert.equal(wasm.demo_position_ms(), wasm.demo_duration_ms());
  assert.equal(mediaId(), "northline-10");
  input(4);
  assert.equal(wasm.demo_playing(), 1);
  assert.equal(wasm.demo_position_ms(), 0);

  reset();
  select("Books");
  select("Continue Listening");
  select("The Wonderful Wizard of Oz");
  input(2);
  assert.equal(mediaId(), "demo-spoken-0");
  assert.equal(wasm.demo_media_kind(), 3);
  const spokenGeneration = wasm.demo_playback_generation();
  const beforeSkip = wasm.demo_position_ms();
  input(6);
  assert.equal(wasm.demo_position_ms(), beforeSkip + 30000);
  assert.equal(wasm.demo_seek_revision(), 1);
  assert.equal(wasm.demo_playback_generation(), spokenGeneration);
  input(4);
  input(5);
  const pausedSkip = beforeSkip + 15000;
  assert.equal(wasm.demo_position_ms(), pausedSkip);
  step(1000);
  assert.equal(wasm.demo_position_ms(), pausedSkip, "a spoken skip preserves pause");
  input(4);
  step(wasm.demo_duration_ms());
  assert.equal(wasm.demo_playing(), 0, "a finished book stops");

  reset();
  for (const name of ["Settings", "Playback", "Default Spoken Speed"]) select(name);
  input(1);
  home();
  for (const name of ["Books", "Continue Listening", "The Wonderful Wizard of Oz"]) select(name);
  input(2);
  assert.equal(wasm.demo_speed_percent(), 105, "books inherit the Settings default");
  input(2);
  input(2);
  input(1);
  assert.equal(wasm.demo_speed_percent(), 110, "the Now Playing speed wheel steps five points");
  const beforeSpeedAdvance = wasm.demo_position_ms();
  step(1000);
  assert.equal(wasm.demo_position_ms() - beforeSpeedAdvance, 1100);

  home();
  for (const name of ["Podcasts", "Library", "NASA's Curious Universe", "Encore: A Year In Mars Dune Alpha"]) select(name);
  assert.equal(wasm.demo_speed_percent(), 105, "the book override leaves the default intact");
  input(7);
  assert.match(label(), /^Podcast Episode · /);
  select("Playback Speed");
  assert.match(label(), /^Playback Speed · Playing/);
  input(1);
  input(1);
  assert.equal(wasm.demo_speed_percent(), 115);
  input(3);
  input(3);
  select("Encore: Welcome to the Dark Side");
  assert.equal(wasm.demo_speed_percent(), 115, "episodes inherit their own series' override");
  input(7);
  select("Playback Speed");
  assert.equal(wasm.demo_clickable(329, 182), 1);
  assert.equal(wasm.demo_click(329, 182), 1);
  frame();
  assert.equal(wasm.demo_speed_percent(), 300, "the pointer slider reaches its upper endpoint");
  input(1);
  assert.equal(wasm.demo_speed_percent(), 300, "wheel input clamps at the maximum");
  assert.equal(wasm.demo_click(46, 182), 1);
  frame();
  input(0);
  assert.equal(wasm.demo_speed_percent(), 50, "pointer and wheel clamp at the minimum");
  input(7);
  assert.equal(wasm.demo_speed_percent(), 105, "Hold Center erases the series override");
  home();
  for (const name of ["Books", "Continue Listening", "The Wonderful Wizard of Oz"]) select(name);
  input(2);
  assert.equal(wasm.demo_speed_percent(), 110, "reopening the book restores its override");
  input(7);
  assert.match(label(), /^Audiobook · /);
  select("Chapters");
  select("Chapters 3–4");
  assert.equal(wasm.demo_position_ms(), 838000, "the audiobook menu seeks to the selected chapter");

  reset();
  select("Radios");
  select("Featured Stations");
  select("FIP");
  assert.equal(mediaId(), "demo-radio-0");
  assert.equal(wasm.demo_media_kind(), 2);
  assert.equal(wasm.demo_duration_ms(), 0);
  step(300000);
  input(5);
  input(6);
  assert.equal(wasm.demo_position_ms(), 300000);
  assert.equal(mediaId(), "demo-radio-0");
  assert.equal(wasm.demo_playing(), 1, "a radio station has no timed finish");

  for (const [id, entry] of Object.entries(require("./catalog/radio.json"))) {
    reset();
    select("Radios");
    select("Featured Stations");
    select(entry.title);
    assert.equal(mediaId(), id);
    assert.equal(wasm.demo_duration_ms(), 0);
  }

  reset();
  wasm.demo_external_clock(1);
  openAlbum("Soft Rock Champion");
  select("1. She Can Live Without Me");
  const externalGeneration = wasm.demo_playback_generation();
  step(120000);
  assert.equal(wasm.demo_position_ms(), 0, "browser buffering holds the playhead");
  assert.equal(mediaId(), "northline-1");
  wasm.demo_media_progress(externalGeneration, 0, 29500, 110500);
  assert.equal(wasm.demo_position_ms(), 29500);
  assert.equal(wasm.demo_duration_ms(), 110500);
  input(2);
  input(1);
  input(3);
  const afterSeek = wasm.demo_position_ms();
  assert.ok(afterSeek > 29500);
  wasm.demo_media_progress(externalGeneration, 0, 30000, 110500);
  assert.equal(wasm.demo_position_ms(), afterSeek, "old media time cannot undo a seek");
  wasm.demo_media_progress(externalGeneration, 1, afterSeek + 1000, 110500);
  assert.equal(wasm.demo_position_ms(), afterSeek + 1000);
  wasm.demo_media_ended(externalGeneration);
  assert.equal(mediaId(), "northline-2", "browser media EOF advances the queue");
  wasm.demo_media_ended(externalGeneration);
  wasm.demo_media_progress(externalGeneration, 1, 100000, 110500);
  assert.equal(mediaId(), "northline-2", "stale callbacks cannot advance the replacement track");
  assert.equal(wasm.demo_position_ms(), 0);
  wasm.demo_external_clock(0);
  step(1000);
  assert.equal(wasm.demo_position_ms(), 1000, "preview mode owns its clock");

  reset();
  select("Settings");
  select("Connections");
  select("Offline Mode");
  assert.equal(wasm.demo_online(), 0);
  input(3);
  input(3);
  openAlbum("Soft Rock Champion");
  select("1. She Can Live Without Me");
  assert.match(label(), /^Soft Rock Champion .*Unavailable offline/);
  input(3);
  input(3);
  input(3);
  openAlbum("Cylinders");
  select("1. Cylinder One");
  assert.match(label(), /Playing Cylinder One/, "a downloaded album remains playable offline");

  console.log("Passed Wasm imports, framebuffer, audio state, volume, queue, seeks, radio, spoken media and offline controls.");
})().catch(error => { console.error(error); process.exitCode = 1; });
