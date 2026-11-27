// Exercise the packaged browser's actual Web Audio graph. No sound device is required.
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { chromium, firefox } = require("playwright");

function observeAudio() {
  const destinationTaps = new Map();
  const sourceIds = new WeakMap();
  const starts = [];
  let source;
  const connect = AudioNode.prototype.connect;
  const start = AudioBufferSourceNode.prototype.start;
  AudioNode.prototype.connect = function (destination, ...args) {
    const result = connect.call(this, destination, ...args);
    if (destination === this.context.destination && !destinationTaps.has(this)) {
      const analyser = this.context.createAnalyser();
      // Keep enough history to inspect rendered audio after a short main-thread stall.
      analyser.fftSize = 8192;
      connect.call(this, analyser);
      destinationTaps.set(this, { analyser, samples: new Float32Array(analyser.fftSize) });
    }
    return result;
  };
  AudioBufferSourceNode.prototype.start = function (...args) {
    const result = start.apply(this, args);
    if (this.buffer) {
      source = this;
      if (!sourceIds.has(this.buffer)) sourceIds.set(this.buffer, starts.length + 1);
      starts.push({
        clip: sourceIds.get(this.buffer), offset: args[1] ?? 0,
        duration: this.buffer.duration, time: this.context.currentTime,
      });
    }
    return result;
  };
  window.audioObservation = (history = false) => {
    let peak = 0;
    let gain;
    let clock;
    let tailLength;
    let stats;
    let historyPeaks;
    for (const [output, { analyser, samples }] of destinationTaps) {
      // Suspended contexts can retain samples from before suspension.
      if (output.context.state !== "running") continue;
      const context = output.context;
      clock = context.currentTime;
      tailLength = 2048 / context.sampleRate;
      if (history) stats = context.playbackStats?.toJSON?.();
      if (output instanceof GainNode) gain = output.gain.value;
      analyser.getFloatTimeDomainData(samples);
      for (let i = samples.length - 2048; i < samples.length; i++) {
        peak = Math.max(peak, Math.abs(samples[i]));
      }
      if (history) {
        historyPeaks = [];
        for (let i = 0; i < samples.length; i += 1024) {
          let binPeak = 0;
          for (let j = i; j < i + 1024; j++) binPeak = Math.max(binPeak, Math.abs(samples[j]));
          historyPeaks.push(binPeak);
        }
      }
    }
    return { peak, gain, clock, tailLength, stats, historyPeaks,
      rate: source?.playbackRate.value, loop: source?.loop, starts: starts.slice() };
  };
}

const audible = 0.001;
const firstAlbum = { key: "tidal", name: "Cylinders" };
const otherAlbum = { key: "northline", name: "Soft Rock Champion" };
const featuredStations = ["Radios", "Featured Stations"];

function playing(title) {
  const escaped = title.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(` · Playing ${escaped} by `);
}

const readCatalog = async page => JSON.parse(await page.locator("#media-catalog").textContent());

function phaseChange(before, after) {
  const expected = before.offset + after.time - before.time;
  const phase = (after.offset - expected) / before.duration;
  return Math.abs(phase - Math.round(phase)) * before.duration;
}

