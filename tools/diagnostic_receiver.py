#!/usr/bin/env python3
"""Receive bounded, numeric Q2 diagnostics; never ingest ordinary player logs.

Each receiver owns an output directory and a generated transport token.
Explicit --resume preserves its identity and reports across service restarts.
Retries with identical report IDs/content are idempotent. An ID collision with
different bytes is refused. Acknowledgements follow the SQLite durable commit.

Only version2 endpoints are accepted:
/v2/production/reports and /v2/automated-test/reports, each with a distinct
private SQLite database and capacity. Path, batch and captured report context
must agree. Labels are classification, not authentication or attestation.
"""
import argparse
import fcntl
import hashlib
import hmac
import json
import math
import os
import re
import secrets
import signal
import sqlite3
import stat
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

MAX_BODY = 64 * 1024
MAX_BATCH = 16
MAX_REPORTS = 20_000
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
UUID = re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\Z")
STREAMS = ("production", "automated-test")
MODULE = re.compile(r"(?:q2nd|lib[a-zA-Z0-9_.+-]+\.so(?:\.[0-9]+)*)\Z")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON field")
        result[key] = value
    return result


def integer(value, maximum=(1 << 63) - 1):
    if type(value) is not int or not 0 <= value <= maximum:
        raise ValueError("invalid numeric field")


WAKE_MAPPING = "logarithmic-1pct-us-v1"
WAKE_MAX_US = 86400000000
WAKE_GAMMA = 1.01 / .99


def wake_index(value):
    return int(math.log(value) / math.log(WAKE_GAMMA))


def validate_sketch(sketch):
    fields = {"mapping", "count", "zero_count", "sum_us", "min_us", "max_us", "bins"}
    if not isinstance(sketch, dict) or set(sketch) != fields or sketch["mapping"] != WAKE_MAPPING:
        raise ValueError("unknown wake distribution schema")
    for name, bound in [("count",64),("zero_count",64),("sum_us",64*WAKE_MAX_US),
                        ("min_us",WAKE_MAX_US),("max_us",WAKE_MAX_US)]:
        integer(sketch[name],bound)
    count, zero, low, high = (sketch[k] for k in ("count","zero_count","min_us","max_us"))
    if (count == 0 or zero > count or low > high or (zero > 0) != (low == 0)
            or not high+(count-1)*low <= sketch["sum_us"] <= low+(count-1)*high):
        raise ValueError("inconsistent distribution summary")
    bins = sketch["bins"]
    if not isinstance(bins,list) or len(bins)>64:
        raise ValueError("wake bin bound")
    total, previous = zero, -1
    for pair in bins:
        if not isinstance(pair,list) or len(pair)!=2:
            raise ValueError("invalid wake bin")
        key, n = pair
        integer(key,1279); integer(n,64)
        if (n == 0 or key <= previous or high == 0 or key > wake_index(high)
                or (low > 0 and key < wake_index(low))):
            raise ValueError("invalid wake bin domain/order")
        total += n; previous = key
    if total != count:
        raise ValueError("wake distribution count mismatch")


