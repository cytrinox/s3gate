# s3gate

s3gate is a small HTTP server that sits in front of a private S3 bucket. For every `GET` or `HEAD` request, it:

1. maps the request path to an object key,
2. checks that the object exists, and
3. answers with a `302` redirect to a short-lived presigned S3 URL.

The S3 credentials never leave the process, and every access is logged as structured JSON.

s3gate has no rate limiting, IP blocklists, authentication or TLS. Add those in the ingress or gateway in front of it.

```
client ──► ingress (TLS, rate limit, blocklist) ──► s3gate ──302──► client ──► S3 (presigned URL)
```

## Behaviour

| Request | Response |
|---|---|
| `GET /path/to/file` or `HEAD /path/to/file`, object exists | `302 Found`, `Location: <presigned URL>`, `Cache-Control: no-store` |
| Object does not exist | `404` |
| Path with an empty, `.` or `..` segment, a trailing `/`, or control characters | `404` |
| Invalid percent-encoding | `400` |
| Any other method | `405`, `Allow: GET, HEAD` |
| S3 denies access (credentials or bucket policy) | `502` |
| S3 unreachable or erroring | `502`, or `504` on timeout |
| Request takes longer than `S3GATE_REQUEST_TIMEOUT` | `504`; any in-flight S3 call is abandoned |

- **Key mapping:** the path is percent-decoded and appended to `S3GATE_PREFIX`. Keys can never escape the prefix.
- **Query strings:** the client's query string is ignored.
- **Signing:** the presigned URL signs the same method the client used, `GET` or `HEAD`.
- **Missing objects:** S3 reports a missing object as `403` unless the credentials also have `s3:ListBucket`, and s3gate then answers `502` instead of `404`. Grant `s3:GetObject` and `s3:ListBucket` on the bucket or prefix.

### Probes

The first path segment `_s3gate` is reserved. Objects under it are never served.

| Path | Purpose |
|---|---|
| `/_s3gate/healthz` | Liveness. Always `200`, never contacts S3. |
| `/_s3gate/readyz` | Readiness. `200` if `S3GATE_READINESS_PATH_EXISTS` is unset. Otherwise `200` only if that object exists, and `503` if it is missing, access is denied, or the check times out. |

## Overrides

Some paths should not map to objects: `/` might redirect to a homepage, `/robots.txt` might return a fixed text, or a folder might be hidden. An optional YAML file, set with `S3GATE_OVERRIDES_FILE`, defines fixed responses for such paths:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/cytrinox/s3gate/main/overrides.schema.json
overrides:
  - path: /
    type: redirect
    location: https://example.com/
    status: 301            # 301 | 302 | 303 | 307 | 308, default 302
    preserve_query: true   # append the request's query string, default false

  - path: /robots.txt
    type: content
    content_type: text/plain; charset=utf-8   # default
    body: |
      User-agent: *
      Disallow: /

  - path: /favicon.ico
    type: nocontent

  - path: /private/
    match: prefix          # exact (default) | prefix
    type: notfound
```

| `type` | Response |
|---|---|
| `redirect` | `status` (default `302`) with `Location: <location>`. `location` is an `http(s)://` URL or a path on this host. |
| `notfound` | `404`, without contacting S3 |
| `content` | `200` with `body` (default empty) and `content_type` (default `text/plain; charset=utf-8`) |
| `nocontent` | `204` without a body |

- **Precedence:** overrides are checked after the method check (other methods still get `405`) and **before** key mapping, the S3 metadata query and signing. A matching override always wins, even when an object with that key exists or `S3GATE_QUERY_METADATA=false`. Health checks under `/_s3gate/` can't be overridden.
- **Matching:** the percent-decoded path is compared, without the query string. Paths are case-sensitive, and a trailing `/` counts.
  - `match: prefix` matches every path starting with `path`. Use a trailing `/` (`/private/`) to match a folder, since `/private` would also match `/privateer`. A prefix of `/` matches everything, e.g. for maintenance mode.
  - An exact rule always beats prefix rules, and among prefixes the longest wins. The order in the file doesn't matter.
