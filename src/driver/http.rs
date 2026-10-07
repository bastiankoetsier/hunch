//! Shared HTTP plumbing for the concrete drivers.
//!
//! Both providers speak the same protocol over plain JSON-over-HTTPS, so the
//! transport and the status-code mapping live here once. What *differs* between
//! providers (headers, error-body shape) is passed in by each driver.

use std::time::Duration;

use serde::Serialize;

use super::DriverError;
use crate::wire::{Evaluation, Response};

/// What came back from the server, before any provider-specific interpretation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawResponse {
    pub status: u16,
    pub body: String,
    /// Parsed `Retry-After` header, if the server sent one in seconds.
    pub retry_after: Option<Duration>,
}

/// Pulls a human-readable message out of a provider's error body. Returns
/// `None` when the body is not in the provider's documented shape, so the
/// caller can fall back to the raw text.
pub(crate) type ExtractMessage = fn(&str) -> Option<String>;

/// A thin wrapper around a configured [`ureq::Agent`]. The agent keeps a
/// connection pool, so a driver holds one `Transport` for its whole lifetime.
pub(crate) struct Transport {
    agent: ureq::Agent,
    /// Kept to report it in [`DriverError::Timeout`].
    timeout: Duration,
}

impl Transport {
    /// `timeout` bounds each whole request: connect + send + receive.
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            // ureq's default turns 4xx/5xx into `Err`, which would throw away
            // the status-specific body we need for good error messages.
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .user_agent(concat!("hunch/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            timeout,
        }
    }

    /// POSTs `body` as JSON with bearer auth plus any driver-specific headers.
    ///
    /// Only network-level failures are errors here; every HTTP status, good or
    /// bad, comes back as a [`RawResponse`] for [`into_evaluation`] to judge.
    pub fn post_json(
        &self,
        url: &str,
        bearer_token: &str,
        extra_headers: &[(&str, &str)],
        body: &impl Serialize,
    ) -> Result<RawResponse, DriverError> {
        // Serialising ourselves instead of using ureq's `send_json` avoids
        // enabling ureq's optional `json` feature just for one call.
        let payload = serde_json::to_vec(body)
            .map_err(|e| DriverError::Transport(format!("could not encode request: {e}")))?;

        let mut request = self
            .agent
            .post(url)
            .header("Authorization", format!("Bearer {bearer_token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }

        let response = request
            .send(payload)
            .map_err(|error| self.transport_error(error))?;

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after);
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|error| self.transport_error(error))?;

        Ok(RawResponse {
            status,
            body,
            retry_after,
        })
    }
}

impl Transport {
    /// A timeout gets its own error: the server may well be reachable, it
    /// just did not answer in time, which "could not reach" would misstate.
    fn transport_error(&self, error: ureq::Error) -> DriverError {
        match error {
            ureq::Error::Timeout(_) => DriverError::Timeout {
                after: self.timeout,
            },
            other => DriverError::Transport(other.to_string()),
        }
    }
}

/// Parses a `Retry-After` header given in whole seconds.
///
/// The header may also be an HTTP date; we ignore that form rather than pull
/// in a date parser, and callers fall back to their own backoff.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Turns a raw HTTP response into the driver-neutral result.
///
/// The status mapping is identical for every provider, so it lives here; only
/// `extract_message` knows what that provider's error body looks like.
pub(crate) fn into_evaluation(
    raw: RawResponse,
    extract_message: ExtractMessage,
) -> Result<Evaluation, DriverError> {
    if (200..300).contains(&raw.status) {
        let response: Response =
            serde_json::from_str(&raw.body).map_err(|e| DriverError::Decode(e.to_string()))?;
        return Ok(Evaluation {
            response,
            raw: raw.body,
        });
    }

    let message = || error_message(&raw.body, extract_message);
    Err(match raw.status {
        401 | 403 => DriverError::Unauthorized { message: message() },
        400 | 422 => DriverError::InvalidRequest { message: message() },
        429 => DriverError::RateLimited {
            retry_after: raw.retry_after,
        },
        503 | 529 => DriverError::Overloaded,
        status => DriverError::Api {
            status,
            message: message(),
        },
    })
}

/// The provider's message if we understand the body, else the body itself so
/// the user still sees *something* useful (e.g. an HTML page from a proxy).
fn error_message(body: &str, extract_message: ExtractMessage) -> String {
    extract_message(body).unwrap_or_else(|| match body.trim() {
        "" => "(empty response body)".to_string(),
        text => text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for a driver's extractor: understands `{"msg": "..."}` only.
    fn extract(body: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        value["msg"].as_str().map(str::to_string)
    }

    fn raw(status: u16, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after("30"), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after(" 5 "), Some(Duration::from_secs(5)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn success_decodes_body_and_keeps_raw_text() {
        let body = r#"{"model":"jev-1.13.0","answers":{"x":{"type":"noul","noul":0.5}},
                       "usage":{"input_tokens":1,"output_tokens":2}}"#;
        let evaluation = into_evaluation(raw(200, body), extract).unwrap();
        assert_eq!(evaluation.response.model, "jev-1.13.0");
        assert_eq!(evaluation.raw, body);
    }

    #[test]
    fn success_with_bad_body_is_a_decode_error() {
        assert!(matches!(
            into_evaluation(raw(200, "not json"), extract),
            Err(DriverError::Decode(_))
        ));
    }

    #[test]
    fn maps_statuses_to_driver_errors() {
        let body = r#"{"msg":"nope"}"#;
        let message = "nope".to_string();
        let cases = [
            (
                401,
                DriverError::Unauthorized {
                    message: message.clone(),
                },
            ),
            (
                403,
                DriverError::Unauthorized {
                    message: message.clone(),
                },
            ),
            (
                400,
                DriverError::InvalidRequest {
                    message: message.clone(),
                },
            ),
            (
                422,
                DriverError::InvalidRequest {
                    message: message.clone(),
                },
            ),
            (429, DriverError::RateLimited { retry_after: None }),
            (503, DriverError::Overloaded),
            (529, DriverError::Overloaded),
            (
                500,
                DriverError::Api {
                    status: 500,
                    message: message.clone(),
                },
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(
                into_evaluation(raw(status, body), extract),
                Err(expected),
                "status {status}"
            );
        }
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let response = RawResponse {
            retry_after: Some(Duration::from_secs(7)),
            ..raw(429, "")
        };
        assert_eq!(
            into_evaluation(response, extract),
            Err(DriverError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            })
        );
    }

    #[test]
    fn falls_back_to_raw_body_when_extractor_gives_up() {
        assert_eq!(
            into_evaluation(raw(502, "  <html>Bad Gateway</html>\n"), extract),
            Err(DriverError::Api {
                status: 502,
                message: "<html>Bad Gateway</html>".into()
            })
        );
        assert_eq!(
            into_evaluation(raw(401, ""), extract),
            Err(DriverError::Unauthorized {
                message: "(empty response body)".into()
            })
        );
    }
}
