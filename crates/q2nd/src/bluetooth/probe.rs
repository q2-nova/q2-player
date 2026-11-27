#![forbid(unsafe_code)]
//! `q2nd --bt-probe`: bring-up and listening checks from a shell, with the
//! player's Bluetooth off. Output is numbers, classes and states only: never
//! a device address or name.
use super::{Bluetooth, Link, Options, Phase, Port, State, pcm};
use crate::rt::Blocking;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: q2nd --bt-probe bench [--seconds N] | version|scan|diag [--inquiry-length N] [--inquiry-mode M]|pair [--nearest] [--any]|play SECONDS|wait SECONDS|bench-sink SECONDS --bench-dir DIR [--silence]|forget-all|lines \
[--codec auto|ldac|aptx-hd|aptx|aac|sbc] [--ldac-quality high|standard|mobile] [--ldac-fixed] [--port /dev/ttyS0|fake] [--firmware DIR] [--store FILE] [--baud N] [--flow] [--power-hold FILE]";

fn wait(_blocking: &Blocking, bluetooth: &Bluetooth, limit: Duration, done: impl Fn(&State) -> bool) -> (State, bool, Duration) {
    let start = Instant::now();
    loop {
        let state = bluetooth.state();
        let finished = done(&state);
        if finished || matches!(state.phase, Phase::Failed(_)) || start.elapsed() >= limit { return (state, finished, start.elapsed()) }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn report(state: &State) {
    match &state.phase { Phase::Failed(message) => println!("bt-probe: phase=failed message={message:?}"), phase => println!("bt-probe: phase={phase:?}") }
    if let Some(v) = state.controller {
        println!("bt-probe: at_start hci_version={:#04x} hci_revision={:#06x} lmp_version={:#04x} lmp_subversion={:#06x} manufacturer={:#06x} rom_version={} patch_kept={}",
            v.hci_version, v.hci_revision, v.lmp_version, v.lmp_subversion, v.manufacturer, v.rom_version.map_or("unread".into(), |r| r.to_string()), v.already_patched);
    }
    let link = match state.link { Link::Idle => "idle", Link::Connecting(_) => "connecting", Link::Connected(_) => "connected", Link::Playing(_) => "playing" };
    println!("bt-probe: link={link} paired={} dropped_packets={} retransmissions={} underruns={}", state.paired.len(), state.counters.0, state.counters.1, state.counters.2);
    let d = &state.diagnostics;
    println!("bt-probe: hci events={} inquiry_results={} inquiry_completes={} name_replies={} command_timeouts={} scan_timeouts={} unparsed={}",
        d.events, d.inquiry_results, d.inquiry_completes, d.name_replies, d.command_timeouts, d.scan_timeouts, d.unparsed);
    if d.sink_checks.0 > 0 { println!("bt-probe: sink_checks asked={} sink={} no_sink={}", d.sink_checks.0, d.sink_checks.1, d.sink_checks.2); }
    let errors: Vec<String> = d.command_errors.iter().map(|(opcode, status)| format!("{opcode:#06x}:{status:#04x}")).collect();
    println!("bt-probe: command_errors=[{}]", errors.join(","));
    println!("bt-probe: h5 sleep={} wakeup={} woken={} retransmissions={} rejected={}", d.h5_sleep[0], d.h5_sleep[1], d.h5_sleep[2], d.h5_retransmissions, d.h5_rejected);
    if let Some(stream) = state.stream {
        println!("bt-probe: stream codec={:?} mtu={}", stream.codec, stream.mtu);
    }
    let l = &state.link_stats;
    println!("bt-probe: link acl_buffers={} acl_in_use={} credit_stalls={} credit_stall_ms={} credit_stall_max_ms={} rssi={} link_quality={}",
        l.acl_buffers, l.acl_in_use, l.credit_stalls, l.credit_stall_ms, l.credit_stall_max_ms, l.rssi.map_or("unread".into(), |r| r.to_string()),
        l.link_quality.map_or("unread".into(), |q| q.to_string()));
    println!("bt-probe: h5 resends={} bad_frames={} out_of_sequence={}", l.h5_resends, l.h5_bad_frames, l.h5_out_of_sequence);
    if let Some(m) = state.media {
        println!("bt-probe: media kbps={} ldac_level={} ldac_changes={} late={} restarts={} behind_max_ms={} queue_max={} encode_errors={}",
            m.kbps, m.ldac_level.map_or("-".into(), |l| l.to_string()), m.ldac_changes, m.late, m.restarts, m.behind_max_ms, m.queue_max, m.encode_errors);
    }
    println!("bt-probe: uart baud={} flow={}", state.uart.0, state.uart.1);
    match state.uart_errors {
        Some(u) => println!("bt-probe: uart frame={} overrun={} parity={} buffer={}", u.frame, u.overrun, u.parity, u.buffer),
        None => println!("bt-probe: uart errors unread"),
    }
    if !state.notice.is_empty() { println!("bt-probe: notice={:?}", state.notice); }
}

fn scan(blocking: &Blocking, bluetooth: &Bluetooth) -> State {
    bluetooth.scan();
    std::thread::sleep(Duration::from_millis(200));
    let (state, _, took) = wait(blocking, bluetooth, Duration::from_secs(40), |s| !s.scanning);
    let mut classes: Vec<u32> = state.found.iter().map(|d| d.class).collect();
    classes.sort_unstable();
    println!("bt-probe: scan_ms={} found={} audio={} named={} eir_sink={}", took.as_millis(), state.found.len(), state.found.iter().filter(|d| d.is_audio()).count(),
        state.found.iter().filter(|d| d.name != "Unnamed Device").count(), state.found.iter().filter(|d| d.sink).count());
    for class in classes.chunk_by(|a, b| a == b) { println!("bt-probe: class={:#08x} count={}", class[0], class.len()); }
    // An inquiry the controller did not report complete: the events around it, codes only.
    let d = &state.diagnostics;
    if d.scan_timeouts > 0 {
        for (ms, code, opcode, status) in d.timeline.iter().filter(|t| matches!(t.1, 0x01 | 0x02 | 0x22 | 0x2f) || matches!(t.2, 0x0401 | 0x0402 | 0x0419)) {
            println!("bt-probe: t={ms} event={code:#04x} opcode={opcode:#06x} status={status:#04x}");
        }
    }
    state
}

/// The device `pair` chooses: one candidate, or with `--nearest` one that is close
/// (-60 dBm or stronger) and 10 dB above the next, so a neighbour's device is never
/// paired by accident. Candidates are audio-class devices; with `--any`, devices that
/// take audio by class or inquiry response, or when there are none, every device found.
fn choose(found: &[super::Device], nearest: bool, any: bool) -> Result<super::Device, String> {
    let mut candidates: Vec<_> = found.iter().filter(|d| d.is_audio()).collect();
    if any {
        candidates = found.iter().filter(|d| d.takes_audio()).collect();
        if candidates.is_empty() { candidates = found.iter().collect(); }
    }
    candidates.sort_by_key(|d| std::cmp::Reverse(d.rssi));
    if candidates.len() > 1 { println!("bt-probe: strongest_dbm={} next_dbm={}", candidates[0].rssi, candidates[1].rssi); }
    let clear = candidates.len() == 1 || nearest && candidates.len() > 1 && candidates[0].rssi >= -60 && i16::from(candidates[0].rssi) - i16::from(candidates[1].rssi) >= 10;
    if !clear {
        let kind = if any { "devices" } else { "audio devices" };
        return Err(format!("{} {kind} in pairing mode and none is clearly the nearest", candidates.len()));
    }
    Ok(candidates[0].clone())
}

/// Controller checks that separate a silent radio from a stalled baseband:
/// the version after the patch, whether the Bluetooth clock advances, and a
/// short inquiry watched for its completion. Codes and counts only.
fn diag(blocking: &Blocking, bluetooth: &Bluetooth, length: u8, mode: Option<u8>) -> bool {
    let pause = |ms: u64| { wait(blocking, bluetooth, Duration::from_millis(ms), |_| false); };
    match bluetooth.state().diagnostics.patched_version {
        Some((revision, subversion)) => println!("bt-probe: after_patch hci_revision={revision:#06x} lmp_subversion={subversion:#06x}"),
        None => println!("bt-probe: after_patch not read (no download in this run)"),
    }
    // Read Clock, local, twice one second apart. The clock counts 312.5 us ticks: 3200 a second.
    let clock = |n: usize| bluetooth.state().diagnostics.replies.iter().filter(|r| r.0 == 0x1407).nth(n)
        .filter(|r| r.1.len() >= 7 && r.1[0] == 0).map(|r| u32::from_le_bytes([r.1[3], r.1[4], r.1[5], r.1[6]]));
    let started = Instant::now();
    bluetooth.raw(0x1407, vec![0, 0, 0]);
    pause(1000);
    let between = started.elapsed().as_millis() as u64;
    bluetooth.raw(0x1407, vec![0, 0, 0]);
    pause(300);
    let ticking = match (clock(0), clock(1)) {
        (Some(first), Some(second)) => {
            let ticks = second.wrapping_sub(first);
            println!("bt-probe: clock ticks={ticks} in_ms={between} expected={}", between * 16 / 5);
            ticks > 0
        }
        _ => { println!("bt-probe: clock not read"); false }
    };
    if let Some(mode) = mode { bluetooth.raw(0x0c45, vec![mode]); pause(100); }
    let before = bluetooth.state().diagnostics;
    bluetooth.raw(0x0401, vec![0x33, 0x8b, 0x9e, length, 0]);
    // Three times the inquiry's length: an inquiry that overruns is timed, not only reported incomplete.
    let limit = Duration::from_millis(u64::from(length) * 1280 * 3 + 3000);
    let (state, complete, took) = wait(blocking, bluetooth, limit, |s| s.diagnostics.inquiry_completes > before.inquiry_completes);
    println!("bt-probe: inquiry length={length} complete={complete} after_ms={} results={}", took.as_millis(), state.diagnostics.inquiry_results - before.inquiry_results);
    if !complete { bluetooth.raw(0x0402, Vec::new()); pause(300); }
    let d = bluetooth.state().diagnostics;
    let codes: Vec<String> = d.by_code.iter().map(|(code, count)| format!("{code:#04x}x{count}")).collect();
    println!("bt-probe: event_codes=[{}]", codes.join(","));
    for (ms, code, opcode, status) in &d.timeline { println!("bt-probe: t={ms} event={code:#04x} opcode={opcode:#06x} status={status:#04x}"); }
    ticking && complete
}

/// Encoder cost: each codec encodes `seconds` of generated 44.1 kHz stereo audio as it would for a stream.
/// No controller is used.
fn bench(seconds: u32) -> bool {
    use q2_bt::codec::{self, Kind};
    let mut ok = true;
    let mut one = |label: &str, kind: Kind, ldac: super::coder::LdacQuality| {
        let stream = q2_bt::host::Stream { codec: codec::choose(&codec::capabilities(kind), false).expect("our own offer"), mtu: 895 };
        let started = Instant::now();
        match super::coder::run(stream, ldac, seconds) {
            Ok((packets, bytes, samples)) => {
                let audio_seconds = samples as f64 / 44100.0;
                println!("bt-probe: bench codec={label} ms_per_audio_second={:.2} cpu_percent={:.1} kbit_s={:.0} packets_per_second={:.0}",
                    started.elapsed().as_secs_f64() * 1000.0 / audio_seconds, started.elapsed().as_secs_f64() * 100.0 / audio_seconds,
                    bytes as f64 * 8.0 / 1000.0 / audio_seconds, f64::from(packets) / audio_seconds);
            }
            Err(error) => { println!("bt-probe: bench codec={label} failed: {error}"); ok = false; }
        }
    };
    use super::coder::LdacQuality;
    one("sbc", Kind::Sbc, LdacQuality::Standard);
    one("aptx", Kind::AptX, LdacQuality::Standard);
    one("aptx-hd", Kind::AptXHd, LdacQuality::Standard);
    one("aac-256k", Kind::Aac, LdacQuality::Standard);
    one("ldac-330k", Kind::Ldac, LdacQuality::Mobile);
    one("ldac-660k", Kind::Ldac, LdacQuality::Standard);
    one("ldac-990k", Kind::Ldac, LdacQuality::High);
    ok
}

/// A 440 Hz tone at -20 dBFS through the Bluetooth PCM, as the player would write it.
fn tone(blocking: &Blocking, seconds: u32, silence: bool) -> Result<(), String> {
    let writer = pcm::Writer::open(pcm::endpoint(), 44100, 2, 400_000)?;
    let mut phase = 0f64;
    let step = 2.0 * std::f64::consts::PI * 440.0 / 44100.0;
    for _ in 0..seconds * 10 {
        let block: Vec<i16> = (0..4410).flat_map(|_| { phase += step; let v = if silence { 0 } else { (3276.7 * phase.sin()) as i16 }; [v, v] }).collect();
        let mut written = 0;
        while written < 4410 {
            match writer.write_s16(blocking, &block[written * 2..]) {
                n if n > 0 => written += n as usize,
                n if writer.recover(n as i32) == 0 => {}
                n => return Err(pcm::Writer::error(n as i32)),
            }
        }
    }
    writer.drain(blocking);
    Ok(())
}

/// The modem lines with the controller held off and powered: whether the
/// controller's RTS reaches the Q2's CTS (docs/bluetooth.md, flow control).
/// With Wi-Fi on, the Wi-Fi slot keeps BT_DIS_N high and "off" is not off.
fn lines(blocking: &Blocking, options: &Options) -> bool {
    let Port::Tty(path) = &options.port else { println!("bt-probe: lines needs a tty"); return false };
    let hold = options.power_hold.as_deref().filter(|hold| hold.exists());
    let wifi_bound = std::path::Path::new(super::WIFI_SLOT).join("driver").exists();
    println!("bt-probe: lines power_hold={} wifi_slot_bound={wifi_bound}", hold.is_some());
    let set = |value: &str| hold.map(|hold| std::fs::write(hold, value));
    let mut ok = true;
    for (label, power) in [("off", "disabled"), ("on", "enabled"), ("off_again", "disabled")] {
        if let Some(Err(error)) = set(power) { println!("bt-probe: power hold: {error}"); return false }
        std::thread::sleep(Duration::from_millis(500));
        match super::uart::Uart::open(blocking, path).and_then(|uart| uart.line_state(blocking)) {
            Ok(line) => println!("bt-probe: lines power={label} cts={} rts={}", line.cts, line.rts),
            Err(error) => { println!("bt-probe: lines power={label} error={error}"); ok = false; }
        }
    }
    ok
}

fn forget_all(_blocking: &Blocking, store: &std::path::Path) -> std::io::Result<()> { std::fs::remove_file(store) }

fn bench_peer(value: &str) -> Option<q2_bt::host::Addr> {
    let chunks: Vec<_> = value.trim().split(':').collect();
    let mut addr = [0; 6];
    if chunks.len() != addr.len() { return None }
    for (out, part) in addr.iter_mut().rev().zip(chunks) {
        if part.len() != 2 { return None }
        *out = u8::from_str_radix(part, 16).ok()?;
    }
    (addr != [0; 6]).then_some(addr)
}

fn bench_nonce(directory: &Path) -> Option<&str> {
    if directory.parent()? != Path::new("/run") { return None }
    let nonce = directory.file_name()?.to_str()?.strip_prefix("q2-bench-a2dp-")?;
    (nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(nonce)
}

fn bench_file(path: &Path, limit: u64) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit || metadata.permissions().mode() & 0o077 != 0 {
        return None
    }
    std::fs::read_to_string(path).ok()
}

fn bench_setup(options: &mut Options, directory: &Path) -> Option<String> {
    let nonce = bench_nonce(directory)?;
    let metadata = std::fs::symlink_metadata(directory).ok()?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o777 != 0o700
        || bench_file(&directory.join("owner"), 40)?.trim() != nonce
        || std::fs::symlink_metadata(directory.join("go")).is_ok()
        || std::fs::symlink_metadata(directory.join("pairing")).is_ok()
        || std::fs::symlink_metadata(directory.join("pairing.tmp")).is_ok()
        || std::fs::symlink_metadata(directory.join("local-address")).is_ok()
    {
        return None
    }
    options.bench_peer = Some(bench_peer(&bench_file(&directory.join("peer"), 32)?)?);
    options.store = directory.join("pairing");
    options.codecs = vec![q2_bt::codec::Kind::Sbc];
    options.status_file = None;
    Some(nonce.to_owned())
}

fn bench_gate(blocking: &Blocking, bluetooth: &Bluetooth, peer: q2_bt::host::Addr,
              directory: &Path, nonce: &str, seconds: u32, silence: bool) -> bool {
    let Some(address) = bluetooth.state().bench_local_address else { return false };
    let mut private = match std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600)
        .open(directory.join("local-address"))
    {
        Ok(file) => file,
        Err(_) => return false,
    };
    let identity = address.iter().rev().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join(":");
    if writeln!(private, "{identity}").is_err() { return false }
    drop(private);
    println!("bt-probe: bench_ready=true");
    let linked = |state: &State| state.link == Link::Connected(peer) && state.stream
        .is_some_and(|stream| stream.codec.kind() == q2_bt::codec::Kind::Sbc);
    let (_, connected, _) = wait(blocking, bluetooth, Duration::from_secs(45), linked);
    if !connected { return false }
    println!("bt-probe: bench_transport_open=true");
    let gate = directory.join("go");
    let (state, released, _) = wait(blocking, bluetooth, Duration::from_secs(25), |state| {
        !linked(state) || std::fs::symlink_metadata(&gate).is_ok()
    });
    if !released || !linked(&state) || bench_file(&gate, 40).is_none_or(|value| value.trim() != nonce)
        || std::fs::remove_file(&gate).is_err()
    {
        return false
    }
    println!("bt-probe: bench_go_consumed=true");
    tone(blocking, seconds, silence).is_ok()
}

/// Runs one probe; the process exit code.
pub fn run(args: &[String]) -> i32 {
    let blocking = &Blocking::dedicated_thread();
    let mut options = Options::mainline(&crate::config::Bluetooth::default());
    options.reconnect = false;
    let (mut action, mut seconds, mut nearest, mut any) = (None, 10u32, false, false);
    let (mut inquiry_length, mut inquiry_mode) = (2u8, None);
    let mut bench_seconds = 30u32;
    let (mut bench_dir, mut silence) = (None, false);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = |name: &str| rest.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        let parsed: Result<(), String> = (|| {
            match arg.as_str() {
                "--port" => { let port = value(arg)?; options.port = if port == "fake" { Port::Fake } else { Port::Tty(port.into()) }; }
                "--firmware" => options.firmware_dirs = vec![PathBuf::from(value(arg)?)],
                "--store" => options.store = value(arg)?.into(),
                "--baud" => { options.baud = value(arg)?.parse().map_err(|_| "--baud needs a number".to_string())?; options.uart_auto = false; options.uart_fallback = false; }
                "--flow" => { options.flow = true; options.uart_auto = false; options.uart_fallback = false; }
                "--nearest" => nearest = true,
                "--bench-dir" => bench_dir = Some(PathBuf::from(value(arg)?)),
                "--silence" => silence = true,
                "--any" => any = true,
                "--seconds" => bench_seconds = value(arg)?.parse().ok().filter(|n| (1..=600).contains(n)).ok_or("--seconds needs 1 to 600")?,
                "--codec" => options.codecs = super::codecs(&value(arg)?),
                "--ldac-quality" => options.ldac = super::ldac_quality(&value(arg)?),
                "--ldac-fixed" => options.ldac_adaptive = false,
                "bench" => action = Some(arg.clone()),
                "--inquiry-length" => inquiry_length = value(arg)?.parse().ok().filter(|n| (1..=48).contains(n)).ok_or("--inquiry-length needs 1 to 48 (units of 1.28 s)")?,
                "--inquiry-mode" => inquiry_mode = Some(value(arg)?.parse().ok().filter(|n| *n <= 2).ok_or("--inquiry-mode needs 0, 1 or 2")?),
                "--power-hold" => options.power_hold = Some(value(arg)?.into()),
                "play" | "wait" | "bench-sink" => { action = Some(arg.clone()); seconds = value(arg)?.parse().map_err(|_| format!("{arg} needs seconds"))?; }
                "version" | "scan" | "diag" | "pair" | "forget-all" | "lines" => action = Some(arg.clone()),
                other => return Err(format!("unknown argument {other}")),
            }
            Ok(())
        })();
        if let Err(error) = parsed { eprintln!("bt-probe: {error}\n{USAGE}"); return 2 }
    }
    let Some(action) = action else { eprintln!("{USAGE}"); return 2 };
    let bench_nonce = if action == "bench-sink" {
        let validated = bench_dir.as_deref().filter(|_| (5..=30).contains(&seconds))
            .and_then(|dir| bench_setup(&mut options, dir));
        let Some(nonce) = validated else {
            eprintln!("bt-probe: invalid disposable bench scope");
            return 2
        };
        Some(nonce)
    } else {
        if bench_dir.is_some() || silence { eprintln!("bt-probe: bench options need bench-sink"); return 2 }
        None
    };
    if action == "bench" { return i32::from(!bench(bench_seconds)); }
    if action == "forget-all" {
        return match forget_all(blocking, &options.store) {
            Ok(()) => { println!("bt-probe: paired devices removed"); 0 }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => { println!("bt-probe: no paired devices"); 0 }
            Err(error) => { println!("bt-probe: {error}"); 1 }
        }
    }
    if action == "lines" { return i32::from(!lines(blocking, &options)); }
    options.reconnect = matches!(action.as_str(), "play");
    let bench_peer = options.bench_peer;
    println!("bt-probe: action={action} baud={} flow={} uart_auto={} uart_fallback={} firmware={}", options.baud, options.flow, options.uart_auto, options.uart_fallback, super::firmware(blocking, &options).is_some() || options.port == Port::Fake);
    let bluetooth = Bluetooth::new(options, Arc::new(|| {}));
    bluetooth.set_enabled(true);
    let (state, ready, took) = wait(blocking, &bluetooth, Duration::from_secs(25), |s| s.phase == Phase::Ready);
    println!("bt-probe: bring_up_ms={}", took.as_millis());
    if !ready { report(&state); return 1 }
    let ok = match action.as_str() {
        "bench-sink" => bench_gate(blocking, &bluetooth, bench_peer.unwrap(), bench_dir.as_deref().unwrap(),
                                   bench_nonce.as_deref().unwrap(), seconds, silence),
        "version" => true,
        "scan" => { scan(blocking, &bluetooth); true }
        "diag" => diag(blocking, &bluetooth, inquiry_length, inquiry_mode),
        "pair" => {
            let found = scan(blocking, &bluetooth);
            match choose(&found.found, nearest, any) {
                Err(reason) => { println!("bt-probe: pair refused: {reason}"); false }
                Ok(device) => {
                    // A device not of the audio class is paired only if its SDP records show an A2DP sink.
                    let check = !device.is_audio();
                    println!("bt-probe: chosen class={:#08x} audio_class={} eir_sink={} sdp_check={check}", device.class, device.is_audio(), device.sink);
                    if check { bluetooth.connect_checked(device.addr); } else { bluetooth.connect(device.addr); }
                    let (state, _, took) = wait(blocking, &bluetooth, Duration::from_secs(40), |s| s.stream.is_some() || !s.notice.is_empty());
                    let open = state.stream.is_some();
                    println!("bt-probe: pair_ms={} stream_open={open}", took.as_millis());
                    open
                }
            }
        }
        "wait" => {
            let (_, open, took) = wait(blocking, &bluetooth, Duration::from_secs(seconds.into()), |s| s.stream.is_some());
            println!("bt-probe: incoming_ms={} stream_open={open}", took.as_millis());
            open
        }
        _ if bluetooth.state().paired.is_empty() => { println!("bt-probe: no paired headphones; run pair first"); false }
        _ => {
            let (state, _, took) = wait(blocking, &bluetooth, Duration::from_secs(20), |s| s.stream.is_some() || !s.notice.is_empty());
            let open = state.stream.is_some();
            println!("bt-probe: connect_ms={} stream_open={open}", took.as_millis());
            open && match tone(blocking, seconds, false) { Ok(()) => { println!("bt-probe: tone_seconds={seconds}"); true } Err(error) => { println!("bt-probe: tone failed: {error}"); false } }
        }
    };
    report(&bluetooth.state());
    bluetooth.shutdown();
    println!("bt-probe: result={}", if ok { "pass" } else { "fail" });
    i32::from(!ok)
}

