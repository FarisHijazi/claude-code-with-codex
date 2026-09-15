//! `cursor-cli` backend — the locally installed `cursor-agent` binary.
//!
//! Distinct from the `cursor` backend, which speaks Cursor's private API and
//! keeps its own login. This one shells out to the CLI the user has already
//! signed in, so it needs no credentials of its own.
//!
//! # Semantics: this backend is an agent, not a model
//!
//! `cursor-agent` runs its own tool loop inside its own workspace. It cannot be
//! asked to emit a tool call and pause for an external executor, so Claude
//! Code's tools are not advertised and no `tool_use` block is ever returned.
//! One Claude Code turn delegates the whole turn to a Cursor agent and gets its
//! final answer back; what the agent did along the way is surfaced as progress
//! so the turn is not opaque.
//!
//! # Safety
//!
//! Read-only (`--mode ask`) is the default, because a write-capable run can
//! edit files without Claude Code mediating it. Writing requires both the
//! explicit `cursor-cli-agent:` prefix and `cursorCli.allowWrite` in config.

pub mod events;
pub mod models;
pub mod process;
pub mod prompt;
pub mod workspace;

use std::convert::Infallible;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use tokio::io::AsyncReadExt;

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

use self::events::EventTranslator;
use self::models::{ParsedModel, parse_model};
use self::process::{RunOptions, spawn};
use self::prompt::render_prompt;

pub struct CursorCliProvider;

impl Default for CursorCliProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorCliProvider {
    pub fn new() -> Self {
        Self
    }
}

/// Reject a write-capable run unless it has been explicitly enabled.
///
/// The prefix alone is not enough: picking `cursor-cli-agent:` from a `/model`
/// menu should not be sufficient to let an agent edit the working tree.
fn check_write_permission(parsed: &ParsedModel) -> Result<(), ProviderError> {
    if !parsed.mode.is_write() || crate::config::cursor_cli_allow_write() {
        return Ok(());
    }
    Err(ProviderError::new(
        StatusCode::BAD_REQUEST,
        ProviderErrorKind::Permission,
        "cursor-cli-agent runs cursor-agent with write and shell access to the workspace. \
         It is disabled by default. Enable it with CCP_CURSOR_CLI_ALLOW_WRITE=1, or \
         `{\"cursorCli\":{\"allowWrite\":true}}` in the proxy config, and consider setting \
         cursorCli.workspace to scope it. Use cursor-cli: or cursor-cli-plan: for read-only runs."
            .to_string(),
    ))
}

#[async_trait]
impl Provider for CursorCliProvider {
    /// The CLI has to be there to spawn. Whether it is signed in is left to the
    /// run itself — asking would cost a subprocess every time models are listed.
    fn availability(&self) -> crate::provider::Availability {
        if process::binary_is_runnable(&crate::config::cursor_cli_binary()) {
            crate::provider::Availability::Ready
        } else {
            crate::provider::Availability::unavailable(
                "install the Cursor CLI (https://cursor.com/cli) and run `cursor-agent login`",
            )
        }
    }

    fn name(&self) -> &'static str {
        "cursor-cli"
    }

    fn supported_models(&self) -> Vec<String> {
        models::supported_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &CURSOR_CLI_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.as_deref().unwrap_or("cursor-cli");
        let parsed = parse_model(requested);
        if let Err(error) = check_write_permission(&parsed) {
            return json_error(error.status, error.error_type(), error.message);
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &parsed.model);
        }

        let options = RunOptions::from_request(&parsed, &body);
        let prompt = render_prompt(&body);
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("020-cursor-cli-prompt.txt", prompt.as_bytes());
        }

        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        let agent = match spawn(&options, &prompt) {
            Ok(agent) => agent,
            Err(error) => {
                return json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    error.to_string(),
                );
            }
        };

        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let model_label = requested.to_string();

        if body.stream {
            return stream_agent_response(
                agent,
                options.timeout,
                message_id,
                model_label,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
            );
        }

        let sse = match collect_agent_sse(agent, options.timeout, &message_id, &model_label).await {
            Ok(sse) => sse,
            Err(error) => return json_error(StatusCode::BAD_GATEWAY, "api_error", error),
        };
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("050-anthropic-intermediate.sse", &sse);
        }
        match accumulate_response(&sse, &message_id, &model_label) {
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
        body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        let requested = body.model.as_deref().unwrap_or("cursor-cli");
        let parsed = parse_model(requested);
        check_write_permission(&parsed)?;
        let options = RunOptions::from_request(&parsed, &body);
        let prompt = render_prompt(&body);
        let agent = spawn(&options, &prompt).map_err(|error| {
            ProviderError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ProviderErrorKind::Api,
                error.to_string(),
            )
        })?;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let sse = collect_agent_sse(agent, options.timeout, &message_id, requested)
            .await
            .map_err(|error| {
                ProviderError::new(StatusCode::BAD_GATEWAY, ProviderErrorKind::Api, error)
            })?;
        if let Some(monitor) = ctx.monitor.as_ref() {
            let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse);
            monitor.stream_progress(
                &ctx.req_id,
                sse.len() as u64,
                1,
                input_tokens,
                output_tokens,
            );
        }
        Ok(Generation {
            body: GenerationBody::BufferedSse(sse.into()),
            resolved_model: parsed.model,
        })
    }
}

