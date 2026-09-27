// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

mod config;
mod keys;
mod metrics;
mod routes;
mod store;

use std::{future::IntoFuture, net::SocketAddr, process::ExitCode, sync::Arc};

use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::watch,
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::{
    config::{Config, LogFormat},
    metrics::{HttpMetrics, Metrics},
    routes::{AppState, Settings},
    store::S3Store,
};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("s3gate: configuration error: {err}");
            return ExitCode::FAILURE;
        }
    };
    init_tracing(config.log_format);

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "s3gate terminated");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Json => builder
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .init(),
        LogFormat::Text => builder.init(),
    }
}

async fn run(config: Config) -> std::io::Result<()> {
    info!(
        bucket = %config.bucket,
        prefix = %config.prefix,
        endpoint = config.endpoint.as_deref().unwrap_or("aws"),
        public_endpoint = config.public_endpoint.as_deref().unwrap_or("-"),
        region = %config.region,
        presign_ttl_s = config.presign_ttl.as_secs(),
        request_timeout_s = config.request_timeout.as_secs(),
        check_exists = config.check_exists,
        readiness_path_exists = config.readiness_path_exists.as_deref().unwrap_or("-"),
        metrics_enabled = config.metrics_enabled,
        "starting s3gate"
    );

    let state = AppState::new(S3Store::new(&config), Settings::from(&config));
    let listener = TcpListener::bind(config.listen).await?;
    info!(listen = %listener.local_addr()?, "listening");

    let (http_metrics, metrics_server) = if config.metrics_enabled {
        let metrics = Arc::new(Metrics::install()?);
        let http_metrics = HttpMetrics::new(&metrics.meter());
        let listener = TcpListener::bind(config.metrics_listen).await?;
        info!(listen = %listener.local_addr()?, "listening (metrics)");
        (http_metrics, Some((listener, metrics::router(metrics))))
    } else {
        (HttpMetrics::new(&metrics::disabled_meter()), None)
    };

    // One signal task drives a watch that every server observes.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let api = axum::serve(
        listener,
        routes::router(state, http_metrics).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()))
    .into_future();
    match metrics_server {
        Some((listener, router)) => {
            let metrics = axum::serve(listener, router)
                .with_graceful_shutdown(wait_for_shutdown(shutdown_rx))
                .into_future();
            tokio::try_join!(api, metrics)?;
        }
        None => api.await?,
    }

    info!("shutdown complete");
    Ok(())
}

async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

async fn shutdown_signal() {
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                error!(error = %err, "cannot install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
    info!("shutdown signal received, draining connections");
}
