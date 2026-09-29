# s3gate Helm chart

Deploys [s3gate](../../README.md) with a hardened default configuration:
- settings that satisfy the `restricted` Pod Security Standard
- a user namespace (`hostUsers: false`)
- a read-only root filesystem
- no service account token
- startup, liveness and readiness probes
- a PodDisruptionBudget
- replicas spread across nodes
- optional exposure through an Ingress and/or a Gateway API HTTPRoute, with Traefik rate limiting and secure headers

Requires Kubernetes >= 1.33, and a node runtime and kernel that support user namespaces (containerd >= 2.0 or CRI-O >= 1.25, Linux >= 6.3). On older clusters, set `hostUsers: true`.

## Install

```sh
kubectl create namespace s3gate
kubectl label namespace s3gate pod-security.kubernetes.io/enforce=restricted

kubectl -n s3gate create secret generic s3gate-credentials \
  --from-literal=S3GATE_ACCESS_KEY_ID=... \
  --from-literal=S3GATE_SECRET_ACCESS_KEY=...

helm install s3gate oci://ghcr.io/cytrinox/charts/s3gate --version 0.1.0 -n s3gate \
  --set s3.bucket=my-bucket \
  --set credentials.existingSecret=s3gate-credentials
```

The chart is published to `oci://ghcr.io/cytrinox/charts/s3gate` for every `vX.Y.Z` git tag. The chart version and its default image tag both equal `X.Y.Z`. To install from a checkout instead, use `./k8s/chart` as the chart reference.

Rendering fails early if `s3.bucket` or the credentials are missing.

Example: expose s3gate through an existing Traefik Gateway with per-IP rate limiting:

```sh
helm upgrade --install s3gate oci://ghcr.io/cytrinox/charts/s3gate -n s3gate \
  --set s3.bucket=my-bucket \
  --set credentials.existingSecret=s3gate-credentials \
  --set gateway.enabled=true \
  --set gateway.parentRefs[0].name=traefik-gateway \
  --set gateway.parentRefs[0].namespace=traefik \
  --set gateway.hostnames[0]=files.example.com \
  --set traefik.middlewares.enabled=true \
  --set traefik.middlewares.rateLimit.average=20
```

Traefik must have its Kubernetes Gateway provider enabled to attach Middlewares via `ExtensionRef` and to route to a TraefikService.

## Values

The full list is in [values.yaml](values.yaml).

| Value | Default | Description |
|---|---|---|
| `replicaCount` | `3` | Ignored when `autoscaling.enabled` |
| `image.repository` | `ghcr.io/cytrinox/s3gate` | |
| `image.tag` | chart `appVersion` | e.g. `latest` to track `main` |
| `image.digest` | – | Pins the image; overrides `tag` |
| `s3.bucket` | required | Bucket to serve from |
| `s3.region` | `us-east-1` | |
| `s3.prefix` | – | Key prefix requests are confined to |
| `s3.endpoint` / `s3.publicEndpoint` | – | For S3-compatible stores and split internal/public addresses |
| `s3.forcePathStyle` | `false` | Needed for MinIO |
| `credentials.existingSecret` | – | Secret with `S3GATE_ACCESS_KEY_ID`, `S3GATE_SECRET_ACCESS_KEY` and optionally `S3GATE_SESSION_TOKEN` |
| `credentials.accessKeyId` / `secretAccessKey` / `sessionToken` | – | Used to create a Secret when `existingSecret` is empty |
| `config.presignTtl` | `300` | Presigned URL lifetime in seconds |
| `config.queryMetadata` | `true` | Query object metadata before redirecting: early 404 for missing objects and the size for the traffic metric |
| `config.requestTimeout` | `30` | Per-request timeout in seconds |
| `config.readinessPathExists` | – | Object that must exist for the pod to be ready |
| `config.readinessTimeout` | `2` | Keep below `readinessProbe.timeoutSeconds` |
| `config.logFormat` / `config.logLevel` | `json` / `info` | |
| `extraEnv` | `[]` | Additional container env entries |
| `hostUsers` | `false` | Run in a user namespace |
| `preStopSleepSeconds` | `5` | Delay before SIGTERM so ingresses stop routing to the pod |
| `terminationGracePeriodSeconds` | `45` | Keep above `preStopSleepSeconds + config.requestTimeout` |
| `metrics.enabled` | `false` | Serve `/metrics` on a separate container and Service port |
| `metrics.port` | `9090` | Metrics port; must differ from 8080 |
| `metrics.fetchLevels` | `0` | Path depth for `s3gate_fetches_total`; 0 disables it |
| `metrics.serviceMonitor.enabled` | `false` | Create a Prometheus Operator ServiceMonitor; requires `metrics.enabled` |
| `metrics.serviceMonitor.*` | see values | `namespace`, `interval`, `scrapeTimeout`, `honorLabels`, `labels`, `metricRelabelings`, `relabelings` |
| `ingress.*` | disabled | Standard Ingress. Gets the Traefik middleware annotation automatically when `traefik.middlewares.enabled` |
| `gateway.enabled` | `false` | Render an HTTPRoute (Gateway API v1) attached to an existing Gateway |
| `gateway.parentRefs` | – | Existing Gateway(s): `name` (required), `namespace`, `sectionName`, `port` |
| `gateway.hostnames` / `gateway.rules` | `s3gate.example.com` / `PathPrefix /` | Route hostnames and matches; `rules[].filters` for extra filters |
| `traefik.middlewares.enabled` | `false` | Render Traefik Middlewares and attach them to the Ingress and HTTPRoute |
| `traefik.middlewares.rateLimit.*` | on, 100/s average, 200 burst | Per-source-IP rate limit (`average`, `burst`, `period`, `sourceCriterion`) |
| `traefik.middlewares.secureHeaders.*` | on | HSTS, nosniff, frame deny, referrer policy, CSP and custom headers |
| `traefik.middlewares.compression.*` | off | Response compression. s3gate only sends empty redirects, so it rarely helps. |
| `traefik.traefikService.enabled` | `false` | Route the HTTPRoute through a weighted TraefikService (gateway mode only) |
| `traefik.traefikService.weighted.sticky.*` | off | Sticky-cookie affinity to one pod. s3gate is stateless, so this is usually not needed. |
| `podDisruptionBudget.*` | enabled, `maxUnavailable: 1` | Only rendered with more than one replica |
| `autoscaling.*` | disabled | CPU-based HorizontalPodAutoscaler |
| `topologySpreadConstraints` | spread by hostname | The pod selector is added automatically |
