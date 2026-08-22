//! Endpoint client for the Anthropic Messages API.

use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::anthropic::AnthropicCachePolicy;
use crate::requests::anthropic::AnthropicRequest;
use crate::requests::anthropic::AnthropicRequestBuilder;
use crate::sse::anthropic::spawn_anthropic_stream;
use crate::telemetry::SseTelemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use tracing::instrument;

/// Everything `stream_prompt` needs beyond the transcript itself.
#[derive(Debug, Clone)]
pub struct AnthropicPromptOptions<'a> {
    pub max_tokens: i64,
    /// Already clamped to a spelling this wire accepts.
    pub effort: Option<&'a str>,
    pub thinking_enabled: bool,
    /// `ModelInfo::supports_mid_conversation_system`.
    pub supports_inline_system: bool,
    pub output_schema: Option<&'a Value>,
    /// Breakpoint placement, computed from the assembled body.
    pub cache_policy: Option<AnthropicCachePolicy>,
    pub conversation_id: Option<String>,
    pub session_source: Option<SessionSource>,
}

pub struct AnthropicClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

impl<T: HttpTransport> AnthropicClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            sse_telemetry: None,
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            sse_telemetry: sse,
        }
    }

    fn path() -> &'static str {
        "messages"
    }

    pub async fn stream_request(
        &self,
        request: AnthropicRequest,
    ) -> Result<ResponseStream, ApiError> {
        self.stream(request.body, request.headers).await
    }

    /// Build and stream a request from a transcript.
    pub async fn stream_prompt(
        &self,
        model: &str,
        instructions: &str,
        input: &[ResponseItem],
        tools: &[Value],
        options: AnthropicPromptOptions<'_>,
    ) -> Result<ResponseStream, ApiError> {
        let request = AnthropicRequestBuilder::new(model, instructions, input, tools)
            .max_tokens(options.max_tokens)
            .effort(options.effort)
            .thinking_enabled(options.thinking_enabled)
            .supports_inline_system(options.supports_inline_system)
            .output_schema(options.output_schema)
            .cache_policy(options.cache_policy)
            .conversation_id(options.conversation_id)
            .session_source(options.session_source)
            .build(self.session.provider())?;

        self.stream_request(request).await
    }

    #[instrument(
        name = "anthropic.stream",
        level = "info",
        skip_all,
        fields(
            transport = "anthropic_http",
            http.method = "POST",
            api.path = "messages"
        )
    )]
    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        tracing::trace!("anthropic request: {body}");

        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode anthropic request: {e}")))?;

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                },
            )
            .await?;

        Ok(spawn_anthropic_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
        ))
    }
}