def wake_summaries(database, stream):
    """Merge bounded, idempotently stored reports, keeping capture contexts apart.

    Read only the numeric receiver store; never load its transport credential.
    This is an observed-usage distribution, not a controlled latency benchmark.
    """
    import urllib.parse
    uri = "file:" + urllib.parse.quote(str(Path(database).resolve())) + "?mode=ro"
    groups = {}
    with sqlite3.connect(uri,uri=True) as db:
        if db.execute("SELECT COUNT(*) FROM reports").fetchone()[0] > MAX_REPORTS:
            raise ValueError("receiver report capacity exceeded")
        for body, in db.execute("SELECT body FROM reports"):
            row = json.loads(body,object_pairs_hook=unique_object)
            event = row.get("event",{})
            if event.get("type") != "distribution": continue
            validate(row,stream)
            key = json.dumps(dict(build=row["build"],context=row["context"],metric=event["metric"]),sort_keys=True)
            group = groups.setdefault(key,dict(count=0,zero_count=0,sum_us=0,min_us=WAKE_MAX_US,max_us=0,
                                             bins={},reports=0,reported_loss_counter_max=0))
            sketch = event["sketch"]
            for field in ("count","zero_count","sum_us"): group[field] += sketch[field]
            group["min_us"] = min(group["min_us"],sketch["min_us"])
            group["max_us"] = max(group["max_us"],sketch["max_us"])
            for bucket,n in sketch["bins"]: group["bins"][bucket] = group["bins"].get(bucket,0)+n
            group["reports"] += 1
            group["reported_loss_counter_max"] = max(group["reported_loss_counter_max"],row["dropped_reports"])
    result=[]
    for key,group in sorted(groups.items()):
        def quantile(q):
            rank=math.floor(q*(group["count"]-1)); seen=group["zero_count"]
            if rank<seen: return 0.0
            for bucket,n in sorted(group["bins"].items()):
                seen+=n
                if seen>rank:
                    value=math.exp(bucket*math.log(WAKE_GAMMA))*1.01
                    return min(group["max_us"],max(group["min_us"],value))
            raise ValueError("incomplete aggregate")
        values={"p50_us":quantile(.5),"p95_us":quantile(.95),"p99_us":quantile(.99)}
        result.append(dict(**json.loads(key),**{k:v for k,v in group.items() if k!="bins"},**values,
                           mapping=WAKE_MAPPING,relative_accuracy=.01,unit="us"))
    return dict(schema=1,groups=result,scope="observed wake usage; not controlled or optical latency; missing/lost reports remain unobserved")


def validate(report, stream):
    required = {"id", "build", "boot", "runtime", "kind", "thread", "task",
                "poll", "monotonic_us", "elapsed_us", "frames", "stack_status", "dropped_reports", "process", "context"}
    if not isinstance(report, dict) or set(report) not in (required, required | {"event"}):
        raise ValueError("invalid report fields")
    process = report["process"]
    if not isinstance(process, dict) or set(process) != {"pid", "start_ticks"}:
        raise ValueError("invalid process fields")
    integer(process["pid"], (1 << 32) - 1)
    integer(process["start_ticks"])
    if process["pid"] == 0 or process["start_ticks"] == 0:
        raise ValueError("invalid process identity")
    for key, pattern in (("id", UUID), ("boot", UUID), ("build", HEX64)):
        if not isinstance(report[key], str) or not pattern.fullmatch(report[key]):
            raise ValueError("invalid report identity")
    if "event" in report:
        validate_event(report)
    elif report["runtime"] not in ("ui", "io", "player", "job", "blocking", "db") or report["kind"] not in ("stall", "completed", "waiting", "bug"):
        raise ValueError("unknown runtime report")
    if report["stack_status"] not in ("complete", "truncated", "unavailable", "not_requested"):
        raise ValueError("unknown stack status")
    for key in ("thread", "task", "poll", "monotonic_us", "elapsed_us", "dropped_reports"):
        integer(report[key])
    frames = report["frames"]
    if not isinstance(frames, list) or len(frames) > 48:
        raise ValueError("stack frame bound")
    if report["stack_status"] in ("unavailable", "not_requested") and frames:
        raise ValueError("inconsistent stack status")
    if report["stack_status"] == "complete" and not frames:
        raise ValueError("empty complete stack")
    for frame in frames:
        if not isinstance(frame, dict) or set(frame) != {"module", "build", "offset"}:
            raise ValueError("invalid stack frame")
        if not isinstance(frame["module"], str) or not MODULE.fullmatch(frame["module"]):
            raise ValueError("module basename required")
        if not isinstance(frame["build"], str) or not HEX64.fullmatch(frame["build"]):
            raise ValueError("module identity required")
        integer(frame["offset"])
    # Context is captured with the event, not inferred from the upload time.
    context = report["context"]
    if not isinstance(context, dict) or set(context) != {"stream", "labels", "run_id", "kernel", "platform", "clock"}:
        raise ValueError("invalid capture context fields")
    if context["stream"] != stream or context["labels"] != (["automated-test"] if stream == "automated-test" else []):
        raise ValueError("capture stream or labels mismatch")
    if context["clock"] != "boottime":
        raise ValueError("unknown event clock")
    run_id = context["run_id"]
    if run_id is not None and (not isinstance(run_id, str) or not UUID.fullmatch(run_id)):
        raise ValueError("invalid generated run identity")
    for key in ("kernel", "platform"):
        value = context[key]
        if value is not None and (not isinstance(value, str) or not HEX64.fullmatch(value)):
            raise ValueError("invalid capture artifact identity")
    return json.dumps(report, sort_keys=True, separators=(",", ":"))


