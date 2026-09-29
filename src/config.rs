// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Runtime configuration, read exclusively from `S3GATE_*` environment variables.

use std::{env, fmt, net::SocketAddr, time::Duration};

/// Longest lifetime SigV4 allows for a presigned URL.
const MAX_PRESIGN_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Upper bound for the fetch counter's path depth, as a guard against runaway cardinality.
const MAX_FETCH_LEVELS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Text,
}

#[derive(Clone)]
pub struct Config {
    pub bucket: String,
    /// Normalized to either empty or ending in `/`.
    pub prefix: String,
    pub endpoint: Option<String>,
    pub public_endpoint: Option<String>,
    pub force_path_style: bool,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub presign_ttl: Duration,
    pub query_metadata: bool,
    pub readiness_path_exists: Option<String>,
    pub readiness_timeout: Duration,
    pub request_timeout: Duration,
    pub listen: SocketAddr,
    pub metrics_enabled: bool,
    pub metrics_listen: SocketAddr,
    /// Path depth for the fetch counter; 0 disables it.
    pub metrics_fetch_levels: usize,
    pub log_format: LogFormat,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} must be set")]
    Missing(&'static str),
    #[error("{var} has invalid value {value:?}: {reason}")]
    Invalid {
        var: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    /// Builds the config from an arbitrary variable source; empty values count as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let required = |name: &'static str| get(name).ok_or(ConfigError::Missing(name));

        let config = Self {
            bucket: required("S3GATE_BUCKET")?,
            prefix: normalize_prefix(get("S3GATE_PREFIX").as_deref().unwrap_or("")),
            endpoint: get("S3GATE_ENDPOINT")
                .map(|value| parse_endpoint("S3GATE_ENDPOINT", value))
                .transpose()?,
            public_endpoint: get("S3GATE_PUBLIC_ENDPOINT")
                .map(|value| parse_endpoint("S3GATE_PUBLIC_ENDPOINT", value))
                .transpose()?,
            force_path_style: get("S3GATE_FORCE_PATH_STYLE")
                .map(|value| parse_bool("S3GATE_FORCE_PATH_STYLE", value))
                .transpose()?
                .unwrap_or(false),
            region: get("S3GATE_REGION").unwrap_or_else(|| "us-east-1".to_owned()),
            access_key_id: required("S3GATE_ACCESS_KEY_ID")?,
            secret_access_key: required("S3GATE_SECRET_ACCESS_KEY")?,
            session_token: get("S3GATE_SESSION_TOKEN"),
            presign_ttl: get("S3GATE_PRESIGN_TTL")
                .map(|value| parse_secs("S3GATE_PRESIGN_TTL", value, MAX_PRESIGN_TTL_SECS))
                .transpose()?
                .unwrap_or(Duration::from_secs(300)),
            query_metadata: get("S3GATE_QUERY_METADATA")
                .map(|value| parse_bool("S3GATE_QUERY_METADATA", value))
                .transpose()?
                .unwrap_or(true),
            readiness_path_exists: get("S3GATE_READINESS_PATH_EXISTS")
                .map(|value| value.trim_start_matches('/').to_owned())
                .filter(|value| !value.is_empty()),
            readiness_timeout: get("S3GATE_READINESS_TIMEOUT")
                .map(|value| parse_secs("S3GATE_READINESS_TIMEOUT", value, 60))
                .transpose()?
                .unwrap_or(Duration::from_secs(2)),
            request_timeout: get("S3GATE_REQUEST_TIMEOUT")
                .map(|value| parse_secs("S3GATE_REQUEST_TIMEOUT", value, 3600))
                .transpose()?
                .unwrap_or(Duration::from_secs(30)),
            listen: get("S3GATE_LISTEN")
                .map(|value| parse_addr("S3GATE_LISTEN", value))
                .transpose()?
                .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 8080))),
            metrics_enabled: get("S3GATE_METRICS_ENABLED")
                .map(|value| parse_bool("S3GATE_METRICS_ENABLED", value))
                .transpose()?
                .unwrap_or(false),
            metrics_listen: get("S3GATE_METRICS_LISTEN")
                .map(|value| parse_addr("S3GATE_METRICS_LISTEN", value))
                .transpose()?
                .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 9090))),
            metrics_fetch_levels: get("S3GATE_METRICS_FETCH_LEVELS")
                .map(|value| parse_count("S3GATE_METRICS_FETCH_LEVELS", value, MAX_FETCH_LEVELS))
                .transpose()?
                .unwrap_or(0),
            log_format: match get("S3GATE_LOG_FORMAT").as_deref() {
                None | Some("json") => LogFormat::Json,
                Some("text") => LogFormat::Text,
                Some(other) => {
                    return Err(ConfigError::Invalid {
                        var: "S3GATE_LOG_FORMAT",
                        value: other.to_owned(),
                        reason: "expected json or text",
                    });
                }
            },
        };

        if config.metrics_enabled && config.metrics_listen == config.listen {
            return Err(ConfigError::Invalid {
                var: "S3GATE_METRICS_LISTEN",
                value: config.metrics_listen.to_string(),
                reason: "must differ from S3GATE_LISTEN",
            });
        }
        Ok(config)
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("endpoint", &self.endpoint)
            .field("public_endpoint", &self.public_endpoint)
            .field("force_path_style", &self.force_path_style)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("presign_ttl", &self.presign_ttl)
            .field("query_metadata", &self.query_metadata)
            .field("readiness_path_exists", &self.readiness_path_exists)
            .field("readiness_timeout", &self.readiness_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("listen", &self.listen)
            .field("metrics_enabled", &self.metrics_enabled)
            .field("metrics_listen", &self.metrics_listen)
            .field("metrics_fetch_levels", &self.metrics_fetch_levels)
            .field("log_format", &self.log_format)
            .finish()
    }
}