/// Run to completion and return the full Anthropic SSE.
async fn collect_agent_sse(
    mut agent: process::RunningAgent,
    timeout: Duration,
    message_id: &str,
    model: &str,
) -> Result<Vec<u8>, String> {
    let mut translator = EventTranslator::new(message_id, model);
    let mut out = Vec::new();
    let mut buffer = vec![0u8; 8192];

    let read_loop = async {
        loop {
            match agent.stdout.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => out.extend(translator.push_bytes(&buffer[..read])),
                Err(error) => return Err(format!("cursor-agent stdout read failed: {error}")),
            }
        }
        Ok(())
    };

    match tokio::time::timeout(timeout, read_loop).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            agent.kill().await;
            translator.fail(
                &format!("cursor-agent timed out after {}s", timeout.as_secs()),
                &mut out,
            );
            return Ok(out);
        }
    }

    if !translator.is_finished() {
        // No `result` event: the process died or printed nothing useful.
        let stderr = agent.stderr_tail().await;
        let detail = if stderr.is_empty() {
            "cursor-agent exited without producing a result".to_string()
        } else {
            format!("cursor-agent failed: {stderr}")
        };
        translator.fail(&detail, &mut out);
    } else {
        agent.wait().await;
    }
    Ok(out)
}

/// Stream the agent's output as it arrives, so a long run is visible while it
/// is still going.
fn stream_agent_response(
    agent: process::RunningAgent,
    timeout: Duration,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
) -> Response {
    struct State {
        agent: Option<process::RunningAgent>,
        translator: EventTranslator,
        buffer: Vec<u8>,
        monitor: Option<MonitorHandle>,
        req_id: String,
        deadline: tokio::time::Instant,
        started: bool,
        terminal: bool,
    }

    let state = State {
        agent: Some(agent),
        translator: EventTranslator::new(message_id, model),
        buffer: vec![0u8; 8192],
        monitor,
        req_id,
        deadline: tokio::time::Instant::now() + timeout,
        started: false,
        terminal: false,
    };

    let stream = futures_util::stream::unfold(state, move |mut state| async move {
        if state.terminal {
            return None;
        }
        loop {
            // The agent handle is taken only on a path that also ends the
            // stream, so reaching here means there is nothing left to read.
            let agent = state.agent.as_mut()?;

            let read =
                tokio::time::timeout_at(state.deadline, agent.stdout.read(&mut state.buffer)).await;

            match read {
                Ok(Ok(0)) => {
                    // Process closed stdout.
                    state.terminal = true;
                    let mut out = Vec::new();
                    if !state.translator.is_finished() {
                        let mut agent = state.agent.take().expect("agent present");
                        let stderr = agent.stderr_tail().await;
                        let detail = if stderr.is_empty() {
                            "cursor-agent exited without producing a result".to_string()
                        } else {
                            format!("cursor-agent failed: {stderr}")
                        };
                        state.translator.fail(&detail, &mut out);
                    } else if let Some(agent) = state.agent.take() {
                        agent.wait().await;
                    }
                    if let Some(monitor) = state.monitor.as_ref() {
                        let (input_tokens, output_tokens) = usage_from_anthropic_sse(&out);
                        monitor.usage_updated(&state.req_id, input_tokens, output_tokens);
                    }
                    return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), state));
                }
                Ok(Ok(read)) => {
                    if !state.started {
                        state.started = true;
                        if let Some(monitor) = state.monitor.as_ref() {
                            monitor.generation_started(&state.req_id);
                        }
                    }
                    let chunk: Vec<u8> = state.buffer[..read].to_vec();
                    let out = state.translator.push_bytes(&chunk);
                    if let Some(monitor) = state.monitor.as_ref() {
                        monitor.stream_progress(&state.req_id, read as u64, 1, None, None);
                    }
                    if state.translator.is_finished() {
                        state.terminal = true;
                        if let Some(agent) = state.agent.take() {
                            agent.wait().await;
                        }
                        return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), state));
                    }
                    if out.is_empty() {
                        continue;
                    }
                    return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), state));
                }
                Ok(Err(_)) | Err(_) => {
                    // Read error, or the deadline passed.
                    state.terminal = true;
                    if let Some(agent) = state.agent.take() {
                        agent.kill().await;
                    }
                    let mut out = Vec::new();
                    state.translator.fail(
                        &format!("cursor-agent timed out after {}s", timeout.as_secs()),
                        &mut out,
                    );
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

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct CursorCliCli;

impl CliHandlers for CursorCliCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "cursor-cli reuses the Cursor CLI's own login. Run `cursor-agent login`, \
             then `claude-codex cursor-cli auth status`."
        )
    }

    fn device(&self) -> anyhow::Result<()> {
        self.login()
    }

    fn status(&self) -> anyhow::Result<()> {
        let binary = crate::config::cursor_cli_binary();
        println!("Binary: {binary}");
        let output = std::process::Command::new(&binary)
            .arg("status")
            .stdin(std::process::Stdio::null())
            .output();
        let output = match output {
            Ok(output) => output,
            Err(error) => anyhow::bail!(
                "cannot run `{binary}` ({error}). Install the Cursor CLI \
                 (https://cursor.com/cli) or set CCP_CURSOR_CLI_BINARY."
            ),
        };
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !stdout.is_empty() {
            println!("{stdout}");
        }
        println!(
            "Write access: {}",
            if crate::config::cursor_cli_allow_write() {
                "enabled (cursor-cli-agent: may edit files)"
            } else {
                "disabled (read-only; set CCP_CURSOR_CLI_ALLOW_WRITE=1 to allow)"
            }
        );
        if let Some(workspace) = crate::config::cursor_cli_workspace() {
            println!("Workspace: {workspace}");
        }
        // Printed here rather than advertised over /v1/models: there are a few
        // hundred, and any of them works as `cursor-cli:<model>`.
        let discovered = models::discover_models();
        println!(
            "Models ({}): use any as cursor-cli:<model>",
            discovered.len()
        );
        for model in discovered.iter().take(12) {
            println!("  {model}");
        }
        if discovered.len() > 12 {
            println!("  … run `cursor-agent --list-models` for the rest");
        }
        if !output.status.success() {
            anyhow::bail!(if stderr.is_empty() {
                "cursor-agent is not signed in; run `cursor-agent login`".to_string()
            } else {
                stderr
            });
        }
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        println!(
            "cursor-cli stores no credentials in the proxy; run `cursor-agent logout` instead."
        );
        Ok(())
    }
}

