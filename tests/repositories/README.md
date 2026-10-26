# Named-repository acceptance

Docker listener, upstream-auth/cache isolation, restart/offline and retained-data
checks use only Python's standard library and local fake upstreams:

```sh
cargo build --locked -p nora-registry
python3 tests/repositories/docker_acceptance.py --binary target/debug/nora
```

The fixture uses isolated ports, storage and environment, stops its processes,
and retains logs plus `result.json` in the printed temporary evidence directory.
No Docker daemon, external registry or user daemon configuration is needed.

The real Docker CLI smoke is separate and explicitly opt-in:

```sh
python3 tests/repositories/docker_client_smoke.py --binary target/debug/nora
```

It prints `SKIP` when the CLI or a local daemon is unavailable. It checks
credentialed login, a fresh local image pull, push/pull on another local port,
proxy pull and rejected proxy writes. Only temporary client configuration and
unique fixture tags are used; its processes, tags and containers are cleaned up.
No external registry or daemon/user configuration changes are needed.

If Docker Desktop's VM cannot reach host-loopback listeners, use an
already-cached Python 3 image for a temporary, loopback-published relay:

```sh
python3 tests/repositories/docker_client_smoke.py --binary target/debug/nora \
  --relay-image python:3.15.1-alpine3.24
```

The relay image is never pulled. Logs and `result.json` remain in the printed
temporary evidence directory.

Build Nora, then run the self-contained localhost fixtures:

```sh
cargo build --locked -p nora-registry
python3 tests/repositories/client_acceptance.py --binary target/debug/nora
python3 tests/repositories/streaming_acceptance.py --binary target/debug/nora
```

The client check requires npm on `PATH`, plus pip and twine installed for the
chosen Python interpreter. It tests two-local publication, package-wide group
selection, JSON and HTML metadata-only resolution, dependency installation,
`save_files`, permanent local deletion/re-upload with replacement bytes, and real
restart/offline behavior. Client configurations, caches and
installation targets are temporary; user configuration and installations are not
modified. No external registry is needed.

The streaming check uses Python's standard library and `ps`. It transfers 128 MiB
artifacts and checks bounded RSS, true HEAD, cached Range, disconnect completion,
graceful drain, killed-fill recovery and bounded metadata fallback.

Each script stops its own Nora/source processes and prints a temporary evidence
directory containing logs and `result.json`. Evidence is retained for inspection.
The accepted client run used npm 11.20.0, pip 26.2.2, twine 7.0.0 and Python 3.15.

`smoke.toml` is an opt-in fixture for the older smoke, S3 and browser checks. Its
`/npm/` and `/simple/` aliases are read-only groups; publication uses `/npm-local/`
and `/simple-local/`. These aliases and public upstreams are never defaults.
For browser checks, see [`../e2e/README.md`](../e2e/README.md).
