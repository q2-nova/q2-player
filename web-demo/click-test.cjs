// Exercise physical screen input against the packaged canvas and audio.
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { chromium, firefox } = require("playwright");

const fip = "https://icecast.radiofrance.fr/fip-midfi.mp3?id=openapi";
const timeout = 4000;

function observeMedia() {
  const play = HTMLMediaElement.prototype.play;
  let media;
  HTMLMediaElement.prototype.play = function (...args) {
    media = this;
    return play.apply(this, args);
  };
  window.fipPlayback = () => media && {
    url: media.currentSrc,
    position: media.currentTime,
    paused: media.paused,
  };
}

function observePreview() {
  const connect = AudioNode.prototype.connect;
  let analyser;
  let samples;
  AudioNode.prototype.connect = function (destination, ...args) {
    const result = connect.call(this, destination, ...args);
    if (destination === this.context.destination) {
      analyser = this.context.createAnalyser();
      samples = new Float32Array(analyser.fftSize);
      connect.call(this, analyser);
    }
    return result;
  };
  window.previewAudible = () => {
    if (analyser?.context.state !== "running") return false;
    analyser.getFloatTimeDomainData(samples);
    return samples.some(sample => Math.abs(sample) > 0.001);
  };
}

async function controls(page, url, tap = false) {
  page.setDefaultTimeout(timeout);
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(url, { timeout });
  const canvas = page.getByRole("img", { name: /^Q2 player screen/ });
  const state = page.locator("#current-state");
  const screenCoordinates = async () => {
    await canvas.scrollIntoViewIfNeeded();
    const box = await canvas.boundingBox();
    assert.ok(box && Math.abs(box.width - 375) > 1, "exercise a CSS-scaled canvas");
    return (x, y) => ({
      x: Math.round(box.x + x * box.width / 375),
      y: Math.round(box.y + y * box.height / 320),
    });
  };
  const press = async (x, y) => {
    const point = await screenCoordinates();
    const p = point(x, y);
    await (tap ? page.touchscreen.tap(p.x, p.y) : page.mouse.click(p.x, p.y));
  };
  const wait = async expected => {
    try {
      await page.waitForFunction(
        ({ source, flags }) => new RegExp(source, flags).test(
          document.getElementById("current-state").textContent),
        { source: expected.source, flags: expected.flags }, { timeout },
      );
    } catch (cause) {
      throw new Error(`Expected ${expected}, saw "${await state.textContent()}"`, { cause });
    }
  };
  const unchanged = async (action) => {
    const before = await state.textContent();
    const onChange = await state.evaluateHandle(node => {
      const labels = [];
      const observer = new MutationObserver(records => {
        for (const record of records) {
          for (const child of record.addedNodes) labels.push(child.textContent);
        }
      });
      observer.observe(node, { childList: true });
      return { labels, observer };
    });
    try {
      await action();
      // Observe enough canvas frames for delayed or duplicate input to appear.
      await page.evaluate(() => new Promise(resolve => {
        let frames = 8;
        const next = () => --frames ? requestAnimationFrame(next) : resolve();
        requestAnimationFrame(next);
      }));
      assert.equal(await state.textContent(), before);
      assert.deepEqual(await onChange.evaluate(({ labels }) => labels), []);
    } finally {
      await onChange.evaluate(({ observer }) => observer.disconnect());
      await onChange.dispose();
    }
  };
  const back = async expected => {
    await page.keyboard.press("Escape");
    await wait(expected);
  };
  await wait(/^Q2 · Music$/);
  return { errors, canvas, screenCoordinates, press, wait, unchanged, back, state };
}

