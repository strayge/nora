#!/usr/bin/env python3
# Copyright (c) 2026 strayge
# SPDX-License-Identifier: MIT

"""Exercise real clients against isolated temporary repositories, never user configuration."""

import argparse
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from zipfile import ZipFile, ZIP_DEFLATED


def wheel(name, version, dependency=None, value="fixture"):
    """Valid RECORD hashes keep client verification part of the acceptance check."""
    normalized = name.replace("-", "_")
    metadata = f"Metadata-Version: 2.3\nName: {name}\nVersion: {version}\nRequires-Python: >=3.8\n"
    if dependency:
        metadata += f"Requires-Dist: {dependency}\n"
    metadata = (metadata + "\n").encode()
    body = io.BytesIO()
    with ZipFile(body, "w", ZIP_DEFLATED) as archive:
        archive.writestr(f"{normalized}/__init__.py", f"VALUE = {value!r}\n")
        archive.writestr(f"{normalized}-{version}.dist-info/METADATA", metadata)
        archive.writestr(f"{normalized}-{version}.dist-info/WHEEL", "Wheel-Version: 1.0\nGenerator: nora-acceptance\nRoot-Is-Purelib: true\nTag: py3-none-any\n")
        record = f"{normalized}-{version}.dist-info/RECORD"
        rows = [f"{path},sha256={base64.urlsafe_b64encode(hashlib.sha256(archive.read(path)).digest()).rstrip(b'=').decode()},{len(archive.read(path))}\n" for path in archive.namelist()]
        archive.writestr(record, "".join(rows) + f"{record},,\n")
    return {"filename": f"{normalized}-{version}-py3-none-any.whl", "body": body.getvalue(), "metadata": metadata}


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/debug/nora"))
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="nora-repositories-clients-"))
    print(f"Acceptance directory: {root}", flush=True)
    env = {key: value for key, value in os.environ.items() if not key.upper().startswith(("NORA_", "PIP_", "NPM_CONFIG_", "TWINE_"))}
    env.update(PIP_CONFIG_FILE=os.devnull, PIP_DISABLE_PIP_VERSION_CHECK="1", TWINE_USERNAME="fixture", TWINE_PASSWORD="fixture", TWINE_NON_INTERACTIVE="1")
    empty = root / "empty-config"
    empty.write_text("")
    global_config=root / "empty-global-config"
    global_config.write_text("")
    env.update(NETRC=str(empty), PIP_KEYRING_PROVIDER="disabled")
    npm = ["npm", "--userconfig", str(empty), "--globalconfig", str(global_config), "--cache", str(root / "npm-cache"), "--ignore-scripts"]
    results = []

    def run(label, command, cwd=None, success=True):
        completed = subprocess.run(command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=90)
        (root / f"{label}.log").write_bytes(completed.stdout)
        if (completed.returncode == 0) != success:
            raise AssertionError(f"{label} returned {completed.returncode}: {completed.stdout.decode(errors='replace')}")
        results.append(label)
        return completed.stdout

    run("twine-version", [sys.executable, "-m", "twine", "--version"])
    packages = {name: wheel(name, "1.0", dependency) for name, dependency in [("nora-demo-root", "nora-demo-dep==1.0"), ("nora-demo-dep", None)]}
    npm_source = root / "npm-source"
    npm_source.mkdir()
    (npm_source / "package.json").write_text(json.dumps({"name": "nora-proxy-demo", "version": "3.0.0", "main": "index.js"}))
    (npm_source / "index.js").write_text("module.exports = 'proxy';\n")
    packed = json.loads(run("npm-pack", npm + ["pack", "--json"], npm_source))
    npm_body = (npm_source / packed[0]["filename"]).read_bytes()
    requests = []

    class Source(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            requests.append({"method": "GET", "path": self.path, "accept": self.headers.get("Accept")})
            parts = self.path.strip("/").split("/")
            if len(parts) == 2 and parts[0] in ("json", "html") and parts[1] in packages:
                package = packages[parts[1]]
                url = f"http://127.0.0.1:{self.server.server_port}/files/{package['filename']}"
                digest = hashlib.sha256(package["body"]).hexdigest()
                metadata_digest = hashlib.sha256(package["metadata"]).hexdigest()
                if parts[0] == "html":
                    if "json" in self.headers.get("Accept", ""):
                        self.send_response(406)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    body = f'<a href="{url}#sha256={digest}" data-core-metadata="sha256={metadata_digest}" data-requires-python="&gt;=3.8">{package["filename"]}</a>'.encode()
                    content_type = "text/html"
                else:
                    body = json.dumps({"meta": {"api-version": "1.4"}, "name": parts[1], "versions": ["1.0"], "files": [{"filename": package["filename"], "url": url, "hashes": {"sha256": digest}, "size": len(package["body"]), "requires-python": ">=3.8", "core-metadata": {"sha256": metadata_digest}}]}).encode()
                    content_type = "application/vnd.pypi.simple.v1+json"
            elif len(parts) == 2 and parts[0] == "files":
                package = next((value for value in packages.values() if parts[1] in (value["filename"], value["filename"] + ".metadata")), None)
                if package is None:
                    self.send_error(404)
                    return
                body = package["metadata"] if parts[1].endswith(".metadata") else package["body"]
                content_type = "application/octet-stream"
            elif parts == ["npm", "nora-proxy-demo"]:
                tarball = f"http://127.0.0.1:{self.server.server_port}/npm-files/nora-proxy-demo-3.0.0.tgz"
                body = json.dumps({"name": "nora-proxy-demo", "dist-tags": {"latest": "3.0.0"}, "versions": {"3.0.0": {"name": "nora-proxy-demo", "version": "3.0.0", "dist": {"tarball": tarball, "integrity": "sha512-" + base64.b64encode(hashlib.sha512(npm_body).digest()).decode(), "shasum": hashlib.sha1(npm_body).hexdigest()}}}}).encode()
                content_type = "application/json"
            elif parts == ["npm-files", "nora-proxy-demo-3.0.0.tgz"]:
                body, content_type = npm_body, "application/octet-stream"
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("ETag", '"' + hashlib.sha256(body).hexdigest() + '"')
            self.end_headers()
            self.wfile.write(body)

    source = ThreadingHTTPServer(("127.0.0.1", 0), Source)
    threading.Thread(target=source.serve_forever, daemon=True).start()
    port = unused_port()
    base = f"http://127.0.0.1:{port}"
    source_url = f"http://127.0.0.1:{source.server_port}"
    empty.write_text("".join(f"//127.0.0.1:{port}/{repo}/:_authToken=fixture\n" for repo in ("npm-a","npm-b")))
    config = root / "config.toml"
    text = f'[server]\nhost="127.0.0.1"\nport={port}\npublic_url="{base}"\nshutdown_timeout=10\n[storage]\nmode="local"\npath="{root / "data"}"\n[registries]\nenable=["npm","pypi"]\n'
    for name, fmt, kind, alias, extra in [
        ("npm-a", "npm", "local", "npm-a", ""), ("npm-b", "npm", "local", "npm-b", ""),
        ("npm-all", "npm", "group", "npm", 'members=["npm-a","npm-b"]\n'),
        ("python-a", "pypi", "local", "python-a", ""), ("python-b", "pypi", "local", "python-b", ""),
        ("python-all", "pypi", "group", "python", 'members=["python-a","python-b"]\n'),
        ("json", "pypi", "proxy", "json", f'upstream="{source_url}/json"\n'),
        ("html", "pypi", "proxy", "html", f'upstream="{source_url}/html"\n'),
        ("nosave", "pypi", "proxy", "nosave", f'upstream="{source_url}/json"\nsave_files=false\n'),
        ("json-all", "pypi", "group", "json-group", 'members=["json"]\n'),
        ("html-all", "pypi", "group", "html-group", 'members=["html"]\n'),
        ("npm-save", "npm", "proxy", "npm-save", f'upstream="{source_url}/npm"\n'),
        ("npm-nosave", "npm", "proxy", "npm-nosave", f'upstream="{source_url}/npm"\nsave_files=false\n'),
    ]:
        text += f'[repositories.{name}]\nformat="{fmt}"\nkind="{kind}"\npaths=["/{alias}/"]\n{extra}'
        if kind == "proxy":
            text += 'metadata_ttl=0\nmetadata_timeout=1\nconnect_timeout=1\nread_timeout=3\n'
    config.write_text(text)
    env.update(NORA_CONFIG_PATH=str(config), NORA_CONFIG_STRICT="true")
    process = None
    handles = []

    def start(label):
        nonlocal process
        log = (root / f"nora-{label}.log").open("wb")
        handles.append(log)
        process = subprocess.Popen([str(args.binary.resolve()), "serve"], env=env, stdout=log, stderr=subprocess.STDOUT)
        for _ in range(300):
            if process.poll() is not None:
                raise AssertionError(f"NORA exited: {(root / f'nora-{label}.log').read_text()[-5000:]}")
            try:
                urllib.request.urlopen(base + "/health", timeout=0.2).close()
                return
            except (urllib.error.URLError, TimeoutError):
                time.sleep(0.05)
        raise AssertionError("NORA startup timed out")

    def stop():
        nonlocal process
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait(timeout=18)
        process = None

    def wait_stored(repository, suffix):
        for _ in range(500):
            found = list((root / "data" / "repositories" / repository / "sources").glob("*/" + suffix))
            if found:
                return found
            time.sleep(0.02)
        raise AssertionError(f"No completed stored entry: {repository}/{suffix}")

    def pip_install(label, alias, package, dry=False, success=True):
        command = [sys.executable, "-m", "pip", "install", "--ignore-installed", "--no-cache-dir", "--no-input", "--target", str(root / label), "--index-url", f"{base}/{alias}/", "--report", str(root / f"{label}.json")]
        if dry:
            command.append("--dry-run")
        return run(label, command + [package], success=success)

    def npm_install(label, alias, package, expected):
        target = root / label
        target.mkdir()
        run(label, npm + ["--cache",str(root / (label + "-cache")),"install", "--no-audit", "--no-fund", "--registry", f"{base}/{alias}/", package], target)
        name, version = expected
        installed = json.loads((target / "node_modules" / name / "package.json").read_text())
        assert installed["version"] == version, installed

    def delete_coordinate(repository, name, version=None):
        """Cleanup uses repository identity, independently of package-client aliases."""
        path = f"{base}/api/v1/repositories/{repository}/packages/{urllib.parse.quote(name, safe='')}"
        if version is not None:
            path += "?" + urllib.parse.urlencode({"version": version})
        response = urllib.request.urlopen(urllib.request.Request(path, method="DELETE", headers={"HX-Request": "true"}))
        result = json.load(response)
        assert result["status"] in ("deleted", "already_absent"), result
        return result

    try:
        start("online")
        versions = {}
        for alias in ("json", "html"):
            pip_install(f"{alias}-metadata", f"{alias}-group", "nora-demo-root==1.0", dry=True)
            report = json.loads((root / f"{alias}-metadata.json").read_text())
            assert {entry["metadata"]["name"] for entry in report["install"]} == set(packages)
            index = json.load(urllib.request.urlopen(urllib.request.Request(f"{base}/{alias}-group/nora-demo-root/", headers={"Accept": "application/vnd.pypi.simple.v1+json"})))
            versions[alias] = index["meta"]["api-version"]
            assert "/-/source/" in index["files"][0]["url"] and index["files"][0]["core-metadata"]
            if alias == "html":
                assert versions[alias] == "1.0" and "size" not in index["files"][0]
        assert not [request for request in requests if request["path"].endswith(".whl")]
        metadata_gets = [request for request in requests if request["path"].endswith(".whl.metadata")]
        assert len(metadata_gets) == 4, metadata_gets
        summaries=json.load(urllib.request.urlopen(base + "/api/ui/repositories"))
        for alias in ("json","html"):
            summary=next(entry for entry in summaries if entry["name"]==alias)
            assert summary["stored_artifacts"]==0 and summary["stored_packages"]==0 and summary["stored_bytes"]>0,summary
        for alias in ("json", "html"):
            pip_install(f"{alias}-install", f"{alias}-group", "nora-demo-root==1.0")
            assert (root / f"{alias}-install/nora_demo_dep/__init__.py").is_file()
            for package in packages.values():
                wait_stored(alias, ".nora-pypi-ready/nora-demo-*/" + package["filename"] + ".json")
        pip_install("nosave-install", "nosave", "nora-demo-root==1.0")
        assert not list((root / "data/repositories/nosave/sources").glob("*/pypi/**/*.whl"))
        assert not list((root / "data/repositories/nosave/sources").glob("*/.nora-pypi-ready/**/*.json"))
        for repo, version, value in [("python-a", "1.0", "a"), ("python-b", "2.0", "b")]:
            built = wheel("nora-local-demo", version, value=value)
            file = root / built["filename"]
            file.write_bytes(built["body"])
            run(f"twine-{repo}", [sys.executable, "-m", "twine", "upload", "--config-file", str(empty), "--non-interactive", "--repository-url", f"{base}/{repo}/", str(file)])
        pip_install("local-group-install", "python", "nora-local-demo")
        assert "'a'" in (root / "local-group-install/nora_local_demo/__init__.py").read_text()
        for repo, version in [("npm-a", "1.0.0"), ("npm-b", "2.0.0")]:
            package_dir = root / f"publish-{repo}"
            package_dir.mkdir()
            (package_dir / "package.json").write_text(json.dumps({"name": "nora-local-demo", "version": version, "main": "index.js"}))
            (package_dir / "index.js").write_text(f"module.exports = {repo!r};\n")
            run(f"publish-{repo}", npm + ["publish", "--registry", f"{base}/{repo}/"], package_dir)
        assert json.loads(run("npm-version", npm + ["view", "nora-local-demo", "version", "--json", "--registry", base + "/npm/"])) == "1.0.0"
        tags = run("npm-tags", npm + ["dist-tag", "ls", "nora-local-demo", "--registry", base + "/npm/"]).decode()
        assert "latest: 1.0.0" in tags.splitlines(), tags
        npm_install("npm-group-install", "npm", "nora-local-demo", ("nora-local-demo", "1.0.0"))
        audit = run("npm-group-audit", npm + ["audit", "--json", "--registry", base + "/npm/"], root / "npm-group-install", success=False)
        assert b"not supported" in audit or b"501" in audit, audit
        assert delete_coordinate("npm-a", "nora-local-demo", "1.0.0")["package_removed"]
        assert json.loads(run("npm-other-local-after-delete", npm + ["view", "nora-local-demo", "version", "--json", "--registry", base + "/npm-b/"])) == "2.0.0"
        package_dir = root / "publish-npm-a"
        (package_dir / "index.js").write_text("module.exports = 'replacement';\n")
        run("npm-republish-deleted-version", npm + ["publish", "--registry", base + "/npm-a/"], package_dir)
        npm_install("npm-replacement-install", "npm-a", "nora-local-demo", ("nora-local-demo", "1.0.0"))
        assert "replacement" in (root / "npm-replacement-install/node_modules/nora-local-demo/index.js").read_text()
        assert delete_coordinate("python-a", "nora-local-demo", "1.0")["package_removed"]
        replacement = wheel("nora-local-demo", "1.0", value="replacement")
        file = root / replacement["filename"]
        file.write_bytes(replacement["body"])
        run("twine-reupload-deleted-filename", [sys.executable, "-m", "twine", "upload", "--config-file", str(empty), "--non-interactive", "--repository-url", f"{base}/python-a/", str(file)])
        pip_install("python-replacement-install", "python-a", "nora-local-demo==1.0")
        assert "replacement" in (root / "python-replacement-install/nora_local_demo/__init__.py").read_text()
        assert delete_coordinate("npm-a", "nora-local-demo")["package_removed"]
        assert delete_coordinate("python-a", "nora-local-demo")["package_removed"]
        assert delete_coordinate("npm-a", "nora-local-demo")["status"] == "already_absent"
        results.append("local-package-deletion-and-reupload")
        for repo in ("npm-save", "npm-nosave"):
            npm_install(repo + "-install", repo, "nora-proxy-demo", ("nora-proxy-demo", "3.0.0"))
        wait_stored("npm-save", "npm/nora-proxy-demo/tarballs/nora-proxy-demo-3.0.0.tgz")
        wait_stored("npm-save", ".nora-origins/npm/nora-proxy-demo/tarballs/nora-proxy-demo-3.0.0.tgz")
        assert not list((root / "data/repositories/npm-nosave/sources").glob("*/npm/**/*.tgz"))
        stop()
        source.shutdown()
        source.server_close()
        start("offline")
        started = time.monotonic()
        pip_install("offline-json", "json-group", "nora-demo-root==1.0")
        offline_seconds = time.monotonic() - started
        assert offline_seconds < 12, offline_seconds
        pip_install("offline-nosave", "nosave", "nora-demo-root==1.0", success=False)
        npm_install("offline-npm", "npm-save", "nora-proxy-demo", ("nora-proxy-demo", "3.0.0"))
        report = {"status": "PASS", "root": str(root), "base": base, "api_versions": versions, "metadata_only_source_wheel_gets": 0, "metadata_only_source_sidecar_gets": len(metadata_gets), "offline_pip_seconds": offline_seconds, "checks": results}
        (root / "result.json").write_text(json.dumps(report, indent=2))
        print(json.dumps(report), flush=True)
    finally:
        stop()
        source.shutdown()
        source.server_close()
        for handle in handles:
            handle.close()
        (root / "requests.json").write_text(json.dumps(requests, indent=2))


if __name__ == "__main__":
    main()
