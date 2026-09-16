use crate::anthropic::schema::MessagesRequest;
use crate::monitor::MonitorHandle;
use crate::request_identity::ConversationIdentity;
use crate::traffic::TrafficCapture;
use anyhow::Result;
use async_trait::async_trait;
use axum::{body::Body, http::StatusCode, response::Response};
use bytes::Bytes;
use clap::Subcommand;
use std::sync::Arc;

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Sign in using browser-based authentication
    Login,
    /// Sign in using a device code
    Device,
    /// Show the current authentication status
    Status,
    /// Delete stored authentication credentials
    Logout,
}

/// Whether a backend can actually serve a request right now.
///
/// Listing a backend nobody is signed into is worse than not listing it: the
/// `/model` picker fills with ids that 400, and the unknown-model error grows
/// long enough to bury the ids that do work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Ready,
    /// Hidden from listings. `hint` says what to do about it, in one line.
    Unavailable {
        hint: String,
    },
}

impl Availability {
    pub fn unavailable(hint: impl Into<String>) -> Self {
        Self::Unavailable { hint: hint.into() }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// `Ready` when the credential file exists and holds something.
    pub fn from_file(path: &std::path::Path, hint: impl Into<String>) -> Self {
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > 0 => Self::Ready,
            _ => Self::unavailable(hint),
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Every model id this backend ACCEPTS. Drives routing, so a model missing
    /// here cannot be reached at all.
    fn supported_models(&self) -> Vec<String>;

    /// The subset worth OFFERING in `/v1/models`, the `models` banner and the
    /// picker. Defaults to everything accepted.
    ///
    /// These differ when an upstream drops a model but we still want the id to
    /// work: keeping it in `supported_models` preserves routing, while leaving
    /// it out here stops us offering something the backend would silently serve
    /// as a different model.
    fn advertised_models(&self) -> Vec<String> {
        self.supported_models()
    }
    fn cli(&self) -> &'static dyn CliHandlers;

    /// Whether this backend can serve a request right now.
    ///
    /// Runs when models are listed, never on the request path, so it must stay
    /// cheap and local: a file on disk, a binary on `PATH`, a socket that
    /// accepts a connection. Routing stays permissive either way — a backend
    /// that comes up mid-session still answers without a restart.
    fn availability(&self) -> Availability {
        Availability::Ready
    }
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let _ = conversation_identity;
        self.handle_messages(body, ctx).await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn generate_anthropic_stream(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        Err(ProviderError::new(
            StatusCode::NOT_IMPLEMENTED,
            ProviderErrorKind::InvalidRequest,
            format!(
                "provider '{}' does not support OpenAI-compatible generation",
                self.name()
            ),
        ))
    }
}

pub enum GenerationBody {
    BufferedSse(Bytes),
    LiveSse(Body),
}

pub struct Generation {
    pub body: GenerationBody,
    pub resolved_model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    Permission,
    RateLimit,
    InvalidRequest,
    Api,
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub status: StatusCode,
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retry_after: Option<String>,
    pub param: Option<String>,
    pub code: Option<String>,
}

impl ProviderError {
    pub fn new(status: StatusCode, kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
            param: None,
            code: None,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self.kind {
            ProviderErrorKind::Authentication => "authentication_error",
            ProviderErrorKind::Permission => "permission_error",
            ProviderErrorKind::RateLimit => "rate_limit_error",
            ProviderErrorKind::InvalidRequest => "invalid_request_error",
            ProviderErrorKind::Api => "api_error",
        }
    }
}

pub trait CliHandlers: Send + Sync {
    fn login(&self) -> Result<()>;
    fn device(&self) -> Result<()>;
    fn status(&self) -> Result<()>;
    fn logout(&self) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: String,
    pub traffic: Option<Arc<TrafficCapture>>,
    pub monitor: Option<MonitorHandle>,
    /// Raw request material for byte-passthrough providers (the Anthropic backend).
    /// Present on real HTTP requests; None in unit tests. Forwarding these verbatim
    /// keeps the prompt-cache prefix byte-identical.
    pub passthrough: Option<Passthrough>,
}

/// Untranslated request material needed to relay a request to an upstream verbatim.
#[derive(Debug, Clone)]
pub struct Passthrough {
    /// Original request body bytes, forwarded without reserialization.
    pub raw_body: axum::body::Bytes,
    /// Original client request headers (carry Authorization + anthropic-beta).
    pub headers: axum::http::HeaderMap,
    /// Original path and query, e.g. `/v1/messages?beta=true`.
    pub path_and_query: String,
}