async function desktop(page, url) {
  await page.addInitScript(observeMedia);
  await page.addInitScript(observePreview);
  const requests = [];
  const fixture = readFileSync(join(__dirname, "audio", "radio.mp3"));
  await page.route(/^https?:\/\//, route => {
    const request = route.request();
    if (new URL(request.url()).origin === new URL(url).origin) return route.continue();
    requests.push(request.url());
    if (request.url() !== fip) return route.abort();
    const contentType = "audio/mpeg";
    const range = /^bytes=(\d+)-(\d*)$/.exec(request.headers().range || "");
    if (!range) return route.fulfill({ contentType, body: fixture, headers: { "access-control-allow-origin": "*" } });
    const first = Number(range[1]);
    const last = range[2] ? Math.min(Number(range[2]), fixture.length - 1) : fixture.length - 1;
    if (first > last) {
      return route.fulfill({ status: 416, headers: { "access-control-allow-origin": "*", "content-range": `bytes */${fixture.length}` } });
    }
    return route.fulfill({
      status: 206, contentType, body: fixture.subarray(first, last + 1),
      headers: { "access-control-allow-origin": "*", "content-range": `bytes ${first}-${last}/${fixture.length}` },
    });
  });

  const { errors, canvas, screenCoordinates, press, wait, unchanged, back, state } = await controls(page, url);
  const mediaMode = page.getByRole("combobox", { name: "Audio source" });
  if (new URL(url).protocol === "file:") await mediaMode.selectOption("online");
  await unchanged(() => press(300, 156)); // Home art opposite the Radios row.
  await unchanged(() => press(95, 298)); // Empty area below Shuffle Songs.
  await unchanged(() => press(150, 16)); // Header above the list.
  await press(95, 156);
  await wait(/^Radios · Featured Stations$/);
  await press(150, 48);
  await wait(/^Featured Stations · FIP$/);
  await press(150, 48);
  await wait(/^Now Playing · Playing FIP by Radio France$/);
  await page.waitForFunction(url => {
    const playback = window.fipPlayback();
    return playback?.url === url && !playback.paused && playback.position > 0.15;
  }, fip, { timeout });
  assert.ok(requests.length > 0 && requests.every(url => url === fip));
  await press(75, 127);
  await wait(/^Now Playing · Paused FIP by Radio France$/);
  await page.waitForFunction(() => window.fipPlayback()?.paused, undefined, { timeout });
  await press(75, 127);
  await wait(/^Now Playing · Playing FIP by Radio France$/);
  await page.waitForFunction(() => window.fipPlayback()?.paused === false, undefined, { timeout });
  await back(/^Featured Stations · FIP · Playing FIP/);
  await back(/^Radios · Featured Stations · Playing FIP/);
  await back(/^Q2 · Radios · Playing FIP/);

  await press(95, 228);
  await wait(/^Settings · Sound/);
  await press(150, 48);
  await wait(/^Sound · Equalizer/);
  await press(150, 102); // Second row is 48 pixels tall here.
  await wait(/^Volume Normalization · Off/);
  await back(/^Sound · Volume Normalization/);
  await back(/^Settings · Sound/);

  await press(150, 84);
  await wait(/^Playback · Auto Sleep Timer/);
  const mousePoint = (await screenCoordinates())(150, 194);
  await page.mouse.move(mousePoint.x, mousePoint.y);
  await page.mouse.wheel(0, 440);
  await wait(/^Playback · Last.fm/);
  await page.mouse.wheel(0, 70);
  await wait(/^Playback · Sync Queue/);
  // The last row is selected and the viewport is at the bottom. Click
  // Streaming Quality five rows above it, where Pause on Unplug
  // would be without the vertical offset.
  await press(150, 122);
  await wait(/^Streaming Quality · /);
  await back(/^Playback · Streaming Quality/);
  await back(/^Settings · Playback/);
  await back(/^Q2 · Settings/);

  await press(95, 48);
  await wait(/^Music · Cover Flow/);
  await press(150, 48);
  await wait(/^Cover Flow · /);
  const currentCover = (await state.textContent()).split(" · ")[1];
  await unchanged(() => press(187, 278)); // Caption beneath the artwork.
  await press(280, 127);
  await page.waitForFunction(previous => {
    const [title, selected] = document.getElementById("current-state").textContent.split(" · ");
    return title === "Cover Flow" && selected && selected !== previous;
  }, currentCover, { timeout });
  const selected = (await state.textContent()).split(" · ")[1];
  assert.notEqual(selected, currentCover, "side artwork must select another album");
  // The selection label updates before the selected artwork slides into the center.
  await canvas.evaluate(async canvas => {
    const ctx = canvas.getContext("2d");
    let previous, stillSince = performance.now();
    for (const deadline = stillSince + 4000; performance.now() < deadline;) {
      await new Promise(requestAnimationFrame);
      const strip = ctx.getImageData(90, 127, 195, 1).data;
      if (!previous || strip.some((value, index) => value !== previous[index])) {
        stillSince = performance.now();
      } else if (performance.now() - stillSince > 150) {
        return;
      }
      previous = strip;
    }
    throw new Error("Cover Flow artwork did not settle");
  });
  await press(187, 127);
  await wait(/^.+ · Shuffle · /);
  assert.ok((await state.textContent()).startsWith(`${selected} · Shuffle · `));
  await back(/^Cover Flow · /);
  assert.equal((await state.textContent()).split(" · ")[1], selected);
  await back(/^Music · Cover Flow/);
  await press(150, 156);
  await wait(/^Albums · Shuffle Albums/);
  await press(150, 120); // First album beneath Shuffle Albums.
  await wait(/^Cylinders · Shuffle/);
  await page.getByRole("button", { name: "Play or pause" }).click();
  await wait(/^Cylinders · Shuffle · Paused FIP/);
  const publicRequests = requests.length;
  await mediaMode.selectOption("preview");
  await press(150, 120); // Cylinder Two, while Shuffle is highlighted.
  await wait(/^Now Playing · Playing Cylinder Two by Chris Zabriskie$/);
  await page.waitForFunction(() => window.previewAudible(), undefined, { timeout });
  assert.equal(requests.length, publicRequests, "preview must use bundled audio");
  assert.deepEqual(errors, []);
}

