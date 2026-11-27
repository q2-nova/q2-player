#!/usr/bin/env python3
"""Readable UX incidents from the diagnostic receiver (crate::ux reports).

Prints UX errors, stuck states, unexplained stops, slow journeys and Report
a Problem reports per boot, with decoded state and flight-recorder history, and
latency summaries for the journey distributions. `detail` hashes are mapped back
to string literals found in the Rust sources (FNV-1a over non-digit bytes, as in
`ux::text_hash`). Read-only; never writes the receiver store.

  python3 tools/ux_triage.py                       # live production store
  python3 tools/ux_triage.py --store PATH --since-hours 6 --boot latest
  python3 tools/ux_triage.py --memory --boot latest  # memory-pressure samples
"""
import argparse
import collections
import json
import re
import sqlite3
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_STORE = ROOT / "build/diagnostic-receiver-v2-live-01/production/reports.sqlite"

# Keep in step with crates/q2nd/src/ux.rs.
CODES = {1: "unknown", 10: "offline", 11: "network-unreachable", 12: "navidrome-unreachable", 13: "abs-unreachable",
         14: "abs-changed", 15: "abs-not-configured", 16: "navidrome-not-configured", 17: "server-error", 18: "timeout",
         19: "stream-interrupted", 20: "not-downloaded", 21: "file-missing", 22: "card-unavailable", 23: "card-full",
         24: "descriptor", 25: "card-error", 26: "auth", 27: "abs-server-error", 30: "no-songs", 31: "no-downloaded-songs", 32: "playback-changed",
         33: "decode", 34: "output", 35: "database", 36: "cache-delete", 40: "stuck"}
JOURNEYS = ["none", "shuffle-songs", "shuffle-albums", "play-music", "play-spoken", "resume", "open-page", "open-info",
            "download", "buffering", "wake", "skip"]
OUTCOMES = ["none", "audio-started", "shown", "failed", "cancelled", "timed-out", "superseded"]
STOPS = {1: "user", 2: "unplug", 3: "sleep-timer", 4: "end-of-queue", 5: "error", 6: "system", 7: "settings", 8: "unexplained"}
MARKS = {1: "input", 2: "toast", 3: "error", 4: "journey-begin", 5: "journey-end", 6: "play-state", 7: "wifi",
         8: "navidrome", 9: "abs", 10: "doze", 11: "screen", 12: "player-cmd", 13: "recovered", 14: "stop", 15: "problem", 16: "retry",
         17: "track-open", 18: "stream-open", 19: "stream-wait", 20: "underrun", 21: "fetch", 22: "doze-ahead",
         23: "server-failure", 24: "server-probe", 25: "bt-stall", 26: "bt-transport", 27: "bt-rate", 28: "bt-state",
         29: "wifi-event", 30: "wifi-scan", 31: "analog-volume"}
# subsonic::Failure and q2_machines::server::Priority.
FAILURES = {1: "timeout", 2: "refused", 3: "proxy", 4: "server-error", 5: "network"}
PRIORITIES = ["foreground", "background", "low"]
ORIGINS = {1: "downloaded", 2: "cached", 3: "streaming", 4: "radio"}
FETCH_ENDS = {1: "done", 2: "newer-queue", 3: "offline"}
DOZE_REASONS = ["dozed", "woke-disabled", "woke-wifi-off", "woke-screen", "woke-airplay", "woke-stopped", "woke-low"]
PLAY = ["stopped", "playing", "paused", "buffering"]
SERVICE = ["unconfigured", "reachable", "unreachable", "checking"]
WIFI = ["off", "wanted", "connected"]
KEYS = ["center", "back", "play-pause", "prev", "next"]
ROUTES = ["?", "jack", "usb-dac", "bluetooth"]
BT_STATES = ["off", "starting", "ready", "connecting", "connected", "stream-open", "streaming", "failed"]
BT_CODECS = ["none", "SBC", "AAC", "aptX", "aptX-HD", "LDAC"]
BT_STALLS = {0: "behind schedule, caught up", 1: "schedule restarted (headphones ran dry)", 2: "no controller buffer",
             3: "thread woke late", 4: "UART write"}
BT_TRANSPORT = ["h5 resend timeouts", "h5 damaged frames", "h5 out-of-sequence", "uart receive errors"]
LDAC_LEVELS = ["HQ", "SQ", "Q0", "Q1", "MQ"]
WIFI_EVENTS = ["scan-started", "scan-results", "associating", "connected", "disconnected", "network-not-found"]
WIFI_SCANS = {1: "not-found full scan", 2: "band steer 5 GHz (page open)", 3: "wifi page", 4: "reconnect"}
CMDS = {1: "play", 2: "play-pause", 3: "resume", 4: "pause", 5: "next", 6: "prev", 7: "jump", 8: "seek", 9: "scrub", 10: "volume"}


