use std::time::Duration;

use codex_api::ApiError;
use codex_api::TransportError;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use http::HeaderMap;
use http::StatusCode;
use serde_json::Map;
use serde_json::Value;

/// Google's capacity signal: `503 UNAVAILABLE`, "the model is overloaded".
/// It is the Gemini analogue of Anthropic's 529, and just as transient.
const UNAVAILABLE_STATUS: u16 = 503;

/// Ceiling on an honoured retry hint: the delay is slept inside the turn.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// The `google.rpc.RetryInfo` detail type, matched by suffix because the
/// `@type` field is a full type URL (`type.googleapis.com/google.rpc.RetryInfo`)
/// whose host has changed before.
const RETRY_INFO_TYPE_SUFFIX: &str = "google.rpc.RetryInfo";

pub(super) const GEMINI_OVERLOADED_MESSAGE: &str = concat!(
    "Gemini is temporarily overloaded and rejected the request (HTTP 503 UNAVAILABLE). ",
    "This is a capacity signal, not a problem with the request; it is retried ",
    "automatically, and a Flash model is usually less contended",
);

/// Maps an API error, rendering Gemini's JSON error envelope as prose.
///
/// Without this the user is shown the raw body. Every generative-language
/// failure is `{"error":{"code":…,"message":…,"status":…}}`, and the one part
/// worth reading -- `message` -- is buried in it. The shared mapper cannot pick
/// it out: its 503 branch looks for a *string* `error.code` (`server_is_overloaded`)
/// and Gemini's `code` is the numeric HTTP status, so nothing matches.
///
/// 503 stays an `UnexpectedStatus` rather than becoming `ServerOverloaded`
/// because `ServerOverloaded` is non-retryable, and an overloaded Gemini is
/// exactly the case worth retrying.
pub(super) fn map_api_error(error: ApiError) -> CodexErr {
    // Read before mapping: `UnexpectedResponseError` keeps the body but drops
    // the headers, so `retry-after` is unrecoverable afterwards.
    let header_retry_after = http_headers(&error).and_then(retry_after_delay);
    let error = codex_api::map_api_error(error);
    let CodexErrorDetails::UnexpectedStatus(response) = error.details() else {
        // 400 and 429 keep their shared mappings (`InvalidRequest`, `RetryLimit`).
        // Neither carries a user-facing message field to improve, and widening
        // upstream's 429 handling to Gemini's RESOURCE_EXHAUSTED -- which covers
        // both a per-minute rate limit and an exhausted daily quota -- is a
        // codex-api change this fork does not take.
        return error;
    };
    let Some(gemini) = GeminiErrorEnvelope::parse(&response.body) else {
        return error;
    };

    let mut response = response.clone();
    // A shared message is more specific than anything derivable here (the
    // Cloudflare block, for one, is not a Gemini response at all), so it wins.
    if response.user_message.is_none() {
        response.user_message = Some(gemini.user_message(response.status));
    }
    let mapped_error = CodexErr::new(CodexErrorDetails::UnexpectedStatus(response));

    match header_retry_after
        .or(gemini.retry_info_delay)
        .or_else(|| error.retry_delay())
    {
        Some(retry_delay) => mapped_error.with_retry_delay(retry_delay),
        None => mapped_error,
    }
}

fn http_headers(error: &ApiError) -> Option<&HeaderMap> {
    let ApiError::Transport(TransportError::Http { headers, .. }) = error else {
        return None;
    };
    headers.as_ref()
}

/// Gemini's error envelope: the `error` object every failure body carries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GeminiErrorEnvelope {
    /// Numerically equal to the HTTP status in practice, but a fronting gateway
    /// can disagree with itself, so the body's own value is preferred.
    code: Option<i64>,
    /// The canonical google.rpc name: `UNAVAILABLE`, `RESOURCE_EXHAUSTED`,
    /// `INVALID_ARGUMENT`, `PERMISSION_DENIED`.
    status: Option<String>,
    message: String,
    /// Gemini does not send `Retry-After`; the backoff hint it does send lives in
    /// a `RetryInfo` entry of `error.details`. A header-only reader backs off blind.
    retry_info_delay: Option<Duration>,
}

