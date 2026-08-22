use std::time::Duration;

use codex_api::ApiError;
use codex_api::TransportError;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use http::HeaderMap;

/// Anthropic's "temporarily overloaded" status, which is outside the IANA registry.
const OVERLOADED_STATUS: u16 = 529;

/// Ceiling on an honoured `retry-after`: the delay is slept inside the turn.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

pub(super) const ANTHROPIC_OVERLOADED_MESSAGE: &str = concat!(
    "Anthropic is temporarily overloaded and rejected the request (HTTP 529). ",
    "This is a capacity signal, not a problem with the request; it is retried ",
    "automatically, and a smaller model is usually less contended",
);

/// Maps an API error, replacing Anthropic's 529 with an accurate message.
/// 529 stays an `UnexpectedStatus` because `ServerOverloaded` is non-retryable.
pub(super) fn map_api_error(error: ApiError) -> CodexErr {
    let retry_after = overloaded_retry_after(&error);
    let error = codex_api::map_api_error(error);
    let CodexErrorDetails::UnexpectedStatus(response) = error.details() else {
        return error;
    };
    if response.status.as_u16() != OVERLOADED_STATUS {
        return error;
    }

    let mut response = response.clone();
    response.user_message = Some(ANTHROPIC_OVERLOADED_MESSAGE.to_string());
    let mapped_error = CodexErr::new(CodexErrorDetails::UnexpectedStatus(response));
    match retry_after.or_else(|| error.retry_delay()) {
        Some(retry_delay) => mapped_error.with_retry_delay(retry_delay),
        None => mapped_error,
    }
}

/// The server's backoff hint. Read before mapping: `UnexpectedResponseError`
/// does not retain `retry-after`.
fn overloaded_retry_after(error: &ApiError) -> Option<Duration> {
    let ApiError::Transport(TransportError::Http {
        status, headers, ..
    }) = error
    else {
        return None;
    };
    if status.as_u16() != OVERLOADED_STATUS {
        return None;
    }
    retry_after_delay(headers.as_ref()?)
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    // Only the delta-seconds form; the HTTP-date form is not what Anthropic sends.
    let seconds = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    // `Duration::from_secs_f64` panics above u64::MAX seconds, and any proxy can
    // set this header.
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

    use super::ANTHROPIC_OVERLOADED_MESSAGE;
    use super::map_api_error;

    const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
    const OVERLOADED: u16 = 529;

    fn http_error(status: u16, body: &str, retry_after: Option<&'static str>) -> ApiError {
        let mut headers = HeaderMap::new();
        headers.insert("request-id", HeaderValue::from_static("req_anthropic"));
        if let Some(retry_after) = retry_after {
            headers.insert(
                http::header::RETRY_AFTER,
                HeaderValue::from_static(retry_after),
            );
        }
        ApiError::Transport(TransportError::Http {
            status: StatusCode::from_u16(status).expect("valid status"),
            url: Some(ANTHROPIC_MESSAGES_URL.to_string()),
            headers: Some(headers),
            body: Some(body.to_string()),
        })
    }

    const OVERLOADED_BODY: &str =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

    #[test]
    fn overloaded_stays_retryable_with_an_actionable_message() {
        let error = map_api_error(http_error(
            OVERLOADED,
            OVERLOADED_BODY,
            /*retry_after*/ None,
        ));

        let CodexErrorDetails::UnexpectedStatus(response) = error.details() else {
            panic!("expected unexpected status error, got {error:?}");
        };
        assert_eq!(
            response.user_message.as_deref(),
            Some(ANTHROPIC_OVERLOADED_MESSAGE)
        );
        assert!(
            error.is_retryable(),
            "529 is a capacity signal and must keep being retried"
        );
        assert_eq!(error.retry_delay(), None);
    }

    #[test]
    fn overloaded_honours_retry_after() {
        let error = map_api_error(http_error(OVERLOADED, OVERLOADED_BODY, Some("7")));

        assert_eq!(error.retry_delay(), Some(Duration::from_secs(7)));
    }

    /// The delay is slept inside the turn, so an outlier must not stall it.
    #[test]
    fn overloaded_caps_an_outlier_retry_after() {
        let error = map_api_error(http_error(OVERLOADED, OVERLOADED_BODY, Some("86400")));

        assert_eq!(error.retry_delay(), Some(Duration::from_secs(60)));
    }

    /// `Duration::from_secs_f64` panics past `u64::MAX` seconds, and any proxy can
    /// set this header.
    #[test]
    fn overloaded_survives_a_retry_after_beyond_duration_range() {
        for header in ["1e300", "99999999999999999999", "inf", "-1", "NaN"] {
            let error = map_api_error(http_error(OVERLOADED, OVERLOADED_BODY, Some(header)));
            assert!(
                error
                    .retry_delay()
                    .is_none_or(|delay| delay <= Duration::from_secs(60)),
                "retry-after {header} produced {:?}",
                error.retry_delay()
            );
        }
    }

    #[test]
    fn overloaded_ignores_an_unparseable_retry_after() {
        let error = map_api_error(http_error(
            OVERLOADED,
            OVERLOADED_BODY,
            // The HTTP-date form, which Anthropic does not send.
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
        ));

        assert_eq!(error.retry_delay(), None);
        assert!(error.is_retryable());
    }

    #[test]
    fn retry_after_on_other_statuses_is_left_alone() {
        let error = map_api_error(http_error(503, "upstream unavailable", Some("7")));

        let CodexErrorDetails::UnexpectedStatus(response) = error.details() else {
            panic!("expected unexpected status error, got {error:?}");
        };
        assert_eq!(response.user_message, None);
        assert_eq!(error.retry_delay(), None);
    }

    #[test]
    fn other_errors_keep_their_shared_mapping() {
        let error = map_api_error(http_error(
            StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            "boom",
            /*retry_after*/ None,
        ));

        assert!(matches!(
            error.details(),
            CodexErrorDetails::InternalServerError
        ));
    }
}
