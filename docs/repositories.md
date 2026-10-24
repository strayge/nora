# Named npm and PyPI repositories

npm and PyPI have no implicit repository or public upstream. Configure named
repositories, then point clients at one of their explicit aliases. The read-only
browser at `/ui/repositories` shows identities, aliases, stored inventory and
client commands. Other package formats keep their existing configuration.

The browser only observes stored state: it never refreshes a proxy or rebuilds
metadata. Group browsing applies current ownership/order to trusted stored package
metadata. Missing or stale negative proxy metadata cannot authorize fallback;
refresh through a package client endpoint before reopening such a page.

## Configuration

```toml
[server]
public_url = "https://packages.example.com"

[repositories.npm-local]
format = "npm"
kind = "local"
paths = ["/repos/npm-local/"]

[repositories.npm-public]
format = "npm"
kind = "proxy"
paths = ["/repos/npm-public/"]
upstream = "https://registry.npmjs.org"

[repositories.npm-all]
format = "npm"
kind = "group"
paths = ["/repos/npm/", "/npm/"]
members = ["npm-local", "npm-public"]
owners = { "@company/*" = "npm-local" }

[repositories.python-local]
format = "pypi"
kind = "local"
paths = ["/repos/python-local/"]

[repositories.python-public]
format = "pypi"
kind = "proxy"
paths = ["/repos/python-public/"]
upstream = "https://pypi.org/simple/"

[repositories.python-all]
format = "pypi"
kind = "group"
paths = ["/repos/python/", "/simple/"]
members = ["python-local", "python-public"]
owners = { "company-*" = "python-local" }
```

Names are stable lowercase identities using letters, digits, `-` and `_`.
Aliases must have leading and trailing slashes and must not overlap other aliases
or reserved service/format paths. Multiple aliases refer to the same repository;
they do not create copies. `/npm/` and `/simple/` above are optional explicit
aliases, not built-in defaults.

The general `[registries].enable` / `NORA_REGISTRIES_ENABLE` selection still
controls enabled formats. A configured repository whose format is disabled is a
configuration error. Existing `[curation.npm]` and `[curation.pypi]` settings still
apply. There are no per-repository ACLs: existing authentication, roles, OIDC
namespace restrictions and curation boundaries remain in force.

### Repository kinds

- **Local:** published contents only; never fetches packages into its namespace.
  Publish directly to a local alias.
- **Proxy:** exactly one upstream; read-only to package clients. A local and a
  proxy can contain the same package/version without overwriting each other.
- **Group:** flat, same-format, read-only composition of named locals/proxies.
  Groups have no separate artifact or metadata cache. Nested groups are rejected.

### Group selection and ownership

An exact owner wins, then the longest matching trailing-`*` prefix. An owner is
exclusive even if its package is absent or its source is unavailable. Without an
owner, members are tried in order; only confirmed absence tries the next member.
Authentication failures, malformed metadata, storage/integrity failures and
outages do not count as absence. An existing empty, quarantined or offline-filtered
project still selects its member.

Selection is package-wide: versions, tags, files and Core Metadata sidecars are
not merged. A missing version/file in the selected package does not reopen the
search. This deliberately replaces the former multi-upstream PyPI merging model.

Generated file links carry an opaque source binding. Each request rechecks current
ownership, order and source identity; old links can return 404 after a policy or
upstream change. A binding cannot select a client-chosen member. Direct member
aliases remain independently reachable; group ownership applies only through
that group.

### Proxy policies

All durations are integer seconds. Policies belong to the proxy member, including
when accessed through a group.

| Setting | Default | Meaning |
|---|---:|---|
| `upstream` | required | One HTTP(S) endpoint; no URL userinfo or fragment |
| `save_files` | `true` | Persist new artifact bodies and PyPI sidecars |
| `metadata_ttl` | `300` | Fresh metadata window; `0` revalidates each operation |
| `connect_timeout` | `5` | Connection establishment budget |
| `read_timeout` | `30` | Per-read stall limit |
| `metadata_timeout` | `10` | Absolute metadata operation budget, including redirects/parsing |
| `total_timeout` | `0` | Artifact total budget; `0` means no total cap while progressing |
| `tls_verify` | `true` | Certificate and hostname verification at the configured origin |
| `tls_ca_cert` | unset | Extra PEM trust for this proxy origin |
| `auth` | unset | Existing Basic `user:password` credential |
| `auth_bearer_env` | unset | npm-only environment variable containing a Bearer token |

Basic and Bearer credentials are mutually exclusive. Do not put credentials in
`upstream` or public aliases. Only the exact retired singleton environment keys
are rejected; a custom secret such as `NORA_NPM_VENDOR_TOKEN` is allowed as an
`auth_bearer_env` reference.

Global TLS roots apply to upstream clients. Repository-specific trust or disabled
verification applies only to the configured HTTPS origin, not to redirect/CDN
origins. Cross-origin redirects lose credentials; redirects cannot downgrade
HTTPS to HTTP. `tls_verify=false` is an explicit insecure exception and cannot be
combined with `tls_ca_cert`.