DISTRIBUTIONS = ("input-to-present", "resume-return-to-present", "shuffle-to-audio", "shuffle-albums-to-audio",
                 "music-play-to-audio", "spoken-play-to-audio", "resume-to-audio", "skip-to-audio",
                 "page-open", "info-open")
# KiB at a screen-off heap trim (platform/resident.rs).
MEMORY_METRICS = ("memory-resident-kib", "memory-available-kib", "heap-free-kib", "heap-released-kib")
UX_WHAT = ("error", "recovered", "journey", "stop", "stuck", "problem")
UX_STATE = {"wifi": 2, "navidrome": 3, "abs": 2, "doze": None, "offline": None, "screen_on": None,
            "play": 3, "pages": 255, "idle_s": (1 << 32) - 1}
# Added later (crate::ux::Snapshot::route): optional, so earlier reports still validate.
UX_STATE_OPTIONAL = {"route": 3}
U32 = (1 << 32) - 1
# crate::ux::bluetooth::Bluetooth, present while Bluetooth is on. "bool" a flag,
# "i8" a signed byte, "name" the device name, an int the bound; None-able keys in UX_BT_NULLABLE.
UX_BT = {"state": 7, "codec": 5, "rate": 384000, "kbps": 4000, "ldac_level": 4, "ldac_adaptive": "bool",
         "baud": 4000000, "flow_control": "bool", "uart_fallbacks": U32, "rssi": "i8", "link_quality": 255, "acl_buffers": U32, "acl_in_use": U32,
         "h5_retransmissions": U32, "h5_resends": U32, "h5_bad_frames": U32, "h5_out_of_sequence": U32,
         "credit_stalls": U32, "credit_stall_ms": U32, "credit_stall_max_ms": U32, "late": U32, "restarts": U32,
         "behind_max_ms": U32, "ldac_changes": U32, "queue_max": U32, "dropped": U32, "underruns": U32,
         "uart_frame": U32, "uart_overrun": U32, "uart_parity": U32, "uart_buffer": U32,
         "device_class": 0xffffff, "device_name": "name", "device_vendor_source": 0xffff, "device_vendor": 0xffff,
         "device_product": 0xffff, "device_version": 0xffff, "tx_power": "i8", "afh_channels": 79, "mtu": 65535,
         "sbc_bitpool": 255, "wake_late": U32, "wake_late_max_ms": U32, "write_max_ms": U32, "encode_max_us": U32}
UX_BT_NULLABLE = {"ldac_level", "rssi", "link_quality", "uart_frame", "uart_overrun", "uart_parity", "uart_buffer",
                  "device_class", "device_name", "device_vendor_source", "device_vendor", "device_product",
                  "device_version", "tx_power", "afh_channels", "sbc_bitpool"}
# Reports carry the headphones' advertised name.
UX_BT_NAME_MAX = 64
# Optional (added with the UART fallback), so earlier reports still validate.
UX_BT_OPTIONAL = {"uart_fallbacks"}


