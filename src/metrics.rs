// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prometheus metrics: an OpenTelemetry meter provider exporting into a private registry,
//! served on a dedicated listener.

use std::{io, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::State,
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use opentelemetry::{
    Key, KeyValue,
    metrics::{Counter, Histogram, Meter, MeterProvider},
};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider};
use prometheus::{Encoder, Registry, TextEncoder};
use tracing::error;

const METER_NAME: &str = env!("CARGO_PKG_NAME");

/// Latency buckets in seconds for requests and S3 calls: from millisecond answers up to
/// the timeout range.
const DURATION_BUCKETS: [f64; 15] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Object size buckets in bytes: powers of 4 from 1 KiB to 16 GiB.
const OBJECT_SIZE_BUCKETS: [f64; 13] = [
    1024.0,
    4096.0,
    16384.0,
    65536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
    16_777_216.0,
    67_108_864.0,
    268_435_456.0,
    1_073_741_824.0,
    4_294_967_296.0,
    17_179_869_184.0,
];

/// Owns the meter provider and the registry its exporter writes into.
pub struct Metrics {
    registry: Registry,
    provider: SdkMeterProvider,
}

impl Metrics {
    /// Builds a Prometheus-backed `SdkMeterProvider` without touching the global provider.
    pub fn new() -> io::Result<Self> {
        let registry = Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .map_err(|err| io::Error::other(format!("building Prometheus exporter: {err}")))?;
        let resource = Resource::builder()
            .with_service_name(env!("CARGO_PKG_NAME"))
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(exporter)
            .build();
        Ok(Self { registry, provider })
    }

    /// Like [`Metrics::new`], and also installs the provider globally so
    /// `opentelemetry::global::meter` records into this registry.
    pub fn install() -> io::Result<Self> {
        let metrics = Self::new()?;
        opentelemetry::global::set_meter_provider(metrics.provider.clone());
        Ok(metrics)
    }

    pub fn meter(&self) -> Meter {
        self.provider.meter(METER_NAME)
    }

    /// Prometheus text exposition of the private registry. The global `prometheus::gather()`
    /// would be empty, because the exporter only writes into `Metrics::registry`.
    pub fn encode(&self) -> prometheus::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        TextEncoder::new().encode(&self.registry.gather(), &mut buffer)?;
        Ok(buffer)
    }
}

/// Meter to use when metrics are disabled: the global one, which is a no-op
/// unless a provider was installed.
pub fn disabled_meter() -> Meter {
    opentelemetry::global::meter(METER_NAME)
}

/// Instruments for the HTTP gate.
#[derive(Clone)]
pub struct HttpMetrics {
    requests: Counter<u64>,
    request_duration: Histogram<f64>,
    s3_duration: Histogram<f64>,
    fetches: Counter<u64>,
    fetched_bytes: Counter<u64>,
    object_size: Histogram<u64>,
    rejected: Counter<u64>,
    /// `level1..levelN` label keys for the fetch counter; empty disables it.
    level_keys: Arc<[Key]>,
}

impl HttpMetrics {
    pub fn new(meter: &Meter, fetch_levels: usize) -> Self {
        Self {
            requests: meter
                .u64_counter("s3gate.http.requests")
                .with_description("Count of HTTP requests by method and status code")
                .build(),
            request_duration: meter
                .f64_histogram("s3gate.http.request.duration")
                .with_unit("s")
                .with_description("HTTP request latency by method and status code")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            s3_duration: meter
                .f64_histogram("s3gate.s3.request.duration")
                .with_unit("s")
                .with_description("Latency of S3 API calls by operation and result")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            fetches: meter
                .u64_counter("s3gate.fetches")
                .with_description("Successful GET redirects by path prefix")
                .build(),
            fetched_bytes: meter
                .u64_counter("s3gate.approx.fetched")
                .with_unit("By")
                .with_description(
                    "Approximate traffic: object size of each successful GET redirect",
                )
                .build(),
            object_size: meter
                .u64_histogram("s3gate.approx.fetched.object.size")
                .with_unit("By")
                .with_description("Object sizes of successful GET redirects")
                .with_boundaries(OBJECT_SIZE_BUCKETS.to_vec())
                .build(),
            rejected: meter
                .u64_counter("s3gate.rejected.requests")
                .with_description("Requests refused before contacting S3, by reason")
                .build(),
            level_keys: (1..=fetch_levels)
                .map(|level| Key::new(format!("level{level}")))
                .collect(),
        }
    }

