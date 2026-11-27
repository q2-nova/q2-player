#!/usr/bin/env python3
"""Run a generated Delta TLS venue against a native helper or an XBurst2 target helper."""
import argparse
import base64
from collections import Counter
import contextlib
import hashlib
import http.cookies
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
from socket import inet_aton
from socketserver import DatagramRequestHandler, UDPServer
import ssl
import struct
import subprocess
import sys
import threading
import time
from urllib.parse import parse_qs, urlsplit
from uuid import uuid4

PORTAL, SIGNIN = "wifi.delta.com", "signin.delta.com"
IP, VENUE = "192.0.2.10", "198.51.100.7"
USER, PASSWORD = "fixture-user", "fixture-%&password"
ACTIVATE = "/api/portableElectronicDevice/v1/flights/fixture-flight/devices/fixture-device/sessions"
VIDEO = (b'<video src="/video"></video><a href="/skip">Skip</a>'
         b'<input type="hidden" name="session" value="fixture-private-hidden-canary">')


class Venue(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, value=None, status=200, kind="application/json", cookie=None):
        if self.server.fixture.get("trace"):
            # Keep the generated WLAN responses from arriving in a single
            # burst while the trace writer synchronizes the fixture card.
            time.sleep(.04)
        body = value if isinstance(value, bytes) else (json.dumps(value).encode() if value else b"")
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.dispatch("GET")

    def do_POST(self):
        self.dispatch("POST")

    def dispatch(self, method):
        fixture = self.server.fixture
        counts = fixture["counts"]
        path = urlsplit(self.path)
        try:
            assert self.client_address[0] == IP
            host = self.headers["Host"]
            if not isinstance(self.connection, ssl.SSLSocket):
                assert (host, method, path.path) == ("detectportal.firefox.com", "GET", "/success.txt")
                assert fixture["premium"]
                counts["internet"] += 1
                return self.reply(b"success\n", kind="text/plain")
            cookie = http.cookies.SimpleCookie(self.headers.get("Cookie", ""))
            assert self.headers["Origin"] == "https://" + PORTAL
            assert self.headers["Cache-Control"] == "no-store"
            if host == PORTAL:
                assert "FLOW" not in cookie
            if host == SIGNIN:
                assert "XSRF-TOKEN" not in cookie
                assert self.headers["X-XSRF-Header"] == "PingFederate"
            if (host, method, path.path) == (PORTAL, "GET", "/api/device-details"):
                if counts["activation"]:
                    fixture["readbacks"].append(time.monotonic() - fixture["activated"])
                    fixture["premium"] = fixture["ordinary"] or (fixture["delayed"] and len(fixture["readbacks"]) >= 3)
                else:
                    counts["initial_device"] += 1
                return self.reply({
                    "deviceId": "fixture-device", "deviceLanIp": IP,
                    "devicePolicy": "DalPedPremium" if fixture["premium"] else "DalPedDefault",
                }, cookie="XSRF-TOKEN=fixture-csrf; Domain=.delta.com; Secure; Path=/")
            if (host, method, path.path) == (PORTAL, "GET", "/api/flight-data"):
                return self.reply({"flightId": "fixture-flight"})
            if (host, method, path.path) == (SIGNIN, "GET", "/as/authorization.oauth2"):
                assert counts["initial_device"] == 1
                counts["prompt"] += 1
                fields = parse_qs(path.query)
                assert fields["code_challenge_method"] == ["S256"]
                fixture["challenge"], = fields["code_challenge"]
                fixture["identity_secrets"].append(fixture["challenge"])
                return self.reply({"id": "fixture-flow", "status": "USERNAME_PASSWORD_REQUIRED"},
                                  cookie="FLOW=fixture-flow; Secure; HttpOnly; Path=/")
            data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if (host, method, path.path) == (SIGNIN, "POST", "/pf-ws/authn/flows/fixture-flow"):
                assert cookie["FLOW"].value == "fixture-flow"
                assert self.headers["Content-Type"] == "application/vnd.pingidentity.checkUsernamePassword+json"
                assert json.loads(data) == {"username": USER, "password": PASSWORD}
                counts["password"] += 1
                return self.reply({"status": "COMPLETED", "authorizeResponse": {"code": "fixture-code"}})
            if (host, method, path.path) == (SIGNIN, "POST", "/as/token.oauth2"):
                fields = parse_qs(data.decode())
                assert fields["code"] == ["fixture-code"]
                verifier, = fields["code_verifier"]
                fixture["identity_secrets"].append(verifier)
                digest = hashlib.sha256(verifier.encode()).digest()
                assert base64.urlsafe_b64encode(digest).decode().rstrip("=") == fixture["challenge"]
                counts["pkce"] += 1
                return self.reply({"expires_in": 3600, "access_token": "fixture-token"})
            if (host, method, path.path) == (PORTAL, "POST", ACTIVATE):
                assert counts["initial_device"] == 2
                assert self.headers["UserAuth"] == "fixture-token"
                assert self.headers["X-XSRF-TOKEN"] == cookie["XSRF-TOKEN"].value == "fixture-csrf"
                assert json.loads(data) == {"type": "freeWithSkyMiles"}
                counts["activation"] += 1
                fixture["activated"] = time.monotonic()
                if fixture["ordinary"]:
                    return self.reply(status=204)
                if fixture["json"]:
                    return self.reply({"accepted": True})
                return self.reply(VIDEO, kind="text/html; charset=utf-8")
            raise AssertionError("unexpected request")
        except (AssertionError, KeyError, ValueError):
            counts["unexpected"] += 1
            self.reply(status=500)


