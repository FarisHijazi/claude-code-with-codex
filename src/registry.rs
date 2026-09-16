use crate::{
    anthropic::{json_error, schema::MessagesRequest},
    config::AliasProvider,
    provider::{Availability, CliHandlers, Provider, RequestContext},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use axum::{http::StatusCode, response::Response};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "fable",
    "claude-fable-5",
];

pub const CURSOR_PREFIXES: &[&str] = &["cursor:", "cursor-plan:", "cursor-ask:"];

const CURSOR_LEGACY_MODELS: &[&str] = &[
    "cursor",
    "cursor-agent",
    "cursor-composer",
    "cursor-composer-fast",
    "cursor-plan",
    "cursor-ask",
    "composer-2.5",
    "composer-2.5-fast",
];

pub(crate) const CODEX_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
];

pub(crate) const KIMI_MODELS: &[&str] = &["kimi-for-coding", "kimi-k2.6", "kimi-k3", "k2.6", "k3"];
pub(crate) use crate::providers::gemini::models::GEMINI_MODELS;
pub(crate) const GROK_MODELS: &[&str] = &["grok-composer-2.5-fast", "grok-4.5"];

pub struct Registry {
    alias_provider: AliasProvider,
    /// Accepted ids -> provider. Routing reads this.
    models: BTreeMap<String, Vec<String>>,
    /// Offered ids. A subset of `models`; listings read this.
    advertised: BTreeMap<String, Vec<String>>,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
}

/// The offered-model map, derived from each handler's own answer.
fn advertised_from(handlers: &BTreeMap<String, Arc<dyn Provider>>) -> BTreeMap<String, Vec<String>> {
    handlers
        .iter()
        .map(|(name, provider)| (name.clone(), provider.advertised_models()))
        .collect()
}

impl Registry {
    pub fn new(alias_provider: AliasProvider) -> Self {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        models.insert(
            "anthropic".into(),
            ANTHROPIC_STYLE_ALIASES
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
        );
        models.insert("codex".into(), expand_codex_models());
        models.insert(
            "kimi".into(),
            KIMI_MODELS.iter().map(|m| (*m).to_string()).collect(),
        );
        models.insert("cursor".into(), build_cursor_models());
        models.insert(
            "gemini".into(),
            GEMINI_MODELS.iter().map(|m| (*m).to_string()).collect(),
        );
        // Discovered from `cursor-agent --list-models`, so this list reflects
        // whatever the installed CLI actually offers.
        models.insert(
            "cursor-cli".into(),
            crate::providers::cursor_cli::models::supported_models(),
        );
        models.insert(
            "grok".into(),
            GROK_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect(),
        );
        let mut handlers = BTreeMap::new();
        for (name, entries) in &models {
            let handler: Arc<dyn Provider> = match name.as_str() {
                "anthropic" => Arc::new(crate::providers::anthropic::AnthropicProvider::new()),
                "codex" => Arc::new(crate::providers::codex::CodexProvider::new()),
                "kimi" => Arc::new(crate::providers::kimi::KimiProvider::new()),
                "cursor" => Arc::new(crate::providers::cursor::CursorProvider::new()),
                "cursor-cli" => Arc::new(crate::providers::cursor_cli::CursorCliProvider::new()),
                "gemini" => Arc::new(crate::providers::gemini::GeminiProvider::new()),
                "grok" => Arc::new(crate::providers::grok::GrokProvider::new()),
                _ => Arc::new(PlaceholderProvider::new(name, entries.clone())),
            };
            handlers.insert(name.clone(), handler);
        }

        let advertised = advertised_from(&handlers);
        Self {
            alias_provider,
            models,
            advertised,
            handlers,
        }
    }

    pub fn with_default_alias() -> Self {
        Self::new(crate::config::alias_provider())
    }

