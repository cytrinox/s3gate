// SPDX-FileCopyrightText: 2026 Daniel Vogelbacher <daniel@chaospixel.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Fixed responses for configured URL paths (`overrides.yaml`), evaluated before key
//! mapping, the S3 metadata query and signing.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::keys::RESERVED_SEGMENT;

const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverridesFile {
    overrides: Vec<RawRule>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MatchKind {
    #[default]
    Exact,
    Prefix,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum RawRule {
    Redirect {
        path: String,
        #[serde(default, rename = "match")]
        match_kind: MatchKind,
        location: String,
        #[serde(default = "default_redirect_status")]
        status: u16,
        #[serde(default)]
        preserve_query: bool,
    },
    Notfound {
        path: String,
        #[serde(default, rename = "match")]
        match_kind: MatchKind,
    },
    Content {
        path: String,
        #[serde(default, rename = "match")]
        match_kind: MatchKind,
        #[serde(default)]
        body: String,
        #[serde(default = "default_content_type")]
        content_type: String,
    },
    Nocontent {
        path: String,
        #[serde(default, rename = "match")]
        match_kind: MatchKind,
    },
}

fn default_redirect_status() -> u16 {
    302
}

fn default_content_type() -> String {
    "text/plain; charset=utf-8".to_owned()
}

#[derive(Debug, thiserror::Error)]
pub enum OverridesError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid overrides file: {0}")]
    Parse(String),
    #[error("override entry {entry} ({path}): {reason}")]
    Invalid {
        entry: usize,
        path: String,
        reason: String,
    },
}

/// Override type, used as metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Redirect,
    NotFound,
    Content,
    NoContent,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Redirect => "redirect",
            Self::NotFound => "notfound",
            Self::Content => "content",
            Self::NoContent => "nocontent",
        }
    }
}

#[derive(Debug, Clone)]
enum Action {
    Redirect {
        location: String,
        status: StatusCode,
        preserve_query: bool,
    },
    NotFound,
    Content {
        body: String,
        content_type: HeaderValue,
    },
    NoContent,
}

/// A validated override rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// The configured path, with a trailing `*` for prefix rules; bounded metric label.
    label: Arc<str>,
    action: Action,
}

impl Rule {
    pub fn label(&self) -> &Arc<str> {
        &self.label
    }

    pub fn kind(&self) -> Kind {
        match self.action {
            Action::Redirect { .. } => Kind::Redirect,
            Action::NotFound => Kind::NotFound,
            Action::Content { .. } => Kind::Content,
            Action::NoContent => Kind::NoContent,
        }
    }

    /// Builds the configured response. `query` is the request's query string, used by
    /// redirects with `preserve_query`.
    pub fn respond(&self, query: Option<&str>) -> Response {
        match &self.action {
            Action::Redirect {
                location,
                status,
                preserve_query,
            } => {
                let target = match query.filter(|query| *preserve_query && !query.is_empty()) {
                    Some(query) => {
                        let separator = if location.contains('?') { '&' } else { '?' };
                        format!("{location}{separator}{query}")
                    }
                    None => location.clone(),
                };
                // Query strings come from a parsed URI, so this only fails on invalid
                // `location`s, which load() already rejected.
                let location =
                    HeaderValue::try_from(target).unwrap_or_else(|_| HeaderValue::from_static("/"));
                (*status, [(header::LOCATION, location)]).into_response()
            }
            Action::NotFound => (StatusCode::NOT_FOUND, "Not Found\n").into_response(),
            Action::Content { body, content_type } => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type.clone())],
                body.clone(),
            )
                .into_response(),
            Action::NoContent => StatusCode::NO_CONTENT.into_response(),
        }
    }
}

/// All rules, indexed for lookup.
#[derive(Debug, Default)]
pub struct Overrides {
    exact: HashMap<String, Rule>,
    /// Prefix rules, longest path first.
    prefixes: Vec<(String, Rule)>,
}

impl Overrides {
    pub fn load(path: &Path) -> Result<Self, OverridesError> {
        let yaml = std::fs::read_to_string(path).map_err(|source| OverridesError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&yaml)
    }