def validate_bluetooth(value):
    if not isinstance(value, dict) or not set(UX_BT) - UX_BT_OPTIONAL <= set(value) <= set(UX_BT):
        raise ValueError("invalid ux bluetooth")
    for key, kind in UX_BT.items():
        if key not in value:
            continue
        field = value[key]
        if field is None and key in UX_BT_NULLABLE:
            continue
        if kind == "bool":
            if type(field) is not bool:
                raise ValueError("invalid ux bluetooth flag")
        elif kind == "i8":
            if type(field) is not int or not -128 <= field <= 127:
                raise ValueError("invalid ux bluetooth signed field")
        elif kind == "name":
            if (not isinstance(field, str) or len(field) > UX_BT_NAME_MAX
                    or any(not ch.isprintable() for ch in field)):
                raise ValueError("invalid ux bluetooth name")
        else:
            integer(field, kind)


# platform/memory.rs: a screen-off memory-pressure sample. Every leaf is a
# bounded integer or null; nothing else is admitted.
MEMORY_MEMINFO = ("mem_free_kib", "mem_available_kib", "cached_kib", "active_file_kib", "inactive_file_kib",
                  "anon_pages_kib", "mlocked_kib", "shmem_kib", "slab_kib", "sreclaimable_kib", "sunreclaim_kib")
MEMORY_VMSTAT = ("workingset_refault_file", "workingset_refault_anon", "pgmajfault", "pgsteal_kswapd",
                 "pgsteal_direct", "pgscan_kswapd", "pgscan_direct", "allocstall")
MEMORY_PROCESS = ("vm_rss_kib", "vm_hwm_kib", "majflt", "majflt_delta", "rss_kib", "pss_kib", "private_clean_kib",
                  "private_dirty_kib", "shared_clean_kib", "anonymous_kib")
MEMORY_HEAP = ("heap_arena_kib", "heap_used_kib", "heap_free_kib", "heap_mmap_kib", "art_decoded_kib", "art_images")
MEMORY_KIB = 1 << 22


def validate_memory(report, event):
    if report["kind"] != "memory" or set(event) != {"type", "sample"}:
        raise ValueError("invalid memory event")
    sample = event["sample"]
    if not isinstance(sample, dict) or set(sample) != {"interval_s", "read_us", "meminfo", "vmstat", "vmstat_delta",
                                                        "player", "tailscaled", "heap"}:
        raise ValueError("invalid memory sample")
    integer(sample["interval_s"], 1 << 32)
    integer(sample["read_us"], 60000000)

    def group(value, keys, bound, nullable_group=False, nullable_fields=True):
        if value is None and nullable_group:
            return
        if not isinstance(value, dict) or set(value) != set(keys):
            raise ValueError("invalid memory group")
        for key in keys:
            if value[key] is None and nullable_fields:
                continue
            integer(value[key], MEMORY_KIB if key.endswith("_kib") else bound)

    group(sample["meminfo"], MEMORY_MEMINFO, MEMORY_KIB)
    group(sample["vmstat"], MEMORY_VMSTAT, (1 << 63) - 1)
    group(sample["vmstat_delta"], MEMORY_VMSTAT, (1 << 63) - 1, nullable_group=True)
    group(sample["player"], MEMORY_PROCESS, (1 << 63) - 1)
    group(sample["tailscaled"], MEMORY_PROCESS, (1 << 63) - 1, nullable_group=True)
    group(sample["heap"], MEMORY_HEAP, 1 << 20, nullable_fields=False)


def validate_ux(report, event):
    """crate::ux reports: fixed codes, small enums, a numeric snapshot and history."""
    keys = {"type", "what", "code", "journey", "outcome", "latency_us", "detail", "state", "recent"}
    if report["kind"] != "ux" or set(event) not in (keys, keys | {"bluetooth"}) or event["what"] not in UX_WHAT:
        raise ValueError("invalid ux event")
    if "bluetooth" in event:
        validate_bluetooth(event["bluetooth"])
    integer(event["code"], 65535)
    integer(event["journey"], 255)
    integer(event["outcome"], 255)
    integer(event["latency_us"], 86400000000)
    integer(event["detail"], (1 << 32) - 1)
    state = event["state"]
    if not isinstance(state, dict) or not set(UX_STATE) <= set(state) <= set(UX_STATE) | set(UX_STATE_OPTIONAL):
        raise ValueError("invalid ux state")
    for key, bound in UX_STATE_OPTIONAL.items():
        if key in state:
            integer(state[key], bound)
    for key, bound in UX_STATE.items():
        if bound is None:
            if type(state[key]) is not bool:
                raise ValueError("invalid ux state flag")
        else:
            integer(state[key], bound)
    recent = event["recent"]
    if not isinstance(recent, list) or len(recent) > 64:
        raise ValueError("ux history bound")
    for entry in recent:
        if not isinstance(entry, list) or len(entry) != 3:
            raise ValueError("invalid ux history entry")
        for value in entry:
            integer(value, (1 << 32) - 1)


