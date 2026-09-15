//! HTTP client for an OpenAI-compatible chat-completions server.
//!
//! Points at gemini-web-api by default. That server drives a real browser
//! session, so timeouts are generous and only the *connect* phase is short —
//! a slow reply is normal, an unreachable server is not.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use http::StatusCode;

use super::translate::request::GeminiChatRequest;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A Gemini turn routinely runs past a minute; this only bounds a hung stream.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_BUFFERED_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct GeminiError {
    pub status: StatusCode,
    pub message: String,
    pub retry_after: Option<String>,
}

impl GeminiError {
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            retry_after: None,
        }
    }
}

pub struct GeminiClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl GeminiClient {
    pub fn new(base_url: String, api_key: Option<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tcp_nodelay(true)
            .build()?;
        Ok(Self {
            client,
            base_url: normalize_base_url(&base_url),
            api_key,
        })
    }

    pub fn from_config() -> anyhow::Result<Self> {
        Self::new(
            crate::config::gemini_base_url(),
            crate::config::gemini_api_key(),
        )
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    pub async fn post_chat(&self, body: &GeminiChatRequest) -> Result<GeminiResponse, GeminiError> {
        let mut request = self.client.post(self.endpoint()).json(body);
        if let Some(key) = self.api_key.as_deref() {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|error| {
            // The most common failure by far is "the server isn't running",
            // so say that rather than surfacing a bare transport error.
            GeminiError::upstream(format!(
                "cannot reach the gemini-web-api server at {} ({error}); \
                 start it with `uvx --from git+https://github.com/FarisHijazi/gemini-web-api gemini-web-api` \
                 or set CCP_GEMINI_BASE_URL",
                self.endpoint()
            ))
        })?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let detail = response.text().await.unwrap_or_default();
            return Err(GeminiError {
                status,
                message: if detail.is_empty() {
                    format!("gemini upstream returned {status}")
                } else {
                    detail
                },
                retry_after,
            });
        }

        Ok(GeminiResponse { response })
    }
}

pub struct GeminiResponse {
    response: reqwest::Response,
}

impl GeminiResponse {
    pub fn into_stream(self) -> impl Stream<Item = Result<bytes::Bytes, GeminiError>> + Send {
        self.response.bytes_stream().map(|chunk| {
            chunk.map_err(|error| GeminiError::upstream(format!("gemini stream failed: {error}")))
        })
    }

    pub async fn into_bytes(self) -> Result<Vec<u8>, GeminiError> {
        let mut stream = Box::pin(self.into_stream());
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                return Err(GeminiError::upstream(
                    "gemini upstream response exceeds the size limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// Accept `http://host:8100`, `.../v1`, or a trailing slash, and normalize to a
/// form `endpoint()` can extend.
fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        return trimmed
            .trim_end_matches("/chat/completions")
            .trim_end_matches('/')
            .to_string();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint_for(base: &str) -> String {
        GeminiClient::new(base.to_string(), None)
            .expect("client")
            .endpoint()
    }

    #[test]
    fn base_url_variants_normalize_to_one_endpoint() {
        let expected = "http://localhost:8100/v1/chat/completions";
        for base in [
            "http://localhost:8100/v1",
            "http://localhost:8100/v1/",
            "http://localhost:8100/v1/chat/completions",
            "  http://localhost:8100/v1  ",
        ] {
            assert_eq!(endpoint_for(base), expected, "base {base:?}");
        }
    }

    #[test]
    fn base_url_without_version_is_left_alone() {
        assert_eq!(
            endpoint_for("http://localhost:8100"),
            "http://localhost:8100/chat/completions"
        );
    }
}