    pub fn parse(yaml: &str) -> Result<Self, OverridesError> {
        let file: OverridesFile =
            serde_saphyr::from_str(yaml).map_err(|err| OverridesError::Parse(err.to_string()))?;

        let mut overrides = Self::default();
        let mut seen = HashSet::new();
        for (index, raw) in file.overrides.into_iter().enumerate() {
            let (path, match_kind, action) =
                compile(raw).map_err(|(path, reason)| OverridesError::Invalid {
                    entry: index + 1,
                    path,
                    reason,
                })?;
            if !seen.insert((path.clone(), match_kind)) {
                return Err(OverridesError::Invalid {
                    entry: index + 1,
                    path,
                    reason: "duplicate path with the same match type".to_owned(),
                });
            }
            match match_kind {
                MatchKind::Exact => {
                    let rule = Rule {
                        label: Arc::from(path.as_str()),
                        action,
                    };
                    overrides.exact.insert(path, rule);
                }
                MatchKind::Prefix => {
                    let rule = Rule {
                        label: Arc::from(format!("{path}*")),
                        action,
                    };
                    overrides.prefixes.push((path, rule));
                }
            }
        }
        overrides
            .prefixes
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        Ok(overrides)
    }

    /// Exact rules win; otherwise the longest matching prefix.
    pub fn find(&self, decoded_path: &str) -> Option<&Rule> {
        self.exact.get(decoded_path).or_else(|| {
            self.prefixes
                .iter()
                .find(|(prefix, _)| decoded_path.starts_with(prefix.as_str()))
                .map(|(_, rule)| rule)
        })
    }

    /// Number of exact and prefix rules.
    pub fn counts(&self) -> (usize, usize) {
        (self.exact.len(), self.prefixes.len())
    }
}

/// Validates a raw rule; errors carry the rule's path and the reason.
fn compile(raw: RawRule) -> Result<(String, MatchKind, Action), (String, String)> {
    let (path, match_kind, action) = match raw {
        RawRule::Redirect {
            path,
            match_kind,
            location,
            status,
            preserve_query,
        } => {
            let action = (|| {
                if !(location.starts_with("http://")
                    || location.starts_with("https://")
                    || location.starts_with('/'))
                {
                    return Err("location must start with http://, https:// or /".to_owned());
                }
                HeaderValue::try_from(location.as_str())
                    .map_err(|_| "location is not a valid header value".to_owned())?;
                if !REDIRECT_STATUSES.contains(&status) {
                    return Err(format!(
                        "status {status} is not a redirect status (301, 302, 303, 307, 308)"
                    ));
                }
                let status = StatusCode::from_u16(status).map_err(|err| err.to_string())?;
                Ok(Action::Redirect {
                    location,
                    status,
                    preserve_query,
                })
            })();
            (path, match_kind, action)
        }
        RawRule::Notfound { path, match_kind } => (path, match_kind, Ok(Action::NotFound)),
        RawRule::Content {
            path,
            match_kind,
            body,
            content_type,
        } => {
            let action = HeaderValue::try_from(content_type.as_str())
                .ok()
                .filter(|_| !content_type.is_empty())
                .map(|content_type| Action::Content { body, content_type })
                .ok_or_else(|| "content_type is not a valid header value".to_owned());
            (path, match_kind, action)
        }
        RawRule::Nocontent { path, match_kind } => (path, match_kind, Ok(Action::NoContent)),
    };

    let checked = validate_path(&path).and(action);
    match checked {
        Ok(action) => Ok((path, match_kind, action)),
        Err(reason) => Err((path, reason)),
    }
}