def validate_event(report):
    event = report["event"]
    if (report["frames"] != [] or report["stack_status"] != "not_requested"
            or any(report[key] != 0 for key in ("task", "poll", "elapsed_us"))):
        raise ValueError("invalid process event report shape")
    if not isinstance(event, dict):
        raise ValueError("invalid crash event")
    if event.get("type") == "kernel-archive-observed":
        if (report["kind"] != "kernel-archive-observed" or report["runtime"] != "kernel"
                or set(event) != {"type", "records", "bytes", "collecting_boot", "source_boot", "source_context"}
                or event["collecting_boot"] != report["boot"]
                or event["source_boot"] is not None or event["source_context"] is not None):
            raise ValueError("invalid kernel archive observation")
        integer(event["records"], 32)
        integer(event["bytes"], 1048576)
        if event["records"] == 0:
            raise ValueError("empty kernel archive observation")
        return
    if report["runtime"] != "process":
        raise ValueError("invalid process event runtime")
    if event.get("type") == "ux":
        validate_ux(report, event)
        return
    if event.get("type") == "memory":
        validate_memory(report, event)
        return
    if event.get("type") == "distribution":
        if (report["kind"] != "health" or set(event) != {"type","metric","sketch","last_us"}
                or event["metric"] not in DISTRIBUTIONS):
            raise ValueError("invalid wake distribution event")
        integer(event["last_us"])
        if event["last_us"] < report["monotonic_us"]:
            raise ValueError("distribution observation interval regressed")
        validate_sketch(event["sketch"])
        return
    if event.get("type") == "health":
        if report["kind"] != "health" or set(event) != {"type", "metric", "value"}:
            raise ValueError("invalid health event")
        integer(event["value"])
        metric, value = event["metric"], event["value"]
        if not isinstance(metric, str):
            raise ValueError("invalid health metric")
        if metric in ("app-start", "first-frame", "screen-wake-requested"):
            valid = value == 0
        elif metric in DISTRIBUTIONS or metric in ("wifi-associated-observed", "tailscale-ready-observed"):
            valid = value <= 86400000000
        elif metric == "badge-selected":
            valid = value <= 0x4fff
        elif metric in ("audio-error", "audio-recovery-succeeded", "audio-recovery-exhausted"):
            valid = 1 <= value <= 4095
        elif metric == "badge-selected-limit":
            valid = value == 1024
        elif metric in MEMORY_METRICS:
            valid = value <= 1 << 20
        else:
            valid = False
        if not valid:
            raise ValueError("health metric bound or allowlist")
        return
    if event.get("type") == "process-exit":
        if (report["kind"] != "process-exit"
                or set(event) != {"type", "wait_status", "reason"}
                or event["reason"] not in ("unexpected-exit", "commanded-stop", "launcher-cleanup")):
            raise ValueError("invalid process exit event")
        integer(event["wait_status"], 255)
        return
    if report["kind"] != "crash":
        raise ValueError("invalid crash kind")
    if event == {"type": "app-panic"}:
        return
    if set(event) != {"type", "signal", "code"} or event["type"] != "fatal-signal":
        raise ValueError("unknown crash event fields")
    integer(event["signal"], (1 << 32) - 1)
    # SIGBUS differs between Linux x86 (7) and MIPS/macOS (10); the exact
    # executable identity determines its target ABI. No signal addresses/text.
    if event["signal"] not in (4, 6, 7, 8, 10, 11):
        raise ValueError("unsupported fatal signal")
    if type(event["code"]) is not int or not -(1 << 31) <= event["code"] < (1 << 31):
        raise ValueError("invalid signal code")