def text_hash(text):
    h = 2166136261
    for b in text.encode():
        if 48 <= b <= 57:
            continue
        h = ((h ^ b) * 16777619) & 0xFFFFFFFF
    return h


def literal_hashes():
    table = {}
    pattern = re.compile(r'"((?:[^"\\\n]|\\.){3,160})"')
    for path in (ROOT / "crates").rglob("*.rs"):
        if "vendor" in path.parts:
            continue
        for match in pattern.finditer(path.read_text(errors="replace")):
            text = match.group(1).replace('\\"', '"').replace("\\n", "\n")
            # A format string's fixed prefix is the most useful match.
            for candidate in {text, text.split("{")[0].rstrip()}:
                if candidate:
                    table.setdefault(text_hash(candidate), candidate)
    return table


def mark(entry, origin):
    t, kind, value = entry
    name = MARKS.get(kind, f"mark-{kind}")
    if name == "input":
        group = value >> 8
        detail = {1: lambda v: f"key {KEYS[(v & 0xff) >> 1] if (v & 0xff) >> 1 < len(KEYS) else v} {'down' if v & 1 else 'up'}",
                  2: lambda v: "keys-reset", 3: lambda v: "jack", 4: lambda v: f"wheel {((v & 0xff) ^ 0x80) - 0x80:+d}",
                  5: lambda v: "wheel-end", 6: lambda v: f"media {v & 0xff}"}.get(group, lambda v: hex(v))(value)
    elif name in ("journey-begin", "journey-end"):
        detail = JOURNEYS[value] if value < len(JOURNEYS) else value
    elif name in ("error", "recovered"):
        detail = CODES.get(value, value)
    elif name == "play-state":
        detail = PLAY[value] if value < 4 else value
    elif name in ("navidrome", "abs"):
        detail = SERVICE[value] if value < 4 else value
    elif name == "wifi":
        detail = WIFI[value] if value < 3 else value
    elif name == "stop":
        detail = STOPS.get(value, value)
    elif name == "player-cmd":
        detail = CMDS.get(value, value)
    elif name == "analog-volume":
        detail = (f"{value >> 25} to {(value >> 18) & 127}, {value & 65535} ms"
                  + (", changed compensation" if value & (1 << 17) else "")
                  + (", failed" if value & (1 << 16) else ""))
    elif name == "screen":
        detail = {0: "off", 1: "on", 20: "wake-begin", 21: "wake-blank-joined", 22: "wake-battery-ready",
                  23: "wake-panel-enabled", 24: "wake-presented"}.get(value, value)
    elif name == "retry":
        detail = f"{CODES.get(value >> 8, value >> 8)} attempt {value & 0xff}"
    elif name == "track-open":
        detail = (f"{ORIGINS.get(value >> 29, value >> 29)}{' spoken' if value >> 28 & 1 else ''}"
                  f" source {value >> 14 & 0x3fff} ms probe {value & 0x3fff} ms")
    elif name == "stream-open":
        percent = value >> 24
        detail = f"{'?' if percent == 127 else percent}% written ({value & 0xffffff} KiB)"
    elif name == "stream-wait":
        detail = f"{value} ms"
    elif name == "underrun":
        detail = f"errno {value}"
    elif name == "fetch":
        tag, quantity = value >> 24, value & 0xffffff
        detail = {0: f"begin {quantity} songs", 1: f"song done in {quantity} s",
                  2: f"end {FETCH_ENDS.get(quantity, quantity)}", 3: "nothing to fetch"}.get(tag, hex(value))
    elif name == "doze-ahead":
        tag = value >> 24
        detail = f"{DOZE_REASONS[tag] if tag < len(DOZE_REASONS) else tag} ahead {value & 0xffffff} s"
    elif name == "server-failure":
        priority = value >> 8 & 3
        detail = f"{PRIORITIES[priority] if priority < 3 else priority} {FAILURES.get(value & 0xff, value & 0xff)}"
    elif name == "server-probe":
        detail = (f"attempt {value >> 4 & 0xf} " + ("answered" if value & 1 else
                  f"failed {FAILURES.get(value >> 8 & 0xff, value >> 8 & 0xff)}"))
    elif name in ("bt-stall", "bt-transport", "bt-rate", "bt-state"):
        tag, quantity = value >> 24, value & 0xffffff
        if name == "bt-stall":
            detail = f"{BT_STALLS.get(tag, tag)} {quantity} ms"
        elif name == "bt-transport":
            detail = f"{BT_TRANSPORT[tag] if tag < 4 else tag} +{quantity}"
        elif name == "bt-rate":
            detail = f"LDAC {LDAC_LEVELS[tag] if tag < 5 else tag} {quantity} kbit/s"
        else:
            detail = f"{BT_STATES[tag] if tag < 8 else tag} {BT_CODECS[quantity] if quantity < 6 else quantity}"
    elif name == "wifi-event":
        detail = WIFI_EVENTS[value] if value < len(WIFI_EVENTS) else value
    elif name == "wifi-scan":
        detail = WIFI_SCANS.get(value, value)
    elif name == "toast":
        detail = f"{value:08x}"
    else:
        detail = value
    return f"{(t - origin) / 1000:+8.2f}s {name} {detail}"