async function touchscreen(browser, url) {
  const page = await browser.newPage({
    viewport: { width: 360, height: 740 }, hasTouch: true, isMobile: true,
  });
  try {
    const { errors, screenCoordinates, press, wait, unchanged } = await controls(page, url, true);
    const cdp = await page.context().newCDPSession(page);
    const touch = (type, position) => cdp.send("Input.dispatchTouchEvent", {
      type, touchPoints: position ? [position] : [],
    });
    await unchanged(async () => {
      const point = await screenCoordinates();
      await touch("touchStart", point(95, 156));
      await touch("touchCancel");
    });
    await unchanged(async () => {
      const point = await screenCoordinates();
      await touch("touchStart", point(95, 156));
      for (const y of [144, 132, 108, 85]) await touch("touchMove", point(95, y));
      await touch("touchEnd");
    });
    await press(95, 156);
    await wait(/^Radios · Featured Stations$/);
    await press(150, 48);
    await wait(/^Featured Stations · FIP$/);
    assert.deepEqual(errors, []);
  } finally {
    await page.close();
  }
}

async function run(urls) {
  for (const engine of [chromium, firefox]) {
    const browser = await engine.launch();
    try {
      for (const url of urls) {
        const page = await browser.newPage({ viewport: { width: 1180, height: 900 } });
        try {
          await desktop(page, url);
          if (engine === chromium) await touchscreen(browser, url);
          console.log(`Passed ${engine.name()} direct canvas input at ${url}`);
        } catch (error) {
          throw new Error(`${engine.name()} direct canvas input at ${url}: ${error.message}`, { cause: error });
        } finally {
          await page.close();
        }
      }
    } finally {
      await browser.close();
    }
  }
}

const urls = process.argv.slice(2);
if (!urls.length) throw new Error("Pass the served demo URL and/or the standalone file URL.");
run(urls).catch(error => { console.error(error); process.exitCode = 1; });
