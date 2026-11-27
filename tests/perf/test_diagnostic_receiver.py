import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import http.client
import subprocess
import sys
import time
import os
import copy

_spec = importlib.util.spec_from_file_location("diagnostic_receiver", Path(__file__).resolve().parents[2] / "tools/diagnostic_receiver.py")
receiver = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(receiver)


def report(number=1, stream="production"):
    return dict(id=f"00000000-0000-4000-8000-{number:012x}", build="a" * 64,
                boot="11111111-1111-4111-8111-111111111111", runtime="ui", kind="stall",
                thread=12, task=3, poll=4, monotonic_us=500000, elapsed_us=250000, dropped_reports=0,
                frames=[dict(module="q2nd", build="a" * 64, offset=1234)], stack_status="complete",
                process={"pid":42,"start_ticks":12345}, context={"stream":stream,
                "labels":["automated-test"] if stream == "automated-test" else [],
                "run_id":"22222222-2222-4222-8222-222222222222" if stream == "automated-test" else None,
                "kernel":None,"platform":"b"*64,"clock":"boottime"})


def batch(*rows, stream="production"):
    return json.dumps(dict(schema=2, stream=stream, reports=list(rows))).encode()


def crash(number=1, stream="production", event=None):
    row=report(number,stream)
    row.update(kind="crash",runtime="process",task=0,poll=0,elapsed_us=0,
               thread=0,monotonic_us=0,dropped_reports=0,frames=[],stack_status="not_requested",
               event={"type":"app-panic"} if event is None else event)
    return row


def process_exit(number=1, stream="production", status=0, reason="unexpected-exit"):
    row=crash(number,stream,{"type":"process-exit","wait_status":status,"reason":reason})
    row["kind"]="process-exit"
    return row


def health(number=1, stream="production", metric="first-frame", value=0):
    row=crash(number,stream,{"type":"health","metric":metric,"value":value})
    row["kind"]="health"
    return row


def ux(number=1, stream="production", **changes):
    event={"type":"ux","what":"error","code":13,"journey":4,"outcome":3,"latency_us":1500000,"detail":123456,
           "state":{"wifi":2,"navidrome":1,"abs":2,"doze":False,"offline":False,"screen_on":True,"play":2,"pages":1,"idle_s":4},
           "recent":[[1000,1,257],[1010,4,4],[1020,3,13]]}
    event.update(changes)
    row=crash(number,stream,event)
    row["kind"]="ux"
    return row


# Same shape as platform::memory's `memory_event_wire_format_matches_receiver`.
MEMORY_SAMPLE = {"interval_s": 600, "read_us": 1800,
    "meminfo": {"mem_free_kib": 5120, "mem_available_kib": 30000, "cached_kib": 21000, "active_file_kib": 9000,
                "inactive_file_kib": 11000, "anon_pages_kib": 40000, "mlocked_kib": 18000, "shmem_kib": 100,
                "slab_kib": 6000, "sreclaimable_kib": 2000, "sunreclaim_kib": 4000},
    "vmstat": {"workingset_refault_file": 90000, "workingset_refault_anon": None, "pgmajfault": 7000,
               "pgsteal_kswapd": 100, "pgsteal_direct": 3, "pgscan_kswapd": 200, "pgscan_direct": 4, "allocstall": 1},
    "vmstat_delta": None,
    "player": {"vm_rss_kib": 43800, "vm_hwm_kib": 45000, "majflt": 321, "majflt_delta": None, "rss_kib": 43800,
               "pss_kib": 40000, "private_clean_kib": 15000, "private_dirty_kib": 25800, "shared_clean_kib": 3000,
               "anonymous_kib": 25300},
    "tailscaled": None,
    "heap": {"heap_arena_kib": 19800, "heap_used_kib": 12000, "heap_free_kib": 7800, "heap_mmap_kib": 900,
             "art_decoded_kib": 3000, "art_images": 40}}


def memory(number=1, stream="production", **changes):
    row=crash(number,stream,{"type":"memory","sample":{**copy.deepcopy(MEMORY_SAMPLE),**changes}})
    row["kind"]="memory"
    return row