class FixtureDns(DatagramRequestHandler):
    """An isolated authoritative A reply for each generated venue hostname."""

    def handle(self):
        packet, sock = self.request
        if len(packet) < 17 or packet[4:6] != b"\0\1":
            return
        end = 12
        labels = []
        while end < len(packet) and 0 < packet[end] <= 63:
            size = packet[end]
            labels.append(packet[end + 1:end + size + 1])
            end += size + 1
        if end + 5 != len(packet) or packet[end] != 0:
            return
        hostname = b".".join(labels)
        known = hostname in (PORTAL.encode(), SIGNIN.encode(), b"detectportal.firefox.com")
        query_type, query_class = struct.unpack("!HH", packet[end + 1:])
        answer = known and (query_type, query_class) == (1, 1)
        flags = 0x8580 if known else 0x8583
        reply = packet[:2] + struct.pack("!HHHHH", flags, 1, int(answer), 0, 0) + packet[12:]
        if answer:
            self.server.hostnames.add(hostname.decode())
            reply += struct.pack("!HHHIH", 0xC00C, 1, 1, 0, 4) + inet_aton(VENUE)
        sock.sendto(reply, self.client_address)


def ordinary_login(binary, *, trusted, env):
    command = {"v": 1, "op": "begin_login", "ip": IP,
               "bssid": "02:00:00:00:00:01", "ssid": "DeltaWiFi.com"}
    with subprocess.Popen(
        [binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
    ) as process:
        try:
            process.stdin.write(json.dumps(command) + "\n")
            process.stdin.flush()
            assert select.select([process.stdout], [], [], 10)[0], "helper did not report its first state"
            first = json.loads(process.stdout.readline())
            if trusted:
                assert first == {"state": "credentials"}, first
                process.stdin.write(json.dumps({"op": "credentials", "user": USER, "password": PASSWORD}) + "\n")
                process.stdin.flush()
            else:
                assert first == {"state": "failed", "message": "Could not securely reach Delta’s sign-in service."}
            process.stdin.close()
            process.stdin = None
            stdout, stderr = process.communicate(timeout=15)
            assert process.returncode == 0 and not stderr
            if trusted:
                assert json.loads(stdout) == {"state": "online"}
            else:
                assert not stdout
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=4)


