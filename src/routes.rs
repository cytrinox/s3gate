// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP surface: Kubernetes probes, the presign-and-redirect gate, and access logging.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use tracing::{Span, debug, field::Empty, info, info_span, warn};

use crate::{
    config::Config,
    keys::{self, KeyError},
    metrics::HttpMetrics,
    store::{ObjectInfo, ObjectStore, PresignMethod, StoreError},
};

pub struct Settings {
    pub bucket: String,
    pub prefix: String,
    pub presign_ttl: Duration,
    pub check_exists: bool,
    pub readiness_path_exists: Option<String>,
    pub readiness_timeout: Duration,
    pub request_timeout: Duration,
}

impl From<&Config> for Settings {
    fn from(config: &Config) -> Self {
        Self {
            bucket: config.bucket.clone(),
            prefix: config.prefix.clone(),
            presign_ttl: config.presign_ttl,
            check_exists: config.check_exists,
            readiness_path_exists: config.readiness_path_exists.clone(),
            readiness_timeout: config.readiness_timeout,
            request_timeout: config.request_timeout,
        }
    }
}

pub struct AppState<S> {
    store: Arc<S>,
    settings: Arc<Settings>,
}

impl<S> AppState<S> {
    pub fn new(store: S, settings: Settings) -> Self {
        Self {
            store: Arc::new(store),
            settings: Arc::new(settings),
        }
    }
}

impl<S> Clone for AppState<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            settings: Arc::clone(&self.settings),
        }
    }
}

/// Response marker: probe traffic is logged at debug instead of info and not counted in metrics.
#[derive(Clone, Copy)]
struct Probe;

/// Response marker for a successful GET redirect.
#[derive(Clone)]
struct Fetched {
    /// Object path below the prefix.
    path: String,
    /// Object size from the existence check; `None` when the check is disabled.
    size: Option<u64>,
}

/// Response marker with the timing of the S3 call made while handling the request.
#[derive(Clone, Copy)]
struct S3Call {
    operation: &'static str,
    result: &'static str,
    elapsed: Duration,
}

pub fn router<S: ObjectStore>(state: AppState<S>, http_metrics: HttpMetrics) -> Router {
    let request_timeout = state.settings.request_timeout;
    Router::new()
        .route("/_s3gate/healthz", get(healthz))
        .route("/_s3gate/readyz", get(readyz::<S>))
        .fallback(gate::<S>)
        .with_state(state)
        // Inside the trace layer, so timed-out requests are still logged with their span.
        .layer(middleware::from_fn_with_state(
            request_timeout,
            enforce_timeout,
        ))
        // Outside the timeout, so the 504s it produces are counted too.
        .layer(middleware::from_fn_with_state(http_metrics, count_requests))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_span)
                .on_request(())
                .on_response(on_response)
                .on_failure(()),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

async fn count_requests(
    State(http_metrics): State<HttpMetrics>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let started = Instant::now();
    let response = next.run(request).await;
    let elapsed = started.elapsed();
    if response.extensions().get::<Probe>().is_none() {
        http_metrics.record_request(&method, response.status(), elapsed);
    }
    if let Some(fetched) = response.extensions().get::<Fetched>() {
        http_metrics.record_fetch(&fetched.path, fetched.size);
    }
    if let Some(call) = response.extensions().get::<S3Call>() {
        http_metrics.record_s3(call.operation, call.result, call.elapsed);
    }
    response
}

/// Aborts request handling (including in-flight S3 calls) once `timeout` has elapsed.
async fn enforce_timeout(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            Span::current().record("outcome", "timeout");
            warn!(
                timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                "request timed out"
            );
            plain(StatusCode::GATEWAY_TIMEOUT)
        }
    }
}

async fn healthz() -> Response {
    probe(StatusCode::OK, "ok\n")
}

