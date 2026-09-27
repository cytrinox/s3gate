# s3gate Helm chart

Deploys [s3gate](../../README.md) with a hardened default configuration:
- settings that satisfy the `restricted` Pod Security Standard
- a user namespace (`hostUsers: false`)
- a read-only root filesystem
- no service account token
- startup, liveness and readiness probes
- a PodDisruptionBudget
- replicas spread across nodes

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
| `config.checkExists` | `true` | HEAD the object before redirecting |
| `config.requestTimeout` | `30` | Per-request timeout in seconds |
| `config.readinessPathExists` | – | Object that must exist for the pod to be ready |
| `config.readinessTimeout` | `2` | Keep below `readinessProbe.timeoutSeconds` |
| `config.logFormat` / `config.logLevel` | `json` / `info` | |
| `extraEnv` | `[]` | Additional container env entries |
| `hostUsers` | `false` | Run in a user namespace |
| `preStopSleepSeconds` | `5` | Delay before SIGTERM so ingresses stop routing to the pod |
| `terminationGracePeriodSeconds` | `45` | Keep above `preStopSleepSeconds + config.requestTimeout` |
| `ingress.*` | disabled | Standard Ingress; rate limiting and blocklists go in its annotations |
| `podDisruptionBudget.*` | enabled, `maxUnavailable: 1` | Only rendered with more than one replica |
| `autoscaling.*` | disabled | CPU-based HorizontalPodAutoscaler |
| `topologySpreadConstraints` | spread by hostname | The pod selector is added automatically |
