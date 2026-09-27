// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

mod config;
mod keys;
mod routes;
mod store;

use std::{net::SocketAddr, process::ExitCode};

use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::{
    config::{Config, LogFormat},
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
        "starting s3gate"
    );

    let state = AppState::new(S3Store::new(&config), Settings::from(&config));
    let listener = TcpListener::bind(config.listen).await?;
    info!(listen = %listener.local_addr()?, "listening");

    axum::serve(
        listener,
        routes::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    info!("shutdown complete");
    Ok(())
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