async fn readyz<S: ObjectStore>(State(state): State<AppState<S>>) -> Response {
    let settings = &state.settings;
    let Some(key) = settings.readiness_path_exists.as_deref() else {
        return probe(StatusCode::OK, "ready\n");
    };
    match tokio::time::timeout(settings.readiness_timeout, timed_head(&*state.store, key)).await {
        Ok((Ok(_), call)) => with_s3_call(probe(StatusCode::OK, "ready\n"), call),
        Ok((Err(err), call)) => {
            warn!(key, error = %err, "readiness check failed");
            with_s3_call(probe(StatusCode::SERVICE_UNAVAILABLE, "not ready\n"), call)
        }
        Err(_) => {
            warn!(
                key,
                timeout_ms = settings.readiness_timeout.as_millis(),
                "readiness check timed out"
            );
            let call = S3Call {
                operation: "HeadObject",
                result: StoreError::Timeout.kind(),
                elapsed: settings.readiness_timeout,
            };
            with_s3_call(probe(StatusCode::SERVICE_UNAVAILABLE, "not ready\n"), call)
        }
    }
}

fn probe(status: StatusCode, body: &'static str) -> Response {
    let mut response = (status, body).into_response();
    response.extensions_mut().insert(Probe);
    response
}

/// Maps any other path to an object and redirects to a presigned URL for it.
async fn gate<S: ObjectStore>(
    State(state): State<AppState<S>>,
    method: Method,
    uri: Uri,
) -> Response {
    let span = Span::current();
    let presign_method = match method {
        Method::GET => PresignMethod::Get,
        Method::HEAD => PresignMethod::Head,
        _ => {
            span.record("outcome", "method_not_allowed");
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, "GET, HEAD")],
                "Method Not Allowed\n",
            )
                .into_response();
        }
    };

    let settings = &state.settings;
    span.record("bucket", settings.bucket.as_str());
    let key = match keys::map_path(&settings.prefix, uri.path()) {
        Ok(key) => key,
        Err(err) => {
            span.record("outcome", "rejected");
            debug!(reason = %err, "path rejected");
            return match err {
                KeyError::BadEncoding => plain(StatusCode::BAD_REQUEST),
                _ => plain(StatusCode::NOT_FOUND),
            };
        }
    };
    span.record("key", key.as_str());

    let (head_call, size) = if settings.check_exists {
        match timed_head(&*state.store, &key).await {
            (Ok(info), call) => (Some(call), info.size),
            (Err(err), call) => return with_s3_call(store_failure(&span, err), call),
        }
    } else {
        (None, None)
    };

    let mut response = redirect(&state, &span, presign_method, &key, size).await;
    if let Some(call) = head_call {
        response.extensions_mut().insert(call);
    }
    response
}

