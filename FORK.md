# Fork differences

This fork of [upstream NORA](https://github.com/getnora-io/nora) focuses on isolated
npm/PyPI repositories and more reliable proxy, metadata and offline behavior.

## Main changes

- **Named Docker repositories.** Local and single-upstream proxy repositories use
  unique ports; an omitted port shares the main listener. Extra ports expose only
  the Docker API, with isolated images, caches and runtime state. See the
  [Docker guide](docs/docker-repositories.md).
- **Named npm and PyPI repositories.** Configure separate local, proxy and group
  repositories with explicit URL aliases. Contents and caches are isolated, even
  when repositories contain the same package/version or use the same upstream.
- **Package-wide groups.** Ordered members and ownership rules select one source
  for the whole package. Versions, tags and files are not merged; errors do not
  trigger fallback to another member. Groups have no separate cache.
- **Per-proxy policies.** Each proxy has its own upstream, metadata freshness,
  connection/read/operation timeouts, origin-scoped TLS settings and failure state.
  `save_files=false` disables new artifact persistence without deleting retained
  files or disabling metadata/security checks.
- **Richer PyPI metadata and downloads.** Preserve Requires-Python, yanked state,
  hashes, upload times, authoritative versions, project status and advertised Core
  Metadata sidecars. Downloads use bounded disk spooling, integrity checks, real
  HEAD requests, conditional requests and ranges.
- **Persistent metadata and explicit readiness.** PyPI snapshots survive restart;
  shared refreshes/fills survive caller disconnects. Offline serving depends on
  verified, retained files—not merely a previously successful download.
- **Repository-aware operations.** The browser at `/ui/repositories` shows aliases,
  stored inventory and client commands, with permanent package/release deletion
  for local npm/PyPI repositories. Deleted coordinates can be republished. Metrics,
  audit records and maintenance account for member repositories rather than group
  copies or legacy singleton storage.

## Compatibility and migration

The repository model changes apply to **npm, PyPI and Docker**; other formats
retain their existing repository configuration and storage backends.

- `[npm]`, `[pypi]`, `[docker]` and their retired singleton environment controls are rejected.
  There is no default npm/PyPI upstream or implicit endpoint. `/npm/` and `/simple/`
  work only when explicitly configured as aliases.
- Publish directly to a **local** repository. Proxies and groups are read-only;
  npm audit through a group returns an explicit unsupported response, not a clean
  audit result.
- Existing unscoped npm/PyPI/Docker bytes are not automatically adopted. Legacy and
  inactive namespaces remain available for operator-managed backup and cleanup,
  but are excluded from current repository inventory and destructive maintenance.
- `nora import` and the old npm/yarn/pip `nora mirror` targets do not provide a named
  local destination contract and are refused. Use npm/twine publication or package
  clients with explicit repository URLs instead. Backend migration remains
  supported; it does not reclassify legacy data.
- A present invalid or unreadable configuration file stops startup instead of
  silently falling back to defaults. This safety change applies to all formats.

For configuration examples, defaults and conversion steps, see the
[repository guide](docs/repositories.md). Existing authentication and curation
boundaries still apply; this fork does not add per-repository ACLs or repository
configuration CRUD.

## Builds and further reading

The README's release/image links and external documentation refer to upstream.
Do not assume upstream binaries or container images include these changes. To
build this checkout:

```sh
cargo build --locked --release -p nora-registry
```

- [Repository configuration and client setup](docs/repositories.md)
- [Protocol compatibility](COMPAT.md)
- [Real-client and streaming acceptance checks](tests/repositories/README.md)
