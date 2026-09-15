//! Gemini backend, via a local [gemini-web-api] server.
//!
//! [gemini-web-api] exposes the gemini.google.com web app as an
//! OpenAI-compatible API, so this backend is a plain Anthropic <-> OpenAI
//! chat-completions translation over a configurable base URL. It needs no
//! credentials of its own: the Google session lives in that server.
//!
//! Chat only. The server can also generate images and Veo video, but the
//! Anthropic Messages surface has nowhere to express either, so those routes
//! are deliberately not wired up.
//!
//! Because the base URL is configurable, this also works against any other
//! OpenAI-compatible server.
//!
//! [gemini-web-api]: https://github.com/FarisHijazi/gemini-web-api

pub mod client;
pub mod models;
pub mod translate;

use std::convert::Infallible;

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::anthropic::{
    accumulate::accumulate_response,
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::monitor::{MonitorHandle, usage_from_anthropic_sse};
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};

use self::client::{GeminiClient, GeminiError};
use self::models::{GEMINI_DEFAULT_MODEL, GEMINI_MODELS, assert_allowed_model, resolve_model};
use self::translate::request::translate_request;
use self::translate::stream::StreamTranslator;

pub struct GeminiProvider;

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiProvider {
    pub fn new() -> Self {
        Self
    }

    /// Shared prologue: resolve the model and build the upstream request.
    fn prepare(
        body: &MessagesRequest,
        ctx: &RequestContext,
    ) -> Result<(String, translate::request::GeminiChatRequest), ProviderError> {
        let requested = body.model.as_deref().unwrap_or(GEMINI_DEFAULT_MODEL);
        let resolved = resolve_model(requested);
        assert_allowed_model(&resolved).map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!(
                    "Model \"{requested}\" resolves to unsupported model \"{}\"",
                    error.model
                ),
            )
        })?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved);
        }
        let translated = translate_request(body, &resolved).map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                error.to_string(),
            )
        })?;
        Ok((resolved, translated))
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    /// Reachability, not credentials: the Google session lives in the
    /// gemini-web-api server, so "is that server up" is the whole question.
    ///
    /// A TCP connect rather than an HTTP request — enough to tell a listening
    /// server from nothing at all, and bounded so a listing never hangs on it.
    fn availability(&self) -> crate::provider::Availability {
        if server_is_listening(&crate::config::gemini_base_url()) {
            crate::provider::Availability::Ready
        } else {
            crate::provider::Availability::unavailable(format!(
                "start gemini-web-api at {}",
                crate::config::gemini_base_url()
            ))
        }
    }

    fn name(&self) -> &'static str {
        "gemini"
    }

    fn supported_models(&self) -> Vec<String> {
        GEMINI_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &GEMINI_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let want_stream = body.stream;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        // gemini-web-api reports no usage, so carry an estimate for the
        // translator to fall back on. Same estimator as /count_tokens, so the
        // two agree.
        let estimated_input_tokens = crate::providers::kimi::count_tokens::count_tokens(&body);

        let (resolved, mut translated) = match Self::prepare(&body, &ctx) {
            Ok(prepared) => prepared,
            Err(error) => return provider_error_response(&error),
        };
        // Always stream upstream: one code path feeds both reply shapes, and
        // a long browser-backed turn is less likely to hit an idle timeout.
        translated.stream = true;

        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json(
                "020-upstream-request",
                &serde_json::to_value(&translated).unwrap_or_default(),
            );
        }

        let client = match GeminiClient::from_config() {
            Ok(client) => client,
            Err(error) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    format!("gemini client setup failed: {error}"),
                );
            }
        };

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match client.post_chat(&translated).await {
            Ok(response) => response,
            Err(error) => return gemini_error_response(&error),
        };

        if want_stream {
            return stream_anthropic_response(
                Box::pin(upstream.into_stream()),
                message_id,
                resolved,
                estimated_input_tokens,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
            );
        }

        let bytes = match upstream.into_bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return gemini_error_response(&error),
        };
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("032-upstream-response-body.sse", &bytes);
        }
        let mut translator = StreamTranslator::new(&message_id, &resolved)
            .with_fallback_input_tokens(estimated_input_tokens);
        let mut sse = translator.push_bytes(&bytes);
        translator.finish(&mut sse);
        match accumulate_response(&sse, &message_id, &resolved) {
            Ok(value) => {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.usage_updated(
                        &ctx.req_id,
                        value
                            .pointer("/usage/input_tokens")
                            .and_then(|v| v.as_u64()),
                        value
                            .pointer("/usage/output_tokens")
                            .and_then(|v| v.as_u64()),
                    );
                }
                (StatusCode::OK, Json(value)).into_response()
            }
            Err(error) => json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Accumulation error: {error}"),
            ),
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        // Gemini's tokenizer is not public; Claude Code only needs a
        // monotonic estimate for its compaction logic. Shared with kimi so
        // both backends estimate identically.
        let tokens = crate::providers::kimi::count_tokens::count_tokens(&body);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let (resolved, mut translated) = Self::prepare(&body, &ctx)?;
        translated.stream = true;

        let client = GeminiClient::from_config().map_err(|error| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Api,
                format!("gemini client setup failed: {error}"),
            )
        })?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = client
            .post_chat(&translated)
            .await
            .map_err(gemini_provider_error)?;
        let bytes = upstream.into_bytes().await.map_err(gemini_provider_error)?;
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("032-upstream-response-body.sse", &bytes);
        }
        let mut translator = StreamTranslator::new(&message_id, &resolved)
            .with_fallback_input_tokens(crate::providers::kimi::count_tokens::count_tokens(&body));
        let mut sse = translator.push_bytes(&bytes);
        translator.finish(&mut sse);
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("050-anthropic-intermediate.sse", &sse);
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse);
            monitor.stream_progress(
                &ctx.req_id,
                sse.len() as u64,
                sse.windows(6)
                    .filter(|window| **window == b"event:"[..])
                    .count() as u64,
                input_tokens,
                output_tokens,
            );
        }
        Ok(Generation {
            body: GenerationBody::BufferedSse(sse.into()),
            resolved_model: resolved,
        })
    }
}