def assert_trace(attempt, case, network, configured):
    path = Path("/mnt/mmc/portal-debug") / attempt / "events.jsonl"
    raw = path.read_text()
    forbidden = {USER, PASSWORD, "fixture-token", "fixture-code",
                 "fixture-csrf", "FLOW=fixture-flow", "fixture-private-hidden-canary"}
    assert all(secret not in raw for secret in forbidden.union(network["identity_secrets"]))
    records = [json.loads(line) for line in raw.splitlines()]
    assert records[0]["kind"] == "start" and records[0]["attempt"] == attempt
    assert records[-1]["kind"] == "close" and records[-1]["dropped"] == 0
    assert [record["seq"] for record in records] == list(range(len(records)))
    last = [record["detail"] for record in records if record["kind"] == "terminal"]
    assert len(last) == 1 and last[0]["outcome"] == ("finished" if configured else "failed")

    def exchanges(event):
        return [record for record in records if record["kind"] == event]

    requests = exchanges("request")
    responses = exchanges("response")
    actions = [record["detail"]["action"] for record in exchanges("action")]
    if case == "untrusted":
        assert not responses and not actions
        assert len(requests) == len(exchanges("transport_error")) == len(exchanges("request_failed")) == 1
        transport, = exchanges("transport_error")
        assert transport["detail"]["detail"] in ("certificate", "tls")
        assert requests[0]["detail"]["http"]["url"]["route"] == "/api/device-details"
        return {"record_count": len(records), "requests": 1, "responses": 0}

    def for_route(events, route):
        return [record for record in events if record["detail"]["http"]["url"]["route"] == route]

    auth_request, = for_route(requests, "/as/authorization.oauth2")
    assert "code_challenge" in auth_request["detail"]["http"]["url"]["query_fields"]
    authorization, = for_route(responses, "/as/authorization.oauth2")
    fields = dict(authorization["detail"]["http"]["body"]["fields"])
    assert fields["status"]["state"] == "USERNAME_PASSWORD_REQUIRED"
    password, = for_route(requests, "/pf-ws/authn/flows/:flow")
    names = dict(password["detail"]["http"]["body"]["fields"])
    assert names["username"]["type"] == names["password"]["type"] == "string"
    token_request, = for_route(requests, "/as/token.oauth2")
    assert "code_verifier" in token_request["detail"]["http"]["body"]["fields"]
    token_response, = for_route(responses, "/as/token.oauth2")
    assert dict(token_response["detail"]["http"]["body"]["fields"])["access_token"] == {"type": "string"}
    activate_route = "/api/portableElectronicDevice/v1/flights/:flight/devices/:device/sessions"
    activation, = for_route(responses, activate_route)
    assert len(for_route(requests, activate_route)) == 1
    assert all(response["detail"]["n"] == request["detail"]["n"]
               for request, response in zip(requests, responses, strict=True))
    readbacks = [record["ms"] - activation["ms"] for record in for_route(responses, "/api/device-details")
                 if record["seq"] > activation["seq"]]
    assert len(readbacks) == len(network["readbacks"])
    assert all(abs(actual / 1000 - expected) < 2 for actual, expected in zip(readbacks, network["readbacks"]))
    if case == "trusted":
        action, = exchanges("action")
        assert actions == ["credentials_supplied"]
        assert authorization["seq"] < action["seq"] < password["seq"]
    if case.endswith("video"):
        projected = activation["detail"]["http"]["body"]
        assert ["video", 1] in projected["landmarks"]
        assert projected["controls"][0]["kind"] == "hidden"
        assert projected["links"][0]["caption"]["labels"] == ["skip"]
        assert any(node["tag"] == "video" for node in projected["outline"]["nodes"])
    if case == "gated-video":
        assert last[0]["reason"] == "Delta showed a video. This player still needs Wi-Fi access."
    states = [record["detail"]["state"] for record in exchanges("state")]
    assert ("online" in states) == configured
    return {"record_count": len(records), "requests": len(requests), "responses": len(responses),
            "readback_ms": readbacks}


def attempt_environment():
    attempt = str(uuid4())
    return attempt, dict(os.environ, Q2_PORTAL_ATTEMPT=attempt)


