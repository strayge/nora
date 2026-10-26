# Named Docker repositories

Configure each Docker/OCI endpoint as a named local or proxy repository. Local
repositories support push/pull; proxies cache one upstream and reject writes.
Docker groups and image-prefix-based upstream routing are not supported.

```toml
[server]
host = "0.0.0.0"
port = 4000
public_url = "https://nora.example.com"

[repositories.images]
format = "docker"
kind = "local"
# Omitted port shares NORA's main listener.

[repositories.docker-hub]
format = "docker"
kind = "proxy"
port = 19443
upstream = "https://registry-1.docker.io"
# Optional Basic credentials:
# auth = "username:password"
```

```sh
docker tag myapp:latest nora.example.com/myapp:latest
docker push nora.example.com/myapp:latest
docker pull nora.example.com:19443/library/nginx:latest
```

These HTTPS examples assume a TLS-terminating proxy. NORA's listeners use HTTP;
direct HTTP needs the Docker client's insecure-registry configuration. A port
number does not enable TLS. For a container, publish every configured extra port,
for example `-p 4000:4000 -p 19443:19443`, and route `/v2/` at each public endpoint.

## Ports and public addresses

An omitted `port` uses the effective `server.port`, including `NORA_PORT`
overrides. No two Docker repositories may use the same effective port. Two omitted
ports, or an omitted port and an explicit main port, are configuration errors.
Port zero is rejected. All listeners bind to `server.host`; a bind failure stops
startup rather than leaving some repositories unavailable.

The main port also serves the UI/API and other formats. Extra ports serve only
Docker's `/v2/` API. Health, metrics, admin and token-management requests belong on
the main port. Without a main-bound Docker repository, the main port is not a
Docker endpoint. Ports and repository definitions require restart.

Optional Docker-only `public_url = "https://images.example.com:9443"` overrides
client commands for reverse-proxy/NAT mappings, not the listening address. It must
be an HTTP(S) origin without credentials, a path, query or fragment. Otherwise,
commands use the main public hostname with the repository's port; a main-bound
repository preserves the main public authority. UI base paths are not appended
to Docker addresses.

## Isolation and proxy policies

Each repository has independent contents, caches, upstream authentication and
in-flight downloads, even with identical image names or upstream URLs. Image
names remain verbatim: `app` and `docker.io/app` are different images. The existing
single-component `library/` upstream fallback remains; `nginx` and
`library/nginx` may have separate cache entries.

Proxy settings: `upstream`, optional Basic `auth`, `metadata_ttl` (default `0`,
revalidate mutable tags on every pull), `read_timeout` (60 seconds), and
`proxy_timeout` (300 seconds). Positive timeouts are required. Existing Docker
stale-cache and integrity behavior remains. `save_files`, per-repository TLS/CA,
connection/operation deadlines and static bearer settings are rejected rather
than ignored. Global outbound `[tls]` still applies.

Use `/ui/repositories?format=docker` for stored images, tags and client commands.
Browsing never refreshes upstreams. Existing global authentication, namespace
restrictions and curation still apply; ports are not per-repository ACLs.

## Upgrading from singleton Docker

`[docker]` and its former `NORA_DOCKER_*` singleton controls are rejected. There is
no implicit repository or upstream. Define each destination explicitly; the
container's shipped configuration defines one local repository on the main port.

Back up old storage and pull/export local images using the previous version if
needed, then republish them into a named local endpoint. Legacy root `docker/`
bytes are retained but never automatically served, adopted or swept by GC or
retention. No conversion command is provided; `migrate-docker-keys` was removed.
`[curation.docker]` and `NORA_REGISTRIES_ENABLE` remain supported. Docker mirroring
must target the desired local repository's full registry URL, including its port.