pub(crate) static CURSOR_CLI_CLI: CursorCliCli = CursorCliCli;

#[cfg(test)]
mod tests {
    use self::models::AgentMode;
    use super::*;

    fn model(name: &str) -> ParsedModel {
        parse_model(name)
    }

    #[test]
    fn read_only_modes_need_no_permission() {
        for name in ["cursor-cli", "cursor-cli:auto", "cursor-cli-plan:auto"] {
            assert!(
                check_write_permission(&model(name)).is_ok(),
                "{name} should be allowed"
            );
        }
    }

    /// Selecting the write-capable prefix must not be enough on its own.
    #[test]
    fn write_mode_is_refused_without_explicit_opt_in() {
        // The default config has allowWrite unset.
        if crate::config::cursor_cli_allow_write() {
            return; // Environment has opted in; the negative case cannot be tested here.
        }
        let error = check_write_permission(&model("cursor-cli-agent:auto"))
            .expect_err("write mode must be gated");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(
            error.message.contains("CCP_CURSOR_CLI_ALLOW_WRITE"),
            "{}",
            error.message
        );
        assert!(error.message.contains("read-only"), "{}", error.message);
    }

    #[test]
    fn provider_is_named_and_advertises_read_only_ids() {
        let provider = CursorCliProvider::new();
        assert_eq!(provider.name(), "cursor-cli");
        let models = provider.supported_models();
        assert!(models.contains(&"cursor-cli".to_string()));
        // Write-capable ids are hidden unless explicitly enabled.
        if !crate::config::cursor_cli_allow_write() {
            assert!(
                !models
                    .iter()
                    .any(|model| model.starts_with("cursor-cli-agent:")),
                "write ids must not be advertised by default: {models:?}"
            );
        }
    }

    #[test]
    fn mode_defaults_to_read_only() {
        assert_eq!(model("cursor-cli").mode, AgentMode::Ask);
    }
}