def describe(row, literals):
    e = row["event"]
    what = e["what"]
    journey = JOURNEYS[e["journey"]] if e["journey"] < len(JOURNEYS) else e["journey"]
    if what == "stop":
        head = f"stop: {STOPS.get(e['code'], e['code'])}"
        if e["code"] == 5:
            head += f" ({CODES.get(e['detail'], e['detail'])})"
    elif what == "journey":
        head = f"journey {journey}: {OUTCOMES[e['outcome']]} in {e['latency_us'] / 1000:.0f} ms"
    elif what == "stuck":
        head = f"stuck {journey} for {e['latency_us'] / 1000:.0f} ms"
    elif what == "problem":
        head = "Report a Problem"
    elif what == "recovered" and e["code"] == 34:
        head = f"underrun (errno {e['detail']}) during {journey}"
    else:
        head = f"{what} {CODES.get(e['code'], e['code'])} during {journey}"
        if e["detail"]:
            text = literals.get(e["detail"], "text %08x" % e["detail"])
            head += f" — {text!r}"
    s = e["state"]
    state = (f"wifi={WIFI[s['wifi']]} navidrome={SERVICE[s['navidrome']]} abs={SERVICE[s['abs']]}"
             f"{' doze' if s['doze'] else ''}{' offline' if s['offline'] else ''} screen={'on' if s['screen_on'] else 'off'}"
             f" play={PLAY[s['play']]} pages={s['pages']} idle={s['idle_s']}s"
             f" route={ROUTES[s.get('route', 0)] if s.get('route', 0) < 4 else s['route']}")
    b = e.get("bluetooth")
    if b:
        n = lambda v: "?" if v is None else v
        level = "" if b["ldac_level"] is None else f" level={LDAC_LEVELS[b['ldac_level']]}{' adaptive' if b['ldac_adaptive'] else ''}"
        dev_id = ("" if b["device_vendor"] is None else
                  f" id={b['device_vendor_source']}:{b['device_vendor']:04x}:{b['device_product']:04x}:{b['device_version']:04x}")
        state += (f"\n   bluetooth: {BT_STATES[b['state']]} {BT_CODECS[b['codec']]} {b['rate']} Hz {b['kbps']} kbit/s{level}"
                  f" mtu={b['mtu']} bitpool={n(b['sbc_bitpool'])} uart={b['baud']}{' rts/cts' if b['flow_control'] else ''} fallbacks={b.get('uart_fallbacks', 0)}"
                  f"\n   device: {b['device_name']!r} class={n(b['device_class']) if b['device_class'] is None else hex(b['device_class'])}{dev_id}"
                  f"\n   link: rssi={n(b['rssi'])} quality={n(b['link_quality'])} tx_power={n(b['tx_power'])} afh={n(b['afh_channels'])}/79"
                  f" acl {b['acl_in_use']}/{b['acl_buffers']} credit stalls={b['credit_stalls']} ({b['credit_stall_ms']} ms, max {b['credit_stall_max_ms']})"
                  f"\n   h5: retransmitted={b['h5_retransmissions']} resends={b['h5_resends']} bad={b['h5_bad_frames']} out_of_seq={b['h5_out_of_sequence']}"
                  f" uart: frame={n(b['uart_frame'])} overrun={n(b['uart_overrun'])} parity={n(b['uart_parity'])} buffer={n(b['uart_buffer'])}"
                  f"\n   stream: late={b['late']} restarts={b['restarts']} behind_max={b['behind_max_ms']} ms queue_max={b['queue_max']}"
                  f" ldac_changes={b['ldac_changes']} dropped={b['dropped']} underruns={b['underruns']}"
                  f"\n   thread: late wakes={b['wake_late']} (max {b['wake_late_max_ms']} ms) write max={b['write_max_ms']} ms"
                  f" encode max={b['encode_max_us']} us")
    return head, state


