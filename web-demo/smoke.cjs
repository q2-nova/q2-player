// Exercise the packaged browser boundary. Playwright is an optional QA dependency.
const assert = require("node:assert/strict");
const { chromium } = require("playwright");

async function run(url) {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
    const errors = [];
    const external = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("request", request => {
      if (new URL(request.url()).origin !== new URL(url).origin) external.push(request.url());
    });
    await page.goto(url);
    await page.locator("#media-mode").selectOption("preview");
    await page.locator(".player").focus();
    const state = page.locator("#current-state");
    const wait = async (expression) => {
      await page.waitForFunction(pattern =>
        new RegExp(pattern, "i").test(document.getElementById("current-state").textContent),
        expression, { timeout: 4000 });
    };
    const changed = async (action) => {
      const before = await state.textContent();
      await action();
      await page.waitForFunction(previous =>
        document.getElementById("current-state").textContent !== previous,
        before, { timeout: 4000 });
      return state.textContent();
    };
    await wait("Music");
    const home = await state.textContent();
    await page.keyboard.press("Enter");
    await wait("Cover Flow");
    const music = await state.textContent();
    await changed(() => page.getByRole("button", { name: "Select", exact: true }).click());
    await wait("Cover Flow");
    const sleeve = await changed(() => page.keyboard.press("ArrowDown"));
    await changed(() => page.keyboard.press("Enter"));
    await wait("Shuffle");
    const track = await changed(() => page.keyboard.press("ArrowDown"));
    await changed(() => page.keyboard.press("Enter"));
    await wait("Now Playing");
    const playing = await state.textContent();
    const wheelBox = await page.locator(".wheel").boundingBox();
    const radius = wheelBox.width * .40;
    const cx = wheelBox.x + wheelBox.width / 2;
    const cy = wheelBox.y + wheelBox.height / 2;
    await page.mouse.move(cx, cy - radius);
    await page.mouse.down();
    for (const degrees of [-80, -70, -60, -50]) {
      const angle = degrees * Math.PI / 180;
      await page.mouse.move(cx + radius * Math.cos(angle), cy + radius * Math.sin(angle));
    }
    await page.mouse.up();
    await wait("Volume 46%");
    await page.getByRole("button", { name: "Scroll down" }).click();
    await wait("Volume 44%");
    await changed(() => page.keyboard.press("Space"));
    await wait("pause");
    await changed(() => page.getByRole("button", { name: "Play or pause" }).click());
    await wait(" · Playing ");
    await changed(() => page.keyboard.press("ArrowRight"));
    await page.keyboard.press("Escape");
    await wait(track.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
    await page.keyboard.press("Escape");
    await wait(sleeve.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
    await page.keyboard.press("Escape");
    await wait("Cover Flow");
    await page.keyboard.press("Escape");
    await wait("Music");
    assert.ok((await state.textContent()).startsWith(home));
    await page.keyboard.down("Escape");
    await wait("^Now Playing");
    await page.keyboard.up("Escape");
    assert.ok((await state.textContent()).startsWith("Now Playing"));
    await page.keyboard.press("Escape");
    await wait("^Q2");
    assert.notEqual(music, sleeve);
    assert.notEqual(track, playing);
    assert.deepEqual(errors, []);
    assert.deepEqual(external, []);
    await touchJourney(browser, url);
    if (new URL(url).protocol !== "file:") {
      await retryJourney(browser, url);
      await stalledSampleJourney(browser, url);
    }
    console.log("Passed Cover Flow, select, transport, Back, hold shortcut and touch-wheel journeys.");

  } finally {
    await browser.close();
  }
}

async function touchJourney(browser, url) {
  const page = await browser.newPage({ viewport: { width: 360, height: 740 }, hasTouch: true, isMobile: true });
  await page.goto(url);
  await page.waitForFunction(() => document.getElementById("current-state").textContent.includes("Music"));
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
  const wheel = page.locator(".wheel");
  await wheel.scrollIntoViewIfNeeded();
  const box = await wheel.boundingBox();
  const center = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  const point = (degrees) => ({
    x: center.x + Math.cos(degrees * Math.PI / 180) * box.width * .39,
    y: center.y + Math.sin(degrees * Math.PI / 180) * box.height * .39,
  });
  const cdp = await page.context().newCDPSession(page);
  // Start on MENU and rotate just over one hardware detent.
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [point(-90)] });
  for (let a = -80; a <= -50; a += 10) {
    await cdp.send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: [point(a)] });
  }
  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  await page.waitForFunction(() => document.getElementById("current-state").textContent === "Q2 · Books");
  await page.getByRole("button", { name: "Select", exact: true }).tap();
  await page.waitForFunction(() => document.getElementById("current-state").textContent.startsWith("Books ·"));
  await page.close();
}