    pub fn from_providers(
        alias_provider: AliasProvider,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
    ) -> Self {
        let mut models = BTreeMap::new();
        let mut handlers = BTreeMap::new();
        for provider in providers {
            let name = provider.name().to_string();
            models.insert(name.clone(), provider.supported_models());
            handlers.insert(name, provider);
        }
        let advertised = advertised_from(&handlers);
        Self {
            alias_provider,
            models,
            advertised,
            handlers,
        }
    }

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    /// Ids offered for a backend. Narrower than what it accepts — see
    /// `Provider::advertised_models`.
    pub fn advertised_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self.advertised.get(provider).cloned().unwrap_or_default();
        if provider == self.alias_provider.as_str() {
            for alias in ANTHROPIC_STYLE_ALIASES {
                if !models.iter().any(|value| value == alias) {
                    models.push((*alias).to_string());
                }
            }
        }
        models.sort_unstable();
        models
    }

    /// Backends that can serve a request right now, with why the others cannot.
    ///
    /// Probed on demand rather than cached: this runs only when models are
    /// listed, and a fresh answer means signing into a backend takes effect
    /// without restarting the proxy.
    pub fn availability(&self) -> BTreeMap<String, Availability> {
        self.handlers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.availability()))
            .collect()
    }

    /// Provider names worth listing — everything, if `CCP_SHOW_ALL_MODELS` is set.
    fn listable_providers(&self) -> Vec<String> {
        if crate::config::show_all_models() {
            return self.handlers.keys().cloned().collect();
        }
        self.handlers
            .iter()
            .filter(|(_, provider)| provider.availability().is_ready())
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in self.listable_providers() {
            for model in self.advertised_models_for(&provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for provider in self.listable_providers() {
            let models = self.advertised_models_for(&provider);
            out.insert(provider, models);
        }
        out
    }

    /// Every backend's models, available or not. For diagnostics only.
    pub fn grouped_models_all(&self) -> BTreeMap<String, Vec<String>> {
        self.handlers
            .keys()
            .map(|provider| (provider.clone(), self.advertised_models_for(provider)))
            .collect()
    }

    pub fn provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        // Claude-shaped models always resolve to the configured alias target (the
        // Anthropic passthrough by default). Session affinity is deliberately NOT
        // consulted here: a codex request earlier in the same session must never drag
        // the opus/haiku slots off the Anthropic backend. This is what lets the opus
        // slot stay on Max while the sonnet slot runs on codex within one session.
        let _ = session_affinity;
        if is_anthropic_alias(&normalized) || normalized.starts_with("claude-") {
            return self.handlers.get(self.alias_provider.as_str()).cloned();
        }
        // Checked before the API-backed cursor backend so a `cursor-cli*` id
        // can never be swallowed by a `cursor*` prefix match.
        if crate::providers::cursor_cli::models::is_cursor_cli_model(&normalized) {
            return self.handlers.get("cursor-cli").cloned();
        }
        if is_cursor_model(&normalized) {
            return self.handlers.get("cursor").cloned();
        }

        // Exact model-name match reaches a specific backend regardless of the alias
        // target: this is how `ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5.6-terra` sends the
        // sonnet slot to codex even while aliases default to the Anthropic passthrough.
        for (name, models) in &self.models {
            if name == "anthropic" {
                continue;
            }
            if models.iter().any(|candidate| candidate == &normalized) {
                return self.handlers.get(name).cloned();
            }
        }

        None
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            let mut models = models;
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        let mut message = format!("Supported: {}.", parts.join("; "));
        // Name what is missing and how to get it, or a signed-out backend looks
        // like a backend that was never built.
        let hidden: Vec<String> = self
            .availability()
            .into_iter()
            .filter_map(|(provider, state)| match state {
                Availability::Unavailable { hint } => Some(format!("{provider} ({hint})")),
                Availability::Ready => None,
            })
            .collect();
        if !hidden.is_empty() {
            message.push_str(&format!(" Not signed in: {}.", hidden.join("; ")));
        }
        message
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    ANTHROPIC_STYLE_ALIASES.contains(&model)
}

pub fn is_cursor_model(model: &str) -> bool {
    if CURSOR_LEGACY_MODELS.contains(&model) {
        return true;
    }

    CURSOR_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

struct PlaceholderProvider {
    name: &'static str,
    models: Vec<String>,
}

impl PlaceholderProvider {
    fn new(name: &str, models: Vec<String>) -> Self {
        let name = match name {
            "codex" => "codex",
            "kimi" => "kimi",
            "cursor" => "cursor",
            "cursor-cli" => "cursor-cli",
            "gemini" => "gemini",
            "grok" => "grok",
            _ => "codex",
        };
        Self { name, models }
    }
}

#[async_trait]
impl Provider for PlaceholderProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        match self.name {
            "codex" => &CODEX_CLI,
            "kimi" => &KIMI_CLI,
            "cursor" => &CURSOR_CLI,
            "cursor-cli" => &CURSOR_CLI_PLACEHOLDER,
            "gemini" => &GEMINI_PLACEHOLDER,
            "grok" => &GROK_CLI,
            _ => &CODEX_CLI,
        }
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("messages", &ctx.provider)
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("count_tokens", &ctx.provider)
    }
}

