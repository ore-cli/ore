//! Endpoint client for the Chat Completions API.
//!
//! Restored in ore, rebuilt on today's [`EndpointSession`] rather than reverted:
//! the `StreamingClient` the original was built on is gone. Two consequences:
//! `Provider` no longer carries a `wire` field, so the path is hardcoded, and
//! `codex_api::Prompt` is gone, so `stream_prompt` takes the pieces the request
//! builder needs instead of a prompt struct.

use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::chat::ChatCachePolicy;
use crate::requests::chat::ChatRequest;
use crate::requests::chat::ChatRequestBuilder;
use crate::sse::chat::spawn_chat_stream;
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

pub struct ChatClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

impl<T: HttpTransport> ChatClient<T> {
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
        "chat/completions"
    }

    pub async fn stream_request(&self, request: ChatRequest) -> Result<ResponseStream, ApiError> {
        self.stream(request.body, request.headers).await
    }

    /// Build and stream a request from a transcript.
    ///
    /// Takes the prompt's constituent parts rather than a `Prompt`: upstream
    /// deleted `codex_api::Prompt` in `3322b99900`.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_prompt(
        &self,
        model: &str,
        instructions: &str,
        input: &[ResponseItem],
        tools: &[Value],
        conversation_id: Option<String>,
        session_source: Option<SessionSource>,
        output_schema: Option<&Value>,
        output_schema_strict: bool,
        max_tokens: Option<i64>,
        reasoning_effort: Option<&str>,
        cache_policy: Option<ChatCachePolicy>,
    ) -> Result<ResponseStream, ApiError> {
        let request = ChatRequestBuilder::new(model, instructions, input, tools)
            .conversation_id(conversation_id)
            .session_source(session_source)
            .output_schema(output_schema, output_schema_strict)
            .max_tokens(max_tokens)
            .reasoning_effort(reasoning_effort)
            .cache_policy(cache_policy)
            .build(self.session.provider())?;

        self.stream_request(request).await
    }

    #[instrument(
        name = "chat.stream",
        level = "info",
        skip_all,
        fields(
            transport = "chat_http",
            http.method = "POST",
            api.path = "chat/completions"
        )
    )]
    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        // Symmetrical with the SSE trace: without it only half the exchange is
        // inspectable, and a malformed request looks like a model problem.
        tracing::trace!("chat request: {body}");

        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode chat request: {e}")))?;

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

        Ok(spawn_chat_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
            /*turn_state*/ None,
        ))
    }
}
