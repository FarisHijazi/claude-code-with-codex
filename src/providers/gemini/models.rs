//! Model ids exposed by the gemini backend, and how an incoming name resolves.
//!
//! These mirror `gemini_openai/config.py` in the gemini-web-api server. The
//! proxy sends the resolved id straight through, so a server that adds a model
//! only needs the id listing here to advertise it over `/v1/models`.

/// Used when a request names the backend without a specific model, and as the
/// target for Claude-shaped aliases when gemini is the alias provider.
pub const GEMINI_DEFAULT_MODEL: &str = "gemini-3-pro";

/// Ids accepted by gemini-web-api. `-plus` / `-advanced` are subscription
/// tiers, not different families.
pub const GEMINI_MODELS: &[&str] = &[
    "gemini-3-pro",
    "gemini-3-pro-plus",
    "gemini-3-pro-advanced",
    "gemini-3-flash",
    "gemini-3-flash-plus",
    "gemini-3-flash-advanced",
    "gemini-3-flash-thinking",
    "gemini-3-flash-thinking-plus",
    "gemini-3-flash-thinking-advanced",
];

/// Models whose replies carry a reasoning stream worth surfacing as Anthropic
/// `thinking` blocks.
pub fn is_thinking_model(model: &str) -> bool {
    model.contains("thinking")
}

/// Resolve an incoming model name to an id the backend understands.
///
/// A Claude-shaped alias reaches this only when gemini is the configured alias
/// provider, in which case it maps to the default rather than failing.
pub fn resolve_model(model: &str) -> String {
    let normalized = model.trim();
    // Tolerate the `models/` prefix the Google SDKs use, as the server does.
    let normalized = normalized
        .strip_prefix("models/")
        .unwrap_or(normalized)
        .strip_prefix("gemini:")
        .unwrap_or_else(|| normalized.strip_prefix("models/").unwrap_or(normalized));

    if GEMINI_MODELS.contains(&normalized) {
        return normalized.to_string();
    }
    if crate::registry::is_anthropic_alias(normalized) || normalized.starts_with("claude-") {
        return GEMINI_DEFAULT_MODEL.to_string();
    }
    // An unrecognized but gemini-shaped id is forwarded verbatim so a server
    // running ahead of this list still works.
    if normalized.starts_with("gemini-") {
        return normalized.to_string();
    }
    GEMINI_DEFAULT_MODEL.to_string()
}

#[derive(Debug, Clone)]
pub struct ModelNotAllowedError {
    pub model: String,
}

/// Rejects only what is clearly not a gemini id, so the proxy does not have to
/// be redeployed every time the server gains a model.
pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if model.starts_with("gemini-") {
        return Ok(());
    }
    Err(ModelNotAllowedError {
        model: model.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ids_round_trip() {
        for model in GEMINI_MODELS {
            assert_eq!(&resolve_model(model), model);
            assert!(assert_allowed_model(model).is_ok());
        }
    }

    #[test]
    fn claude_aliases_fall_back_to_the_default() {
        for alias in ["opus", "sonnet", "haiku", "claude-opus-5", "claude-fable-5"] {
            assert_eq!(resolve_model(alias), GEMINI_DEFAULT_MODEL);
        }
    }

    #[test]
    fn google_style_prefix_is_tolerated() {
        assert_eq!(resolve_model("models/gemini-3-pro"), "gemini-3-pro");
        assert_eq!(resolve_model("gemini:gemini-3-flash"), "gemini-3-flash");
    }

    #[test]
    fn unknown_gemini_id_is_forwarded_verbatim() {
        // A server ahead of this build must still be reachable.
        assert_eq!(resolve_model("gemini-4-ultra"), "gemini-4-ultra");
        assert!(assert_allowed_model("gemini-4-ultra").is_ok());
    }

    #[test]
    fn non_gemini_id_is_rejected_after_resolution() {
        assert!(assert_allowed_model("gpt-5.6-sol").is_err());
    }

    #[test]
    fn thinking_models_are_detected() {
        assert!(is_thinking_model("gemini-3-flash-thinking"));
        assert!(is_thinking_model("gemini-3-flash-thinking-advanced"));
        assert!(!is_thinking_model("gemini-3-pro"));
    }
}
