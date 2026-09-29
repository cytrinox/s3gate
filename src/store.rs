// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! S3 access: existence checks and presigned URL generation.

use std::time::Duration;

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region, http::HttpResponse, timeout::TimeoutConfig},
    error::{DisplayErrorContext, SdkError},
    operation::head_object::HeadObjectError,
    presigning::PresigningConfig,
};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresignMethod {
    Get,
    Head,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    #[error("object not found")]
    NotFound,
    #[error("access denied by S3")]
    Forbidden,
    #[error("S3 request timed out")]
    Timeout,
    #[error("S3 request failed: {0}")]
    Upstream(String),
    #[error("presigning failed: {0}")]
    Presign(String),
}

impl StoreError {
    /// Short, bounded label for metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Forbidden => "forbidden",
            Self::Timeout => "timeout",
            Self::Upstream(_) | Self::Presign(_) => "error",
        }
    }
}

pub trait ObjectStore: Send + Sync + 'static {
    /// Succeeds if the object exists.
    fn head(&self, key: &str) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Returns a presigned URL for `method` on `key`, valid for `ttl`.
    fn presign(
        &self,
        method: PresignMethod,
        key: &str,
        ttl: Duration,
    ) -> impl Future<Output = Result<String, StoreError>> + Send;
}

pub struct S3Store {
    bucket: String,
    /// Talks to the internal endpoint.
    client: Client,
    /// Signs URLs for the endpoint clients will follow; never sends requests itself.
    presigner: Client,
}

impl S3Store {
    pub fn new(config: &Config) -> Self {
        let client = build_client(config, config.endpoint.as_deref());
        let presigner = match config.public_endpoint.as_deref() {
            Some(public) => build_client(config, Some(public)),
            None => client.clone(),
        };
        Self {
            bucket: config.bucket.clone(),
            client,
            presigner,
        }
    }
}

/// Builds a client without `aws-config`, so no `AWS_*` variables, profiles or IMDS are consulted.
fn build_client(config: &Config, endpoint: Option<&str>) -> Client {
    let credentials = Credentials::new(
        &config.access_key_id,
        &config.secret_access_key,
        config.session_token.clone(),
        None,
        "s3gate",
    );
    let mut builder = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(config.region.clone()))
        .credentials_provider(credentials)
        .force_path_style(config.force_path_style)
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(3))
                .operation_timeout(Duration::from_secs(10))
                .build(),
        );
    if let Some(endpoint) = endpoint {
        builder = builder.endpoint_url(endpoint);
    }
    Client::from_conf(builder.build())
}

impl ObjectStore for S3Store {
    async fn head(&self, key: &str) -> Result<(), StoreError> {
        self.client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map(|_| ())
            .map_err(classify_head_error)
    }

    async fn presign(
        &self,
        method: PresignMethod,
        key: &str,
        ttl: Duration,
    ) -> Result<String, StoreError> {
        let presigning = PresigningConfig::expires_in(ttl)
            .map_err(|err| StoreError::Presign(err.to_string()))?;
        let request = match method {
            PresignMethod::Get => self
                .presigner
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .presigned(presigning)
                .await
                .map_err(|err| StoreError::Presign(DisplayErrorContext(&err).to_string()))?,
            PresignMethod::Head => self
                .presigner
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .presigned(presigning)
                .await
                .map_err(|err| StoreError::Presign(DisplayErrorContext(&err).to_string()))?,
        };
        Ok(request.uri().to_owned())
    }
}

fn classify_head_error(err: SdkError<HeadObjectError, HttpResponse>) -> StoreError {
    match &err {
        SdkError::TimeoutError(_) => StoreError::Timeout,
        SdkError::DispatchFailure(failure) if failure.is_timeout() => StoreError::Timeout,
        SdkError::ServiceError(ctx) if ctx.err().is_not_found() => StoreError::NotFound,
        SdkError::ServiceError(ctx) => match ctx.raw().status().as_u16() {
            404 => StoreError::NotFound,
            403 => StoreError::Forbidden,
            _ => StoreError::Upstream(DisplayErrorContext(&err).to_string()),
        },
        _ => StoreError::Upstream(DisplayErrorContext(&err).to_string()),
    }
}
