import fs from 'node:fs';
import assert from 'node:assert/strict';
const path = process.argv[2];
if (!path) throw new Error('Pass the built player.wasm path.');
const {instance:{exports:demo}} = await WebAssembly.instantiate(fs.readFileSync(path));
const read = () => new TextDecoder().decode(new Uint8Array(demo.memory.buffer, demo.demo_label(), demo.demo_label_len()));
let ms;
function step(code) { demo.demo_input(code); demo.demo_input(8); demo.demo_frame(ms += 150); }
function init() { demo.demo_init(); ms=0; demo.demo_frame(ms); }
function check(fragment) { assert.ok(read().includes(fragment), JSON.stringify({expected:fragment, actual:read()})); }
function route(label) {
  pick(label);
  step(2);
}
function pick(label) {
  for (const direction of [1,0]) {
    for (let i=0;i<64;i++) {
      if (read().split(' · ')[1] === label) return;
      const old = read();
      step(direction);
      if (read() === old) break;
    }
  }
  assert.fail(`Cannot find ${label} from ${read()}`);
}

init();
route('Settings'); route('Connections'); check('Connections · Wi-Fi');
route('Wi-Fi'); route('Scan'); check('Wi-Fi · Scan');
route('Station Café'); check('Connected to Station Café');
route('Forget This Network…'); route('Cancel'); check('Wi-Fi');
step(3); route('Offline Mode');
route('Audiobookshelf'); route('Test Connection'); check("Can't reach Audiobookshelf");
step(3); route('Offline Mode');
route('Navidrome Server'); route('Test Connection'); check('Connection successful');
route('Address'); check('Server Address');
step(3); step(3); step(3); route('Display'); route('Theme');
route('Selection Color'); route('Orange'); check('Display · Selection Color');
route('Brightness'); step(1); step(2); check('Display · Brightness');
step(3); route('Sound'); route('Equalizer'); route('Presets'); route('Jazz'); check('Equalizer · Presets');
route('Adjust Bands'); step(2); step(1); step(3); check('Graphic EQ'); step(3); check('Equalizer');
console.log('PASS settings, synthetic Wi-Fi, server checks, editor Back, theme/accent, brightness, EQ editing.');

init();
route('Settings'); route('System'); route('About');
const nativeManifest = fs.readFileSync(new URL('../crates/q2nd/Cargo.toml', import.meta.url), 'utf8');
const appVersion = nativeManifest.match(/^version\s*=\s*"([^"]+)"/m)[1];
const about = `About · Q2 Navidrome ${appVersion} · OS Version Web Demo · System Information unavailable · Report a Problem unavailable`;
assert.equal(read(), about);
for (const button of [0, 1, 2, 7]) {
  step(button);
  assert.equal(read(), about);
}
for (let y = 40; y < 300; y += 5) {
  assert.equal(demo.demo_clickable(180, y), 0, `About hit at y=${y}`);
  assert.equal(demo.demo_click(180, y), 0, `About action at y=${y}`);
}
assert.equal(read(), about);
step(3); check('System · About');
console.log('PASS About uses native build version and unavailable rows ignore wheel, click, Center and long Center.');

init();
route('Books'); route('Continue Listening'); check('Continue Listening · The Wonderful Wizard of Oz');
step(2); check('Book Info');
step(2); check('Playing The Wonderful Wizard of Oz');
step(3); check('Continue Listening · The Wonderful Wizard of Oz');
step(7); check('Book Info'); route('Download for Offline'); check('Audiobook queued');
step(3); step(3); route('Downloads'); check('Audiobooks · 1 to download');
route('Pause Downloads'); check('Resume Downloads');
route('Resume Downloads');
demo.demo_frame(ms += 9000);
assert.ok(!read().startsWith('Downloads · Audiobooks · 1'), read());
step(3); route('Books'); route('Downloads'); pick('The Wonderful Wizard of Oz');
step(7); route('Remove Download'); check('Download removed');
console.log('PASS book preview/play/Back, staged offline download, pause/resume/progress, and local removal.');

init();
route('Podcasts'); route('Library'); route('NASA\'s Curious Universe'); check('NASA\'s Curious Universe · Encore: A Year In Mars Dune Alpha');
step(7); route('Episode Info'); check('Episode Info'); step(3); check('NASA\'s Curious Universe · Encore: A Year In Mars Dune Alpha');
step(2); check('Playing Encore: A Year In Mars Dune Alpha');
console.log('PASS podcast show, episode information and direct playback.');

init();
route('Radios'); route('Featured Stations'); check('Featured Stations · FIP');
step(7); route('Add to Navidrome'); check('Station saved'); step(3); route('FIP'); check('Playing FIP');
console.log('PASS featured radio add and playback from saved station.');

init();
route('Settings'); route('Sound'); route('Equalizer'); route('Presets'); route('Jazz');
route('Reset Bands…'); check('Reset EQ Bands? · Cancel');
pick('Reset Bands'); step(3); route('Presets'); check('EQ Presets · Jazz');
step(3); route('Reset Bands…'); route('Reset Bands'); route('Presets'); check('EQ Presets · Flat');
console.log('PASS reset confirmation preserves the selected preset on Back and resets it on acceptance.');

init();
route('Books'); route('Continue Listening'); step(2); step(2);
step(7); route('Sleep Timer'); check('Sleep Timer · Off');
route('5 Minutes'); check('Pausing in 5 minutes');
step(7); route('Sleep Timer'); check('Sleep Timer · 5 Minutes');
pick('Off'); step(3);
demo.demo_frame(ms += 301000);
assert.equal(demo.demo_playing(), 0, 'Expired sleep timer pauses the book after backing out of the picker');
step(7); route('Sleep Timer'); check('Sleep Timer · Off');
console.log('PASS manual sleep timer reopens on the running period and Back preserves its deadline.');
