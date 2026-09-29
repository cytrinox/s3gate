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
| `S3GATE_CHECK_EXISTS` | `true` | Check with `HEAD` that the object exists before redirecting. Turn off to save one S3 round-trip per request; missing objects then fail at S3 instead. |
| `S3GATE_READINESS_PATH_EXISTS` | – | Object key that must exist for `/_s3gate/readyz` to report ready. Used as-is, not joined with the prefix. |
| `S3GATE_READINESS_TIMEOUT` | `2` | Timeout in seconds for the readiness check (1–60) |
| `S3GATE_REQUEST_TIMEOUT` | `30` | Maximum time in seconds to handle one request, including S3 calls and probes (1–3600) |
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
| `s3gate_http_request_duration_seconds` | histogram | `method`, `status_code` | Time to answer a request on the main port, measured inside s3gate (including the S3 existence check). Same scope as `s3gate_http_requests_total`. Buckets from 1 ms to 60 s. |
| `s3gate_s3_request_duration_seconds` | histogram | `operation` (`HeadObject`), `result` (`ok`, `not_found`, `forbidden`, `timeout`, `error`) | Latency of calls to S3: the existence check before each redirect (only with `S3GATE_CHECK_EXISTS=true`) and the readiness check (`S3GATE_READINESS_PATH_EXISTS`). A readiness check that hits `S3GATE_READINESS_TIMEOUT` is recorded as `timeout` with that duration. Signing presigned URLs is local and not an S3 call. Buckets from 1 ms to 60 s. |
| `s3gate_fetches_total` | counter | `level1`…`levelN`, `subpath` | Successful `GET` redirects, grouped by the first N path segments (N = `S3GATE_METRICS_FETCH_LEVELS`). `HEAD`, 404s, errors and timeouts are not counted. |

For `s3gate_fetches_total`, the labels come from the object path below `S3GATE_PREFIX`. A `GET /foo/bar/baz` is labelled:

| Level | Labels |
|---|---|
| 1 | `level1="foo"`, `subpath="/foo"` |
| 2 | `level1="foo"`, `level2="bar"`, `subpath="/foo/bar"` |
| 3 | `level1="foo"`, `level2="bar"`, `level3="baz"`, `subpath="/foo/bar/baz"` |

Paths with fewer segments than N get empty labels for the missing levels. For example, `/foo.txt` at level 2 gives `level1="foo.txt"`, `level2=""`, `subpath="/foo.txt"`.

Every distinct prefix becomes its own time series. A level deep enough to reach object names therefore creates one series per downloaded object, so choose N to match the directory layout.

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