async function checkRadioContinuity(page) {
  // Compare the same source amplitude across output gain curves.
  const threshold = audible * (await page.evaluate(() => window.audioObservation().gain)) / 0.44;
  const result = await page.evaluate(async threshold => {
    const start = window.audioObservation(true);
    const started = performance.now();
    const durationMs = 11500;
    let sampleCount = 0;
    let lastAudible = started;
    let longestQuietMs = 0;
    let minRate = start.rate;
    let maxRate = start.rate;
    let stall;
    do {
      await new Promise(resolve => requestAnimationFrame(resolve));
      const now = performance.now();
      const sample = window.audioObservation();
      sampleCount++;
      if (sample.peak > threshold) {
        longestQuietMs = Math.max(longestQuietMs, now - lastAudible);
        lastAudible = now;
      }
      minRate = Math.min(minRate, sample.rate);
      maxRate = Math.max(maxRate, sample.rate);
      if (!stall && now - started > 2000) {
        const cpuStart = performance.now();
        const audioStart = sample.clock;
        while (performance.now() - cpuStart < 100) {
          // Hold the main thread while the audio rendering thread keeps running.
        }
        // Firefox exposes the new audio clock on the next main-thread task.
        await new Promise(resolve => setTimeout(resolve, 0));
        const after = window.audioObservation(true);
        stall = { wallMs: performance.now() - cpuStart,
          audioMs: (after.clock - audioStart) * 1000, peaks: after.historyPeaks };
      }
    } while (performance.now() - started < durationMs);
    const end = window.audioObservation(true);
    const totalMs = performance.now() - started;
    return { totalMs, audioMs: (end.clock - start.clock) * 1000, sampleCount,
      longestQuietMs: Math.max(longestQuietMs, performance.now() - lastAudible),
      minRate, maxRate, stall, starts: end.starts.length, loop: end.loop,
      clipMs: start.starts.at(-1).duration * 1000, initialStats: start.stats, finalStats: end.stats };
  }, threshold);
  console.log(`FIP continuity ${page.url()} ${JSON.stringify(result)}`);
  assert.equal(result.starts, 1, "radio must keep a single source through the loop boundary");
  assert.equal(result.loop, true, "radio must loop its local preview");
  assert.equal(result.minRate, 1, "radio must play at normal speed");
  assert.equal(result.maxRate, 1, "radio must play at normal speed");
  assert.ok(result.audioMs > result.clipMs, "radio continuity check must cover a full loop");
  assert.ok(Math.abs(result.audioMs / result.totalMs - 1) < 0.08,
    "radio audio clock must advance at approximately wall-clock rate");
  assert.ok(result.longestQuietMs < 180, "radio must keep producing audible PCM");
  assert.ok(result.stall.audioMs > 60, "the audio clock must continue during the main-thread stall");
  assert.ok(result.stall.peaks.filter(peak => peak > threshold).length >= 6,
    "PCM around the main-thread stall must remain audible");
}

function browserControls(page) {
  const state = page.locator("#current-state");
  const press = key => page.keyboard.press(key);
  const wait = async expression => {
    try {
      await page.waitForFunction(pattern =>
        new RegExp(pattern).test(document.getElementById("current-state").textContent),
      expression.source, { timeout: 5000 });
    } catch (error) {
      throw new Error(`Expected ${expression}, got ${await state.textContent()}`, { cause: error });
    }
  };
  const toggle = () => page.getByRole("button", { name: "Play or pause" }).click();

  const select = async (label) => {
    for (const direction of ["ArrowDown", "ArrowUp"]) {
      for (let step = 0; step < 24; step++) {
        const before = await state.textContent();
        const selection = before.slice(before.indexOf(" · ") + 3);
        if (selection === label || selection.startsWith(`${label} · `)) {
          await press("Enter");
          await page.waitForFunction(previous =>
            document.getElementById("current-state").textContent !== previous,
          before, { timeout: 5000 });
          return;
        }
        await press(direction);
        // A clamped menu at its endpoint has no update. Try the other direction.
        try {
          await page.waitForFunction(previous =>
            document.getElementById("current-state").textContent !== previous,
          before, { timeout: 250 });
        } catch (error) {
          if (error.name !== "TimeoutError") throw error;
          break;
        }
      }
    }
    assert.fail(`Cannot find ${label} from ${await state.textContent()}`);
  };

  return { state, press, wait, select, toggle };
}

