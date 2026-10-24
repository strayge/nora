# Browser and protocol checks

From the repository root, build Nora and start an isolated test instance with the
explicit npm/PyPI local, proxy and group fixture:

```sh
cargo build -p nora-registry
NORA_CONFIG_PATH="$PWD/tests/repositories/smoke.toml" \
  NORA_HOST=127.0.0.1 \
  NORA_REGISTRIES_ENABLE=docker,maven,npm,cargo,pypi,go,raw,nuget,gems,terraform,ansible,pub,conan,rpm,deb \
  NORA_STORAGE_PATH="$(mktemp -d)" \
  NORA_PUBLIC_URL=http://localhost:4000 target/debug/nora serve
```

In another terminal:

```sh
cd tests/e2e
npm ci
npm test -- ui-contracts.spec.ts npm-proxy.spec.ts
```

Use an already installed Playwright browser, or install one separately with your
approval. `NORA_URL` selects a different running test instance. The fixture is for
tests only: it enables public upstream access and optional `/npm/` and `/simple/`
group aliases. Publication goes to `/npm-local/` or `/simple-local/`, never a group.

The repository browser tests cover named inventory and source selection; unchanged
formats retain their existing UI contracts. Optional pixel tests require reviewed
local baselines first (`npm test -- ui-screenshots.spec.ts --update-snapshots`). For real npm, pip and twine acceptance,
see [`../repositories/README.md`](../repositories/README.md).