class V2Store:
    def __init__(self, path, stream, capacity=MAX_REPORTS):
        if stream not in STREAMS:
            raise ValueError("unknown receiver stream")
        self.db = sqlite3.connect(path)
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.execute("CREATE TABLE IF NOT EXISTS reports (id TEXT PRIMARY KEY, body TEXT NOT NULL, sha256 TEXT NOT NULL)")
        self.capacity = capacity
        self.stream = stream

    def close(self):
        self.db.close()

    def commit(self, rows):
        """Commit the entire validated batch before returning any acknowledgement."""
        with self.db:
            # Reserve the write transaction before checking identities/capacity.
            self.db.execute("BEGIN IMMEDIATE")
            new = []
            for key, body in rows:
                old = self.db.execute("SELECT body FROM reports WHERE id=?", (key,)).fetchone()
                if old is not None:
                    if old[0] != body:
                        raise FileExistsError("report identity conflict")
                else:
                    new.append((key, body, hashlib.sha256(body.encode()).hexdigest()))
            count = self.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0]
            if count + len(new) > self.capacity:
                raise OverflowError("receiver capacity")
            self.db.executemany("INSERT INTO reports VALUES(?,?,?)", new)

    def accept(self, data):
        if len(data) > MAX_BODY:
            raise ValueError("batch byte bound")
        payload = json.loads(data, object_pairs_hook=unique_object)
        if (not isinstance(payload, dict) or set(payload) != {"schema", "stream", "reports"}
                or type(payload["schema"]) is not int or payload["schema"] != 2
                or payload["stream"] != self.stream):
            raise ValueError("invalid versioned stream batch")
        reports = payload["reports"]
        if not isinstance(reports, list) or not 1 <= len(reports) <= MAX_BATCH:
            raise ValueError("batch count bound")
        rows = []
        for report in reports:
            body = validate(report, self.stream)
            rows.append((report["id"], body))
        if len({key for key, _ in rows}) != len(rows):
            raise ValueError("duplicate ID within batch")
        self.commit(rows)
        return {"schema": 2, "stream": self.stream, "accepted": [key for key, _ in rows]}


def handler(stores, token):
    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            self.request.settimeout(3)
            super().setup()

        def log_message(self, *_):
            pass  # No request paths, headers or bodies in generic HTTP logs.

        def reply(self, code, body=b""):
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            target = next((value for name, value in stores.items()
                           if self.path == f"/v2/{name}/reports"), None)
            if target is None:
                self.reply(404)
                return
            if len(self.headers.get_all("Authorization", [])) != 1 or not hmac.compare_digest(
                    self.headers.get("Authorization", "").encode("utf-8"), ("Bearer " + token).encode("ascii")):
                self.reply(401)
                return
            if self.headers.get("Transfer-Encoding") or len(self.headers.get_all("Content-Length", [])) != 1:
                self.reply(400)
                return
            try:
                size = int(self.headers["Content-Length"])
                if not 0 < size <= MAX_BODY:
                    self.reply(413)
                    return
                data = self.rfile.read(size)
                if len(data) != size:
                    self.reply(400)
                    return
                result = target.accept(data)
            except FileExistsError:
                self.reply(409)
            except OverflowError:
                self.reply(507)
            except (ValueError, TypeError, UnicodeError, RecursionError):
                self.reply(400)
            except sqlite3.Error:
                self.reply(503)
            else:
                self.reply(200, json.dumps(result, separators=(",", ":")).encode())
    return Handler


def owned_file(path, maximum):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size > maximum:
            raise ValueError("receiver requires bounded owned files")
        return stream.read(maximum + 1)


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def storage_layout():
    return {name: f"{name}/reports.sqlite" for name in STREAMS}