async function exercisePreview(page, url, journey) {
  const errors = [];
  const external = [];
  const origin = new URL(url).origin;
  page.on("pageerror", error => errors.push(error.message));
  page.on("request", request => {
    const target = new URL(request.url());
    if (["http:", "https:"].includes(target.protocol) && target.origin !== origin) {
      external.push(target.href);
    }
  });
  await page.addInitScript(observeAudio);
  await page.goto(url);
  await page.locator("#media-mode").selectOption("preview");
  await page.locator(".player").focus();
  const catalog = await readCatalog(page);
  const firstTitle = catalog[`${firstAlbum.key}-1`].title;
  const nextTitle = catalog[`${firstAlbum.key}-2`].title;
  const otherTitle = catalog[`${otherAlbum.key}-1`].title;
  const path = journey.music ? ["Music", "Albums", firstAlbum.name, `1. ${firstTitle}`] : journey.path;
  const nowPlaying = playing(journey.music ? firstTitle : journey.title);
  const { state, press, wait, select, toggle } = browserControls(page);
  const read = () => page.evaluate(() => window.audioObservation());
  const observePlaying = async (afterStarts = 0) => {
    try {
      await page.waitForFunction(({ afterStarts, audible }) => {
        const { peak, starts, clock, tailLength } = window.audioObservation();
        return peak > audible && starts.length > afterStarts && clock - starts.at(-1).time > tailLength;
      }, { afterStarts, audible }, { timeout: 5000 });
    } catch (error) {
      throw new Error(`No audible PCM after start ${afterStarts}. State ${await state.textContent()}. Audio ${JSON.stringify(await read())}. Browser errors ${JSON.stringify(errors)}`, { cause: error });
    }
    return read();
  };
  const checkSilence = () => page.evaluate(async threshold => {
    const end = performance.now() + 250;
    let peak = 0;
    do {
      peak = Math.max(peak, window.audioObservation().peak);
      await new Promise(resolve => requestAnimationFrame(resolve));
    } while (performance.now() < end);
    return peak <= threshold;
  }, audible / 10);
  const checkGainChange = async (before, direction) => {
    try {
      await page.waitForFunction(({ before, direction }) =>
        direction * (window.audioObservation().gain / before - 1) > 0.06,
      { before, direction }, { timeout: 5000 });
    } catch (error) {
      const { gain } = await read();
      throw new Error(`Expected output gain to ${direction > 0 ? "increase" : "decrease"} from ${before}, got ${gain}`,
        { cause: error });
    }
  };

  await wait(/^Q2 · Music$/);
  assert.ok(await checkSilence(), "initial load must be silent");
  assert.equal((await read()).starts.length, 0, "initial load must not start a clip");
  for (const label of path) await select(label);
  if (journey.pressPlay) await press("Enter");
  await wait(nowPlaying);
  const initial = await observePlaying();
  assert.ok(initial.gain > 0, "playing should deliver PCM through an audible output gain");
  if (journey.radio) {
    // The PCM tap measures Web Audio output before the operating system mixer.
    // Sampled coverage tolerates scheduler jitter and cannot certify physical playback.
    await checkRadioContinuity(page);
    await wait(nowPlaying);
  }

  await toggle();
  await wait(/ · Paused /);
  await page.waitForFunction(threshold => window.audioObservation().peak <= threshold,
    audible / 10, { timeout: 5000 });
  assert.ok(await checkSilence(), "paused audio must stay silent");
  await press("Space");
  await wait(nowPlaying);
  const resumed = await observePlaying();
  assert.equal(resumed.starts.at(-1).clip, initial.starts.at(-1).clip,
    "resume should keep the selected clip");

  if (journey.music) {
    await press("ArrowUp");
    await wait(/ · Volume 46%/);
    await checkGainChange(resumed.gain, 1);
    const higher = await observePlaying();
    await press("ArrowDown");
    await wait(/ · Volume 44%/);
    await checkGainChange(higher.gain, -1);
    const lower = await observePlaying();
    assert.equal(lower.starts.length, resumed.starts.length, "volume should not restart the clip");

    await press("ArrowRight");
    await wait(playing(nextTitle));
    const next = await observePlaying(lower.starts.length);
    const previousTrack = lower.starts.at(-1);
    const nextTrack = next.starts.at(-1);
    assert.ok(nextTrack.clip !== previousTrack.clip || phaseChange(previousTrack, nextTrack) > 1,
      "next should switch to a different audio excerpt");

    await press("Enter");
    for (let step = 0; step < 5; step++) await press("ArrowDown");
    const seek = await observePlaying(next.starts.length);
    assert.equal(seek.starts.at(-1).clip, nextTrack.clip,
      "seeking should keep the same clip");
    assert.ok(phaseChange(nextTrack, seek.starts.at(-1)) > 1,
      "seek should jump away from the natural playback position");
    await wait(playing(nextTitle));

    await press("Escape"); // Exit the scrub control first.
    await press("Escape");
    await wait(/^Cylinders · /);
    await press("Escape");
    await wait(/^Albums · /);
    await select(otherAlbum.name);
    await select(`1. ${otherTitle}`);
    await wait(playing(otherTitle));
    const other = await observePlaying(seek.starts.length);
    assert.notEqual(other.starts.at(-1).clip, nextTrack.clip,
      "another album should deliver a different audio buffer");
  }

  assert.deepEqual(errors, []);
  assert.deepEqual(external, [], "demo playback should not request network media");
}