fn normalize_prefix(raw: &str) -> String {
    let prefix = raw.trim_start_matches('/');
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_owned()
    } else {
        format!("{prefix}/")
    }
}

fn parse_endpoint(var: &'static str, value: String) -> Result<String, ConfigError> {
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(value.trim_end_matches('/').to_owned())
    } else {
        Err(ConfigError::Invalid {
            var,
            value,
            reason: "expected an http:// or https:// URL",
        })
    }
}

fn parse_addr(var: &'static str, value: String) -> Result<SocketAddr, ConfigError> {
    value.parse().map_err(|_| ConfigError::Invalid {
        var,
        value,
        reason: "expected a socket address such as 0.0.0.0:8080",
    })
}

fn parse_bool(var: &'static str, value: String) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Invalid {
            var,
            value,
            reason: "expected true or false",
        }),
    }
}

fn parse_count(var: &'static str, value: String, max: usize) -> Result<usize, ConfigError> {
    match value.parse::<usize>() {
        Ok(count) if count <= max => Ok(count),
        _ => Err(ConfigError::Invalid {
            var,
            value,
            reason: "expected a whole number within the allowed range",
        }),
    }
}

fn parse_secs(var: &'static str, value: String, max: u64) -> Result<Duration, ConfigError> {
    match value.parse::<u64>() {
        Ok(secs) if (1..=max).contains(&secs) => Ok(Duration::from_secs(secs)),
        _ => Err(ConfigError::Invalid {
            var,
            value,
            reason: "expected a whole number of seconds within the allowed range",
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|name| vars.get(name).cloned())
    }

    const MINIMAL: [(&str, &str); 3] = [
        ("S3GATE_BUCKET", "files"),
        ("S3GATE_ACCESS_KEY_ID", "AKID"),
        ("S3GATE_SECRET_ACCESS_KEY", "s3cr3t"),
    ];

    #[test]
    fn defaults() {
        let config = load(&MINIMAL).unwrap();
        assert_eq!(config.bucket, "files");
        assert_eq!(config.prefix, "");
        assert_eq!(config.endpoint, None);
        assert_eq!(config.region, "us-east-1");
        assert_eq!(config.presign_ttl, Duration::from_secs(300));
        assert!(config.query_metadata);
        assert!(!config.force_path_style);
        assert_eq!(config.readiness_path_exists, None);
        assert_eq!(config.readiness_timeout, Duration::from_secs(2));
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
        assert!(!config.metrics_enabled);
        assert_eq!(config.metrics_listen, "0.0.0.0:9090".parse().unwrap());
        assert_eq!(config.metrics_fetch_levels, 0);
        assert_eq!(config.log_format, LogFormat::Json);
    }

    #[test]
    fn missing_required() {
        assert_eq!(
            load(&[]).unwrap_err(),
            ConfigError::Missing("S3GATE_BUCKET")
        );
        assert_eq!(
            load(&MINIMAL[..2]).unwrap_err(),
            ConfigError::Missing("S3GATE_SECRET_ACCESS_KEY")
        );
        let empty_key = [MINIMAL[0], ("S3GATE_ACCESS_KEY_ID", "  "), MINIMAL[2]];
        assert_eq!(
            load(&empty_key).unwrap_err(),
            ConfigError::Missing("S3GATE_ACCESS_KEY_ID")
        );
    }

    #[test]
    fn ignores_aws_variables() {
        let vars = [
            ("S3GATE_BUCKET", "files"),
            ("AWS_ACCESS_KEY_ID", "AKID"),
            ("AWS_SECRET_ACCESS_KEY", "s3cr3t"),
        ];
        assert_eq!(
            load(&vars).unwrap_err(),
            ConfigError::Missing("S3GATE_ACCESS_KEY_ID")
        );
    }

    #[test]
    fn prefix_is_normalized() {
        for (raw, expected) in [("public", "public/"), ("/a/b/", "a/b/"), ("/", "")] {
            let mut vars = MINIMAL.to_vec();
            vars.push(("S3GATE_PREFIX", raw));
            assert_eq!(load(&vars).unwrap().prefix, expected, "prefix {raw:?}");
        }
    }

    #[test]
    fn parses_optional_values() {
        let mut vars = MINIMAL.to_vec();
        vars.extend([
            ("S3GATE_ENDPOINT", "http://minio:9000/"),
            ("S3GATE_PUBLIC_ENDPOINT", "https://s3.example.com"),
            ("S3GATE_FORCE_PATH_STYLE", "TRUE"),
            ("S3GATE_REGION", "eu-central-1"),
            ("S3GATE_SESSION_TOKEN", "tok"),
            ("S3GATE_PRESIGN_TTL", "60"),
            ("S3GATE_QUERY_METADATA", "off"),
            ("S3GATE_READINESS_PATH_EXISTS", "/.ready"),
            ("S3GATE_READINESS_TIMEOUT", "5"),
            ("S3GATE_REQUEST_TIMEOUT", "12"),
            ("S3GATE_LISTEN", "127.0.0.1:9999"),
            ("S3GATE_METRICS_ENABLED", "yes"),
            ("S3GATE_METRICS_LISTEN", "127.0.0.1:9100"),
            ("S3GATE_METRICS_FETCH_LEVELS", "2"),
            ("S3GATE_LOG_FORMAT", "text"),
        ]);
        let config = load(&vars).unwrap();
        assert_eq!(config.endpoint.as_deref(), Some("http://minio:9000"));
        assert_eq!(
            config.public_endpoint.as_deref(),
            Some("https://s3.example.com")
        );
        assert!(config.force_path_style);
        assert_eq!(config.region, "eu-central-1");
        assert_eq!(config.session_token.as_deref(), Some("tok"));
        assert_eq!(config.presign_ttl, Duration::from_secs(60));
        assert!(!config.query_metadata);
        assert_eq!(config.readiness_path_exists.as_deref(), Some(".ready"));
        assert_eq!(config.readiness_timeout, Duration::from_secs(5));
        assert_eq!(config.request_timeout, Duration::from_secs(12));
        assert_eq!(config.listen, "127.0.0.1:9999".parse().unwrap());
        assert!(config.metrics_enabled);
        assert_eq!(config.metrics_listen, "127.0.0.1:9100".parse().unwrap());
        assert_eq!(config.metrics_fetch_levels, 2);
        assert_eq!(config.log_format, LogFormat::Text);
    }

    #[test]
    fn rejects_invalid_values() {
        for (var, value) in [
            ("S3GATE_ENDPOINT", "minio:9000"),
            ("S3GATE_FORCE_PATH_STYLE", "maybe"),
            ("S3GATE_PRESIGN_TTL", "0"),
            ("S3GATE_PRESIGN_TTL", "604801"),
            ("S3GATE_PRESIGN_TTL", "5m"),
            ("S3GATE_REQUEST_TIMEOUT", "0"),
            ("S3GATE_REQUEST_TIMEOUT", "3601"),
            ("S3GATE_LISTEN", "8080"),
            ("S3GATE_METRICS_ENABLED", "sometimes"),
            ("S3GATE_METRICS_LISTEN", "9090"),
            ("S3GATE_METRICS_FETCH_LEVELS", "11"),
            ("S3GATE_METRICS_FETCH_LEVELS", "-1"),
            ("S3GATE_METRICS_FETCH_LEVELS", "two"),
            ("S3GATE_LOG_FORMAT", "xml"),
        ] {
            let mut vars = MINIMAL.to_vec();
            vars.push((var, value));
            match load(&vars) {
                Err(ConfigError::Invalid { var: got, .. }) => assert_eq!(got, var),
                other => panic!("{var}={value}: expected Invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn metrics_listen_must_differ_from_listen() {
        let mut vars = MINIMAL.to_vec();
        vars.extend([
            ("S3GATE_LISTEN", "0.0.0.0:9090"),
            ("S3GATE_METRICS_ENABLED", "true"),
        ]);
        match load(&vars) {
            Err(ConfigError::Invalid { var, .. }) => assert_eq!(var, "S3GATE_METRICS_LISTEN"),
            other => panic!("expected Invalid, got {other:?}"),
        }

        // Only a conflict when the metrics listener actually runs.
        vars.pop();
        assert!(load(&vars).is_ok());
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut vars = MINIMAL.to_vec();
        vars.push(("S3GATE_SESSION_TOKEN", "tok3n"));
        let rendered = format!("{:?}", load(&vars).unwrap());
        assert!(!rendered.contains("s3cr3t"));
        assert!(!rendered.contains("tok3n"));
        assert!(rendered.contains("<redacted>"));
    }
}