def memory_line(row):
    """One `memory` report (platform/memory.rs): what grew and what the system reclaimed."""
    s = row["event"]["sample"]
    m, p, t, h = s["meminfo"], s["player"], s["tailscaled"] or {}, s["heap"]
    d = s["vmstat_delta"] or {}
    kib = lambda v: "?" if v is None else f"{v / 1024:.1f}M"
    n = lambda v: "?" if v is None else str(v)
    return (f"-- {row['monotonic_us'] / 1e6:10.1f}s memory after {s['interval_s']} s (read {s['read_us']} us)\n"
            f"   system: free {kib(m['mem_free_kib'])} available {kib(m['mem_available_kib'])} file active/inactive"
            f" {kib(m['active_file_kib'])}/{kib(m['inactive_file_kib'])} anon {kib(m['anon_pages_kib'])}"
            f" mlocked {kib(m['mlocked_kib'])} slab {kib(m['slab_kib'])}\n"
            f"   interval: file refaults {n(d.get('workingset_refault_file'))} anon refaults {n(d.get('workingset_refault_anon'))}"
            f" major faults {n(d.get('pgmajfault'))} kswapd steal/scan {n(d.get('pgsteal_kswapd'))}/{n(d.get('pgscan_kswapd'))}"
            f" direct steal/scan {n(d.get('pgsteal_direct'))}/{n(d.get('pgscan_direct'))} allocstalls {n(d.get('allocstall'))}\n"
            f"   player: rss {kib(p['vm_rss_kib'])} hwm {kib(p['vm_hwm_kib'])} pss {kib(p['pss_kib'])} anon {kib(p['anonymous_kib'])}"
            f" major faults +{n(p['majflt_delta'])}; heap arena {kib(h['heap_arena_kib'])} used {kib(h['heap_used_kib'])}"
            f" free {kib(h['heap_free_kib'])} mmap {kib(h['heap_mmap_kib'])}; art {kib(h['art_decoded_kib'])} in {h['art_images']}\n"
            f"   tailscaled: rss {kib(t.get('vm_rss_kib'))} pss {kib(t.get('pss_kib'))} major faults +{n(t.get('majflt_delta'))}")


def summarize(sketch):
    bins = sorted((int(k), n) for k, n in sketch["bins"])
    gamma = 1.01 / 0.99
    total = sketch["count"]
    def q(p):
        target, seen = p * total, sketch["zero_count"]
        if seen >= target:
            return 0
        for index, n in bins:
            seen += n
            if seen >= target:
                return 2 * gamma ** index / (gamma + 1)
        return sketch["max_us"]
    return total, q(0.5) / 1000, q(0.9) / 1000, sketch["max_us"] / 1000


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--store", type=Path, default=DEFAULT_STORE)
    parser.add_argument("--boot", help="boot UUID prefix, or 'latest'")
    parser.add_argument("--history", type=int, default=20, help="flight-recorder entries to show per incident")
    parser.add_argument("--memory", action="store_true", help="print the memory-pressure samples instead")
    args = parser.parse_args()
    db = sqlite3.connect(f"file:{args.store}?mode=ro", uri=True)
    rows = [json.loads(body) for (body,) in db.execute("select body from reports order by rowid")]
    if args.boot == "latest" and rows:
        args.boot = rows[-1]["boot"]
    if args.boot:
        rows = [r for r in rows if r["boot"].startswith(args.boot)]
    literals = literal_hashes()
    by_boot = collections.OrderedDict()
    for r in rows:
        by_boot.setdefault((r["boot"], r["build"]), []).append(r)
    for (boot, build), group in by_boot.items():
        ux = [r for r in group if r.get("kind") == "ux"]
        dist = collections.defaultdict(list)
        for r in group:
            e = r.get("event") or {}
            if e.get("type") == "distribution":
                dist[e["metric"]].append(e["sketch"])
        if args.memory:
            memory = [r for r in group if r.get("kind") == "memory"]
            if memory:
                print(f"== boot {boot[:8]} build {build[:8]}: {len(memory)} memory samples")
                for r in memory:
                    print(memory_line(r))
            continue
        if not ux and not dist:
            continue
        print(f"== boot {boot[:8]} build {build[:8]}: {len(ux)} ux reports")
        for metric, sketches in sorted(dist.items()):
            for sketch in sketches[-1:]:
                n, p50, p90, mx = summarize(sketch)
                print(f"   {metric:24} n={n:<4} p50={p50:8.1f} ms p90={p90:8.1f} ms max={mx:8.1f} ms (last batch)")
        for r in ux:
            head, state = describe(r, literals)
            print(f"-- {r['monotonic_us'] / 1e6:10.1f}s {head}\n   {state}")
            recent = r["event"]["recent"][-args.history:]
            origin = (r["monotonic_us"] // 1000) & 0xFFFFFFFF
            for entry in recent:
                print("     " + mark(entry, origin))


if __name__ == "__main__":
    main()