function observeNativeMedia() {
  const play = HTMLMediaElement.prototype.play;
  let current;
  HTMLMediaElement.prototype.play = function (...args) {
    current = this;
    return play.apply(this, args);
  };
  window.nativeMediaObservation = () => current && {
    source: current.currentSrc,
    time: current.currentTime,
    volume: current.volume,
    rate: current.playbackRate,
    paused: current.paused,
  };
}

function serveMediaFixture(route, bytes) {
  const headers = { "accept-ranges": "bytes", "access-control-allow-origin": "*" };
  const range = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || "");
  if (!range) return route.fulfill({ headers, contentType: "audio/mpeg", body: bytes });
  const first = Number(range[1]);
  const last = range[2] ? Math.min(Number(range[2]), bytes.length - 1) : bytes.length - 1;
  if (first > last) {
    headers["content-range"] = `bytes */${bytes.length}`;
    return route.fulfill({ status: 416, headers });
  }
  headers["content-range"] = `bytes ${first}-${last}/${bytes.length}`;
  return route.fulfill({ status: 206, contentType: "audio/mpeg", headers, body: bytes.subarray(first, last + 1) });
}

async function exerciseOnline(page, url) {
  const fixture = readFileSync(join(__dirname, "audio", "radio.mp3"));
  const origin = new URL(url).origin;
  const requests = [];
  const served = new Set();
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.addInitScript(observeNativeMedia);
  await page.addInitScript(observeAudio);
  await page.route(/^https?:\/\//, route => {
    const target = new URL(route.request().url());
    if (target.origin === origin && !target.pathname.startsWith("/music-")) return route.continue();
    requests.push(target.href);
    return served.has(target.href) ? serveMediaFixture(route, fixture) : route.abort("failed");
  });
  await page.goto(url);
  const mode = page.locator("#media-mode");
  assert.equal(await mode.inputValue(), origin === "null" ? "preview" : "online");
  if (origin === "null") await mode.selectOption("online");
  await page.locator(".player").focus();
  const { state, press, wait, select, toggle } = browserControls(page);
  const catalog = await readCatalog(page);
  const source = id => {
    assert.ok(catalog[id]?.url, `missing a real media URL for ${id}`);
    const entry = new URL(catalog[id].url);
    assert.equal(entry.protocol, "https:");
    assert.notEqual(entry.origin, origin);
    return new URL(catalog[id].localUrl || entry.href, url).href;
  };
  const expectNative = (expectedSource, after = 0.15, timeout = 6000) => page.waitForFunction(({ expectedSource, after }) => {
    const media = window.nativeMediaObservation();
    return media?.source === expectedSource && !media.paused && media.time > after;
  }, { expectedSource, after }, { timeout });
  const read = () => page.evaluate(() => window.nativeMediaObservation());
  const returnHome = async () => {
    for (let step = 0; step < 6; step++) {
      const before = await state.textContent();
      if (before.startsWith("Q2 · ")) return;
      await press("Escape");
      await page.waitForFunction(previous => document.getElementById("current-state").textContent !== previous,
        before, { timeout: 5000 });
    }
    assert.fail(`Cannot return home from ${await state.textContent()}`);
  };

  await wait(/^Q2 · Music$/);
  assert.deepEqual(requests, [], "load must not request public media");
  assert.equal(await read(), undefined, "load must not call native play");

  const firstId = `${firstAlbum.key}-1`;
  const secondId = `${firstAlbum.key}-2`;
  const first = source(firstId);
  const second = source(secondId);
  assert.notEqual(first, second);
  served.add(first);
  for (const label of ["Music", "Albums", firstAlbum.name]) await select(label);
  await select(`1. ${catalog[firstId].title}`);
  await expectNative(first);
  assert.ok(requests.includes(first));
  assert.equal(await page.locator("#media-publisher").getAttribute("href"), catalog[firstId].source);
  assert.equal(await page.locator("#media-license").getAttribute("href"), catalog[firstId].licenseUrl);
  const baseline = await read();
  await press("ArrowUp");
  await wait(/ · Volume 46%/);
  const routed = Boolean(catalog[firstId].localUrl);
  const expectVolume = percent => page.waitForFunction(({ percent, routed }) => {
    const value = routed ? window.audioObservation().gain : window.nativeMediaObservation().volume;
    return Math.abs(value - 10 ** ((percent - 100) / 40)) < 0.0005;
  }, { percent, routed }, { timeout: 3000 });
  if (routed) {
    assert.equal(baseline.volume, 1, "software gain must be applied only once");
    await page.waitForFunction(() => window.audioObservation().peak > 0.0001);
  }
  await expectVolume(46);
  await press("ArrowDown");
  await wait(/ · Volume 44%/);
  await expectVolume(44);
  await toggle();
  await wait(/ · Paused /);
  const paused = await read();
  assert.ok(paused.paused);
  if (routed) {
    await page.waitForFunction(() => {
      const output = window.audioObservation();
      return output.gain === 0 && output.peak === 0;
    }, undefined, { timeout: 2000 });
  }
  await page.waitForTimeout(200);
  assert.equal((await read()).time, paused.time, "the native clock must stop while paused");
  await press("Space");
  await expectNative(first, paused.time + 0.10);
  const beforeSeek = await read();
  await press("Enter");
  for (let step = 0; step < 4; step++) await press("ArrowDown");
  // Allow the wheel to settle, but fail before the clock could advance this far naturally.
  await expectNative(first, beforeSeek.time + 3, 2200);
  await press("Escape");

  await press("ArrowRight");
  await page.getByText("Stream unavailable.", { exact: true }).waitFor({ timeout: 6000 });
  assert.ok(requests.includes(second), "next must request the selected track's public source");
  assert.equal(await mode.inputValue(), "online", "an unavailable stream requires explicit fallback");
  assert.equal(await page.evaluate(() => window.audioObservation().starts.length), 0);
  served.add(second);
  await page.locator("#retry-stream").click();
  await expectNative(second);
  await page.getByText("Stream unavailable.", { exact: true }).waitFor({ state: "hidden" });
  assert.ok(requests.filter(request => request === second).length >= 2, "retry must request the failed track again");
  await page.locator(".player").focus();

  await returnHome();
  for (const label of featuredStations) await select(label);
  const stations = Object.entries(catalog).filter(([id]) => /^demo-radio-\d+$/.test(id));
  assert.ok(stations.some(([, entry]) => entry.title === "FIP" && !entry.external));
  assert.ok(stations.some(([, entry]) => entry.external));
  for (const [id, entry] of stations) {
    const station = source(id);
    const failedFirst = entry.title === "KEXP";
    if (!entry.external && !failedFirst) served.add(station);
    const requestCount = requests.length;
    await select(entry.title);
    if (entry.external) {
      const link = page.locator("#station-page");
      await link.waitFor({ timeout: 6000 });
      assert.equal(await link.getAttribute("href"), station);
      assert.equal(requests.length, requestCount, `${entry.title} requires opening the official station page`);
      assert.ok((await read()).paused, "an external station must stop the previous stream");
    } else {
      if (failedFirst) {
        await page.getByText("Stream unavailable.", { exact: true }).waitFor({ timeout: 6000 });
        await page.locator("#use-preview").click();
        assert.equal(await mode.inputValue(), "preview");
        await page.waitForFunction(() => window.audioObservation().peak > 0.001, undefined, { timeout: 5000 });
        served.add(station);
        await mode.selectOption("online");
      }
      await expectNative(station);
      assert.equal((await read()).rate, 1, `${entry.title} must play at normal speed`);
      assert.ok(requests.includes(station), `${entry.title} must use its advertised public stream`);
    }
    await page.locator(".player").focus();
    await press("Escape");
    await wait(/^Featured Stations · /);
  }
  await returnHome();
  for (const label of ["Settings", "Playback", "Default Spoken Speed"]) await select(label);
  await press("ArrowDown");
  for (const item of [
    { id: "demo-spoken-1", path: ["Books", "Books"], pressPlay: true },
    { id: "demo-spoken-4", path: ["Podcasts", "Library", "Houston We Have a Podcast"] },
  ]) {
    await returnHome();
    for (const label of item.path) await select(label);
    const url = source(item.id);
    served.add(url);
    await select(catalog[item.id].title);
    if (item.pressPlay) await press("Enter");
    await expectNative(url);
    assert.ok(requests.includes(url));
    assert.equal((await read()).rate, 1.05, "fresh spoken media inherits the five-point default");
    await press("Enter");
    await press("Enter");
    await press("ArrowDown");
    await page.waitForFunction(() => window.nativeMediaObservation()?.rate === 1.10);
    assert.equal((await read()).source, url, "speed changes keep the current stream");
    await press("Enter");
  }
  assert.deepEqual(errors, []);
}