/// Pipe upstream bytes through the translator as they arrive, so the caller
/// sees tokens at the same pace the backend produces them.
fn stream_anthropic_response<S>(
    upstream: S,
    message_id: String,
    model: String,
    fallback_input_tokens: u64,
    monitor: Option<MonitorHandle>,
    req_id: String,
) -> Response
where
    S: Stream<Item = Result<Bytes, GeminiError>> + Unpin + Send + 'static,
{
    struct State<S> {
        upstream: S,
        translator: StreamTranslator,
        monitor: Option<MonitorHandle>,
        req_id: String,
        started: bool,
        terminal: bool,
    }

    // `unfold` owns the upstream for the life of the response body.
    let state = State {
        upstream,
        translator: StreamTranslator::new(message_id, model)
            .with_fallback_input_tokens(fallback_input_tokens),
        monitor,
        req_id,
        started: false,
        terminal: false,
    };

    let stream = futures_util::stream::unfold(state, |mut state| async move {
        if state.terminal {
            return None;
        }
        loop {
            match state.upstream.next().await {
                Some(Ok(chunk)) => {
                    if !state.started {
                        state.started = true;
                        if let Some(monitor) = state.monitor.as_ref() {
                            monitor.generation_started(&state.req_id);
                        }
                    }
                    let out = state.translator.push_bytes(&chunk);
                    if let Some(monitor) = state.monitor.as_ref() {
                        monitor.stream_progress(&state.req_id, chunk.len() as u64, 1, None, None);
                    }
                    if state.translator.is_finished() {
                        state.terminal = true;
                        return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), state));
                    }
                    if out.is_empty() {
                        // Nothing complete yet; keep reading rather than
                        // emitting an empty frame.
                        continue;
                    }
                    return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), state));
                }
                // A dropped connection still gets a terminated message, so the
                // client is not left waiting on a stream that will never close.
                Some(Err(_)) | None => {
                    state.terminal = true;
                    let mut out = Vec::new();
                    state.translator.finish(&mut out);
                    if let Some(monitor) = state.monitor.as_ref() {
                        let (input_tokens, output_tokens) = usage_from_anthropic_sse(&out);
                        monitor.usage_updated(&state.req_id, input_tokens, output_tokens);
                    }
                    return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), state));
                }
            }
        }
    });

    (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

fn gemini_provider_error(error: GeminiError) -> ProviderError {
    let kind = match error.status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderErrorKind::Authentication,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimit,
        _ => ProviderErrorKind::Api,
    };
    let status = match error.status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    };
    let mut provider_error = ProviderError::new(status, kind, error.message);
    provider_error.retry_after = error.retry_after;
    provider_error
}

fn provider_error_response(error: &ProviderError) -> Response {
    json_error(error.status, error.error_type(), error.message.clone())
}

