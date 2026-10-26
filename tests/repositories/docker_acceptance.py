#!/usr/bin/env python3
# Copyright (c) 2026 strayge
# SPDX-License-Identifier: MIT

"""Check real listeners and repository isolation without a daemon or external registries."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def manifest(label):
    return json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [], "annotations": {"label": label}}).encode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/debug/nora"))
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    root = Path(tempfile.mkdtemp(prefix="nora-docker-repositories-"))
    print(f"Acceptance directory: {root}", flush=True)
    ports = []
    while len(ports) < 4:
        port = unused_port()
        if port not in ports:
            ports.append(port)
    main_port, local_port, cache_one, cache_two = ports
    env = {key: value for key, value in os.environ.items() if not key.upper().startswith("NORA_")}
    version = ["v1"]
    started = threading.Event()
    release = threading.Event()
    blob = b"private-container-layer"
    digest = "sha256:" + hashlib.sha256(blob).hexdigest()
    checks = []
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    class Source(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def reply(self, status, body=b"", **headers):
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            for key, value in headers.items():
                self.send_header(key.replace("_", "-"), value)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            authorization = self.headers.get("Authorization", "")
            if self.path.startswith("/token?"):
                for identity in ["one", "two"]:
                    expected = "Basic " + base64.b64encode(f"user:{identity}".encode()).decode()
                    if authorization == expected:
                        return self.reply(200, json.dumps({"token": identity}).encode())
                return self.reply(403)
            if not authorization.startswith("Bearer "):
                return self.reply(401, WWW_Authenticate=f'Bearer realm="http://127.0.0.1:{self.server.server_port}/token",service="fixture"')
            identity = authorization.removeprefix("Bearer ")
            if self.path == "/v2/app/manifests/latest":
                return self.reply(200, manifest(f"{identity}:{version[0]}"), Content_Type="application/vnd.oci.image.index.v1+json")
            if self.path.endswith(f"/blobs/{digest}"):
                if identity != "one":
                    return self.reply(403)
                self.send_response(200)
                self.send_header("Content-Length", str(len(blob)))
                self.end_headers()
                started.set()
                release.wait(10)
                self.wfile.write(blob)
                return
            return self.reply(404)

    source = ThreadingHTTPServer(("127.0.0.1", 0), Source)
    source.daemon_threads = True
    threading.Thread(target=source.serve_forever, daemon=True).start()
    config = root / "config.toml"
    storage = root / "storage"
    config.write_text(f'''[server]
host = "127.0.0.1"
port = {main_port}
public_url = "http://127.0.0.1:{main_port}"
shutdown_timeout = 2
[storage]
path = "{storage}"
[registries]
enable = ["docker"]
[curation]
mode = "off"
[gc]
grace_secs = 0
[[retention.rules]]
registry = "docker"
keep_last = 0
[repositories.local]
format = "docker"
kind = "local"
[repositories.other]
format = "docker"
kind = "local"
port = {local_port}
public_url = "https://images.example.invalid:9443"
[repositories.one]
format = "docker"
kind = "proxy"
port = {cache_one}
upstream = "http://127.0.0.1:{source.server_port}"
auth = "user:one"
[repositories.two]
format = "docker"
kind = "proxy"
port = {cache_two}
upstream = "http://127.0.0.1:{source.server_port}"
auth = "user:two"
''')
    env["NORA_CONFIG_PATH"] = str(config)
    legacy_blob = storage / "docker/legacy/blobs" / digest
    legacy_blob.parent.mkdir(parents=True)
    legacy_blob.write_bytes(blob)
    legacy_manifest = storage / "docker/legacy/manifests/old.json"
    legacy_manifest.parent.mkdir(parents=True)
    legacy_manifest.write_bytes(manifest("retained"))
    process = None
    log = None

    def request(port, path, method="GET", body=None, headers=None):
        req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=body, headers=headers or {}, method=method)
        try:
            response = opener.open(req, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, response.read(), response.headers

    def start(label):
        nonlocal process, log
        log = (root / f"{label}.log").open("wb")
        process = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise AssertionError(f"NORA exited {process.returncode}; see {label}.log")
            try:
                if request(main_port, "/health")[0] == 200:
                    return
            except (OSError, urllib.error.URLError):
                time.sleep(0.05)
        raise AssertionError("readiness timeout")

    def stop():
        nonlocal process, log
        if process is not None:
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise AssertionError("shutdown did not finish within the bounded drain")
            if process.returncode != 0:
                raise AssertionError(f"NORA shutdown failed: {process.returncode}")
            process = None
        if log is not None:
            log.close()
            log = None

    try:
        start("serve")
        for port, label in [(main_port, "local"), (local_port, "other")]:
            for image in ["app", "docker.io/app"]:
                body = manifest(f"{label}/{image}")
                path = f"/v2/{image}/manifests/latest"
                assert request(port, path, "PUT", body)[0] == 201
                status, stored, headers = request(port, path)
                assert status == 200 and stored == body
                assert headers["Content-Security-Policy"] == "sandbox"
        checks.append("same-image-tag-and-dotted-name-isolation")
        for path in ["/health", "/metrics", "/ui/", "/api/ui/stats"]:
            assert request(local_port, path)[0] == 404
        checks.append("extra-ports-docker-only-and-sandboxed")
        for port, identity in [(cache_one, "one"), (cache_two, "two")]:
            assert request(port, "/v2/app/manifests/latest")[1] == manifest(f"{identity}:v1")
            assert request(port, "/v2/app/manifests/latest", "PUT", manifest("denied"))[0] == 405
        version[0] = "v2"
        assert request(cache_one, "/v2/app/manifests/latest")[1] == manifest("one:v2")
        checks.append("separate-upstream-token-caches-and-tag-revalidation")
        with ThreadPoolExecutor(max_workers=1) as pool:
            first = pool.submit(request, cache_one, f"/v2/app/blobs/{digest}")
            assert started.wait(5), "first fill did not reach upstream"
            try:
                denied = request(cache_two, f"/v2/app/blobs/{digest}")
                assert denied[0] in (403, 404), denied[0]
            finally:
                release.set()
            assert first.result(timeout=5)[1] == blob
        checks.append("private-blob-fills-not-shared-across-repositories")
        status, data, _ = request(main_port, "/api/ui/repositories/other/package/docker.io/app")
        assert status == 200 and b"images.example.invalid:9443/docker.io/app:latest" in data
        assert request(main_port, "/api/ui/docker")[0] != 200
        checks.append("named-ui-and-advertised-endpoint")
        # Wait for the existing asynchronous cache commit before testing restart/offline.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and not list(storage.glob("repositories/one/sources/*/docker/app/blobs/*")):
            time.sleep(0.02)
        stop()
        source.shutdown()
        source.server_close()
        start("restart-offline")
        assert request(main_port, "/v2/app/manifests/latest")[1] == manifest("local/app")
        assert request(cache_one, f"/v2/app/blobs/{digest}")[1] == blob
        stop()
        checks.append("restart-and-offline-serving")
        for command in [["gc", "--apply"], ["retention-apply", "--yes"]]:
            completed = subprocess.run([binary, *command], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
            (root / f"{command[0]}.log").write_bytes(completed.stdout)
            assert completed.returncode == 0, completed.stdout.decode(errors="replace")
        assert legacy_blob.read_bytes() == blob
        assert legacy_manifest.read_bytes() == manifest("retained")
        checks.append("legacy-data-survives-destructive-maintenance")
        original = config.read_text()
        config.write_text(original.replace(f"port = {local_port}\n", f"port = {main_port}\n"))
        duplicate = subprocess.run([binary], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
        (root / "duplicate-port.log").write_bytes(duplicate.stdout)
        assert duplicate.returncode != 0 and b"conflicts" in duplicate.stdout
        config.write_text(original)
        checks.append("implicit-explicit-main-port-conflict-rejected")
        with socket.socket() as occupied:
            occupied.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            occupied.bind(("127.0.0.1", local_port))
            occupied.listen()
            failed = subprocess.run([binary], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
            (root / "bind-failure.log").write_bytes(failed.stdout)
            assert failed.returncode != 0 and b"other" in failed.stdout
            for port in [main_port, cache_one, cache_two]:
                with socket.socket() as probe:
                    probe.settimeout(0.5)
                    assert probe.connect_ex(("127.0.0.1", port)) != 0
        checks.append("bind-failure-stops-all-listeners")
        result = {"passed": True, "checks": checks}
        (root / "result.json").write_text(json.dumps(result, indent=2))
        print(json.dumps(result), flush=True)
    finally:
        release.set()
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        if log:
            log.close()
        source.shutdown()
        source.server_close()


if __name__ == "__main__":
    main()