def isolated_venue(ordinary, private_dns, verify_trace):
    interfaces = json.loads(subprocess.check_output(["ip", "-j", "link"], timeout=4))
    assert [interface["ifname"] for interface in interfaces] == ["lo"]
    for args in (["link", "set", "lo", "name", "wlan0"], ["link", "set", "wlan0", "up"],
                 ["addr", "add", f"{IP}/32", "dev", "wlan0"],
                 ["addr", "add", f"{VENUE}/32", "dev", "wlan0"]):
        subprocess.run(["ip", *args], check=True, timeout=4)
    with contextlib.ExitStack() as stack:
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain("/fixture/server.pem", "/fixture/server.key")
        https = stack.enter_context(ThreadingHTTPServer((VENUE, 443), Venue))
        https.socket = tls.wrap_socket(https.socket, server_side=True)
        web = stack.enter_context(ThreadingHTTPServer((VENUE, 80), Venue))
        servers = [https, web]
        if private_dns:
            dns = stack.enter_context(UDPServer((VENUE, 53), FixtureDns))
            dns.hostnames = set()
            servers.append(dns)
        for server in servers:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)
        if ordinary:
            for trusted in (False, True):
                fixture = {"counts": Counter(), "ordinary": True, "premium": False, "readbacks": [],
                           "identity_secrets": [], "trace": verify_trace}
                https.fixture = web.fixture = fixture
                binary = "/fixture/q2-portal" if trusted else "/fixture/q2-portal-production"
                if private_dns:
                    dns.hostnames.clear()
                try:
                    attempt, env = attempt_environment()
                    ordinary_login(binary, trusted=trusted, env=env)
                except AssertionError:
                    if private_dns:
                        print("DNS response names:", sorted(dns.hostnames), flush=True)
                    print("Venue HTTP request counts:", dict(fixture["counts"]), flush=True)
                    raise
                expected = {"initial_device": 2, "prompt": 1, "password": 1, "pkce": 1,
                            "activation": 1, "internet": 1} if trusted else {}
                assert fixture["counts"] == expected
                assert len(fixture["readbacks"]) == (1 if trusted else 0)
                result = {"case": "trusted" if trusted else "untrusted", "counts": expected,
                          "state": "online" if trusted else "failed", "credentials_sent": trusted}
                if private_dns:
                    expected_hosts = {PORTAL, SIGNIN, "detectportal.firefox.com"} if trusted else {PORTAL}
                    assert dns.hostnames == expected_hosts
                    result["dns_a"] = sorted(dns.hostnames)
                if verify_trace:
                    result["trace"] = assert_trace(attempt, result["case"], fixture, trusted)
                print(json.dumps(result), flush=True)
            return
        for case in ("delayed-video", "gated-video", "delayed-json"):
            delayed = case.startswith("delayed")
            fixture = {"counts": Counter(), "delayed": delayed, "json": case.endswith("json"),
                       "ordinary": False, "premium": False, "readbacks": [], "identity_secrets": [], "trace": verify_trace}
            https.fixture = web.fixture = fixture
            command = {"v": 1, "op": "login", "user": USER, "password": PASSWORD, "ip": IP,
                       "bssid": "02:00:00:00:00:01", "ssid": "DeltaWiFi.com"}
            attempt, env = attempt_environment()
            result = subprocess.run(
                ["/fixture/q2-portal"], input=json.dumps(command) + "\n",
                capture_output=True, text=True, timeout=36, check=True, env=env,
            )
            assert not result.stderr
            expected = {"state": "online"} if delayed else {
                "state": "failed", "message": "Delta showed a video. This player still needs Wi-Fi access.",
            }
            assert json.loads(result.stdout) == expected
            counts = {"initial_device": 2, "prompt": 1, "password": 1, "pkce": 1, "activation": 1}
            if delayed:
                counts["internet"] = 1
            assert fixture["counts"] == counts
            readbacks = fixture["readbacks"]
            assert len(readbacks) == (3 if delayed else 5)
            assert readbacks[0] < 5
            gaps = [b - a for a, b in zip(readbacks, readbacks[1:])]
            assert all(expected - .1 <= gap <= expected + 3 for expected, gap in zip((2, 8, 10, 10), gaps))
            result = {"case": case, "counts": counts, "state": expected["state"],
                      "readback_seconds": [round(seconds, 3) for seconds in readbacks]}
            if verify_trace:
                result["trace"] = assert_trace(attempt, case, fixture, delayed)
            print(json.dumps(result), flush=True)