impl GeminiErrorEnvelope {
    fn parse(body: &str) -> Option<Self> {
        let value = serde_json::from_str::<Value>(body).ok()?;
        let error = value.get("error")?.as_object()?;
        // A message is the whole point of parsing; an envelope without one is no
        // better than the raw body, so it is left alone.
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())?
            .to_string();

        Some(Self {
            code: error.get("code").and_then(Value::as_i64),
            status: error
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            message,
            retry_info_delay: retry_info_delay(error),
        })
    }

    fn is_overloaded(&self, http_status: StatusCode) -> bool {
        let code = self.code.unwrap_or_else(|| i64::from(http_status.as_u16()));
        code == i64::from(UNAVAILABLE_STATUS)
    }

    fn user_message(&self, http_status: StatusCode) -> String {
        if self.is_overloaded(http_status) {
            return GEMINI_OVERLOADED_MESSAGE.to_string();
        }
        let code = self.code.unwrap_or_else(|| i64::from(http_status.as_u16()));
        let message = &self.message;
        match self.status.as_deref() {
            Some(status) => {
                format!("Gemini rejected the request (HTTP {code} {status}): {message}")
            }
            None => format!("Gemini rejected the request (HTTP {code}): {message}"),
        }
    }
}

/// The `retryDelay` of the `google.rpc.RetryInfo` entry, if the server sent one.
///
/// `details` is a heterogeneous list — `QuotaFailure`, `Help`, `ErrorInfo` all
/// appear there — so the `@type` guard is what keeps a `Help` link from being
/// read as a duration.
fn retry_info_delay(error: &Map<String, Value>) -> Option<Duration> {
    error.get("details")?.as_array()?.iter().find_map(|detail| {
        let detail = detail.as_object()?;
        let type_url = detail.get("@type").and_then(Value::as_str)?;
        if !type_url.ends_with(RETRY_INFO_TYPE_SUFFIX) {
            return None;
        }
        // The protobuf JSON mapping for `Duration` is seconds with an `s`
        // suffix, fractions included: "41s", "1.5s".
        let raw = detail.get("retryDelay").and_then(Value::as_str)?.trim();
        capped_seconds(raw.strip_suffix('s').unwrap_or(raw))
    })
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    // Only the delta-seconds form; the HTTP-date form is not what Google sends.
    let seconds = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?;
    capped_seconds(seconds)
}

