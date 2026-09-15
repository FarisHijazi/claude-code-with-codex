//! End-to-end checks for what this fork adds: routing to the two new backends,
//! and the optional inbound credential.

use assert_cmd::Command;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use claude_codex::{
    config::AliasProvider,
    inbound_auth::{InboundAuth, PROXY_KEY_HEADER},
    registry::Registry,
    server::app,
};
use serde_json::Value;
use std::sync::Arc;
use tower::util::ServiceExt;

fn registry() -> Arc<Registry> {
    Arc::new(Registry::new(AliasProvider::Anthropic))
}

async fn models_payload() -> Value {
    let response = app(registry())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

fn model_ids(payload: &Value) -> Vec<String> {
    payload["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn gemini_models_are_advertised() {
    let ids = model_ids(&models_payload().await);
    for expected in ["gemini-3-pro", "gemini-3-flash", "gemini-3-flash-thinking"] {
        assert!(
            ids.contains(&expected.to_string()),
            "missing {expected}: {ids:?}"
        );
    }
}

#[tokio::test]
async fn cursor_cli_models_are_advertised() {
    let ids = model_ids(&models_payload().await);
    assert!(ids.contains(&"cursor-cli".to_string()), "{ids:?}");
}

/// Write-capable ids must not appear in `/model` unless write access is on,
/// so an agent cannot be given file access by picking it from a menu.
#[tokio::test]
async fn write_capable_cursor_cli_ids_are_hidden_by_default() {
    if claude_codex::config::cursor_cli_allow_write() {
        return;
    }
    let ids = model_ids(&models_payload().await);
    assert!(
        !ids.iter().any(|id| id.starts_with("cursor-cli-agent:")),
        "write ids leaked into the model list: {ids:?}"
    );
}

#[test]
fn gemini_and_cursor_cli_resolve_to_their_own_providers() {
    let registry = registry();
    for model in [
        "gemini-3-pro",
        "gemini-3-flash",
        "gemini-3-flash-thinking-advanced",
    ] {
        let provider = registry
            .provider_for_model(model, None)
            .unwrap_or_else(|| panic!("{model} should route"));
        assert_eq!(provider.name(), "gemini", "{model}");
    }

    for model in [
        "cursor-cli",
        "cursor-cli:composer-2.5",
        "cursor-cli-plan:gpt-5.3-codex",
        "cursor-cli-agent:auto",
    ] {
        let provider = registry
            .provider_for_model(model, None)
            .unwrap_or_else(|| panic!("{model} should route"));
        assert_eq!(provider.name(), "cursor-cli", "{model}");
    }
}

/// `cursor-cli*` must not be captured by the API-backed `cursor` backend.
#[test]
fn cursor_cli_does_not_collide_with_the_cursor_backend() {
    let registry = registry();
    assert_eq!(
        registry
            .provider_for_model("cursor:gpt-5.5", None)
            .expect("cursor routes")
            .name(),
        "cursor"
    );
    assert_eq!(
        registry
            .provider_for_model("cursor-cli:gpt-5.5", None)
            .expect("cursor-cli routes")
            .name(),
        "cursor-cli"
    );
}

/// Adding backends must not move the Claude slots off the Anthropic
/// passthrough, which is the whole point of the fork.
#[test]
fn claude_aliases_still_route_to_anthropic() {
    let registry = registry();
    for model in ["opus", "sonnet", "haiku", "claude-opus-5", "claude-fable-5"] {
        assert_eq!(
            registry
                .provider_for_model(model, None)
                .expect("routes")
                .name(),
            "anthropic",
            "{model}"
        );
    }
}

#[tokio::test]
async fn routes_are_open_when_no_token_is_configured() {
    // The default build configures no token, matching existing deployments.
    let response = app(registry())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn configured_token_gates_v1_but_not_healthz() {
    let auth = InboundAuth::new(Some("s3cret".into()));
    let mut headers = axum::http::HeaderMap::new();
    assert!(auth.authorize("/healthz", &headers));
    assert!(!auth.authorize("/v1/models", &headers));

    headers.insert(
        axum::http::HeaderName::from_static(PROXY_KEY_HEADER),
        axum::http::HeaderValue::from_static("s3cret"),
    );
    assert!(auth.authorize("/v1/models", &headers));
}

/// The banner is built from the registry, but upstream pins the order of the
/// backends it already had. Both properties matter: an earlier version listed
/// providers from a hardcoded array and silently omitted both new backends,
/// and the fix for that reordered the list and broke upstream's own test.
///
/// Asserted with every backend shown, so this stays a test about *order* — on a
/// machine signed into only some of them the listing is legitimately shorter.
#[test]
fn models_banner_lists_new_backends_after_the_upstream_order() {
    let output = Command::cargo_bin("claude-codex")
        .expect("binary")
        .args(["models"])
        .env("CCP_SHOW_ALL_MODELS", "1")
        .output()
        .expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf8");

    let position = |needle: &str| {
        stdout
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{stdout}"))
    };

    // Upstream's order, unchanged.
    assert!(position("codex:") < position("kimi:"));
    assert!(position("kimi:") < position("grok:"));
    assert!(position("grok:") < position("cursor:"));
    // This fork's backends, appended rather than interleaved.
    assert!(position("cursor:") < position("cursor-cli:"));
    assert!(position("cursor-cli:") < position("gemini:"));
    // The anthropic passthrough is deliberately not a banner line.
    assert!(!stdout.contains("anthropic:"), "{stdout}");
}