- **HEAD** requests get the same status and headers as GET, without a body.
- **Validation:** the file is read once at startup. Unknown fields, duplicate paths, invalid redirect targets or status codes, and paths under `/_s3gate/` stop startup with an error naming the entry. [overrides.schema.json](overrides.schema.json) describes the format for editors; the `yaml-language-server` comment above enables completion and checks in VS Code and other editors.

A complete example is in [examples/overrides/overrides.yaml](examples/overrides/overrides.yaml). In the Helm chart, put the rules under `overrides:` in your values; see the [chart README](k8s/chart/README.md).

## Configuration

All settings are environment variables. `AWS_*` variables, `~/.aws` profiles and instance metadata are **not** used.

| Variable | Default | Description |
|---|---|---|
| `S3GATE_BUCKET` | required | Bucket to serve from |
| `S3GATE_ACCESS_KEY_ID` | required | S3 access key |
| `S3GATE_SECRET_ACCESS_KEY` | required | S3 secret key (never logged) |
| `S3GATE_SESSION_TOKEN` | – | Optional session token for temporary credentials |
| `S3GATE_REGION` | `us-east-1` | Region used for signing |
| `S3GATE_PREFIX` | – | Key prefix all requests are confined to. A trailing `/` is added automatically. |
| `S3GATE_ENDPOINT` | AWS | Custom S3 endpoint (MinIO, Ceph, R2, …) |
| `S3GATE_PUBLIC_ENDPOINT` | `S3GATE_ENDPOINT` | Endpoint written into presigned URLs, for when clients reach S3 under a different address than s3gate does |
| `S3GATE_FORCE_PATH_STYLE` | `false` | Use path-style addressing (usually needed for MinIO) |
| `S3GATE_PRESIGN_TTL` | `300` | Presigned URL lifetime in seconds (1–604800) |
| `S3GATE_QUERY_METADATA` | `true` | Query the object's metadata from S3 (`HeadObject`) before redirecting. This enables the early `404` for missing objects and the object size for `s3gate_approx_fetched_bytes_total`. Turn off to save one S3 round-trip per request; missing objects then fail at S3 after the redirect, and no traffic is recorded. |
| `S3GATE_READINESS_PATH_EXISTS` | – | Object key that must exist for `/_s3gate/readyz` to report ready. Used as-is, not joined with the prefix. |
| `S3GATE_READINESS_TIMEOUT` | `2` | Timeout in seconds for the readiness check (1–60) |
| `S3GATE_REQUEST_TIMEOUT` | `30` | Maximum time in seconds to handle one request, including S3 calls and probes (1–3600) |
| `S3GATE_OVERRIDES_FILE` | – | Path of an optional [overrides file](#overrides) with fixed responses for specific paths |
| `S3GATE_LISTEN` | `0.0.0.0:8080` | Listen address |
| `S3GATE_METRICS_ENABLED` | `false` | Serve Prometheus metrics on a separate listener |
| `S3GATE_METRICS_LISTEN` | `0.0.0.0:9090` | Metrics listen address (must differ from `S3GATE_LISTEN`) |
| `S3GATE_METRICS_FETCH_LEVELS` | `0` | Path depth (0–10) for `s3gate_fetches_total`; 0 disables it |
| `S3GATE_LOG_FORMAT` | `json` | `json` or `text` |
| `RUST_LOG` | `info` | Log filter. Set `debug` to also see probe requests and rejected paths. |

Booleans accept `true/false`, `1/0`, `yes/no` and `on/off`. If the configuration is invalid, s3gate exits with status 1 at startup.

## Logging

s3gate writes one `request completed` event per request, with these fields:

| Field | Content |
|---|---|
| `request_id` | Taken from `x-request-id`, or generated and returned in the response |
| `method` | HTTP method |
| `path` | Request path |
| `client_ip` | First `X-Forwarded-For` entry, else `X-Real-IP`, else the peer address |
| `peer` | Address of the TCP peer |
| `user_agent` | Client user agent |
| `bucket` | Bucket served from |
| `key` | Object key the path mapped to |
| `outcome` | `redirect`, `not_found`, `rejected`, `method_not_allowed`, `upstream_error`, `presign_error` or `timeout` |
| `status` | HTTP status code |
| `latency_ms` | Time to answer, in milliseconds |

Presigned URLs and signatures are never logged. `client_ip` comes from headers the client can set, so only trust it when the ingress overwrites them. `peer` is always the real TCP peer.

## Metrics

With `S3GATE_METRICS_ENABLED=true`, s3gate serves `GET /metrics` in the Prometheus text format on `S3GATE_METRICS_LISTEN`. That's a separate port, so scrapes never go through the ingress and never appear in the access log. Metrics are collected with OpenTelemetry and exported through a Prometheus registry. Besides `target_info` (service name and version), s3gate exports:

| Metric | Type | Labels | Description |
|---|---|---|---|
| `s3gate_http_requests_total` | counter | `method`, `status_code` | HTTP requests on the main port, including rejected ones and timeouts (`504`). Probe requests to `/_s3gate/*` are not counted. Unknown methods are reported as `_OTHER`. |
| `s3gate_http_request_duration_seconds` | histogram | `method`, `status_code` | Time to answer a request on the main port, measured inside s3gate (including the S3 metadata query). Same scope as `s3gate_http_requests_total`. Buckets from 1 ms to 60 s. |
| `s3gate_s3_request_duration_seconds` | histogram | `operation` (`HeadObject`), `result` (`ok`, `not_found`, `forbidden`, `timeout`, `error`) | Latency of calls to S3: the metadata query before each redirect (only with `S3GATE_QUERY_METADATA=true`) and the readiness check (`S3GATE_READINESS_PATH_EXISTS`). A readiness check that hits `S3GATE_READINESS_TIMEOUT` is recorded as `timeout` with that duration. Signing presigned URLs is local and not an S3 call. Buckets from 1 ms to 60 s. |
| `s3gate_fetches_total` | counter | `level1`…`levelN`, `subpath` | Successful `GET` redirects, grouped by the first N path segments (N = `S3GATE_METRICS_FETCH_LEVELS`). `HEAD`, 404s, errors and timeouts are not counted. |
| `s3gate_approx_fetched_bytes_total` | counter | none, or `level1`…`levelN`, `subpath` like `s3gate_fetches_total` | Approximate traffic: the object size (from the metadata query) added for every successful `GET` redirect. Without `S3GATE_METRICS_FETCH_LEVELS` it is one total for the bucket. Requires `S3GATE_QUERY_METADATA=true`; otherwise the size is unknown and nothing is recorded. |
| `s3gate_approx_fetched_object_size_bytes` | histogram | none | Object size of every successful `GET` redirect, the same events as `s3gate_approx_fetched_bytes_total`. Shows whether traffic comes from many small or a few large objects. Buckets are powers of 4 from 1 KiB to 16 GiB. It has no path labels on purpose, to keep the number of series small. Requires `S3GATE_QUERY_METADATA=true`. |
| `s3gate_overrides_total` | counter | `type`, `rule` | Requests answered by an [override](#overrides). `rule` is the configured path, with a trailing `*` for prefix rules. |
| `s3gate_rejected_requests_total` | counter | `reason` | Requests refused before contacting S3: `method_not_allowed` (405), `bad_encoding` (400), `not_an_object` (empty path or trailing `/`), `unsafe_segment` (empty, `.` or `..` segments, e.g. traversal attempts), `control_character`, `reserved` (`/_s3gate/` paths other than the probes). Missing objects are not rejections. |

`s3gate_approx_fetched_bytes_total` and `s3gate_approx_fetched_object_size_bytes` assume that every redirected client downloads the whole object once. The real traffic at S3 can differ:
- **Lower:** clients that don't follow the redirect, abort the download, or fetch only a byte range.
- **Higher:** clients that reuse a presigned URL several times within `S3GATE_PRESIGN_TTL`. Those downloads never reach s3gate.

For exact numbers, use the S3 provider's billing or access logs.

For `s3gate_fetches_total`, the labels come from the object path below `S3GATE_PREFIX`. A `GET /foo/bar/baz` is labelled:

| Level | Labels |
|---|---|
| 1 | `level1="foo"`, `subpath="/foo"` |
| 2 | `level1="foo"`, `level2="bar"`, `subpath="/foo/bar"` |
| 3 | `level1="foo"`, `level2="bar"`, `level3="baz"`, `subpath="/foo/bar/baz"` |

Paths with fewer segments than N get empty labels for the missing levels. For example, `/foo.txt` at level 2 gives `level1="foo.txt"`, `level2=""`, `subpath="/foo.txt"`.

Every distinct prefix becomes its own time series. A level deep enough to reach object names therefore creates one series per downloaded object, so choose N to match the directory layout.

### Grafana dashboard

[k8s/chart/dashboards/s3gate-dashboard.json](k8s/chart/dashboards/s3gate-dashboard.json) covers all of these metrics. Import it in Grafana via *Dashboards → New → Import* and pick a Prometheus data source. The Helm chart can also install it automatically (`metrics.grafanaDashboard.enabled`, see the [chart README](k8s/chart/README.md)). It has five rows:

| Row | Contents |
|---|---|
| Overview | Request rate, redirect ratio, 5xx ratio, p95 latency, approximate traffic, running versions |
| HTTP | Requests by status and method, latency percentiles and heatmap |
| S3 | S3 calls by result, S3 latency percentiles, S3 error ratio |
| Downloads & traffic | Downloads per second, approximate throughput, average object size, object size heatmap, top prefixes by downloads and by traffic |
| Rejections | Rejected requests by reason over time and in the selected range |

The `namespace` and `instance` variables filter by the labels Prometheus adds when scraping. Top prefixes need `S3GATE_METRICS_FETCH_LEVELS > 0`, and the traffic and object size panels need `S3GATE_QUERY_METADATA=true`.

## Build and run

Rust 1.98 is pinned in `rust-toolchain.toml`.

```sh
cargo test
podman build -t s3gate -f Containerfile .
```

Example against a local MinIO:

```sh
podman run --rm -p 8080:8080 \
  -e S3GATE_BUCKET=files \
  -e S3GATE_ENDPOINT=http://host.containers.internal:9000 \
  -e S3GATE_PUBLIC_ENDPOINT=http://localhost:9000 \
  -e S3GATE_FORCE_PATH_STYLE=true \
  -e S3GATE_ACCESS_KEY_ID=minioadmin \
  -e S3GATE_SECRET_ACCESS_KEY=minioadmin \
  s3gate

curl -i  http://localhost:8080/hello.txt   # 302 with a presigned Location header
curl -L  http://localhost:8080/hello.txt   # file content
```

## Kubernetes

A Helm chart is in [k8s/chart](k8s/chart/README.md). A complete plain-manifest example is in [examples/k8s/deployment.yml](examples/k8s/deployment.yml). It creates a namespace that enforces the `restricted` Pod Security Standard and runs 3 replicas with `hostUsers: false`, a read-only root filesystem and all three probes. The snippet below shows the essentials.

```yaml
containers:
  - name: s3gate
    image: registry.example.com/s3gate:latest
    ports:
      - containerPort: 8080
    envFrom:
      - secretRef:
          name: s3gate   # S3GATE_ACCESS_KEY_ID, S3GATE_SECRET_ACCESS_KEY
    env:
      - name: S3GATE_BUCKET
        value: files
      - name: S3GATE_READINESS_PATH_EXISTS
        value: .s3gate-ready
    livenessProbe:
      httpGet: { path: /_s3gate/healthz, port: 8080 }
    readinessProbe:
      httpGet: { path: /_s3gate/readyz, port: 8080 }
      timeoutSeconds: 3   # keep above S3GATE_READINESS_TIMEOUT
```

s3gate stops on `SIGTERM`: it stops accepting connections and lets in-flight requests finish.

## License

Copyright 2026 Daniel Vogelbacher.

s3gate is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. It is distributed WITHOUT ANY WARRANTY. See [LICENSE](LICENSE) for the full text.

Anyone who runs a modified version of s3gate as a network service must offer its users the corresponding source code (AGPL section 13).
