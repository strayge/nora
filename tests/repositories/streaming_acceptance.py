#!/usr/bin/env python3
# Copyright (c) 2026 strayge
# SPDX-License-Identifier: MIT

"""Measure a real process: bounded streaming, stalled fallback, graceful drain and SIGKILL recovery."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/debug/nora"))
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="nora-repositories-streaming-"))
    print(f"Acceptance directory: {root}", flush=True)
    chunk = bytes(range(256)) * 256
    chunks = 2048
    size = len(chunk) * chunks
    hasher = hashlib.sha256()
    for _ in range(chunks):
        hasher.update(chunk)
    digest = hasher.hexdigest()
    files = [f"stream_demo-{version}.0-py3-none-any.whl" for version in range(1, 5)]
    file_gets = []
    stalled_attempts=[]
    controls = {"stall": False, "files_down": False}

    class Source(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_HEAD(self):
            self.respond(head=True)

        def do_GET(self):
            self.respond(head=False)

        def respond(self, head):
            try:
                if self.path == "/simple/stream-demo/":
                    if controls["stall"]:
                        stalled_attempts.append(time.monotonic())
                        time.sleep(6)
                    body = json.dumps({"meta": {"api-version": "1.4"}, "name": "stream-demo", "versions": [f"{i}.0" for i in range(1, 5)], "files": [{"filename": name, "url": f"http://127.0.0.1:{self.server.server_port}/files/{name}", "hashes": {"sha256": digest}, "size": size} for name in files]}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/vnd.pypi.simple.v1+json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    if not head:
                        self.wfile.write(body)
                    return
                name = self.path.removeprefix("/files/")
                if name not in files:
                    self.send_error(404)
                    return
                if controls["files_down"]:
                    self.send_error(503)
                    return
                if not head:
                    file_gets.append(name)
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(size))
                self.send_header("ETag", f'"{digest}"')
                self.end_headers()
                if not head:
                    for _ in range(chunks):
                        self.wfile.write(chunk)
                        time.sleep(0.001)
            except (BrokenPipeError, ConnectionResetError):
                pass

    source = ThreadingHTTPServer(("127.0.0.1", 0), Source)
    threading.Thread(target=source.serve_forever, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    config = root / "config.toml"
    config.write_text(f'[server]\nhost="127.0.0.1"\nport={port}\npublic_url="{base}"\nshutdown_timeout=10\n[storage]\npath="{root / "data"}"\n[registries]\nenable=["pypi"]\n[repositories.proxy]\nformat="pypi"\nkind="proxy"\npaths=["/proxy/"]\nupstream="http://127.0.0.1:{source.server_port}/simple"\nmetadata_ttl=0\nmetadata_timeout=1\nread_timeout=3\n[repositories.all]\nformat="pypi"\nkind="group"\npaths=["/group/"]\nmembers=["proxy"]\n')
    env = {key: value for key, value in os.environ.items() if not key.startswith("NORA_")}
    env.update(NORA_CONFIG_PATH=str(config), NORA_CONFIG_STRICT="true")
    process = None
    logs = []

    def start(label):
        nonlocal process
        log = (root / f"nora-{label}.log").open("wb")
        logs.append(log)
        process = subprocess.Popen([str(args.binary.resolve()), "serve"], env=env, stdout=log, stderr=subprocess.STDOUT)
        for _ in range(300):
            if process.poll() is not None:
                raise AssertionError((root / f"nora-{label}.log").read_text()[-4000:])
            try:
                urllib.request.urlopen(base + "/health", timeout=0.2).close()
                return
            except (urllib.error.URLError, TimeoutError):
                time.sleep(0.05)
        raise AssertionError("server startup timed out")

    def stop(kill=False):
        nonlocal process
        if process is not None and process.poll() is None:
            process.kill() if kill else process.terminate()
            process.wait(timeout=16)
        process = None

    def ready(name):
        return list((root / "data/repositories/proxy/sources").glob(f"*/.nora-pypi-ready/stream-demo/{name}.json"))

    def wait_ready(name):
        for _ in range(1500):
            found = ready(name)
            if found:
                proof = json.loads(found[0].read_text())
                assert proof["size"] == size and proof["hashes"]["sha256"] == digest
                return
            time.sleep(0.02)
        raise AssertionError(f"no complete readiness proof: {name}")

    def rss():
        return int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(process.pid)]).strip())

    def consume(url, first=None, output=None):
        try:
            count = 0
            hasher = hashlib.sha256()
            with urllib.request.urlopen(url, timeout=20) as response:
                while data := response.read(64 * 1024):
                    count += len(data)
                    hasher.update(data)
                    if first:
                        first.set()
            assert count == size and hasher.hexdigest() == digest, (count, size)
            if output is not None:
                output.append("complete")
        except Exception as error:
            if output is not None:
                output.append(repr(error))
            else:
                raise

    try:
        start("online")
        index = json.load(urllib.request.urlopen(urllib.request.Request(base + "/group/stream-demo/", headers={"Accept": "application/vnd.pypi.simple.v1+json"})))
        urls = {file["filename"]: file["url"] for file in index["files"]}
        with urllib.request.urlopen(urllib.request.Request(urls[files[0]], method="HEAD")) as response:
            assert response.status == 200 and int(response.headers["Content-Length"]) == size
        assert not file_gets and not ready(files[0])
        baseline = rss()
        samples = [baseline]
        sampling = threading.Event()

        def sample():
            while not sampling.wait(0.05):
                samples.append(rss())

        sampler = threading.Thread(target=sample)
        sampler.start()
        consume(urls[files[0]])
        sampling.set()
        sampler.join()
        wait_ready(files[0])
        peak = max(samples)
        assert peak - baseline < 64 * 1024, (baseline, peak, size)
        before = len(file_gets)
        with urllib.request.urlopen(urllib.request.Request(urls[files[0]], headers={"Range": "bytes=100-199"})) as response:
            assert response.status == 206 and response.read() == chunk[100:200]
        assert len(file_gets) == before
        response = urllib.request.urlopen(urls[files[1]])
        assert response.read(1024)
        response.close()
        wait_ready(files[1])
        first = threading.Event()
        graceful_result = []
        client = threading.Thread(target=consume, args=(urls[files[2]], first, graceful_result))
        client.start()
        assert first.wait(10)
        stop()
        client.join(20)
        assert graceful_result == ["complete"], graceful_result
        wait_ready(files[2])
        start("before-kill")
        first = threading.Event()
        killed_result = []
        client = threading.Thread(target=consume, args=(urls[files[3]], first, killed_result))
        client.start()
        assert first.wait(10)
        stop(kill=True)
        client.join(20)
        assert killed_result and killed_result != ["complete"], killed_result
        assert not ready(files[3])
        orphan_paths = list((root / "data/tmp/pypi-proxy").glob("*"))
        assert orphan_paths, "SIGKILL fixture did not leave an orphan spool"
        for orphan in orphan_paths:
            os.utime(orphan, (time.time() - 5 * 3600,) * 2)
        controls["files_down"] = True
        start("after-kill")
        for _ in range(100):
            if all(not orphan.exists() for orphan in orphan_paths):
                break
            time.sleep(0.05)
        assert all(not orphan.exists() for orphan in orphan_paths), orphan_paths
        try:
            urllib.request.urlopen(urls[files[3]], timeout=10)
        except urllib.error.HTTPError as error:
            assert error.code >= 500, error.code
        else:
            raise AssertionError("killed partial fill became ready")
        controls["stall"] = True
        stop()
        start("stalled")
        began = time.monotonic()
        offline = json.load(urllib.request.urlopen(urllib.request.Request(base + "/group/stream-demo/", headers={"Accept": "application/vnd.pypi.simple.v1+json"}), timeout=5))
        elapsed = time.monotonic() - began
        assert elapsed < 2.5 and stalled_attempts, (elapsed,stalled_attempts)
        assert {file["filename"] for file in offline["files"]} == set(files[:3]), offline
        with urllib.request.urlopen(urllib.request.Request(urls[files[0]], headers={"Range": "bytes=100-199"}), timeout=5) as response:
            assert response.status == 206 and response.read() == chunk[100:200]
        report = {"status": "PASS", "root": str(root), "artifact_bytes": size, "baseline_rss_kib": baseline, "peak_rss_kib": peak, "rss_growth_kib": peak - baseline, "stalled_metadata_fallback_seconds": elapsed, "disconnect_completed": True, "graceful_completed": True, "killed_fill_not_ready": True, "orphan_paths_cleaned": len(orphan_paths), "source_file_gets": file_gets}
        (root / "result.json").write_text(json.dumps(report, indent=2))
        print(json.dumps(report), flush=True)
    finally:
        stop()
        source.shutdown()
        source.server_close()
        for log in logs:
            log.close()


if __name__ == "__main__":
    main()