#[cfg(test)]
mod tests {
    use super::super::Device;
    use super::choose;

    fn device(last: u8, class: u32, rssi: i8, sink: bool) -> Device { Device { addr: [last; 6], name: "Generated".into(), class, rssi, sink } }

    #[test]
    fn pair_chooses_audio_devices_by_default_and_with_any_a_device_that_takes_audio_or_the_only_one() {
        let computer = device(1, 0x7c_0104, -50, false);
        let headphones = device(2, 0x24_0404, -70, false);
        let phone = device(3, 0x5a_020c, -40, false);
        // The default is unchanged: audio-class devices only.
        assert!(choose(std::slice::from_ref(&computer), false, false).is_err());
        assert_eq!(choose(&[computer.clone(), headphones.clone()], false, false), Ok(headphones.clone()));
        // --any: the only device found, whatever its class (its SDP is checked before pairing).
        assert_eq!(choose(std::slice::from_ref(&computer), false, true), Ok(computer.clone()));
        // Devices that take audio come before the rest; two of them need --nearest.
        let sink = Device { sink: true, ..computer.clone() };
        assert_eq!(choose(&[phone.clone(), sink.clone()], false, true), Ok(sink.clone()));
        assert!(choose(&[headphones.clone(), sink.clone()], false, true).is_err());
        assert_eq!(choose(&[headphones, sink.clone()], true, true), Ok(sink));
        assert!(choose(&[phone, computer], false, true).is_err(), "two devices, neither takes audio");
    }
}
