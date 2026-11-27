"use strict";

const fs = require("node:fs");
const assert = require("node:assert/strict");
const path = process.argv[2];
if (!path) throw new Error("Pass the built player.wasm path.");

async function main() {
  const { instance: { exports: demo } } = await WebAssembly.instantiate(fs.readFileSync(path));
  const decoder = new TextDecoder();
  const read = () => decoder.decode(new Uint8Array(demo.memory.buffer, demo.demo_label(), demo.demo_label_len()));
  let elapsed = 0;

  function step(code) {
    demo.demo_input(code);
    demo.demo_input(8);
    demo.demo_frame(elapsed += 200);
  }

  function open(label) {
    for (const direction of [1, 0]) {
      for (let i = 0; i < 64; i++) {
        const before = read();
        if (before.split(" · ")[1] === label) {
          step(2);
          return;
        }
        step(direction);
        if (read() === before) break;
      }
    }
    assert.fail(`Cannot open ${label} from ${read()}`);
  }

  function selected(title, label) {
    assert.equal(read().split(" · ").slice(0, 2).join(" · "), `${title} · ${label}`);
  }

  demo.demo_init();
  demo.demo_external_clock(1);
  demo.demo_frame(elapsed);
  open("Settings"); open("Sound");
  open("DAC Filter"); selected("DAC Filter", "Short Slow");
  step(0); selected("DAC Filter", "Short Sharp");
  step(3);
  open("DAC Filter"); selected("DAC Filter", "Short Slow");
  step(0); step(2);
  open("DAC Filter"); selected("DAC Filter", "Short Sharp");
  step(3);
  open("Headphone Power"); selected("Headphone Power", "Efficient");
  step(1); selected("Headphone Power", "Standard"); step(3);
  open("Volume Normalization"); selected("Volume Normalization", "Off");
  step(1); step(1); selected("Volume Normalization", "Album"); step(3);
  step(3);

  open("Playback");
  open("Streaming Quality"); selected("Streaming Quality", "MP3 320");
  step(1); selected("Streaming Quality", "MP3 192"); step(2);
  open("Streaming Quality"); selected("Streaming Quality", "MP3 192"); step(3);
  open("Download Quality"); selected("Download Quality", "Original"); step(3);
  open("Skip Forward"); selected("Skip Forward", "30 Seconds");
  step(1); selected("Skip Forward", "45 Seconds"); step(2);
  open("Skip Backward"); selected("Skip Backward", "15 Seconds");
  step(0); selected("Skip Backward", "10 Seconds"); step(2);
  open("Auto Sleep Timer");
  open("Duration"); selected("Duration", "Off"); step(3);
  open("Auto Timer Starts"); selected("Auto Timer Starts", "22:00"); step(3);
  open("Auto Timer Ends"); selected("Auto Timer Ends", "07:00"); step(3);

  step(3); step(3); step(3);
  open("Books"); open("Continue Listening"); step(2); step(2);
  const position = demo.demo_position_ms();
  step(6); assert.equal(demo.demo_position_ms(), position + 45000);
  step(5); assert.equal(demo.demo_position_ms(), position + 35000);
  console.log("PASS native setting defaults, checked selections, Back/apply, and separate skip settings reaching playback");
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
