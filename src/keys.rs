// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Maps request paths onto object keys inside the configured prefix.

use percent_encoding::percent_decode_str;

/// First path segment reserved for s3gate's own endpoints; never mapped to objects.
pub const RESERVED_SEGMENT: &str = "_s3gate";

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("path is not valid percent-encoded UTF-8")]
    BadEncoding,
    #[error("path does not name an object")]
    NotAnObject,
    #[error("path contains an empty, '.' or '..' segment")]
    UnsafeSegment,
    #[error("path contains control characters")]
    ControlCharacter,
    #[error("path is in the reserved /{RESERVED_SEGMENT}/ namespace")]
    Reserved,
}

impl KeyError {
    /// Short, bounded label for metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BadEncoding => "bad_encoding",
            Self::NotAnObject => "not_an_object",
            Self::UnsafeSegment => "unsafe_segment",
            Self::ControlCharacter => "control_character",
            Self::Reserved => "reserved",
        }
    }
}

/// Decodes a raw request path and joins it onto `prefix`.
///
/// Segment checks run after decoding, so `%2e%2e` and `%2F` cannot be used to escape the
/// prefix, even on S3-compatible stores that normalize keys.
pub fn map_path(prefix: &str, path: &str) -> Result<String, KeyError> {
    let decoded = percent_decode_str(path)
        .decode_utf8()
        .map_err(|_| KeyError::BadEncoding)?;
    let relative = decoded.strip_prefix('/').unwrap_or(&decoded);

    if relative.is_empty() || relative.ends_with('/') {
        return Err(KeyError::NotAnObject);
    }
    if relative.chars().any(char::is_control) {
        return Err(KeyError::ControlCharacter);
    }
    if relative
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(KeyError::UnsafeSegment);
    }
    if relative.split('/').next() == Some(RESERVED_SEGMENT) {
        return Err(KeyError::Reserved);
    }

    Ok(format!("{prefix}{relative}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_plain_paths() {
        assert_eq!(map_path("", "/hello.txt").unwrap(), "hello.txt");
        assert_eq!(map_path("", "/a/b/c.bin").unwrap(), "a/b/c.bin");
        assert_eq!(map_path("public/", "/a/b.png").unwrap(), "public/a/b.png");
    }

    #[test]
    fn decodes_percent_encoding() {
        assert_eq!(
            map_path("", "/my%20file%C3%A4.txt").unwrap(),
            "my fileä.txt"
        );
        assert_eq!(map_path("", "/a%2Fb").unwrap(), "a/b");
        assert_eq!(map_path("", "/%FF"), Err(KeyError::BadEncoding));
    }

    #[test]
    fn rejects_non_objects() {
        for path in ["", "/", "/dir/", "/a/b/"] {
            assert_eq!(map_path("p/", path), Err(KeyError::NotAnObject), "{path}");
        }
    }

    #[test]
    fn rejects_traversal() {
        for path in [
            "/../etc/passwd",
            "/a/../../b",
            "/a/./b",
            "/a//b",
            "/%2e%2e/secret",
            "/a%2F..%2Fb",
            "//b",
            "/..",
        ] {
            assert_eq!(map_path("p/", path), Err(KeyError::UnsafeSegment), "{path}");
        }
    }

    #[test]
    fn rejects_control_characters() {
        assert_eq!(map_path("", "/a%0Ab"), Err(KeyError::ControlCharacter));
        assert_eq!(map_path("", "/a%00"), Err(KeyError::ControlCharacter));
    }

    #[test]
    fn kinds_are_stable_metric_labels() {
        assert_eq!(KeyError::BadEncoding.kind(), "bad_encoding");
        assert_eq!(KeyError::NotAnObject.kind(), "not_an_object");
        assert_eq!(KeyError::UnsafeSegment.kind(), "unsafe_segment");
        assert_eq!(KeyError::ControlCharacter.kind(), "control_character");
        assert_eq!(KeyError::Reserved.kind(), "reserved");
    }

    #[test]
    fn rejects_reserved_namespace() {
        assert_eq!(map_path("", "/_s3gate"), Err(KeyError::Reserved));
        assert_eq!(map_path("", "/_s3gate/healthz"), Err(KeyError::Reserved));
        assert_eq!(map_path("", "/%5Fs3gate/x"), Err(KeyError::Reserved));
        assert_eq!(map_path("", "/_s3gatex").unwrap(), "_s3gatex");
    }
}
