"use strict";

// Capture the actual WASM renderer without browser scaling, color conversion,
// wall-clock motion, or display timing. Python supplies the shared input plan.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

async function main(wasm, plan, out) {
  const { instance: { exports: demo } } = await WebAssembly.instantiate(fs.readFileSync(wasm));
  const steps = JSON.parse(fs.readFileSync(plan, "utf8"));
  const decoder = new TextDecoder();
  const selections = {};
  let elapsed = 0;
  const label = () => decoder.decode(new Uint8Array(demo.memory.buffer, demo.demo_label(), demo.demo_label_len()));
  function frame() {
    elapsed += 150;
    return demo.demo_frame(elapsed);
  }
  function input(code) {
    demo.demo_input(code);
    frame();
  }
  demo.demo_init();
  demo.demo_local_minutes(12 * 60);
  frame();
  assert.equal(label(), "Q2 · Music");
  for (const step of steps) {
    if (typeof step === "string") {
      input({ center: 2, back: 3 }[step]);
    } else if ("scroll" in step) {
      const count = step.wasm_scroll ?? step.scroll;
      for (let i = 0; i < Math.abs(count); i++) {
        input(count > 0 ? 1 : 0);
        input(8); // Reset wheel acceleration between detents.
      }
    } else {
      const pixels = frame();
      const [title, selection] = label().split(" · ");
      assert.equal(title, step.screen, step.shot);
      selections[step.shot] = selection ?? null;
      fs.writeFileSync(path.join(out, step.shot + ".rgba"),
        Buffer.from(demo.memory.buffer, pixels, 375 * 320 * 4), { flag: "wx" });
      console.log(`${step.shot}: ${label()}`);
    }
  }
  fs.writeFileSync(path.join(out, "labels.json"), JSON.stringify(selections, null, 2), { flag: "wx" });
}

main(...process.argv.slice(2)).catch(error => { console.error(error); process.exitCode = 1; });