async fn redirect<S: ObjectStore>(
    state: &AppState<S>,
    span: &Span,
    presign_method: PresignMethod,
    key: &str,
    size: Option<u64>,
) -> Response {
    let settings = &state.settings;
    let url = match state
        .store
        .presign(presign_method, key, settings.presign_ttl)
        .await
    {
        Ok(url) => url,
        Err(err) => return store_failure(span, err),
    };
    let Ok(location) = HeaderValue::try_from(url) else {
        return store_failure(
            span,
            StoreError::Presign("presigned URL is not a valid header value".to_owned()),
        );
    };

    span.record("outcome", "redirect");
    let mut response = (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response();
    if presign_method == PresignMethod::Get {
        let relative = key.strip_prefix(settings.prefix.as_str()).unwrap_or(key);
        response.extensions_mut().insert(Fetched {
            path: relative.to_owned(),
            size,
        });
    }
    response
}

/// Runs a HeadObject call and measures it.
async fn timed_head<S: ObjectStore>(
    store: &S,
    key: &str,
) -> (Result<ObjectInfo, StoreError>, S3Call) {
    let started = Instant::now();
    let result = store.head(key).await;
    let call = S3Call {
        operation: "HeadObject",
        result: result.as_ref().map_or_else(StoreError::kind, |_| "ok"),
        elapsed: started.elapsed(),
    };
    (result, call)
}

fn with_s3_call(mut response: Response, call: S3Call) -> Response {
    response.extensions_mut().insert(call);
    response
}

fn store_failure(span: &Span, err: StoreError) -> Response {
    let (status, outcome) = match err {
        StoreError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        StoreError::Forbidden => {
            warn!(error = %err, "S3 denied access; check credentials and bucket policy");
            (StatusCode::BAD_GATEWAY, "upstream_error")
        }
        StoreError::Timeout => {
            warn!(error = %err, "S3 request timed out");
            (StatusCode::GATEWAY_TIMEOUT, "upstream_error")
        }
        StoreError::Upstream(_) => {
            warn!(error = %err, "S3 request failed");
            (StatusCode::BAD_GATEWAY, "upstream_error")
        }
        StoreError::Presign(_) => {
            warn!(error = %err, "presigning failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "presign_error")
        }
    };
    span.record("outcome", outcome);
    plain(status)
}

/// Bare status response; error details stay in the log, never in the body.
fn plain(status: StatusCode) -> Response {
    let reason = status.canonical_reason().unwrap_or("Error");
    (status, format!("{reason}\n")).into_response()
}

fn make_span(request: &Request) -> Span {
    let headers = request.headers();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .unwrap_or("-");
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string());
    let client_ip = forwarded_client_ip(headers)
        .or_else(|| peer.clone())
        .unwrap_or_else(|| "-".to_owned());
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-");

    info_span!(
        "request",
        request_id,
        method = %request.method(),
        path = %request.uri().path(),
        client_ip,
        peer = peer.as_deref().unwrap_or("-"),
        user_agent,
        bucket = Empty,
        key = Empty,
        outcome = Empty,
    )
}

/// Client address as reported by the ingress. Only used for logging, never for access decisions.
fn forwarded_client_ip(headers: &HeaderMap) -> Option<String> {
    let forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next());
    let real_ip = headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok());
    forwarded_for
        .or(real_ip)
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .map(str::to_owned)
}