fn validate_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err("path must start with /".to_owned());
    }
    if path.contains(['?', '#']) || path.chars().any(char::is_control) {
        return Err("path must not contain ?, # or control characters".to_owned());
    }
    let reserved = format!("/{RESERVED_SEGMENT}");
    if path == reserved || path.starts_with(&format!("{reserved}/")) {
        return Err(format!("paths under {reserved}/ are reserved"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    const EXAMPLE: &str = include_str!("../examples/overrides/overrides.yaml");

    async fn body(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn rule<'a>(overrides: &'a Overrides, path: &str) -> &'a Rule {
        overrides
            .find(path)
            .unwrap_or_else(|| panic!("no rule for {path}"))
    }

    #[tokio::test]
    async fn example_file_responses() {
        let overrides = Overrides::parse(EXAMPLE).unwrap();
        assert_eq!(overrides.counts(), (3, 1));

        let root = rule(&overrides, "/").respond(None);
        assert_eq!(root.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(root.headers()[header::LOCATION], "https://example.com/");

        let robots = rule(&overrides, "/robots.txt").respond(None);
        assert_eq!(robots.status(), StatusCode::OK);
        assert_eq!(
            robots.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(body(robots).await, "User-agent: *\nDisallow: /\n");

        let favicon = rule(&overrides, "/favicon.ico").respond(None);
        assert_eq!(favicon.status(), StatusCode::NO_CONTENT);

        let private = rule(&overrides, "/private/a/b.txt");
        assert_eq!(private.kind(), Kind::NotFound);
        assert_eq!(&**private.label(), "/private/*");
        assert_eq!(private.respond(None).status(), StatusCode::NOT_FOUND);

        assert!(overrides.find("/other.txt").is_none());
        assert!(overrides.find("/private").is_none());
    }

    #[tokio::test]
    async fn defaults() {
        let overrides = Overrides::parse(
            "overrides:\n  - {path: /r, type: redirect, location: /x}\n  - {path: /c, type: content}\n",
        )
        .unwrap();
        let redirect = rule(&overrides, "/r").respond(Some("a=1"));
        assert_eq!(redirect.status(), StatusCode::FOUND);
        assert_eq!(redirect.headers()[header::LOCATION], "/x");

        let content = rule(&overrides, "/c").respond(None);
        assert_eq!(content.status(), StatusCode::OK);
        assert_eq!(
            content.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(body(content).await, "");
    }

    #[test]
    fn preserve_query_appends_query_string() {
        let overrides = Overrides::parse(
            "overrides:\n  - {path: /a, type: redirect, location: 'https://x.test/', preserve_query: true}\n  - {path: /b, type: redirect, location: 'https://x.test/?s=1', preserve_query: true}\n",
        )
        .unwrap();
        let location = |path: &str, query: Option<&str>| {
            rule(&overrides, path).respond(query).headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(location("/a", Some("q=2")), "https://x.test/?q=2");
        assert_eq!(location("/b", Some("q=2")), "https://x.test/?s=1&q=2");
        assert_eq!(location("/a", None), "https://x.test/");
        assert_eq!(location("/a", Some("")), "https://x.test/");
    }

    #[test]
    fn exact_beats_prefix_and_longest_prefix_wins() {
        let overrides = Overrides::parse(
            "overrides:\n  - {path: /, match: prefix, type: nocontent}\n  - {path: /a/, match: prefix, type: notfound}\n  - {path: /a/b/, match: prefix, type: content}\n  - {path: /a/b/c, type: redirect, location: /x}\n",
        )
        .unwrap();
        assert_eq!(rule(&overrides, "/a/b/c").kind(), Kind::Redirect);
        assert_eq!(rule(&overrides, "/a/b/d").kind(), Kind::Content);
        assert_eq!(rule(&overrides, "/a/x").kind(), Kind::NotFound);
        assert_eq!(rule(&overrides, "/z").kind(), Kind::NoContent);
    }

    #[test]
    fn same_path_with_different_match_is_allowed() {
        let overrides = Overrides::parse(
            "overrides:\n  - {path: /a/, type: content}\n  - {path: /a/, match: prefix, type: notfound}\n",
        )
        .unwrap();
        assert_eq!(rule(&overrides, "/a/").kind(), Kind::Content);
        assert_eq!(rule(&overrides, "/a/b").kind(), Kind::NotFound);
    }

    #[test]
    fn rejects_invalid_files() {
        for (yaml, expected) in [
            (
                "overrides:\n  - {path: /a, type: nocontent, colour: red}\n",
                "colour",
            ),
            ("overrides:\n  - {path: /a, type: teapot}\n", "teapot"),
            ("overrides:\n  - {path: /a, type: redirect}\n", "location"),
            ("overrides: []\nextra: 1\n", "extra"),
            (
                "overrides:\n  - {path: /a, type: nocontent}\n  - {path: /a, type: notfound}\n",
                "duplicate",
            ),
            (
                "overrides:\n  - {path: a, type: nocontent}\n",
                "must start with /",
            ),
            (
                "overrides:\n  - {path: '/a?b', type: nocontent}\n",
                "must not contain",
            ),
            (
                "overrides:\n  - {path: /_s3gate/healthz, type: nocontent}\n",
                "reserved",
            ),
            (
                "overrides:\n  - {path: /_s3gate, type: nocontent}\n",
                "reserved",
            ),
            (
                "overrides:\n  - {path: /a, type: redirect, location: example.com}\n",
                "location must start",
            ),
            (
                "overrides:\n  - {path: /a, type: redirect, location: /x, status: 200}\n",
                "not a redirect status",
            ),
            (
                "overrides:\n  - {path: /a, type: content, content_type: ''}\n",
                "content_type",
            ),
        ] {
            let err = Overrides::parse(yaml).expect_err(yaml).to_string();
            assert!(err.contains(expected), "{yaml}: {err}");
        }
    }

    /// `overrides.schema.json` and the loader must accept and reject the same files.
    #[test]
    fn schema_and_loader_agree() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../overrides.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let schema_accepts = |yaml: &str| {
            let instance: serde_json::Value = serde_saphyr::from_str(yaml).unwrap();
            validator.is_valid(&instance)
        };

        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/overrides");
        let mut valid: Vec<String> = std::fs::read_dir(examples)
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        assert!(!valid.is_empty());
        valid.extend(
            [
                "overrides: []\n",
                "overrides:\n  - {path: /, match: prefix, type: notfound}\n",
                "overrides:\n  - {path: /r, type: redirect, location: 'http://x.test', status: 308}\n",
                "overrides:\n  - {path: /c, type: content, body: '<p>hi</p>', content_type: text/html}\n",
            ]
            .map(str::to_owned),
        );
        for yaml in &valid {
            assert!(schema_accepts(yaml), "schema rejects valid file:\n{yaml}");
            Overrides::parse(yaml).unwrap_or_else(|err| panic!("loader rejects:\n{yaml}\n{err}"));
        }

        // Everything the schema can express; duplicates are only caught by the loader.
        for yaml in [
            "{}\n",
            "overrides: []\nextra: 1\n",
            "overrides:\n  - {path: /a, type: nocontent, colour: red}\n",
            "overrides:\n  - {path: /a, type: teapot}\n",
            "overrides:\n  - {type: nocontent}\n",
            "overrides:\n  - {path: /a, type: redirect}\n",
            "overrides:\n  - {path: /a, type: notfound, body: x}\n",
            "overrides:\n  - {path: /a, match: glob, type: notfound}\n",
            "overrides:\n  - {path: a, type: nocontent}\n",
            "overrides:\n  - {path: '/a?b', type: nocontent}\n",
            "overrides:\n  - {path: '/a#b', type: nocontent}\n",
            "overrides:\n  - {path: \"/a\\tb\", type: nocontent}\n",
            "overrides:\n  - {path: /_s3gate, type: nocontent}\n",
            "overrides:\n  - {path: /_s3gate/x, type: nocontent}\n",
            "overrides:\n  - {path: /a, type: redirect, location: example.com}\n",
            "overrides:\n  - {path: /a, type: redirect, location: /x, status: 200}\n",
            "overrides:\n  - {path: /a, type: redirect, location: /x, preserve_query: 'yes'}\n",
            "overrides:\n  - {path: /a, type: content, content_type: ''}\n",
        ] {
            assert!(
                !schema_accepts(yaml),
                "schema accepts invalid file:\n{yaml}"
            );
            assert!(Overrides::parse(yaml).is_err(), "loader accepts:\n{yaml}");
        }
    }

    #[test]
    fn invalid_entry_errors_are_one_based() {
        let err = Overrides::parse(
            "overrides:\n  - {path: /a, type: nocontent}\n  - {path: b, type: nocontent}\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("override entry 2 (b):"), "{err}");
    }
}