fn placeholder_provider_response(route: &str, provider: &str) -> Response {
    let _ = route;
    json_error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_provider_error",
        format!("provider '{}' is not yet implemented", provider),
    )
}

#[derive(Clone, Copy)]
struct PlaceholderCli {
    provider: &'static str,
}

impl CliHandlers for PlaceholderCli {
    fn login(&self) -> Result<()> {
        Err(anyhow!("{}: browser login not supported", self.provider))
    }

    fn device(&self) -> Result<()> {
        Err(anyhow!("{}: device login not supported", self.provider))
    }

    fn status(&self) -> Result<()> {
        use serde_json::Value;
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        if crate::auth::load_auth_file_with_legacy::<Value>(&path, &legacy).is_some() {
            Ok(())
        } else {
            Err(anyhow!("Not authenticated"))
        }
    }

    fn logout(&self) -> Result<()> {
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        let _ = crate::auth::delete_auth_file(&path, &legacy);
        Ok(())
    }
}

const CODEX_CLI: PlaceholderCli = PlaceholderCli { provider: "codex" };
const KIMI_CLI: PlaceholderCli = PlaceholderCli { provider: "kimi" };
const CURSOR_CLI: PlaceholderCli = PlaceholderCli { provider: "cursor" };
const GROK_CLI: PlaceholderCli = PlaceholderCli { provider: "grok" };
const CURSOR_CLI_PLACEHOLDER: PlaceholderCli = PlaceholderCli {
    provider: "cursor-cli",
};
const GEMINI_PLACEHOLDER: PlaceholderCli = PlaceholderCli { provider: "gemini" };
fn expand_codex_models() -> Vec<String> {
    let mut set = HashSet::new();
    let mut out = Vec::new();
    for model in CODEX_MODELS {
        if set.insert((*model).to_string()) {
            out.push((*model).to_string());
        }
        let fast = format!("{model}-fast");
        if set.insert(fast.clone()) {
            out.push(fast);
        }
    }
    out.sort_unstable();
    out
}

fn build_cursor_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    /// The failure this split exists to prevent: an id we advertise but cannot
    /// route, or one we drop from routing while still offering it.
    #[test]
    fn everything_advertised_can_actually_be_routed() {
        let registry = Registry::with_default_alias();
        for (provider, offered) in registry.grouped_models_all() {
            for model in offered {
                assert!(
                    registry.provider_for_model(&model, None).is_some(),
                    "{provider} offers {model} but it routes nowhere"
                );
            }
        }
    }

    #[test]
    fn gemini_thinking_ids_route_without_being_offered() {
        let registry = Registry::with_default_alias();
        let offered = registry.advertised_models_for("gemini");
        assert!(!offered.iter().any(|m| m.contains("thinking")));
        for model in [
            "gemini-3-flash-thinking",
            "gemini-3-flash-thinking-advanced",
        ] {
            let provider = registry
                .provider_for_model(model, None)
                .unwrap_or_else(|| panic!("{model} should still route"));
            assert_eq!(provider.name(), "gemini");
        }
    }

    use super::*;

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
    }

    #[test]
    fn alias_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Kimi);
        let p = registry.provider_for_model("haiku", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "kimi");
    }

    #[test]
    fn opus_4_8_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-4-8", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "claude-sonnet-5",
            "claude-opus-5",
            "fable",
            "claude-fable-5",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn claude_models_route_to_anthropic_passthrough_by_default() {
        let registry = Registry::new(AliasProvider::Anthropic);
        for model in [
            "opus",
            "claude-opus-4-8",
            "sonnet",
            "haiku",
            "claude-3-5-haiku-20241022",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route");
            assert_eq!(p.expect("provider").name(), "anthropic", "{model}");
        }
    }

    #[test]
    fn explicit_codex_model_routes_to_codex_while_default_is_anthropic() {
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("gpt-5.6-terra", None);
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn session_affinity_cannot_hijack_claude_slot() {
        // Even if a prior codex request set Codex affinity, claude aliases stay on anthropic.
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("claude-opus-4-8", Some(&AliasProvider::Codex));
        assert_eq!(p.expect("provider").name(), "anthropic");
    }

    #[test]
    fn cursor_prefix_routes() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("cursor:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-plan:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-ask:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
    }
}