/// Parses a seconds value and clamps it into a range a turn can survive.
///
/// The `is_finite` guard is load-bearing: `"inf"` and `"1e400"` both parse as
/// `f64::INFINITY`, and `Duration::from_secs_f64` PANICS on a non-finite or
/// out-of-range value. Any proxy in the path can set these fields, so a panic
/// here would be a remotely triggerable crash.
fn capped_seconds(raw: &str) -> Option<Duration> {
    let seconds = raw.trim().parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let seconds = seconds.min(MAX_RETRY_AFTER.as_secs_f64());
    Some(Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use codex_api::ApiError;
    use codex_api::TransportError;
    use codex_protocol::error::CodexErrorDetails;
    use http::HeaderMap;
    use http::HeaderValue;
    use http::StatusCode;
    use pretty_assertions::assert_eq;

    use super::GEMINI_OVERLOADED_MESSAGE;
    use super::GeminiErrorEnvelope;
    use super::map_api_error;

    const GEMINI_GENERATE_URL: &str =
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent";
    const UNAVAILABLE: u16 = 503;

    fn http_error(status: u16, body: &str, retry_after: Option<&'static str>) -> ApiError {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("req_gemini"));
        if let Some(retry_after) = retry_after {
            headers.insert(
                http::header::RETRY_AFTER,
                HeaderValue::from_static(retry_after),
            );
        }
        ApiError::Transport(TransportError::Http {
            status: StatusCode::from_u16(status).expect("valid status"),
            url: Some(GEMINI_GENERATE_URL.to_string()),
            headers: Some(headers),
            body: Some(body.to_string()),
        })
    }

    const OVERLOADED_BODY: &str = r#"{"error":{"code":503,"message":"The model is overloaded. Please try again later.","status":"UNAVAILABLE"}}"#;

    fn unexpected_status(error: &codex_protocol::error::CodexErr) -> &str {
        match error.details() {
            CodexErrorDetails::UnexpectedStatus(response) => response
                .user_message
                .as_deref()
                .unwrap_or("<no user message>"),
            other => panic!("expected an unexpected-status error, got {other:?}"),
        }
    }

    #[test]
    fn the_envelope_is_parsed_into_code_status_and_message() {
        let envelope = GeminiErrorEnvelope::parse(OVERLOADED_BODY).expect("a Gemini envelope");

        assert_eq!(envelope.code, Some(503));
        assert_eq!(envelope.status.as_deref(), Some("UNAVAILABLE"));
        assert_eq!(
            envelope.message,
            "The model is overloaded. Please try again later."
        );
    }

    #[test]
    fn a_body_that_is_not_a_gemini_envelope_is_left_alone() {
        for body in [
            "not json at all",
            "{}",
            r#"{"error":"a string, not an object"}"#,
            // An envelope with no message is no more readable than the raw body.
            r#"{"error":{"code":500,"status":"INTERNAL"}}"#,
            r#"{"error":{"code":500,"message":"   ","status":"INTERNAL"}}"#,
        ] {
            assert_eq!(
                GeminiErrorEnvelope::parse(body),
                None,
                "{body} is not a usable envelope"
            );
        }
    }

    #[test]
    fn overloaded_stays_retryable_with_an_actionable_message() {
        let error = map_api_error(http_error(
            UNAVAILABLE,
            OVERLOADED_BODY,
            /*retry_after*/ None,
        ));

        assert_eq!(unexpected_status(&error), GEMINI_OVERLOADED_MESSAGE);
        assert!(
            error.is_retryable(),
            "503 UNAVAILABLE is a capacity signal and must keep being retried"
        );
        assert_eq!(error.retry_delay(), None);
    }

    /// The prose the user actually sees for a non-capacity failure: the raw JSON
    /// body is unreadable, and the `message` inside it is the whole story.
    #[test]
    fn a_rejected_request_reports_the_message_not_the_raw_json() {
        let error = map_api_error(http_error(
            StatusCode::NOT_FOUND.as_u16(),
            r#"{"error":{"code":404,"message":"models/gemini-9-ultra is not found for API version v1beta","status":"NOT_FOUND"}}"#,
            /*retry_after*/ None,
        ));

        assert_eq!(
            unexpected_status(&error),
            "Gemini rejected the request (HTTP 404 NOT_FOUND): models/gemini-9-ultra is not found \
             for API version v1beta"
        );
    }

    #[test]
    fn a_missing_status_field_falls_back_to_the_http_status() {
        let error = map_api_error(http_error(
            StatusCode::NOT_FOUND.as_u16(),
            r#"{"error":{"message":"no such model"}}"#,
            /*retry_after*/ None,
        ));

        assert_eq!(
            unexpected_status(&error),
            "Gemini rejected the request (HTTP 404): no such model"
        );
    }

    #[test]
    fn overloaded_honours_retry_after() {
        let error = map_api_error(http_error(UNAVAILABLE, OVERLOADED_BODY, Some("7")));

        assert_eq!(error.retry_delay(), Some(Duration::from_secs(7)));
    }

    /// Gemini's real backoff hint: a `RetryInfo` detail, not a header.
    #[test]
    fn a_retry_info_detail_supplies_the_delay_when_no_header_does() {
        let error = map_api_error(http_error(
            UNAVAILABLE,
            r#"{"error":{"code":503,"message":"The model is overloaded.","status":"UNAVAILABLE","details":[{"@type":"type.googleapis.com/google.rpc.Help","links":[]},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"12.5s"}]}}"#,
            /*retry_after*/ None,
        ));

        assert_eq!(
            error.retry_delay(),
            Some(Duration::from_secs_f64(12.5)),
            "a Help entry must not be mistaken for the RetryInfo entry"
        );
    }

    /// The delay is slept inside the turn, so an outlier must not stall it.
    #[test]
    fn an_outlier_retry_hint_is_capped() {
        for body_or_header in [Some("86400"), None] {
            let error = map_api_error(http_error(
                UNAVAILABLE,
                r#"{"error":{"code":503,"message":"Overloaded.","status":"UNAVAILABLE","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"86400s"}]}}"#,
                body_or_header,
            ));

            assert_eq!(error.retry_delay(), Some(Duration::from_secs(60)));
        }
    }

    /// `Duration::from_secs_f64` panics on a non-finite or out-of-range value,
    /// and any proxy in the path can set either field.
    #[test]
    fn a_retry_hint_beyond_duration_range_cannot_panic() {
        for hint in ["1e300", "99999999999999999999", "inf", "-1", "NaN", "1e400"] {
            let body = format!(
                r#"{{"error":{{"code":503,"message":"Overloaded.","status":"UNAVAILABLE","details":[{{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"{hint}s"}}]}}}}"#
            );
            let error = map_api_error(http_error(UNAVAILABLE, &body, /*retry_after*/ None));
            assert!(
                error
                    .retry_delay()
                    .is_none_or(|delay| delay <= Duration::from_secs(60)),
                "retry hint {hint} produced {:?}",
                error.retry_delay()
            );
        }
    }

    #[test]
    fn an_unparseable_retry_after_is_ignored() {
        let error = map_api_error(http_error(
            UNAVAILABLE,
            OVERLOADED_BODY,
            // The HTTP-date form, which Google does not send.
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
        ));

        assert_eq!(error.retry_delay(), None);
        assert!(error.is_retryable());
    }

    /// A 400 is `InvalidRequest` in the shared mapping, which this module does
    /// not override; the raw body still reaches the user through it.
    #[test]
    fn other_statuses_keep_their_shared_mapping() {
        let invalid = map_api_error(http_error(
            StatusCode::BAD_REQUEST.as_u16(),
            r#"{"error":{"code":400,"message":"Invalid JSON payload","status":"INVALID_ARGUMENT"}}"#,
            /*retry_after*/ None,
        ));
        assert!(matches!(
            invalid.details(),
            CodexErrorDetails::InvalidRequest(_)
        ));

        let internal = map_api_error(http_error(
            StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            r#"{"error":{"code":500,"message":"Internal error","status":"INTERNAL"}}"#,
            /*retry_after*/ None,
        ));
        assert!(matches!(
            internal.details(),
            CodexErrorDetails::InternalServerError
        ));

        // RESOURCE_EXHAUSTED: upstream maps every 429 to a terminal RetryLimit,
        // and this fork does not widen that.
        let exhausted = map_api_error(http_error(
            StatusCode::TOO_MANY_REQUESTS.as_u16(),
            r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#,
            Some("30"),
        ));
        assert!(matches!(
            exhausted.details(),
            CodexErrorDetails::RetryLimit(_)
        ));
    }

    /// The Cloudflare block is not a Gemini response, so its more specific
    /// message must survive.
    #[test]
    fn a_shared_user_message_is_not_overwritten() {
        let error = map_api_error(http_error(
            StatusCode::FORBIDDEN.as_u16(),
            r#"{"error":{"code":403,"message":"Cloudflare blocked this request","status":"PERMISSION_DENIED"}}"#,
            /*retry_after*/ None,
        ));

        assert!(
            unexpected_status(&error).contains("Cloudflare"),
            "got {}",
            unexpected_status(&error)
        );
    }
}
