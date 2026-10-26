#!/usr/bin/env python3
# Copyright (c) 2026 strayge
# SPDX-License-Identifier: MIT

"""Opt-in Docker CLI checks without external registries or changes to user configuration."""

import argparse
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import uuid


# This fixture-only hash authenticates smoke:smoke without a password-generation dependency.
PASSWORD_HASH = "$2y$04$q6BS5JX9UaAEm87KZ2WJ9OftM2GV44NHyPkjOX6pkZouzSgyCMIum"
RELAY_CODE = '''
import select
import socket
import socketserver
import sys
import threading

class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        peer = socket.create_connection(('host.docker.internal', self.server.target), timeout=10)
        peer.settimeout(None)
        try:
            readers = {self.request: peer, peer: self.request}
            while readers:
                ready = select.select(list(readers), [], [], 30)[0]
                if not ready:
                    break
                for source in ready:
                    target = readers[source]
                    data = source.recv(65536)
                    if data:
                        target.sendall(data)
                    else:
                        readers.pop(source)
                        try:
                            target.shutdown(socket.SHUT_WR)
                        except OSError:
                            pass
        finally:
            peer.close()

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

for index, target in enumerate(sys.argv[1:]):
    server = Server(('0.0.0.0', 18080 + index), Handler)
    server.target = int(target)
    threading.Thread(target=server.serve_forever, daemon=True).start()
threading.Event().wait()
'''


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def seed_image(request, port, image, architecture, identity):
    """An unseen layer forces a real download instead of reusing the daemon's image cache."""
    tar = io.BytesIO()
    payload = ("Nora Docker client fixture " + identity).encode()
    with tarfile.open(fileobj=tar, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        entry = tarfile.TarInfo("payload.txt")
        entry.size, entry.mode, entry.mtime = len(payload), 0o644, 0
        archive.addfile(entry, io.BytesIO(payload))
    layer = gzip.compress(tar.getvalue(), mtime=0)
    config = json.dumps({
        "architecture": architecture, "os": "linux",
        "config": {"Labels": {"nora.smoke": identity}},
        "rootfs": {"type": "layers", "diff_ids": [digest(tar.getvalue())]},
        "history": [{"created_by": "Nora Docker client fixture"}],
    }).encode()
    for data in (config, layer):
        status, _, headers = request(port, f"/v2/{image}/blobs/uploads/", "POST", b"")
        assert status == 202, status
        location = headers["Location"]
        status, body, _ = request(port, location + "?digest=" + digest(data), "PUT", data,
                                  "application/octet-stream")
        assert status == 201, (status, body)
    manifest = json.dumps({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": digest(config), "size": len(config)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": digest(layer), "size": len(layer)}],
    }).encode()
    status, body, _ = request(port, f"/v2/{image}/manifests/smoke", "PUT", manifest,
                              "application/vnd.oci.image.manifest.v1+json")
    assert status == 201, (status, body)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/nora"))
    parser.add_argument("--relay-image", help="already-cached Python 3 image for Docker Desktop loopback forwarding; never pulled")
    args = parser.parse_args()
    if not shutil.which("docker"):
        print(json.dumps({"status": "SKIP", "reason": "docker CLI is unavailable"}))
        return
    env = {key: value for key, value in os.environ.items() if not key.upper().startswith("NORA_")}
    host = env.get("DOCKER_HOST") if not env.get("DOCKER_CONTEXT") else None
    if not host:
        context = subprocess.run(["docker", "context", "inspect", "--format", "{{.Endpoints.docker.Host}}"],
                                 env=env, capture_output=True, text=True, timeout=15)
        if context.returncode:
            print(json.dumps({"status": "SKIP", "reason": "no usable Docker context", "detail": context.stderr.strip()}))
            return
        host = context.stdout.strip()
    if not host.startswith(("unix://", "npipe://")):
        print(json.dumps({"status": "SKIP", "reason": "a local Docker daemon is required"}))
        return
    client_env = env.copy()
    for key in ("DOCKER_CONTEXT", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"):
        client_env.pop(key, None)
    client_env["DOCKER_HOST"] = host
    try:
        info = subprocess.run(["docker", "info", "--format", "{{.Architecture}}"], env=client_env,
                              capture_output=True, text=True, timeout=15)
    except subprocess.TimeoutExpired:
        print(json.dumps({"status": "SKIP", "reason": "Docker daemon did not respond"}))
        return
    if info.returncode:
        print(json.dumps({"status": "SKIP", "reason": "Docker daemon is unavailable", "detail": info.stderr.strip()}))
        return
    architecture = {"aarch64": "arm64", "x86_64": "amd64"}.get(info.stdout.strip(), info.stdout.strip())
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"build Nora first: binary not found: {binary}")
    root = Path(tempfile.mkdtemp(prefix="nora-docker-client-"))
    print(f"Acceptance directory: {root}", flush=True)
    identity = uuid.uuid4().hex[:12]
    image = f"team/port-smoke-{identity}"
    reservations = [socket.socket() for _ in range(6 if args.relay_image else 3)]
    try:
        for listener in reservations:
            listener.bind(("127.0.0.1", 0))
        chosen = [listener.getsockname()[1] for listener in reservations]
    finally:
        for listener in reservations:
            listener.close()
    ports = chosen[:3]
    advertised = chosen[3:] if args.relay_image else ports
    authorities = [f"127.0.0.1:{port}" for port in advertised]
    tags = [f"{authority}/{image}:smoke" for authority in authorities]
    client_dir = root / "docker-client"
    client_dir.mkdir()
    authorization = "Basic " + base64.b64encode(b"smoke:smoke").decode()
    # Nonempty auths prevent Docker's automatic detection of a system credential store.
    (client_dir / "config.json").write_text(json.dumps({
        "auths": {authority: {"auth": authorization.removeprefix("Basic ")} for authority in authorities},
    }))
    client = ["docker", "--config", str(client_dir)]
    password_file = root / "users.htpasswd"
    password_file.write_text(f"smoke:{PASSWORD_HASH}\n")
    config = root / "config.toml"
    config.write_text(f'''[server]
host="127.0.0.1"
port={ports[0]}
public_url="http://127.0.0.1:{ports[0]}"
shutdown_timeout=2
[storage]
path={json.dumps(str(root / 'data'))}
[auth]
enabled=true
htpasswd_file={json.dumps(str(password_file))}
token_storage={json.dumps(str(root / 'tokens'))}
[registries]
enable=["docker"]
[curation]
mode="off"
[rate_limit]
enabled=false
[repositories.local]
format="docker"
kind="local"
public_url="http://{authorities[0]}"
[repositories.other]
format="docker"
kind="local"
port={ports[1]}
public_url="http://{authorities[1]}"
[repositories.cached]
format="docker"
kind="proxy"
port={ports[2]}
public_url="http://{authorities[2]}"
upstream="http://127.0.0.1:{ports[0]}"
auth="smoke:smoke"
''')
    env["NORA_CONFIG_PATH"] = str(config)
    process = None
    relay = None
    checks = []
    result = {"status": "FAIL", "root": str(root), "ports": ports, "advertised_ports": advertised, "checks": checks}
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    log = (root / "nora.log").open("wb")

    def command(arguments, label, *, check=True, input=None):
        completed = subprocess.run(arguments, cwd=root, env=client_env, input=input, text=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=90)
        (root / f"{label}.log").write_text(completed.stdout)
        if check and completed.returncode:
            raise AssertionError(f"{label}: {completed.stdout[-2000:]}")
        return completed

    def request(port, path, method="GET", body=None, content_type=None, *, authenticated=True):
        headers = {"Authorization": authorization} if authenticated else {}
        if content_type:
            headers["Content-Type"] = content_type
        req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=body, headers=headers, method=method)
        try:
            response = opener.open(req, timeout=10)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, response.read(), response.headers

    try:
        process = subprocess.Popen([str(binary), "serve"], cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 20
        while True:
            if process.poll() is not None:
                raise AssertionError("Nora exited; see nora.log")
            try:
                if request(ports[0], "/health", authenticated=False)[0] == 200:
                    break
            except (OSError, urllib.error.URLError):
                pass
            if time.monotonic() >= deadline:
                raise AssertionError("Nora readiness timeout")
            time.sleep(0.05)
        for port in ports:
            assert request(port, "/v2/", authenticated=False)[0] == 401, port
        if args.relay_image:
            command(client + ["image", "inspect", args.relay_image], "cached-relay-image")
            relay = f"nora-smoke-relay-{identity}"
            arguments = client + ["run", "--detach", "--rm", "--pull=never", "--name", relay,
                                  "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges",
                                  "--user=65534:65534", "--memory=64m", "--pids-limit=64"]
            for index, port in enumerate(advertised):
                arguments += ["-p", f"127.0.0.1:{port}:{18080 + index}"]
            arguments += [args.relay_image, "python", "-u", "-c", RELAY_CODE] + list(map(str, ports))
            command(arguments, "start-loopback-relay")
            deadline = time.monotonic() + 20
            while True:
                try:
                    if request(advertised[0], "/v2/", authenticated=False)[0] == 401:
                        break
                except (OSError, urllib.error.URLError):
                    pass
                if time.monotonic() >= deadline:
                    raise AssertionError("Docker Desktop relay readiness timeout")
                time.sleep(0.05)
        seed_image(request, ports[0], image, architecture, identity)
        assert request(ports[1], f"/v2/{image}/manifests/smoke")[0] == 404
        checks.append("local-repositories-start-isolated")
        for index, authority in enumerate(authorities):
            command(client + ["login", authority, "--username", "smoke", "--password-stdin"],
                    f"login-{index}", input="smoke\n")
        checks.append("authenticated-login-each-port")
        command(client + ["pull", tags[0]], "pull-fresh-local-image")
        inspection = json.loads(command(client + ["image", "inspect", tags[0]], "inspect-fresh-image").stdout)[0]
        assert inspection["Config"]["Labels"]["nora.smoke"] == identity
        checks.append("local-pull-fetches-fresh-valid-layer")
        command(client + ["tag", tags[0], tags[1]], "tag-other-local")
        command(client + ["push", tags[1]], "push-other-local")
        command(client + ["pull", tags[1]], "pull-other-local")
        assert request(ports[1], f"/v2/{image}/manifests/smoke")[0] == 200
        checks.append("other-port-local-push-and-pull")
        command(client + ["pull", tags[2]], "pull-proxy")
        checks.append("separate-port-proxy-pull")
        refused = command(client + ["push", tags[2]], "proxy-push-refused", check=False)
        assert refused.returncode != 0 and ("405" in refused.stdout or "method not allowed" in refused.stdout.lower()), refused.stdout
        checks.append("proxy-push-rejected-with-405")
        result["status"] = "PASS"
    except Exception as error:
        result["error"] = str(error)
        raise
    finally:
        cleanup_errors = []

        def cleanup(arguments, label):
            """One failed subprocess must not strand another fixture resource."""
            try:
                completed = command(arguments, label, check=False)
                if completed.returncode:
                    cleanup_errors.append(f"{label}: exited {completed.returncode}")
            except Exception as error:
                cleanup_errors.append(f"{label}: {error}")

        try:
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                    cleanup_errors.append("Nora exceeded its bounded shutdown")
            if process is not None and process.returncode not in (None, 0):
                cleanup_errors.append(f"Nora exited with {process.returncode}")
        except Exception as error:
            cleanup_errors.append(f"stop Nora: {error}")
        try:
            log.close()
        except Exception as error:
            cleanup_errors.append(f"close Nora log: {error}")
        if relay:
            try:
                cleanup(client + ["logs", relay], "relay-log")
            finally:
                cleanup(client + ["rm", "--force", relay], "cleanup-own-relay")
        cleanup(client + ["image", "rm"] + tags, "cleanup-own-tags")
        if cleanup_errors:
            result["status"] = "FAIL"
            result["cleanup_error"] = "; ".join(cleanup_errors)
        (root / "result.json").write_text(json.dumps(result, indent=2))
        print(json.dumps(result), flush=True)
    assert result["status"] == "PASS", result


if __name__ == "__main__":
    main()