    /// Counts a successful download of `relative_path` (the object path below the prefix)
    /// and adds its `size` to the traffic counter and the size histogram. Without fetch
    /// levels, only the unlabelled traffic total is recorded.
    pub fn record_fetch(&self, relative_path: &str, size: Option<u64>) {
        if let Some(size) = size {
            self.object_size.record(size, &[]);
        }
        if self.level_keys.is_empty() {
            if let Some(size) = size {
                self.fetched_bytes.add(size, &[]);
            }
            return;
        }
        let attributes = fetch_attributes(relative_path, &self.level_keys);
        self.fetches.add(1, &attributes);
        if let Some(size) = size {
            self.fetched_bytes.add(size, &attributes);
        }
    }

    /// Counts a request refused before any S3 call, e.g. `reason = "unsafe_segment"`.
    pub fn record_rejected(&self, reason: &'static str) {
        self.rejected.add(1, &[KeyValue::new("reason", reason)]);
    }

    pub fn record_request(&self, method: &Method, status: StatusCode, elapsed: Duration) {
        let attributes = [
            KeyValue::new("method", method_label(method)),
            KeyValue::new("status_code", i64::from(status.as_u16())),
        ];
        self.requests.add(1, &attributes);
        self.request_duration
            .record(elapsed.as_secs_f64(), &attributes);
    }

    /// Records one S3 API call, e.g. `operation = "HeadObject"`, `result = "not_found"`.
    pub fn record_s3(&self, operation: &'static str, result: &'static str, elapsed: Duration) {
        self.s3_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("operation", operation),
                KeyValue::new("result", result),
            ],
        );
    }
}

/// One `levelN` label per configured level, taken from the path segments (empty when the
/// path is shorter), plus `subpath`: the existing segments up to that depth, e.g.
/// `foo/bar/baz` at two levels gives `level1=foo, level2=bar, subpath=/foo/bar`.
fn fetch_attributes(relative_path: &str, level_keys: &[Key]) -> Vec<KeyValue> {
    let segments: Vec<&str> = relative_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .take(level_keys.len())
        .collect();
    let mut attributes: Vec<KeyValue> = level_keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            KeyValue::new(
                key.clone(),
                segments.get(index).copied().unwrap_or("").to_owned(),
            )
        })
        .collect();
    attributes.push(KeyValue::new("subpath", format!("/{}", segments.join("/"))));
    attributes
}

/// Clients can send arbitrary method tokens; folding unknown ones into `_OTHER`
/// (as the OpenTelemetry HTTP conventions do) keeps the label cardinality bounded.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::OPTIONS => "OPTIONS",
        Method::CONNECT => "CONNECT",
        Method::TRACE => "TRACE",
        _ => "_OTHER",
    }
}

pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(metrics)
}

async fn scrape(State(metrics): State<Arc<Metrics>>) -> Response {
    match metrics.encode() {
        Ok(buffer) => (
            [(
                header::CONTENT_TYPE,
                TextEncoder::new().format_type().to_owned(),
            )],
            buffer,
        )
            .into_response(),
        Err(err) => {
            error!(error = %err, "failed to encode metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "error encoding metrics\n",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, extract::Request};
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn serves_prometheus_text_format() {
        let metrics = Arc::new(Metrics::new().unwrap());
        let request = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let response = router(metrics).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4"
        );
    }

    fn labels(path: &str, levels: usize) -> Vec<(String, String)> {
        let keys: Vec<Key> = (1..=levels)
            .map(|level| Key::new(format!("level{level}")))
            .collect();
        fetch_attributes(path, &keys)
            .into_iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect()
    }

    fn expected(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn fetch_attributes_follow_level() {
        assert_eq!(
            labels("foo/bar/baz", 1),
            expected(&[("level1", "foo"), ("subpath", "/foo")])
        );
        assert_eq!(
            labels("foo/bar/baz", 2),
            expected(&[
                ("level1", "foo"),
                ("level2", "bar"),
                ("subpath", "/foo/bar")
            ])
        );
        assert_eq!(
            labels("foo/bar/baz", 3),
            expected(&[
                ("level1", "foo"),
                ("level2", "bar"),
                ("level3", "baz"),
                ("subpath", "/foo/bar/baz"),
            ])
        );
    }

    #[test]
    fn fetch_attributes_pad_short_paths() {
        assert_eq!(
            labels("foo.txt", 2),
            expected(&[
                ("level1", "foo.txt"),
                ("level2", ""),
                ("subpath", "/foo.txt")
            ])
        );
        assert_eq!(
            labels("foo/bar", 4),
            expected(&[
                ("level1", "foo"),
                ("level2", "bar"),
                ("level3", ""),
                ("level4", ""),
                ("subpath", "/foo/bar"),
            ])
        );
    }

    #[test]
    fn unknown_methods_are_folded() {
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(
            method_label(&Method::from_bytes(b"PROPFIND").unwrap()),
            "_OTHER"
        );
    }
}