def distribution(number, values, stream="production"):
    row=health(number,stream,"input-to-present",0)
    row["monotonic_us"]=500000
    bins={}
    for value in values:
        if value: bins[receiver.wake_index(value)] = bins.get(receiver.wake_index(value),0)+1
    row["event"]={"type":"distribution","metric":"input-to-present","last_us":row["monotonic_us"],
                  "sketch":{"mapping":receiver.WAKE_MAPPING,"count":len(values),"zero_count":values.count(0),
                            "sum_us":sum(values),"min_us":min(values),"max_us":max(values),
                            "bins":[[key,n] for key,n in sorted(bins.items())]}}
    return row


def kernel_observed(number=1, stream="production"):
    row=crash(number,stream)
    row.update(kind="kernel-archive-observed",runtime="kernel",event={
        "type":"kernel-archive-observed","records":1,"bytes":0,
        "collecting_boot":row["boot"],"source_boot":None,"source_context":None})
    return row


class VersionedStoreControls(unittest.TestCase):
    def setUp(self):
        root = Path(__file__).resolve().parents[2] / "build"
        root.mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="receiver-v2-", dir=root)
        self.path = Path(self.temp.name)
        self.stores = {name: receiver.V2Store(self.path / (name + ".sqlite"), name, capacity=2)
                       for name in receiver.STREAMS}

    def tearDown(self):
        for store in self.stores.values():
            store.close()
        self.temp.cleanup()

    def test_physical_store_isolation_and_capacity_do_not_cross_streams(self):
        prod, test = (self.stores[n] for n in receiver.STREAMS)
        prod.accept(batch(report(1), report(2)))
        with self.assertRaises(OverflowError):
            prod.accept(batch(report(3)))
        row = report(1, "automated-test")
        self.assertEqual(test.accept(batch(row, stream="automated-test")),
                         {"schema": 2, "stream": "automated-test", "accepted": [row["id"]]})
        self.assertNotEqual((self.path / "production.sqlite").stat().st_ino,
                            (self.path / "automated-test.sqlite").stat().st_ino)
        self.assertEqual(test.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0], 1)
        self.assertEqual(prod.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0], 2)
        self.assertNotIn("automated-test", prod.db.execute("SELECT body FROM reports LIMIT 1").fetchone()[0])
        self.assertEqual(prod.accept(batch(report(1)))["accepted"], [row["id"]])

    def test_context_is_durable_immutable_and_retry_ack_exact_after_restart(self):
        store = self.stores["automated-test"]
        row = report(1, "automated-test")
        body = batch(row, stream="automated-test")
        expected = store.accept(body)  # Simulate an acknowledged commit whose reply was lost.
        store.close()
        store = self.stores["automated-test"] = receiver.V2Store(
            self.path / "automated-test.sqlite", "automated-test", capacity=2)
        self.assertEqual(store.accept(body), expected)
        self.assertEqual(store.db.execute("PRAGMA synchronous").fetchone()[0], 2)
        saved = store.db.execute("SELECT body,sha256 FROM reports").fetchone()
        canonical = json.dumps(row, sort_keys=True, separators=(",", ":"))
        self.assertEqual(saved, (canonical, receiver.hashlib.sha256(canonical.encode()).hexdigest()))
        for key, value in (("run_id", None), ("kernel", "a" * 64), ("platform", None)):
            changed = copy.deepcopy(row); changed["context"][key] = value
            with self.subTest(key=key), self.assertRaises(FileExistsError):
                store.accept(batch(report(2, "automated-test"), changed, stream="automated-test"))
            self.assertEqual(store.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0], 1)

    def test_strict_context_types_unknown_fields_and_private_canaries_refused(self):
        store = self.stores["production"]
        invalid = []
        for key, values in {"stream": ["automated-test", None, 0],
                            "labels": [["automated-test"], ["private-canary"], None, ""],
                            "run_id": ["private-canary", False, 0, []],
                            "kernel": ["private-canary", False, 1, []],
                            "platform": ["private-canary", False, 1, []],
                            "clock": ["realtime", None, 1]}.items():
            for value in values:
                row = report(); row["context"][key] = value; invalid.append(row)
        for key in report()["context"]:
            row = report(); del row["context"][key]; invalid.append(row)
        for context in (None, [], {}):
            invalid.append(dict(report(), context=context))
        row = report(); row["context"]["secret"] = "private-canary"; invalid.append(row)
        row = report(); row["message"] = "private-canary"; invalid.append(row)
        row = report(); row["kind"] = "crash"; invalid.append(row)
        for row in invalid:
            with self.subTest(row=row), self.assertRaises(ValueError):
                store.accept(batch(row))
        self.assertEqual(store.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0], 0)
        self.assertEqual(store.accept(batch(report()))["accepted"], [report()["id"]])

    def test_batch_version_route_duplicates_bounds_and_mixed_streams_refuse_atomically(self):
        store = self.stores["production"]
        bad = [json.dumps({"schema":1,"reports":[report()]}).encode(), batch(), batch(report(), report()),
               batch(report(), report(2, "automated-test")),
               batch(report(), stream="automated-test"),
               batch(*[report(n) for n in range(17)]), b" " * (receiver.MAX_BODY + 1),
               batch(report()).replace(b'"schema": 2', b'"schema": 2, "schema": 2'),
               batch(report()).replace(b'"clock": "boottime"', b'"clock": "boottime", "clock": "boottime"')]
        for body in bad:
            with self.subTest(body=body[:60]), self.assertRaises(ValueError):
                store.accept(body)
        self.assertEqual(store.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0], 0)

    def test_v2_database_failure_never_partially_acknowledges(self):
        store = self.stores["production"]
        store.db.execute("CREATE TRIGGER fail_insert BEFORE INSERT ON reports "
                         "WHEN NEW.id LIKE '%000000000002' BEGIN SELECT RAISE(ABORT, 'generated'); END")
        with self.assertRaises(receiver.sqlite3.Error):
            store.accept(batch(report(1), report(2)))
        self.assertEqual(store.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0], 0)
        store.db.execute("DROP TRIGGER fail_insert")
        self.assertEqual(store.accept(batch(report(1), report(2)))["accepted"],
                         [report(1)["id"], report(2)["id"]])

    def test_required_process_and_numeric_stack_bounds(self):
        invalid=[]
        for field in ("process","context"):
            row=report();del row[field];invalid.append(row)
        for process in (None,{},[],{"pid":0,"start_ticks":1},{"pid":1,"start_ticks":0},
                        {"pid":True,"start_ticks":1},{"pid":2**32,"start_ticks":1},
                        {"pid":1,"start_ticks":2**63},{"pid":1,"start_ticks":1,"address":123}):
            row=report();row["process"]=process;invalid.append(row)
        for key,value in (("thread",True),("task",-1),("elapsed_us",2**63),("frames",report()["frames"]*49),
                          ("stack_status","unavailable"),("build","unknown")):
            row=report();row[key]=value;invalid.append(row)
        row=report();row["frames"][0]["module"]="/private/path/q2nd";invalid.append(row)
        for row in invalid:
            with self.subTest(row=row),self.assertRaises(ValueError):self.stores["production"].accept(batch(row))
        row=report();row.update(kind="waiting",frames=[],stack_status="not_requested")
        self.assertEqual(self.stores["production"].accept(batch(row))["accepted"],[row["id"]])

    def test_crash_variants_allow_exact_signals_and_signed_codes(self):
        store=self.stores["automated-test"]
        row=crash(1,"automated-test")
        self.assertEqual(store.accept(batch(row,stream="automated-test"))["accepted"],[row["id"]])
        for signal in (4,6,7,8,10,11):
            for code in (-2**31,-6,0,1,2**31-1):
                row=crash(2,"automated-test",{"type":"fatal-signal","signal":signal,"code":code})
                self.assertEqual(json.loads(receiver.validate(row,"automated-test")),row)
        row=crash();row.update(thread=2**32-1,monotonic_us=2**63-1,dropped_reports=1)
        self.assertEqual(json.loads(receiver.validate(row,"production")),row)

    def test_crash_unknown_text_addresses_and_inconsistent_runtime_refused(self):
        invalid=[]
        for event in (None,{},[],{"type":"app-panic","message":"secret-canary"},
                      {"type":"fatal-signal","signal":11}, {"type":"panic"},
                      {"type":"fatal-signal","signal":11,"code":0,"address":123}):
            row=crash();row["event"]=event;invalid.append(row)
        for key,values in (("signal",[True,0,-1,2**32,9,15,"11"]),
                           ("code",[True,-2**31-1,2**31,0.0,"0"])):
            for value in values:
                event={"type":"fatal-signal","signal":11,"code":0};event[key]=value
                invalid.append(crash(event=event))
        for key,value in (("runtime","ui"),("kind","stall"),("task",1),("poll",1),("elapsed_us",1),
                          ("frames",report()["frames"]),("stack_status","unavailable")):
            row=crash();row[key]=value;invalid.append(row)
        row=report();row["event"]={"type":"app-panic"};invalid.append(row)
        for row in invalid:
            with self.subTest(row=row),self.assertRaises(ValueError):self.stores["production"].accept(batch(row))
        self.assertEqual(self.stores["production"].db.execute("SELECT COUNT(*) FROM reports").fetchone()[0],0)

    def test_process_exit_exact_schema_bounds_and_kind(self):
        for status in (0, 1, 128, 255):
            for reason in ("unexpected-exit", "commanded-stop", "launcher-cleanup"):
                receiver.validate(process_exit(status=status,reason=reason),"production")
        invalid=[]
        for status in (-1,256,True,"1",None):
            invalid.append(process_exit(status=status))
        for reason in ("signal", "", None, {}, ["unexpected-exit"]):
            invalid.append(process_exit(reason=reason))
        for field,value in (("kind","crash"),("runtime","ui"),("task",1),
                            ("frames",[dict(module="q2nd",build="a"*64,offset=1)])):
            row=process_exit();row[field]=value;invalid.append(row)
        for field,value in (("signal",11),("message","secret-canary")):
            row=process_exit();row["event"][field]=value;invalid.append(row)
        row=process_exit();del row["event"]["wait_status"];invalid.append(row)
        row=process_exit();del row["event"];invalid.append(row)
        for row in invalid:
            with self.subTest(row=row),self.assertRaises(ValueError):
                receiver.validate(row,"production")

    def test_wake_distribution_validates_counts_bounds_and_field_allowlist(self):
        row=distribution(1,[0,1,285000,9000000,receiver.WAKE_MAX_US])
        receiver.validate(row,"production")
        bad=[]
        for field,value in [("count",64),("zero_count",0),("max_us",receiver.WAKE_MAX_US+1),
                            ("sum_us",0),("mapping","unknown"),("bins",[[0,0]])]:
            copyrow=copy.deepcopy(row);copyrow["event"]["sketch"][field]=value;bad.append(copyrow)
        copyrow=copy.deepcopy(row);copyrow["event"]["sketch"]["message"]="private-canary";bad.append(copyrow)
        copyrow=copy.deepcopy(row);copyrow["event"]["last_us"]=0;bad.append(copyrow)
        copyrow=copy.deepcopy(row);copyrow["event"]["metric"]="audio-error";bad.append(copyrow)
        for invalid in bad:
            with self.subTest(invalid=invalid),self.assertRaises(ValueError):receiver.validate(invalid,"production")

    def test_wake_distribution_retry_merge_preserves_tail_and_context(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/"reports.sqlite"
            store=receiver.V2Store(path,"production")
            values=[100000+i*10000 for i in range(18)]+[4000000,9000000]
            first=distribution(1,values[:10]);second=distribution(2,values[10:])
            payload=batch(first,second)
            store.accept(payload);store.accept(payload)
            other=distribution(3,[1]);other["build"]="c"*64;store.accept(batch(other));store.close()
            summary=receiver.wake_summaries(path,"production")
            groups=summary["groups"];self.assertEqual(len(groups),2)
            measured=next(group for group in groups if group["build"]=="a"*64)
            self.assertEqual((measured["count"],measured["reports"],measured["max_us"]),(20,2,9000000))
            self.assertEqual(measured["sum_us"],sum(values))
            for q,name in [(.5,"p50_us"),(.95,"p95_us"),(.99,"p99_us")]:
                exact=values[int(q*(len(values)-1))]
                self.assertLessEqual(abs(measured[name]-exact),exact*.010001)

    def test_health_allowlist_units_bounds_and_private_fields(self):
        for metric,maximum in (("app-start",0),("first-frame",0),("screen-wake-requested",0),
                               ("input-to-present",86400000000),("resume-return-to-present",86400000000),
                               ("wifi-associated-observed",86400000000),("tailscale-ready-observed",86400000000),
                               ("badge-selected",0x4fff),("badge-selected-limit",1024),("audio-error",4095),("audio-recovery-succeeded",4095),("audio-recovery-exhausted",4095),
                               ("memory-resident-kib",1<<20),("memory-available-kib",1<<20),("heap-free-kib",1<<20),("heap-released-kib",1<<20)):
            receiver.validate(health(metric=metric,value=maximum),"production")
            with self.subTest(metric=metric),self.assertRaises(ValueError):
                receiver.validate(health(metric=metric,value=maximum+1),"production")
        invalid=[health(metric="audio-error",value=0)]
        invalid += [health(metric=name) for name in ("ssid","song-title","server-url",{},None)]
        invalid += [health(value=value) for value in (True,-1,"0",None)]
        row=health();row["event"]["message"]="private-canary";invalid.append(row)
        row=health();row["kind"]="crash";invalid.append(row)
        row=health();row["elapsed_us"]=1;invalid.append(row)
        row=health();row["runtime"]="kernel";invalid.append(row)
        for row in invalid:
            with self.assertRaises(ValueError):receiver.validate(row,"production")

    def test_bluetooth_keys_match_the_rust_struct(self):
        import re
        source = (Path(__file__).resolve().parents[2] / "crates/q2nd/src/ux/bluetooth.rs").read_text()
        start = source.index("pub struct Bluetooth {")
        body = source[start:source.index("\n}\n", start)]
        self.assertEqual(set(re.findall(r"pub (\w+):", body)), set(receiver.UX_BT))
        self.assertEqual(set(re.findall(r"pub (\w+): Option<", body)), receiver.UX_BT_NULLABLE)

    def test_ux_reports_accept_the_route_and_bluetooth_numbers_and_refuse_anything_else(self):
        bt = {key: 0 for key in receiver.UX_BT}
        bt.update(ldac_adaptive=True, flow_control=False, rssi=-7, tx_power=None, device_name="Generated Headphones",
                  device_class=0x240418, ldac_level=None)
        state = {**ux()["event"]["state"], "route": 3}
        receiver.validate(ux(state=state), "production")
        receiver.validate(ux(state=state, bluetooth=bt), "production")
        # Reports from builds before the UART fallback have no uart_fallbacks.
        receiver.validate(ux(state=state, bluetooth={k: v for k, v in bt.items() if k != "uart_fallbacks"}), "production")
        invalid = [ux(bluetooth={**bt, "uart_fallbacks": -1}),ux(state={**state, "route": 4}), ux(bluetooth={**bt, "rssi": -129}), ux(bluetooth={**bt, "codec": 6}),
                   ux(bluetooth={**bt, "device_name": "x" * 65}), ux(bluetooth={**bt, "device_name": "a\nb"}),
                   ux(bluetooth={**bt, "address": "generated"}), ux(bluetooth={**bt, "state": None}),
                   ux(bluetooth={k: v for k, v in bt.items() if k != "mtu"}), ux(bluetooth=[1])]
        for row in invalid:
            with self.assertRaises(ValueError):
                receiver.validate(row, "production")

    def test_ux_reports_fixed_codes_numeric_state_and_bounded_history(self):
        for what in ("error","recovered","journey","stop","stuck","problem"):
            receiver.validate(ux(what=what),"production")
        receiver.validate(ux(recent=[[0,0,0]]*64),"production")
        for metric in ("shuffle-to-audio","spoken-play-to-audio","page-open","info-open"):
            receiver.validate(health(metric=metric,value=50000),"production")
        state=ux()["event"]["state"]
        invalid=[ux(what="crash"),ux(code=65536),ux(recent=[[0,0,0]]*65),ux(recent=[[0,0]]),ux(recent=[["a",0,0]]),
                 ux(state={**state,"ssid":"x"}),ux(state={**state,"doze":1}),ux(state={**state,"wifi":3}),
                 ux(latency_us=86400000001),ux(message="private-canary")]
        row=ux();row["kind"]="health";invalid.append(row)
        row=ux();row["elapsed_us"]=1;invalid.append(row)
        for row in invalid:
            with self.assertRaises(ValueError):receiver.validate(row,"production")

    def test_memory_samples_are_bounded_numbers_with_nullable_counters(self):
        receiver.validate(memory(),"production")
        receiver.validate(memory(vmstat_delta=MEMORY_SAMPLE["vmstat"], tailscaled=MEMORY_SAMPLE["player"]),"production")
        player=MEMORY_SAMPLE["player"]
        heap=MEMORY_SAMPLE["heap"]
        invalid=[memory(read_us=-1),memory(player={**player,"comm":"q2nd"}),memory(player={**player,"vm_rss_kib":"x"}),
                 memory(heap={**heap,"art_images":None}),memory(meminfo={"mem_free_kib":1}),memory(extra=1),
                 memory(player={**player,"rss_kib":(1<<22)+1})]
        row=memory();row["kind"]="health";invalid.append(row)
        for row in invalid:
            with self.assertRaises(ValueError):receiver.validate(row,"production")

    def test_kernel_observation_requires_unknown_source_and_exact_collection_boot(self):
        row=kernel_observed();row["event"].update(records=32,bytes=1048576)
        receiver.validate(row,"production")
        for key,value in (("records",0),("records",33),("records",True),("bytes",1048577),("bytes",-1),
                          ("source_boot",row["boot"]),("source_context",{"stream":"production"}),
                          ("collecting_boot","22222222-2222-4222-8222-222222222222"),("reason","panic")):
            changed=kernel_observed();changed["event"][key]=value
            with self.subTest(key=key,value=value),self.assertRaises(ValueError):
                receiver.validate(changed,"production")
        for key in ("source_boot","source_context"):
            changed=kernel_observed();del changed["event"][key]
            with self.assertRaises(ValueError):receiver.validate(changed,"production")
        for key,value in (("kind","crash"),("runtime","process"),("poll",1)):
            changed=kernel_observed();changed[key]=value
            with self.assertRaises(ValueError):receiver.validate(changed,"production")

    def test_crash_context_retry_and_event_collision_are_atomic(self):
        store=self.stores["production"];row=crash()
        expected=store.accept(batch(row))
        self.assertEqual(store.accept(batch(row)),expected)
        changed=crash(event={"type":"fatal-signal","signal":11,"code":-6})
        with self.assertRaises(FileExistsError):store.accept(batch(report(2),changed))
        self.assertEqual(store.db.execute("SELECT COUNT(*) FROM reports").fetchone()[0],1)


class ReceiverWireControls(unittest.TestCase):
    def setUp(self):
        root=Path(__file__).resolve().parents[2]/"build"
        root.mkdir(exist_ok=True)
        self.temp=tempfile.TemporaryDirectory(prefix="receiver-wire-",dir=root)
        self.output=Path(self.temp.name)/"receiver"
        self.log=(Path(self.temp.name)/"process.log").open("wb")
        self.process=subprocess.Popen([sys.executable,str(Path(receiver.__file__)),"--out",str(self.output),"--port","0"],
            stdin=subprocess.DEVNULL,stdout=self.log,stderr=self.log)
        self.addCleanup(self.cleanup)
        until=time.monotonic()+5
        while not (self.output/"receiver.json").exists():
            self.assertIsNone(self.process.poll(),"generated receiver exited")
            self.assertLess(time.monotonic(),until,"generated receiver did not start")
            time.sleep(.01)
        metadata=json.loads((self.output/"receiver.json").read_text())
        self.address=metadata["address"]
        self.token=(self.output/"transport-token").read_text().strip()

    def cleanup(self):
        self.process.terminate()
        try:self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:self.process.kill();self.process.wait(timeout=5)
        self.log.close();self.temp.cleanup()

    def post(self,body,authorization=None,path="/v2/production/reports"):
        client=http.client.HTTPConnection(*self.address,timeout=5)
        try:
            client.request("POST",path,body=body,headers={"Authorization":authorization or "Bearer "+self.token})
            response=client.getresponse();return response.status,response.read()
        finally:client.close()

    def test_fresh_v2_only_metadata_and_routes_no_compatibility_store(self):
        metadata=json.loads((self.output/"receiver.json").read_text())
        self.assertEqual(metadata["schema"],2)
        self.assertEqual(metadata["stores"],receiver.storage_layout())
        self.assertFalse((self.output/"reports.sqlite").exists())
        self.assertFalse((self.output/"receiver-v2.json").exists())
        self.assertEqual(self.post(batch(report()),path="/v1/reports")[0],404)
        self.assertEqual(self.post(json.dumps({"schema":1,"reports":[report()]}).encode())[0],400)
        self.assertEqual(self.post(batch(report()),path="/v2/unknown/reports")[0],404)

    def test_runtime_and_crash_endpoints_are_isolated_with_exact_ack(self):
        for stream,number in (("production",1),("automated-test",2)):
            route=f"/v2/{stream}/reports"
            rows=[report(number,stream),crash(number+2,stream)]
            body=batch(*rows,stream=stream)
            expected={"schema":2,"stream":stream,"accepted":[row["id"] for row in rows]}
            code,reply=self.post(body,path=route)
            self.assertEqual((code,json.loads(reply)),(200,expected))
            self.assertEqual(self.post(body,path=route),(code,reply))
            other="automated-test" if stream=="production" else "production"
            self.assertEqual(self.post(body,path=f"/v2/{other}/reports")[0],400)
        self.process.terminate();self.process.wait(timeout=5)
        server,stores,lock=receiver.open_receiver(self.output,True)
        try:
            for stream,number in (("production",1),("automated-test",2)):
                self.assertEqual(stores[stream].db.execute("SELECT id FROM reports ORDER BY id").fetchall(),
                                 [(report(number)["id"],),(report(number+2)["id"],)])
        finally:
            server.server_close()
            for store in stores.values():store.close()
            os.close(lock)

    def test_process_exit_wire_retry_restart_and_collision(self):
        row=process_exit(1,"automated-test",143,"commanded-stop")
        route="/v2/automated-test/reports";body=batch(row,stream="automated-test")
        code,reply=self.post(body,path=route)
        self.assertEqual(code,200)
        self.assertEqual(json.loads(reply),dict(schema=2,stream="automated-test",accepted=[row["id"]]))
        self.assertEqual(self.post(body,path=route),(code,reply))
        self.assertEqual(self.post(body)[0],400)
        changed=process_exit(1,"automated-test",143,"unexpected-exit")
        self.assertEqual(self.post(batch(changed,stream="automated-test"),path=route)[0],409)
        self.process.terminate();self.process.wait(timeout=5)
        server,stores,lock=receiver.open_receiver(self.output,True)
        try:
            self.assertEqual(stores["automated-test"].accept(body),json.loads(reply))
            stored=json.loads(stores["automated-test"].db.execute("SELECT body FROM reports").fetchone()[0])
            self.assertEqual(stored,row)
            self.assertEqual(stores["production"].db.execute("SELECT COUNT(*) FROM reports").fetchone()[0],0)
        finally:
            server.server_close()
            for store in stores.values():store.close()
            os.close(lock)

    def test_wake_distribution_and_recovery_wire_retry(self):
        rows=[distribution(31,[200000,300000,5000000]),health(32,"production","audio-recovery-succeeded",9)]
        body=batch(*rows)
        first=self.post(body);self.assertEqual(first[0],200)
        self.assertEqual(self.post(body),first)
        invalid=copy.deepcopy(rows[0]);invalid["event"]["sketch"]["count"]+=1
        self.assertEqual(self.post(batch(invalid))[0],400)

    def test_health_and_kernel_observation_wire_exact_context_and_retry(self):
        for stream in ("production","automated-test"):
            rows=[health(10,stream,"badge-selected",0x2a67),kernel_observed(11,stream)]
            route=f"/v2/{stream}/reports";body=batch(*rows,stream=stream)
            code,reply=self.post(body,path=route)
            self.assertEqual(code,200)
            self.assertEqual(json.loads(reply),dict(schema=2,stream=stream,accepted=[r["id"] for r in rows]))
            self.assertEqual(self.post(body,path=route),(code,reply))
            other="automated-test" if stream=="production" else "production"
            self.assertEqual(self.post(body,path=f"/v2/{other}/reports")[0],400)
        self.process.terminate();self.process.wait(timeout=5)
        server,stores,lock=receiver.open_receiver(self.output,True)
        try:
            for stream in ("production","automated-test"):
                stored=[json.loads(row[0]) for row in stores[stream].db.execute("SELECT body FROM reports ORDER BY id")]
                self.assertEqual(stored,[health(10,stream,"badge-selected",0x2a67),kernel_observed(11,stream)])
        finally:
            server.server_close()
            for store in stores.values():store.close()
            os.close(lock)

    def test_restart_preserves_durable_context_and_byte_exact_ack(self):
        row=crash(1,"automated-test",{"type":"fatal-signal","signal":10,"code":-6})
        body=batch(row,stream="automated-test");route="/v2/automated-test/reports"
        expected=self.post(body,path=route)
        originals={n:(self.output/n).read_bytes() for n in ("receiver.json","transport-token")}
        self.process.terminate();self.process.wait(timeout=5)
        self.process=subprocess.Popen([sys.executable,str(Path(receiver.__file__)),"--out",str(self.output),"--resume"],
            stdin=subprocess.DEVNULL,stdout=self.log,stderr=self.log)
        until=time.monotonic()+5
        while True:
            try:actual=self.post(body,path=route);break
            except ConnectionRefusedError:
                self.assertIsNone(self.process.poll());self.assertLess(time.monotonic(),until);time.sleep(.01)
        self.assertEqual(actual,expected)
        for name,data in originals.items():self.assertEqual((self.output/name).read_bytes(),data)
        with self.assertRaises(BlockingIOError):receiver.open_receiver(self.output,True)

    def test_auth_byte_bounds_duplicate_fields_and_conflicts_leave_service_available(self):
        body=batch(report())
        self.assertEqual(self.post(body,"Bearer wrong")[0],401)
        self.assertEqual(self.post(body,"Bearer \xe9")[0],401)
        self.assertEqual(self.post(b"x"*(receiver.MAX_BODY+1))[0],413)
        self.assertEqual(self.post(b'{"schema":2,"schema":2,"reports":[]}')[0],400)
        self.assertEqual(self.post(body)[0],200)
        changed=report();changed["elapsed_us"]+=1
        self.assertEqual(self.post(batch(changed))[0],409)
        self.assertEqual(self.post(body)[0],200)

    def test_resume_refuses_missing_established_stream_without_recreating_evidence(self):
        self.process.terminate();self.process.wait(timeout=5)
        directory=self.output/"automated-test";database=directory/"reports.sqlite";saved=directory/"saved.sqlite"
        database.rename(saved)
        with self.assertRaises(ValueError):receiver.open_receiver(self.output,True)
        self.assertFalse(database.exists());saved.rename(database)
        directory.rename(self.output/"saved-test")
        with self.assertRaises(FileNotFoundError):receiver.open_receiver(self.output,True)
        self.assertFalse(directory.exists())

    def test_private_store_symlinks_hardlinks_and_metadata_drift_refused(self):
        self.process.terminate();self.process.wait(timeout=5)
        prod=self.output/"production";saved=self.output/"saved-production"
        prod.rename(saved);prod.symlink_to(saved.name)
        with self.assertRaises(ValueError):receiver.open_receiver(self.output,True)
        prod.unlink();saved.rename(prod)
        db=prod/"reports.sqlite";db.unlink();os.link(self.output/"automated-test/reports.sqlite",db)
        with self.assertRaises(ValueError):receiver.open_receiver(self.output,True)
        db.unlink()
        metadata=self.output/"receiver.json";value=json.loads(metadata.read_text());value["schema"]=1
        metadata.write_text(json.dumps(value))
        with self.assertRaises(ValueError):receiver.open_receiver(self.output,True)

    def test_resume_refuses_address_or_token_identity_change(self):
        self.process.terminate();self.process.wait(timeout=5)
        with self.assertRaises(ValueError):receiver.open_receiver(self.output,True,port=self.address[1]+1)
        token=self.output/"transport-token";saved=self.output/"saved-token"
        token.rename(saved);token.symlink_to(saved.name)
        with self.assertRaises(OSError):receiver.open_receiver(self.output,True)


if __name__ == "__main__":unittest.main()