def launch(args):
    import common  # host side only; the sandboxed half runs without it
    args.out.mkdir()
    sources = {"helper": args.helper, "certificate": args.certificate, "venue": Path(__file__).resolve()}
    if args.build_inputs:
        sources["helper_build_inputs"] = args.build_inputs
    if args.ordinary:
        sources["production_helper"] = args.production_helper
    if args.qemu:
        sources["qemu"] = args.qemu
        for name in ("ld-linux-mipsn8.so.1", "libc.so.6", "libgcc_s.so.1"):
            sources[f"sysroot:{name}"] = args.sysroot / "lib" / name
    def identity(path):
        with path.open("rb") as stream:
            return hashlib.file_digest(stream, "sha256").hexdigest()
    hosts = args.out / "hosts"
    private_dns = bool(args.qemu and args.ordinary)
    if private_dns:
        hosts.write_text("127.0.0.1 localhost\n")
        resolver = args.out / "resolv.conf"
        resolver.write_text(f"nameserver {VENUE}\noptions no-aaaa attempts:1 timeout:1\n")
        sources["resolver"] = resolver
    else:
        hosts.write_text(f"{VENUE} {PORTAL} {SIGNIN} detectportal.firefox.com\n")
    status = args.out / "wpa_cli"
    status.write_text("#!/bin/sh\nprintf 'wpa_state=COMPLETED\\nssid=DeltaWiFi.com\\n"
                      f"bssid=02:00:00:00:00:01\\nip_address={IP}\\n'\n")
    status.chmod(0o700)
    sources.update(hosts=hosts, supplicant=status)
    argv = [
        "bwrap", "--unshare-user", "--uid", "0", "--gid", "0", "--unshare-net", "--unshare-pid",
        "--die-with-parent", "--cap-add", "CAP_NET_ADMIN", "--cap-add", "CAP_NET_BIND_SERVICE",
        *common.usr(), "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib",
        "--symlink", "usr/lib64", "/lib64", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
        "--tmpfs", "/run", "--dir", "/etc", "--dir", "/fixture", "--clearenv",
        "--setenv", "PATH", "/usr/bin:/usr/sbin",
    ]
    bindings = [
        (hosts, "/etc/hosts"), (status, "/usr/sbin/wpa_cli"),
        (args.certificate, "/fixture/server.pem"), (args.key, "/fixture/server.key"),
        (sources["venue"], "/fixture/venue.py"),
    ]
    helpers = [(args.helper, "q2-portal")]
    if args.ordinary:
        helpers.append((args.production_helper, "q2-portal-production"))
    if args.qemu:
        bindings.extend(((args.qemu, "/fixture/qemu"), (args.sysroot, "/fixture/sysroot"),
                         (hosts, "/fixture/sysroot/etc/hosts")))
        argv.extend(("--setenv", "QEMU_CPU", "XBurst2"))
        for binary, name in helpers:
            wrapper = args.out / name
            wrapper.write_text(f'#!/bin/sh\nexec /fixture/qemu -L /fixture/sysroot /fixture/{name}-elf "$@"\n')
            wrapper.chmod(0o700)
            bindings.extend(((binary, f"/fixture/{name}-elf"), (wrapper, f"/fixture/{name}")))
            sources[f"wrapper:{name}"] = wrapper
    else:
        bindings.extend((binary, f"/fixture/{name}") for binary, name in helpers)
    for local, guest in bindings:
        argv.extend(("--ro-bind", str(local.resolve()), guest))
    extra = ["--ordinary"] if args.ordinary else []
    if args.verify_trace:
        card = args.out / "generated-card"
        card.mkdir(mode=0o700)
        argv.extend(("--dir", "/mnt", "--bind", str(card.resolve()), "/mnt/mmc"))
        extra.append("--verify-trace")
    if private_dns:
        extra.append("--private-dns")
        argv.extend(("--ro-bind", str(resolver.resolve()), "/etc/resolv.conf",
                     "--tmpfs", "/fixture/sysroot/run",
                     "--ro-bind", str(resolver.resolve()), "/fixture/sysroot/run/resolv.conf"))
    identities = {key: identity(path) for key, path in sources.items()}
    log = args.out / "run.log"
    with log.open("x") as stream:
        try:
            result = subprocess.run([*argv, "/usr/bin/python3", "/fixture/venue.py", "--inside", *extra],
                                    stdout=stream, stderr=subprocess.STDOUT, timeout=62)
            status = result.returncode
        except subprocess.TimeoutExpired:
            stream.write("generated venue timed out\n")
            status = 124
    cases = [json.loads(line) for line in log.read_text().splitlines() if line.startswith('{"case":')]
    unchanged = identities == {key: identity(path) for key, path in sources.items()}
    expected = ["untrusted", "trusted"] if args.ordinary else ["delayed-video", "gated-video", "delayed-json"]
    passed = status == 0 and unchanged and [case["case"] for case in cases] == expected
    traces = {str(path.relative_to(args.out)): identity(path)
              for path in sorted((args.out / "generated-card").glob("portal-debug/*/events.jsonl"))}
    (args.out / "result.json").write_text(json.dumps({
        "passed": passed, "hardware": False, "inputs": identities, "iterations": cases,
        "exit_status": status, "traces": traces, "cpu": "XBurst2" if args.qemu else None, "inputs_unchanged": unchanged, "run_log_sha256": identity(log),
    }, indent=2) + "\n")
    return 0 if passed else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--ordinary", action="store_true", help="test trusted and untrusted ordinary sign-in")
    parser.add_argument("--private-dns", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--verify-trace", action="store_true", help="check trace files on a generated card")
    for option in ("helper", "certificate", "key", "out", "build-inputs", "production-helper", "qemu", "sysroot"):
        parser.add_argument(f"--{option}", type=Path)
    args = parser.parse_args()
    if args.inside:
        isolated_venue(args.ordinary, args.private_dns, args.verify_trace)
    else:
        if not all((args.helper, args.certificate, args.key, args.out)):
            parser.error("--helper, --certificate, --key and --out are required")
        if bool(args.qemu) != bool(args.sysroot):
            parser.error("--qemu and --sysroot are required together")
        if bool(args.production_helper) != args.ordinary:
            parser.error("--production-helper is required only for --ordinary")
        sys.exit(launch(args))
