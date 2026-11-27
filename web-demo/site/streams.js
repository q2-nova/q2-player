"use strict";

// Rust owns the queue. Two reusable elements keep opaque publisher audio
// separate from media that can feed the Web Audio gain node.
class StreamPlayer {
  constructor(catalog, preview) {
    this.catalog = catalog;
    this.preview = preview;
    this.direct = this.makeMedia();
    this.media = this.direct;
    this.attempt = 0;
    this.mode = document.querySelector("#media-mode");
    this.mode.value = location.protocol === "file:" ? "preview" : "online";
    this.status = document.querySelector(".stream-status");
    this.message = document.querySelector("#stream-message");
    this.retry = document.querySelector("#retry-stream");
    this.usePreview = document.querySelector("#use-preview");
    this.stationPage = document.querySelector("#station-page");
    this.credit = document.querySelector("#media-credit");
    this.publisher = document.querySelector("#media-publisher");
    this.license = document.querySelector("#media-license");
  }

  makeMedia() {
    const media = new Audio();
    media.preload = "none";
    const on = (type, callback) => media.addEventListener(type, () => {
      if (this.media === media) callback();
    });
    on("playing", () => {
      if (this.requested && !this.media.paused) this.notice("");
    });
    on("waiting", () => {
      if (this.requested && !this.media.paused) this.buffering();
    });
    on("loadedmetadata", () => {
      if (this.requested) this.sync(this.demo);
    });
    on("error", () => {
      if (this.requested && this.media.error) this.fail();
    });
    on("ended", () => {
      if (!this.requested || !this.media.ended) return;
      this.requested = false;
      if (this.live) this.fail();
      else this.demo.demo_media_ended(this.generation);
    });
    return media;
  }

  notice(message, action = "") {
    clearTimeout(this.deadline);
    this.deadline = null;
    this.message.textContent = message;
    this.status.hidden = !message;
    this.retry.hidden = action !== "retry";
    this.stationPage.hidden = action !== "station";
    this.usePreview.hidden = !action;
  }

  buffering() {
    if (this.deadline != null) return;
    this.notice(this.live ? "Connecting to the live station…" : "Buffering audio…");
    this.deadline = setTimeout(() => this.fail(), 20000);
  }

  release() {
    this.requested = false;
    this.attempt++;
    this.media.pause();
    if (this.media === this.routed) this.preview.setVolume(0);
    if (this.media.hasAttribute("src")) {
      this.media.removeAttribute("src");
      this.media.load();
    }
  }

  reset() {
    this.release();
    this.generation = null;
    this.entry = null;
    this.credit.hidden = true;
    this.preview.reset();
    this.notice("");
  }

  unlock() {
    this.preview.unlock();
  }

  setMode(demo) {
    this.reset();
    demo.demo_external_clock(Number(this.mode.value === "online"));
    this.unlock();
    this.sync(demo);
  }

  fail(message = "Stream unavailable.") {
    this.release();
    if (this.demo?.demo_playing()) this.demo.demo_input(Input.Play);
    this.notice(message, "retry");
  }

  showCredit(entry) {
    this.credit.hidden = !entry;
    if (!entry) return;
    this.publisher.textContent = entry.credit || entry.artist || entry.title;
    this.publisher.href = entry.source;
    this.license.textContent = entry.license || "";
    this.license.hidden = !entry.licenseUrl;
    if (entry.licenseUrl) this.license.href = entry.licenseUrl;
  }

  start() {
    const media = this.media;
    this.requested = true;
    const attempt = ++this.attempt;
    if (!media.hasAttribute("src")) {
      media.src = this.entry.localUrl || this.entry.url;
      this.seek = null;
    }
    this.buffering();
    media.play().catch(error => {
      if (attempt !== this.attempt || !this.requested) return;
      this.fail(error.name === "NotAllowedError" ? "Press Retry to start audio." : "Stream unavailable.");
    });
  }

  sync(demo) {
    this.demo = demo;
    if (this.mode.value === "preview") {
      this.preview.sync(demo);
      return;
    }
    const generation = demo.demo_playback_generation();
    if (this.generation !== generation) {
      this.release();
      this.notice("");
      this.generation = generation;
      const id = decoder.decode(new Uint8Array(demo.memory.buffer, demo.demo_media_id(), demo.demo_media_id_len()));
      this.entry = this.catalog[id];
      this.live = demo.demo_media_kind() === 2;
      this.seek = null;
      this.showCredit(this.entry);
    }
    if (!demo.demo_playing()) {
      if (this.requested) {
        if (this.live) this.release(); // Resume live at the station's current broadcast.
        else {
          this.requested = false;
          this.attempt++;
          this.media.pause();
          if (this.media === this.routed) this.preview.setVolume(0);
        }
        this.notice("");
      }
      return;
    }
    if (!demo.demo_online()) {
      this.fail("The player is in offline mode.");
      return;
    }
    if (!this.entry) {
      this.fail("There is no public stream for this item.");
      return;
    }
    if (this.entry.external) {
      demo.demo_input(Input.Play);
      this.stationPage.href = this.entry.url;
      this.stationPage.textContent = "Open " + this.entry.title.replace(/^SomaFM · /, "") + " on SomaFM";
      this.notice("This station plays on SomaFM’s website.", "station");
      return;
    }
    const volume = demo.demo_volume();
    if (this.entry.localUrl || this.entry.webAudio) {
      if (!this.preview.context) {
        this.fail("Press Retry to enable audio.");
        return;
      }
      if (!this.routed) {
        this.routed = this.makeMedia();
        this.routed.crossOrigin = "anonymous";
        this.source = this.preview.context.createMediaElementSource(this.routed);
        this.source.connect(this.preview.master);
      }
      this.media = this.routed;
      this.preview.setVolume(volume);
    } else {
      // Some publishers allow playback but do not grant access to PCM.
      this.media = this.direct;
      const gain = outputGain(volume);
      if (this.direct.volume !== gain) this.direct.volume = gain;
      this.direct.muted = volume === 0;
    }
    const media = this.media;
    const speed = this.live ? 1 : demo.demo_speed_percent() / 100;
    if (media.playbackRate !== speed) media.playbackRate = speed;
    if (!this.requested) this.start();
    if (media.readyState < HTMLMediaElement.HAVE_METADATA) return;
    const seek = demo.demo_seek_revision();
    if (this.seek !== seek) {
      this.seek = seek;
      const position = demo.demo_position_ms() / 1000;
      if (!this.live && Math.abs(media.currentTime - position) > 0.05) {
        media.currentTime = Number.isFinite(media.duration) ? Math.min(position, media.duration) : position;
      }
    }
    if (!media.seeking && !media.ended && Number.isFinite(media.currentTime)) {
      const duration = !this.live && Number.isFinite(media.duration) ? media.duration : 0;
      demo.demo_media_progress(generation, seek, Math.round(media.currentTime * 1000), Math.round(duration * 1000));
    }
  }
}