fn on_response(response: &Response, latency: Duration, _span: &Span) {
    let status = response.status().as_u16();
    let latency_ms = latency.as_secs_f64() * 1000.0;
    if response.extensions().get::<Probe>().is_some() {
        debug!(status, latency_ms, "request completed");
    } else {
        info!(status, latency_ms, "request completed");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct MockStore {
        objects: Vec<&'static str>,
        head_error: Option<StoreError>,
        head_delay: Option<Duration>,
        head_calls: AtomicUsize,
        presign_error: Option<StoreError>,
    }

    impl ObjectStore for MockStore {
        /// Reports the key length as object size.
        async fn head(&self, key: &str) -> Result<ObjectInfo, StoreError> {
            self.head_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(delay) = self.head_delay {
                tokio::time::sleep(delay).await;
            }
            match &self.head_error {
                Some(err) => Err(err.clone()),
                None if self.objects.contains(&key) => Ok(ObjectInfo {
                    size: Some(key.len() as u64),
                }),
                None => Err(StoreError::NotFound),
            }
        }

        async fn presign(
            &self,
            method: PresignMethod,
            key: &str,
            ttl: Duration,
        ) -> Result<String, StoreError> {
            if let Some(err) = &self.presign_error {
                return Err(err.clone());
            }
            Ok(format!(
                "https://s3.test/bucket/{key}?method={method:?}&ttl={}",
                ttl.as_secs()
            ))
        }
    }

    fn settings() -> Settings {
        Settings {
            bucket: "bucket".to_owned(),
            prefix: String::new(),
            presign_ttl: Duration::from_secs(300),
            check_exists: true,
            readiness_path_exists: None,
            readiness_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(30),
        }
    }

    fn store(objects: &[&'static str]) -> MockStore {
        MockStore {
            objects: objects.to_vec(),
            ..MockStore::default()
        }
    }

    /// Router with no-op metrics.
    fn app(state: AppState<MockStore>) -> Router {
        router(
            state,
            HttpMetrics::new(&crate::metrics::disabled_meter(), 0),
        )
    }

    async fn send_to(app: Router, method: Method, path: &str) -> Response {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        app.oneshot(request).await.unwrap()
    }

    async fn send(state: AppState<MockStore>, method: Method, path: &str) -> Response {
        send_to(app(state), method, path).await
    }

    async fn body(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn location(response: &Response) -> &str {
        response.headers()[header::LOCATION].to_str().unwrap()
    }

    #[tokio::test]
    async fn get_redirects_to_presigned_url() {
        let state = AppState::new(store(&["a/b.txt"]), settings());
        let response = send(state, Method::GET, "/a/b.txt").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            location(&response),
            "https://s3.test/bucket/a/b.txt?method=Get&ttl=300"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(response.headers().contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn head_presigns_head_request() {
        let state = AppState::new(store(&["a.txt"]), settings());
        let response = send(state, Method::HEAD, "/a.txt").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(location(&response).contains("method=Head"));
    }

    #[tokio::test]
    async fn applies_prefix() {
        let state = AppState::new(
            store(&["public/a.txt"]),
            Settings {
                prefix: "public/".to_owned(),
                ..settings()
            },
        );
        let response = send(state, Method::GET, "/a.txt").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(location(&response).contains("/bucket/public/a.txt?"));
    }

    #[tokio::test]
    async fn ignores_client_query_string() {
        let state = AppState::new(store(&["a.txt"]), settings());
        let response = send(state, Method::GET, "/a.txt?X-Amz-Expires=999999").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            location(&response),
            "https://s3.test/bucket/a.txt?method=Get&ttl=300"
        );
    }

    #[tokio::test]
    async fn missing_object_is_not_found() {
        let state = AppState::new(store(&[]), settings());
        let response = send(state, Method::GET, "/missing.txt").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn skips_head_when_existence_check_disabled() {
        let state = AppState::new(
            store(&[]),
            Settings {
                check_exists: false,
                ..settings()
            },
        );
        let response = send(state.clone(), Method::GET, "/missing.txt").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(state.store.head_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_other_methods() {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let state = AppState::new(store(&["a.txt"]), settings());
            let response = send(state, method.clone(), "/a.txt").await;
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method}"
            );
            assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
        }
    }

    #[tokio::test]
    async fn rejects_unsafe_paths_without_touching_s3() {
        for path in [
            "/",
            "/dir/",
            "/../etc/passwd",
            "/a/%2e%2e/b",
            "/_s3gate/other",
        ] {
            let state = AppState::new(store(&[]), settings());
            let response = send(state.clone(), Method::GET, path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(state.store.head_calls.load(Ordering::SeqCst), 0, "{path}");
        }
        let state = AppState::new(store(&[]), settings());
        let response = send(state, Method::GET, "/%FF").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn maps_store_errors() {
        for (error, status) in [
            (StoreError::Forbidden, StatusCode::BAD_GATEWAY),
            (StoreError::Timeout, StatusCode::GATEWAY_TIMEOUT),
            (StoreError::Upstream("boom".into()), StatusCode::BAD_GATEWAY),
        ] {
            let state = AppState::new(
                MockStore {
                    head_error: Some(error.clone()),
                    ..store(&["a.txt"])
                },
                settings(),
            );
            let response = send(state, Method::GET, "/a.txt").await;
            assert_eq!(response.status(), status, "{error}");
            assert!(!body(response).await.contains("boom"));
        }
    }

    #[tokio::test]
    async fn propagates_request_id() {
        let request = Request::builder()
            .uri("/_s3gate/healthz")
            .header("x-request-id", "abc-123")
            .body(Body::empty())
            .unwrap();
        let state = AppState::new(store(&[]), settings());
        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.headers()["x-request-id"], "abc-123");
    }

    #[tokio::test]
    async fn healthz_always_ok() {
        let state = AppState::new(
            MockStore {
                head_error: Some(StoreError::Timeout),
                ..MockStore::default()
            },
            Settings {
                readiness_path_exists: Some(".ready".to_owned()),
                ..settings()
            },
        );
        let response = send(state.clone(), Method::GET, "/_s3gate/healthz").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, "ok\n");
        let response = send(state.clone(), Method::HEAD, "/_s3gate/healthz").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.store.head_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn readyz_without_readiness_path_is_ok() {
        let state = AppState::new(store(&[]), settings());
        let response = send(state.clone(), Method::GET, "/_s3gate/readyz").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.store.head_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn readyz_checks_readiness_path() {
        let ready = Settings {
            readiness_path_exists: Some(".ready".to_owned()),
            ..settings()
        };
        let state = AppState::new(store(&[".ready"]), ready);
        let response = send(state, Method::GET, "/_s3gate/readyz").await;
        assert_eq!(response.status(), StatusCode::OK);

        let not_ready = Settings {
            readiness_path_exists: Some(".ready".to_owned()),
            ..settings()
        };
        let state = AppState::new(store(&[]), not_ready);
        let response = send(state, Method::GET, "/_s3gate/readyz").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn slow_request_times_out() {
        let state = AppState::new(
            MockStore {
                head_delay: Some(Duration::from_secs(30)),
                ..store(&["a.txt"])
            },
            Settings {
                request_timeout: Duration::from_millis(20),
                ..settings()
            },
        );
        let response = send(state, Method::GET, "/a.txt").await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(response.headers().contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn readyz_times_out() {
        let state = AppState::new(
            MockStore {
                head_delay: Some(Duration::from_secs(30)),
                ..store(&[".ready"])
            },
            Settings {
                readiness_path_exists: Some(".ready".to_owned()),
                readiness_timeout: Duration::from_millis(20),
                ..settings()
            },
        );
        let response = send(state, Method::GET, "/_s3gate/readyz").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn counts_requests_by_method_and_status() {
        let metrics = crate::metrics::Metrics::new().unwrap();
        let app = router(
            AppState::new(store(&["a.txt"]), settings()),
            HttpMetrics::new(&metrics.meter(), 0),
        );
        for (method, path) in [
            (Method::GET, "/a.txt"),
            (Method::GET, "/a.txt"),
            (Method::GET, "/missing.txt"),
            (Method::POST, "/a.txt"),
            (Method::from_bytes(b"PROPFIND").unwrap(), "/a.txt"),
            (Method::GET, "/_s3gate/healthz"),
            (Method::GET, "/_s3gate/readyz"),
        ] {
            send_to(app.clone(), method, path).await;
        }

        let exposition = String::from_utf8(metrics.encode().unwrap()).unwrap();
        let count = |method: &str, status: u16| {
            exposition
                .lines()
                .filter(|line| line.starts_with("s3gate_http_requests_total{"))
                .find(|line| {
                    line.contains(&format!("method=\"{method}\""))
                        && line.contains(&format!("status_code=\"{status}\""))
                })
                .and_then(|line| line.rsplit(' ').next())
                .map(str::to_owned)
        };
        assert_eq!(count("GET", 302).as_deref(), Some("2"), "{exposition}");
        assert_eq!(count("GET", 404).as_deref(), Some("1"));
        assert_eq!(count("POST", 405).as_deref(), Some("1"));
        assert_eq!(count("_OTHER", 405).as_deref(), Some("1"));
        // Probes are not counted.
        assert_eq!(count("GET", 200), None);
    }

    fn fetch_series(exposition: &str) -> Vec<&str> {
        series(exposition, "s3gate_fetches_total")
    }

    fn series<'a>(exposition: &'a str, name: &str) -> Vec<&'a str> {
        let prefix = format!("{name}{{");
        exposition
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .collect()
    }

    const BYTES: &str = "s3gate_approx_fetched_bytes_total";

    #[tokio::test]
    async fn counts_fetches_by_path_level() {
        let metrics = crate::metrics::Metrics::new().unwrap();
        let app = router(
            AppState::new(
                store(&["pub/foo/bar/baz", "pub/foo.txt"]),
                Settings {
                    prefix: "pub/".to_owned(),
                    ..settings()
                },
            ),
            HttpMetrics::new(&metrics.meter(), 2),
        );
        for (method, path) in [
            (Method::GET, "/foo/bar/baz"),
            (Method::GET, "/foo/bar/baz"),
            (Method::GET, "/foo.txt"),
            (Method::HEAD, "/foo/bar/baz"),
            (Method::GET, "/foo/missing"),
        ] {
            send_to(app.clone(), method, path).await;
        }

        let exposition = String::from_utf8(metrics.encode().unwrap()).unwrap();
        let series = fetch_series(&exposition);
        assert_eq!(series.len(), 2, "{exposition}");
        let value = |labels: &str| {
            series
                .iter()
                .find(|line| line.contains(labels))
                .and_then(|line| line.rsplit(' ').next())
        };
        assert_eq!(
            value(r#"level1="foo",level2="bar",subpath="/foo/bar""#),
            Some("2"),
            "{exposition}"
        );
        assert_eq!(
            value(r#"level1="foo.txt",level2="",subpath="/foo.txt""#),
            Some("1"),
            "{exposition}"
        );
        assert!(!series.iter().any(|line| line.contains("pub")));

        // Traffic uses the same labels; the mock reports the key length as size.
        let bytes = self::series(&exposition, BYTES);
        assert_eq!(bytes.len(), 2, "{exposition}");
        let bytes_for = |labels: &str| {
            bytes
                .iter()
                .find(|line| line.contains(labels))
                .and_then(|line| line.rsplit(' ').next())
        };
        let per_fetch = "pub/foo/bar/baz".len() * 2;
        assert_eq!(
            bytes_for(r#"subpath="/foo/bar""#),
            Some(per_fetch.to_string().as_str())
        );
        assert_eq!(
            bytes_for(r#"subpath="/foo.txt""#),
            Some("pub/foo.txt".len().to_string().as_str())
        );
    }

    #[tokio::test]
    async fn records_request_and_s3_latency() {
        let metrics = crate::metrics::Metrics::new().unwrap();
        let app = |store: MockStore, settings: Settings| {
            router(
                AppState::new(store, settings),
                HttpMetrics::new(&metrics.meter(), 0),
            )
        };
        let ok_app = app(
            store(&["a.txt", ".ready"]),
            Settings {
                readiness_path_exists: Some(".ready".to_owned()),
                ..settings()
            },
        );
        send_to(ok_app.clone(), Method::GET, "/a.txt").await;
        send_to(ok_app.clone(), Method::GET, "/a.txt").await;
        send_to(ok_app.clone(), Method::HEAD, "/a.txt").await;
        send_to(ok_app.clone(), Method::GET, "/missing.txt").await;
        send_to(ok_app.clone(), Method::GET, "/_s3gate/healthz").await;
        send_to(ok_app.clone(), Method::GET, "/_s3gate/readyz").await;
        send_to(ok_app, Method::GET, "/../etc").await;

        let forbidden_app = app(
            MockStore {
                head_error: Some(StoreError::Forbidden),
                ..store(&[])
            },
            settings(),
        );
        send_to(forbidden_app, Method::GET, "/a.txt").await;

        let presign_failing_app = app(
            MockStore {
                presign_error: Some(StoreError::Presign("boom".into())),
                ..store(&["a.txt"])
            },
            settings(),
        );
        let response = send_to(presign_failing_app, Method::GET, "/a.txt").await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let unchecked_app = app(
            store(&[]),
            Settings {
                check_exists: false,
                ..settings()
            },
        );
        send_to(unchecked_app, Method::GET, "/a.txt").await;

        let exposition = String::from_utf8(metrics.encode().unwrap()).unwrap();
        let sample = |name: &str, labels: &[&str]| {
            exposition
                .lines()
                .filter(|line| line.starts_with(&format!("{name}{{")))
                .find(|line| labels.iter().all(|label| line.contains(label)))
                .and_then(|line| line.rsplit(' ').next())
                .map(str::to_owned)
        };

        let request = "s3gate_http_request_duration_seconds_count";
        let requests = |status: &str| {
            sample(
                request,
                &[r#"method="GET""#, &format!("status_code=\"{status}\"")],
            )
        };
        assert_eq!(requests("302").as_deref(), Some("3"), "{exposition}");
        // The missing object and the rejected traversal path.
        assert_eq!(requests("404").as_deref(), Some("2"));
        assert_eq!(requests("502").as_deref(), Some("1"));
        assert_eq!(requests("500").as_deref(), Some("1"));
        // Probes are not measured.
        assert_eq!(requests("200"), None);

        // One HeadObject per checked request plus the readiness check; nothing for the
        // rejected path, the healthz probe or the app without existence checks.
        let s3 = "s3gate_s3_request_duration_seconds_count";
        let s3_calls = |result: &str| {
            sample(
                s3,
                &[r#"operation="HeadObject""#, &format!("result=\"{result}\"")],
            )
        };
        assert_eq!(s3_calls("ok").as_deref(), Some("5"), "{exposition}");
        assert_eq!(s3_calls("not_found").as_deref(), Some("1"));
        assert_eq!(s3_calls("forbidden").as_deref(), Some("1"));
        assert!(!exposition.contains("presign"));
    }

    #[tokio::test]
    async fn fetch_counter_disabled_at_level_zero() {
        let metrics = crate::metrics::Metrics::new().unwrap();
        let app = router(
            AppState::new(store(&["a.txt"]), settings()),
            HttpMetrics::new(&metrics.meter(), 0),
        );
        send_to(app.clone(), Method::GET, "/a.txt").await;
        send_to(app, Method::GET, "/a.txt").await;
        let exposition = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(fetch_series(&exposition).is_empty(), "{exposition}");

        // The traffic total is still recorded, without path labels.
        let bytes = series(&exposition, BYTES);
        assert_eq!(bytes.len(), 1, "{exposition}");
        assert!(!bytes[0].contains("level1"), "{exposition}");
        assert!(bytes[0].ends_with(" 10"), "{exposition}");
    }

    #[tokio::test]
    async fn no_traffic_without_existence_check() {
        let metrics = crate::metrics::Metrics::new().unwrap();
        let app = router(
            AppState::new(
                store(&["a.txt"]),
                Settings {
                    check_exists: false,
                    ..settings()
                },
            ),
            HttpMetrics::new(&metrics.meter(), 1),
        );
        send_to(app, Method::GET, "/a.txt").await;
        let exposition = String::from_utf8(metrics.encode().unwrap()).unwrap();
        // The fetch is counted, but its size is unknown.
        assert_eq!(fetch_series(&exposition).len(), 1, "{exposition}");
        assert!(series(&exposition, BYTES).is_empty(), "{exposition}");
    }

    #[test]
    fn forwarded_client_ip_prefers_first_forwarded_entry() {
        let mut headers = HeaderMap::new();
        assert_eq!(forwarded_client_ip(&headers), None);
        headers.insert("x-real-ip", HeaderValue::from_static("10.0.0.2"));
        assert_eq!(forwarded_client_ip(&headers).as_deref(), Some("10.0.0.2"));
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
        );
        assert_eq!(
            forwarded_client_ip(&headers).as_deref(),
            Some("203.0.113.7")
        );
    }
}
