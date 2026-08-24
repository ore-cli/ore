//! Endpoint client for the Gemini `generateContent` API.

use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::gemini::GeminiRequest;
use crate::requests::gemini::GeminiRequestBuilder;
use crate::requests::gemini::GeminiThinkingConfig;
use crate::sse::gemini::spawn_gemini_stream;
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
pub struct GeminiPromptOptions<'a> {
    /// `ModelInfo::max_output_tokens`. Optional because this wire defaults it.
    pub max_output_tokens: Option<i64>,
    pub temperature: Option<f64>,
    /// Absent leaves the model's own thinking default in place.
    pub thinking: Option<GeminiThinkingConfig>,
    pub output_schema: Option<&'a Value>,
    pub conversation_id: Option<String>,
    pub session_source: Option<SessionSource>,
}

pub struct GeminiClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

impl<T: HttpTransport> GeminiClient<T> {
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

    pub async fn stream_request(&self, request: GeminiRequest) -> Result<ResponseStream, ApiError> {
        self.stream(&request.model, request.body, request.headers)
            .await
    }

    /// Build and stream a request from a transcript.
    pub async fn stream_prompt(
        &self,
        model: &str,
        instructions: &str,
        input: &[ResponseItem],
        tools: &[Value],
        options: GeminiPromptOptions<'_>,
    ) -> Result<ResponseStream, ApiError> {
        let request = GeminiRequestBuilder::new(model, instructions, input, tools)
            .max_output_tokens(options.max_output_tokens)
            .temperature(options.temperature)
            .thinking(options.thinking)
            .output_schema(options.output_schema)
            .conversation_id(options.conversation_id)
            .session_source(options.session_source)
            .build(self.session.provider())?;

        self.stream_request(request).await
    }

    #[instrument(
        name = "gemini.stream",
        level = "info",
        skip_all,
        fields(
            transport = "gemini_http",
            http.method = "POST",
            api.path = "models:streamGenerateContent",
            model = %model,
        )
    )]
    pub async fn stream(
        &self,
        model: &str,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        tracing::trace!("gemini request: {body}");

        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode gemini request: {e}")))?;

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                &stream_path(model),
                extra_headers,
                Some(body),
                |req| {
                    // Rewritten here rather than folded into the path: the
                    // provider appends its own query string with a `?`, so a
                    // path carrying one would produce a URL with two.
                    append_alt_sse(&mut req.url);
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                },
            )
            .await?;

        Ok(spawn_gemini_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
        ))
    }
}

/// The path for one model's streaming call.
///
/// Unlike the sibling wires, Gemini names the model in the URL rather than in
/// the body, so there is no static path to return. A slug that already spells
/// the full resource name is accepted as-is: doubling the prefix would address
/// `models/models/gemini-...`, which 404s.
fn stream_path(model: &str) -> String {
    let model = model.trim_start_matches('/');
    if QUALIFIED_RESOURCE_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return format!("{}:streamGenerateContent", encode_resource(model));
    }
    format!("models/{}:streamGenerateContent", encode_resource(model))
}

/// Resource-name prefixes that already address a model, so re-prefixing would
/// produce `models/<that>` and 404.
///
/// `projects/` is the Vertex form -- `projects/p/locations/us/publishers/google/
/// models/gemini-2.5-pro` -- which the first version missed, so every Vertex
/// deployment addressed a model that does not exist.
const QUALIFIED_RESOURCE_PREFIXES: [&str; 4] =
    ["models/", "tunedModels/", "projects/", "publishers/"];

/// Percent-encodes a model id without destroying the `/` separators a resource
/// name is built from.
///
/// Discovery takes model ids verbatim from arbitrary gateways, so a slug can
/// carry anything. Two characters matter and both fail silently:
///
/// * `?` ends the path, so `:streamGenerateContent` lands in the QUERY and the
///   request addresses the wrong resource.
/// * `#` starts a fragment, so the `alt=sse` appended afterwards is never sent
///   and the server answers with a chunked JSON array that the SSE parser reads
///   as one unterminated frame and drops -- an empty turn with no error at all.
fn encode_resource(model: &str) -> String {
    model
        .split('/')
        .map(|segment| {
            segment
                .chars()
                .map(|c| match c {
                    // Unreserved per RFC 3986, plus the characters Google's own
                    // model ids use. Everything else is escaped.
                    // `@` is a legal pchar (RFC 3986 3.3) and Vertex's model-version
                    // separator, so escaping it turned `gemini-2.5-pro@001` into a
                    // 404 -- breaking Vertex in the same change that enabled it.
                    // `%` passes through so an id that arrives already encoded is
                    // not double-encoded into a different id.
                    'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' | '@' | '%' | '+' => {
                        c.to_string()
                    }
                    other => other
                        .to_string()
                        .bytes()
                        .map(|b| format!("%{b:02X}"))
                        .collect::<String>(),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Selects SSE framing.
///
/// Without it `:streamGenerateContent` answers with a JSON array delivered in
/// chunks, which the SSE parser reads as one unterminated frame and drops.
fn append_alt_sse(url: &mut String) {
    let separator = if url.contains('?') { '&' } else { '?' };
    url.push(separator);
    url.push_str("alt=sse");
}

#[cfg(test)]
#[path = "gemini_tests.rs"]
mod tests;