async function run(urls) {
  const journeys = [
    { name: "Music", music: true },
    { name: "Books", path: ["Books", "Continue Listening", "The Wonderful Wizard of Oz"], title: "The Wonderful Wizard of Oz", pressPlay: true },
    { name: "Podcasts", path: ["Podcasts", "Library", "NASA's Curious Universe", "Encore: A Year In Mars Dune Alpha"], title: "Encore: A Year In Mars Dune Alpha" },
    { name: "Radios", path: [...featuredStations, "FIP"], title: "FIP", radio: true },
  ];
  for (const engine of [chromium, firefox]) {
    const browser = await engine.launch();
    try {
      for (const url of urls) {
        for (const journey of journeys) {
          const page = await browser.newPage();
          try {
            await exercisePreview(page, url, journey);
          } catch (error) {
            throw new Error(`${engine.name()} ${url} ${journey.name}: ${error.message}`, { cause: error });
          } finally {
            await page.close();
          }
        }
        const page = await browser.newPage();
        try {
          await exerciseOnline(page, url);
        } catch (error) {
          throw new Error(`${engine.name()} ${url} online media: ${error.message}`, { cause: error });
        } finally {
          await page.close();
        }
        console.log(`Passed ${engine.name()} preview output and online media controls at ${url}`);
      }
    } finally {
      await browser.close();
    }
  }
}

const urls = process.argv.slice(2);
if (!urls.length) throw new Error("Pass the served demo URL and/or the standalone file URL.");
run(urls).catch(error => { console.error(error); process.exitCode = 1; });