fn gemini_error_response(error: &GeminiError) -> Response {
    let provider_error = gemini_provider_error(GeminiError {
        status: error.status,
        message: error.message.clone(),
        retry_after: error.retry_after.clone(),
    });
    provider_error_response(&provider_error)
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct GeminiCli;

impl CliHandlers for GeminiCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "gemini has no login of its own: the Google session lives in the gemini-web-api \
             server. Start it (see https://github.com/FarisHijazi/gemini-web-api) and make sure \
             Chrome is signed in to gemini.google.com, then run `claude-codex gemini auth status`."
        )
    }

    fn device(&self) -> anyhow::Result<()> {
        self.login()
    }

    /// Reachability check rather than a credential check — that is the failure
    /// this backend actually has.
    fn status(&self) -> anyhow::Result<()> {
        let base_url = crate::config::gemini_base_url();
        let client = GeminiClient::new(base_url.clone(), crate::config::gemini_api_key())?;
        println!("Base URL: {base_url}");
        println!("Endpoint: {}", client.endpoint());
        println!(
            "API key: {}",
            if crate::config::gemini_api_key().is_some() {
                "configured"
            } else {
                "not set (the server accepts any key by default)"
            }
        );

        let models_url = format!("{}/models", base_url.trim_end_matches('/'));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let reachable = runtime.block_on(async {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .ok()?
                .get(&models_url)
                .send()
                .await
                .ok()
        });

        match reachable {
            Some(response) if response.status().is_success() => {
                println!("Server: reachable");
                Ok(())
            }
            Some(response) => {
                anyhow::bail!("server responded {} at {models_url}", response.status())
            }
            None => anyhow::bail!(
                "cannot reach the gemini-web-api server at {models_url}. Start it with \
                 `uvx --from git+https://github.com/FarisHijazi/gemini-web-api gemini-web-api`, \
                 or point CCP_GEMINI_BASE_URL somewhere else."
            ),
        }
    }

    fn logout(&self) -> anyhow::Result<()> {
        println!(
            "gemini stores no credentials in the proxy; sign out inside the gemini-web-api server."
        );
        Ok(())
    }
}

pub(crate) static GEMINI_CLI: GeminiCli = GeminiCli;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_advertises_its_models() {
        let provider = GeminiProvider::new();
        assert_eq!(provider.name(), "gemini");
        let models = provider.supported_models();
        assert!(models.contains(&"gemini-3-pro".to_string()));
        assert!(models.contains(&"gemini-3-flash".to_string()));
        assert_eq!(models.len(), GEMINI_MODELS.len());
    }

    /// Routing only sends gemini ids here, so a foreign id is not an error to
    /// report — it resolves to the default rather than failing a request that
    /// reached this backend deliberately (via an alias remap, say).
    #[test]
    fn foreign_model_id_resolves_to_the_default() {
        let body = MessagesRequest {
            model: Some("gpt-5.6-sol".into()),
            max_tokens: Some(16),
            messages: vec![],
            stream: false,
            bypass_provider_model_override: false,
            extra: serde_json::Map::new(),
        };
        let ctx = RequestContext {
            req_id: "req".into(),
            session_id: None,
            session_seq: None,
            provider: "gemini".into(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let (resolved, _) = GeminiProvider::prepare(&body, &ctx).expect("prepare");
        assert_eq!(resolved, GEMINI_DEFAULT_MODEL);
    }
}

/// Whether something accepts TCP connections at `base_url`'s host and port.
fn server_is_listening(base_url: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let Some((host, port)) = host_and_port(base_url) else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs.into_iter().any(|addr| {
        TcpStream::connect_timeout(&addr, Duration::from_millis(PROBE_TIMEOUT_MS)).is_ok()
    })
}

const PROBE_TIMEOUT_MS: u64 = 400;

/// Host and port from a base URL, defaulting the port by scheme.
fn host_and_port(base_url: &str) -> Option<(String, u16)> {
    let rest = base_url
        .split_once("://")
        .map(|(scheme, rest)| (scheme, rest))
        .unwrap_or(("http", base_url));
    let (scheme, rest) = rest;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    match authority.rsplit_once(':') {
        // Not a port if it is part of a bare IPv6 address.
        Some((host, port)) if !host.contains(':') => Some((host.to_string(), port.parse().ok()?)),
        _ => Some((
            authority.trim_matches(['[', ']']).to_string(),
            if scheme == "https" { 443 } else { 80 },
        )),
    }
}

#[cfg(test)]
mod availability_tests {
    use super::{host_and_port, server_is_listening};

    #[test]
    fn splits_host_and_port_out_of_a_base_url() {
        assert_eq!(
            host_and_port("http://localhost:8100/v1"),
            Some(("localhost".into(), 8100))
        );
        assert_eq!(
            host_and_port("https://gemini.example.com/v1"),
            Some(("gemini.example.com".into(), 443))
        );
        assert_eq!(
            host_and_port("http://example.com/v1"),
            Some(("example.com".into(), 80))
        );
    }

    #[test]
    fn nothing_listens_on_a_closed_port() {
        // Port 1 is reserved and never bound by this test suite.
        assert!(!server_is_listening("http://127.0.0.1:1/v1"));
    }
}