A finite artifact `total_timeout` can stop a long progressing download; a metadata
budget does not become an artifact total timeout. Repository/source breaker state
is isolated, even for two repositories using the same upstream URL.

## Client setup

Using the example configuration:

```sh
npm install example --registry https://packages.example.com/repos/npm/
npm publish --registry https://packages.example.com/repos/npm-local/
npm view example@1.0.0 --registry https://packages.example.com/repos/npm/

python -m pip install example --index-url https://packages.example.com/repos/python/
python -m twine upload --repository-url https://packages.example.com/repos/python-local/ dist/*
```

Do not publish to a group or proxy. npm group audit is explicitly unsupported
(501); an outage or unsupported audit is never reported as a clean audit. Proxy
audit uses the selected proxy's upstream and existing security filtering.

Configure `server.public_url` with the externally visible reverse-proxy base path,
if any. Protocol links and browser commands use that public base and the requested
alias, not a hard-coded `/npm/` or `/simple/` endpoint.

## Metadata, streaming and offline behavior

PyPI prefers Simple JSON and accepts real HTML fallback. It preserves
Requires-Python, yanked state/reasons, hash dictionaries, Core Metadata hints,
real upload timestamps, authoritative versions and project status. Unknown file
sizes produce honest API 1.0 JSON rather than invented PEP 700 fields or a HEAD
fanout. Core Metadata availability can still be represented in API 1.0.

Metadata snapshots and confirmed negatives persist across restarts. Refreshes are
coalesced; a caller disconnect does not cancel a PyPI shared refresh/fill. A stale
negative cannot hide an outage. Quarantine is not erased by a later 404.

PyPI artifacts stream through a bounded disk spool, with incremental hash checks.
Integrity/quarantine policies that require complete inspection run before release.
HEAD is a real HEAD operation, independent of artifact GET completion. Complete
GETs, HEAD, ranges, validators and advertised `.metadata` sidecars retain their
protocol distinctions; a partial response is not stored as a complete artifact.
Sidecars have their own hashes and remain bound to their current parent.

**Download completion is not cache readiness.** Persistence is best-effort;
readiness is published only after verified complete body/pin/size and current
source/policy checks. A client may receive valid bytes even when cache writing
fails. Browser “stored” inventory is not a promise of offline eligibility.
A graceful shutdown drains tracked fills up to its configured budget; SIGKILL or
a failed/as-yet-unfinished write does not guarantee retention.

`save_files=false` skips new persistent artifact/sidecar bodies and companions,
not metadata, validators, metadata provenance, security/first-seen records, inspection temps
or eligible retained reads. Changing the flag does not delete old files. A file
never saved remains unavailable offline. Local publishing is unaffected.

Fresh metadata may initially advertise uncached files before an outage is known;
the first request for a never-cached file can fail. Offline PyPI views filter to
eligible retained files after an observed outage, and suppress sidecar hints when
its own proof is unavailable. Metadata timeout choices bound stalled fallback;
increase them deliberately for a slow cold private index.

## Storage, maintenance and observability

Repository name and upstream identity select isolated namespaces, metadata,
locks/coalescing, pins and breaker state. Alias changes reuse state; changing a
name or upstream does not adopt another namespace. GC and retention operate on
current member namespaces, never on group copies or retained singleton roots.
Sidecars retire with their distribution coordinate and readiness proofs are
invalidated before destructive body deletion. Metadata-only sidecars are valid
without a cached wheel. Inactive source namespaces remain operator-managed.

Backups/backend copies preserve scoped bodies, pins and hidden source/status
records. Restore is not legacy adoption: old unscoped bytes remain unscoped.
The global storage footprint may include retained legacy/inactive bytes, while
npm/PyPI UI inventories and per-format gauges count current members only.
`nora_repository_artifacts` and `nora_repository_storage_bytes` use configured
repository/format labels, not package names or raw upstream URLs. Named publication
and refusal audit records include bounded repository identity; source IDs are
opaque, not credential-bearing endpoints.

## Converting a singleton configuration

A present but invalid or unreadable configuration file stops startup; it never
discards auth/storage settings by falling back to defaults. Only a missing optional
config file uses defaults. `[npm]`, `[pypi]` and their old singleton environment
controls are rejected with conversion guidance. Remove the old tables/variables, create explicit local/proxy
members and optional groups, then point clients at their aliases. There is no
compatibility mode, automatically created repository, legacy-adoption command or
automatic byte reclassification. Old npm/PyPI storage is retained for
operator-managed backup and cleanup, not exposed through a new default.

`nora import` refuses npm/PyPI targets because it has no explicit named local
target contract; publish with npm/twine instead. The old npm/yarn/pip `nora mirror`
commands likewise refuse implicit targets; use package clients with explicit
repository URLs. Import/mirror behavior for other formats is unchanged.
