//! Reaches Jev through OpenRouter, which proxies the same `/v1/systemone`
//! endpoint but has its own model naming, error format and headers.

use serde::Deserialize;

use super::http::{self, Transport};
use super::{Driver, DriverConfig, DriverError};
use crate::wire::{Evaluation, Request};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api";
pub const DEFAULT_MODEL: &str = "~typesafe/jev-latest";

/// Optional app-attribution headers: OpenRouter uses them to credit traffic
/// to hunch on its dashboards and rankings. They do not affect the answer.
const ATTRIBUTION_HEADERS: [(&str, &str); 2] = [
    ("HTTP-Referer", "https://github.com/bastiankoetsier/hunch"),
    ("X-Title", "hunch"),
];

pub struct OpenRouter {
    api_key: String,
    endpoint: String,
    transport: Transport,
}

impl OpenRouter {
    pub fn new(config: DriverConfig) -> Self {
        Self {
            endpoint: format!("{}/v1/systemone", config.base_url_or(DEFAULT_BASE_URL)),
            api_key: config.api_key,
            transport: Transport::new(),
        }
    }
}

impl Driver for OpenRouter {
    fn name(&self) -> &'static str {
        "openrouter"
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }

    fn evaluate(&self, request: &Request) -> Result<Evaluation, DriverError> {
        let raw = self.transport.post_json(
            &self.endpoint,
            &self.api_key,
            &ATTRIBUTION_HEADERS,
            request,
        )?;
        http::into_evaluation(raw, extract_error_message)
    }
}

/// OpenRouter's error envelope: `{"error": {"message": "...", "code": 401}}`.
/// Only the fields we read are declared; serde ignores the rest.
#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

fn extract_error_message(body: &str) -> Option<String> {
    let ErrorBody { error } = serde_json::from_str(body).ok()?;
    Some(error.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: Option<&str>) -> DriverConfig {
        DriverConfig {
            api_key: "sk-or-test".into(),
            base_url: base_url.map(str::to_string),
        }
    }

    #[test]
    fn defaults() {
        let driver = OpenRouter::new(config(None));
        assert_eq!(driver.name(), "openrouter");
        assert_eq!(driver.default_model(), "~typesafe/jev-latest");
        assert_eq!(driver.endpoint, "https://openrouter.ai/api/v1/systemone");
    }

    #[test]
    fn base_url_override_drops_trailing_slash() {
        let driver = OpenRouter::new(config(Some("http://127.0.0.1:9000//")));
        assert_eq!(driver.endpoint, "http://127.0.0.1:9000/v1/systemone");
    }

    #[test]
    fn extracts_error_message() {
        let body = r#"{"error":{"message":"Missing Authentication header","code":401}}"#;
        assert_eq!(
            extract_error_message(body).as_deref(),
            Some("Missing Authentication header")
        );
    }

    #[test]
    fn gives_up_on_unexpected_bodies() {
        for body in [
            "",
            "<html>Bad Gateway</html>",
            "{}",
            r#"{"error":"just a string"}"#,
            r#"{"error":{"code":500}}"#,
            r#"{"detail":{"message":"wrong provider"}}"#,
        ] {
            assert_eq!(extract_error_message(body), None, "body: {body}");
        }
    }
}