async function retryJourney(browser, url) {
  const page = await browser.newPage();
  await page.route("**/player.wasm", route => route.abort(), { times: 1 });
  await page.goto(url);
  const retry = page.getByRole("button", { name: "Retry" });
  await retry.waitFor();
  assert.ok(await page.getByRole("button", { name: "Select", exact: true }).isDisabled());
  await retry.click();
  await page.waitForFunction(() => document.getElementById("current-state").textContent === "Q2 · Music");
  await page.locator("#media-mode").selectOption("preview");
  await page.locator(".player").focus();
  // A failed audio download should leave the player usable and retry the
  // current selection without reloading the library or changing tracks.
  await page.route("**/audio-*.mp3", route => route.abort(), { times: 1 });
  await page.keyboard.press("Enter"); // Music
  await page.keyboard.press("Enter"); // Cover Flow
  await page.keyboard.press("Enter"); // Album
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter"); // First track
  const retryAudio = page.getByRole("button", { name: "Retry audio" });
  await retryAudio.waitFor({ timeout: 4000 });
  await retryAudio.click();
  await retryAudio.waitFor({ state: "hidden", timeout: 4000 });
  await page.waitForFunction(() => document.getElementById("current-state").textContent.includes(" · Playing "));
  await page.close();
}

async function stalledSampleJourney(browser, url) {
  const page = await browser.newPage();
  await page.addInitScript(() => {
    const start = AudioBufferSourceNode.prototype.start;
    AudioBufferSourceNode.prototype.start = function (...args) {
      start.apply(this, args);
      window.sampleStarted = true;
    };
  });
  await page.goto(url);
  const state = page.locator("#current-state");
  await page.waitForFunction(() => document.getElementById("current-state").textContent === "Q2 · Music");
  await page.locator("#media-mode").selectOption("preview");
  // Hold the first response open. It should time out without a network error
  // being sent by this fixture. Deliver the same asset on retry.
  await page.route("**/audio-*.mp3", () => {}, { times: 1 });
  await page.keyboard.press("Enter"); // Music
  await page.keyboard.press("Enter"); // Cover Flow
  await page.keyboard.press("Enter"); // Album
  await page.keyboard.press("ArrowDown");
  const stalled = page.waitForRequest("**/audio-*.mp3");
  await page.keyboard.press("Enter"); // First track
  const sampleUrl = (await stalled).url();
  await page.waitForFunction(() => document.getElementById("current-state").textContent.includes(" · Playing "));
  const selection = (await state.textContent()).split(" · Playing ")[1].split(" · ")[0];
  const retryAudio = page.getByRole("button", { name: "Retry audio" });
  await retryAudio.waitFor({ timeout: 18000 });
  assert.equal(await page.evaluate(() => window.sampleStarted), undefined);
  const retried = page.waitForResponse(sampleUrl);
  await retryAudio.click();
  assert.ok((await retried).ok());
  await page.waitForFunction(() => window.sampleStarted);
  await retryAudio.waitFor({ state: "hidden" });
  assert.ok((await state.textContent()).includes(` · Playing ${selection}`));
  await page.close();
  console.log("Passed stalled sample timeout and playback retry on the same track.");
}

if (!process.argv[2]) throw new Error("Pass the served demo URL or the standalone file URL.");
run(process.argv[2]).catch(error => { console.error(error); process.exitCode = 1; });
