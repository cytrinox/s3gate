// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prometheus metrics: an OpenTelemetry meter provider exporting into a private registry,
//! served on a dedicated listener.

use std::{io, sync::Arc};

use axum::{
    Router,
    extract::State,
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Meter, MeterProvider},
};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider};
use prometheus::{Encoder, Registry, TextEncoder};
use tracing::error;

const METER_NAME: &str = env!("CARGO_PKG_NAME");

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
}

impl HttpMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests: meter
                .u64_counter("s3gate.http.requests")
                .with_description("Count of HTTP requests by method and status code")
                .build(),
        }
    }

    pub fn record_request(&self, method: &Method, status: StatusCode) {
        self.requests.add(
            1,
            &[
                KeyValue::new("method", method_label(method)),
                KeyValue::new("status_code", i64::from(status.as_u16())),
            ],
        );
    }
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

    #[test]
    fn unknown_methods_are_folded() {
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(
            method_label(&Method::from_bytes(b"PROPFIND").unwrap()),
            "_OTHER"
        );
    }
}
