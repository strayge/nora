# Named-repository acceptance

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
