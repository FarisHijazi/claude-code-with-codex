//! Optional bearer-token gate on the proxy's own inbound routes.
//!
//! Disabled unless a token is configured, so an existing deployment keeps
//! working untouched. It exists because the proxy can bind to a non-loopback
//! address, at which point every `/v1/*` route would otherwise be open.
//!
//! # Why not `Authorization` alone
//!
//! The anthropic passthrough forwards the caller's `Authorization` header
//! verbatim to `api.anthropic.com`; that header carries Claude Code's
//! subscription token. Requiring a *proxy* token in the same slot would force
//! `ANTHROPIC_AUTH_TOKEN`, which overrides the subscription login and makes the
//! Claude route return 401. So the Claude Code path uses a dedicated header and
//! `Authorization` is merely *also accepted*, for plain OpenAI SDK clients that
//! have nowhere else to put a key.
//!
//! Match rule: accept when **any** presented credential matches. A
//! non-matching `Authorization` is not an error — that is precisely what lets
//! Claude Code send its Claude token and its proxy key on the same request.

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::anthropic::json_error;

/// Dedicated proxy credential. Does not collide with `Authorization`
/// (Claude subscription token) or `x-api-key` (Anthropic API key).
pub const PROXY_KEY_HEADER: &str = "x-claude-codex-key";

/// Routes that stay open even when a token is configured, so liveness probes
/// and the TUI monitor do not need the credential.
const UNAUTHENTICATED_PATHS: &[&str] = &["/healthz"];

#[derive(Debug, Clone)]
pub struct InboundAuth {
    token: Option<String>,
}

impl InboundAuth {
    pub fn new(token: Option<String>) -> Self {
        // An empty or whitespace-only token means "not configured" rather than
        // "a token everyone can guess".
        let token = token
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Self { token }
    }

    pub fn from_config() -> Self {
        Self::new(crate::config::inbound_auth_token())
    }

    pub fn is_enabled(&self) -> bool {
        self.token.is_some()
    }

    /// True when the request carries a credential matching the configured
    /// token, or when auth is disabled.
    pub fn authorize(&self, path: &str, headers: &HeaderMap) -> bool {
        let Some(expected) = self.token.as_deref() else {
            return true;
        };
        if UNAUTHENTICATED_PATHS.contains(&path) {
            return true;
        }
        presented_credentials(headers).any(|candidate| constant_time_eq(candidate, expected))
    }
}

/// Every place a caller may legitimately put the proxy credential.
fn presented_credentials(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    let proxy_key = headers
        .get(PROXY_KEY_HEADER)
        .and_then(|value| value.to_str().ok());
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(strip_bearer);
    proxy_key
        .into_iter()
        .chain(bearer)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn strip_bearer(value: &str) -> Option<&str> {
    let value = value.trim_start();
    let (scheme, rest) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| rest.trim_start())
}

/// Length-independent comparison. Compares over a fixed width so a mismatched
/// length does not short-circuit and leak the expected length by timing.
fn constant_time_eq(candidate: &str, expected: &str) -> bool {
    let candidate = candidate.as_bytes();
    let expected = expected.as_bytes();
    let mut diff = (candidate.len() ^ expected.len()) as u8;
    let width = candidate.len().max(expected.len());
    for index in 0..width {
        let lhs = candidate.get(index).copied().unwrap_or(0);
        let rhs = expected.get(index).copied().unwrap_or(0);
        diff |= lhs ^ rhs;
    }
    diff == 0
}

/// Axum middleware. Installed only when a token is configured, so the
/// unauthenticated path costs nothing.
pub async fn require_auth(
    axum::extract::State(auth): axum::extract::State<InboundAuth>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    if auth.authorize(&path, request.headers()) {
        return next.run(request).await;
    }
    json_error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        format!(
            "missing or invalid proxy credential; send it as `{PROXY_KEY_HEADER}: <token>` \
             (or `Authorization: Bearer <token>` for OpenAI-compatible clients)"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        map
    }

    #[test]
    fn disabled_auth_allows_everything() {
        let auth = InboundAuth::new(None);
        assert!(!auth.is_enabled());
        assert!(auth.authorize("/v1/messages", &HeaderMap::new()));
    }

    #[test]
    fn blank_token_is_treated_as_disabled() {
        for blank in ["", "   ", "\t"] {
            let auth = InboundAuth::new(Some(blank.to_string()));
            assert!(!auth.is_enabled(), "{blank:?} should not enable auth");
            assert!(auth.authorize("/v1/messages", &HeaderMap::new()));
        }
    }

    #[test]
    fn proxy_key_header_is_accepted() {
        let auth = InboundAuth::new(Some("s3cret".into()));
        assert!(auth.authorize("/v1/messages", &headers(&[(PROXY_KEY_HEADER, "s3cret")])));
        assert!(!auth.authorize("/v1/messages", &headers(&[(PROXY_KEY_HEADER, "wrong")])));
        assert!(!auth.authorize("/v1/messages", &HeaderMap::new()));
    }

    #[test]
    fn bearer_is_accepted_for_openai_clients() {
        let auth = InboundAuth::new(Some("s3cret".into()));
        assert!(auth.authorize(
            "/v1/chat/completions",
            &headers(&[("authorization", "Bearer s3cret")])
        ));
        // Scheme match is case-insensitive per RFC 7235.
        assert!(auth.authorize(
            "/v1/chat/completions",
            &headers(&[("authorization", "bearer s3cret")])
        ));
    }

    /// The reason the match rule is "any credential matches" rather than
    /// "the Authorization header must match": Claude Code sends its Claude
    /// subscription token in Authorization and cannot also put the proxy key
    /// there.
    #[test]
    fn claude_subscription_token_alongside_proxy_key_is_accepted() {
        let auth = InboundAuth::new(Some("s3cret".into()));
        assert!(auth.authorize(
            "/v1/messages",
            &headers(&[
                (
                    "authorization",
                    "Bearer sk-ant-oat-claude-subscription-token"
                ),
                (PROXY_KEY_HEADER, "s3cret"),
            ])
        ));
    }

    #[test]
    fn non_matching_authorization_alone_is_rejected() {
        let auth = InboundAuth::new(Some("s3cret".into()));
        assert!(!auth.authorize(
            "/v1/messages",
            &headers(&[(
                "authorization",
                "Bearer sk-ant-oat-claude-subscription-token"
            )])
        ));
    }

    #[test]
    fn healthz_stays_open() {
        let auth = InboundAuth::new(Some("s3cret".into()));
        assert!(auth.authorize("/healthz", &HeaderMap::new()));
        assert!(!auth.authorize("/v1/models", &HeaderMap::new()));
    }

    #[test]
    fn malformed_authorization_does_not_panic() {
        let auth = InboundAuth::new(Some("s3cret".into()));
        for value in ["", "Bearer", "Basic abc", "Bearer  ", "   "] {
            assert!(!auth.authorize("/v1/messages", &headers(&[("authorization", value)])));
        }
    }

    #[test]
    fn constant_time_eq_matches_semantics_of_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }
}