def open_streams(directory, resume):
    stores = {}
    try:
        for name in STREAMS:
            path = directory / name
            if not resume:
                path.mkdir(mode=0o700)
                sync_directory(directory)
            info = path.lstat()
            if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise ValueError("stream requires private owned directory")
            if resume and not (path / "reports.sqlite").exists():
                raise ValueError("established stream storage missing")
            for suffix in ("", "-wal", "-shm"):
                db_path = path / ("reports.sqlite" + suffix)
                if db_path.exists() or db_path.is_symlink():
                    info = db_path.lstat()
                    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1:
                        raise ValueError("stream database must be owned and physically separate")
            stores[name] = V2Store(path / "reports.sqlite", name)
            sync_directory(path)
        return stores
    except BaseException:
        for store in stores.values(): store.close()
        raise


def open_receiver(directory, resume, listen=None, port=None):
    if not resume:
        directory.mkdir(mode=0o700)  # Fresh namespaces only; no migration path.
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("receiver requires a private owned directory")
    lock = os.open(directory / "receiver.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    stores, server = {}, None
    try:
        info = os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            raise ValueError("receiver lock ownership")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        token_path = directory / "transport-token"
        if resume:
            metadata = json.loads(owned_file(directory / "receiver.json", 4096), object_pairs_hook=unique_object)
            if (set(metadata) != {"schema", "address", "token_file", "max_body", "max_batch", "max_reports_per_stream", "stores"}
                    or type(metadata["schema"]) is not int or metadata["schema"] != 2
                    or metadata["max_body"] != MAX_BODY or metadata["max_batch"] != MAX_BATCH
                    or metadata["max_reports_per_stream"] != MAX_REPORTS or metadata["stores"] != storage_layout()):
                raise ValueError("receiver metadata mismatch")
            address = metadata["address"]
            if not isinstance(address, list) or len(address) != 2 or not isinstance(address[0], str) or type(address[1]) is not int or not 1 <= address[1] <= 65535:
                raise ValueError("receiver address invalid")
            if (listen is not None and listen != address[0]) or (port is not None and port != address[1]):
                raise ValueError("resume must preserve receiver address")
            token = owned_file(token_path, 65).decode("ascii").strip()
            if not HEX64.fullmatch(token) or token_path.stat().st_mode & 0o077:
                raise ValueError("receiver token invalid")
        else:
            address = [listen or "127.0.0.1", 9842 if port is None else port]
            token = secrets.token_hex(32)
            with token_path.open("x") as stream:
                os.fchmod(stream.fileno(), 0o600)
                stream.write(token + "\n")
                stream.flush()
                os.fsync(stream.fileno())
        stores = open_streams(directory, resume)
        server = HTTPServer(tuple(address), handler(stores, token))
        if not resume:
            pending = directory / "receiver.pending"
            with pending.open("x") as stream:
                os.fchmod(stream.fileno(), 0o600)
                json.dump({"schema": 2, "address": server.server_address, "token_file": str(token_path),
                    "max_body": MAX_BODY, "max_batch": MAX_BATCH, "max_reports_per_stream": MAX_REPORTS,
                    "stores": storage_layout()}, stream, indent=2)
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
            os.link(pending, directory / "receiver.json")
            pending.unlink()
            sync_directory(directory)
        return server, stores, lock
    except BaseException:
        if server is not None: server.server_close()
        for store in stores.values(): store.close()
        os.close(lock)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--listen")
    parser.add_argument("--port", type=int)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--summarize",choices=STREAMS,help="print mergeable wake distribution summaries without starting the server")
    args = parser.parse_args()
    if args.summarize:
        print(json.dumps(wake_summaries(args.out/args.summarize/"reports.sqlite",args.summarize),indent=2))
        return
    server, stores, lock = open_receiver(args.out, args.resume, args.listen, args.port)
    def stop(*_):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop)
    try:
        server.serve_forever(poll_interval=0.5)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        for store in stores.values(): store.close()
        os.close(lock)


if __name__ == "__main__":
    main()
