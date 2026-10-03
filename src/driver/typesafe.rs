//! Talks to Jev directly via TypeSafe's own API.

use serde::Deserialize;
use serde_json::Value;

use super::http::{self, Transport};
use super::{Driver, DriverConfig, DriverError};
use crate::wire::{Evaluation, Request};

pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
pub const DEFAULT_MODEL: &str = "jev-latest";

pub struct TypeSafe {
    api_key: String,
    endpoint: String,
    transport: Transport,
}

impl TypeSafe {
    pub fn new(config: DriverConfig) -> Self {
        Self {
            endpoint: format!("{}/v1/systemone", config.base_url_or(DEFAULT_BASE_URL)),
            api_key: config.api_key,
            transport: Transport::new(),
        }
    }
}

impl Driver for TypeSafe {
    fn name(&self) -> &'static str {
        "typesafe"
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }

    fn evaluate(&self, request: &Request) -> Result<Evaluation, DriverError> {
        let raw = self
            .transport
            .post_json(&self.endpoint, &self.api_key, &[], request)?;
        http::into_evaluation(raw, extract_error_message)
    }
}

/// TypeSafe is a FastAPI service, so every error sits under `detail`, but
/// that field comes in three shapes depending on who raised the error.
#[derive(Deserialize)]
struct ErrorBody {
    detail: Detail,
}

/// `#[serde(untagged)]` tries each variant in order and keeps the first that
/// fits, which is how we accept all three shapes without a type tag.
#[derive(Deserialize)]
#[serde(untagged)]
enum Detail {
    /// TypeSafe's own errors: `{"error_type": "...", "message": "..."}`.
    Structured { message: String },
    /// FastAPI request validation: `[{"loc": [...], "msg": "..."}, ...]`.
    Validation(Vec<ValidationIssue>),
    /// A bare `HTTPException(detail="...")`.
    Text(String),
}

#[derive(Deserialize)]
struct ValidationIssue {
    #[serde(default)]
    loc: Vec<Value>,
    msg: String,
}

impl ValidationIssue {
    /// Renders e.g. `body.questions.q1.type: Input should be 'noul'`.
    fn describe(&self) -> String {
        let path: Vec<String> = self
            .loc
            .iter()
            .map(|part| match part {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        if path.is_empty() {
            self.msg.clone()
        } else {
            format!("{}: {}", path.join("."), self.msg)
        }
    }
}

fn extract_error_message(body: &str) -> Option<String> {
    let ErrorBody { detail } = serde_json::from_str(body).ok()?;
    match detail {
        Detail::Structured { message } | Detail::Text(message) => Some(message),
        Detail::Validation(issues) if issues.is_empty() => None,
        Detail::Validation(issues) => Some(
            issues
                .iter()
                .map(ValidationIssue::describe)
                .collect::<Vec<_>>()
                .join("; "),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: Option<&str>) -> DriverConfig {
        DriverConfig {
            api_key: "sk-test".into(),
            base_url: base_url.map(str::to_string),
        }
    }

    #[test]
    fn defaults() {
        let driver = TypeSafe::new(config(None));
        assert_eq!(driver.name(), "typesafe");
        assert_eq!(driver.default_model(), "jev-latest");
        assert_eq!(driver.endpoint, "https://api.typesafe.ai/v1/systemone");
    }

    #[test]
    fn base_url_override_drops_trailing_slash() {
        let driver = TypeSafe::new(config(Some("http://127.0.0.1:8080/")));
        assert_eq!(driver.endpoint, "http://127.0.0.1:8080/v1/systemone");
    }

    #[test]
    fn extracts_structured_error() {
        let body =
            r#"{"detail":{"error_type":"authentication_error","message":"Cannot authenticate"}}"#;
        assert_eq!(
            extract_error_message(body).as_deref(),
            Some("Cannot authenticate")
        );
    }

    #[test]
    fn extracts_string_detail() {
        assert_eq!(
            extract_error_message(r#"{"detail":"Not Found"}"#).as_deref(),
            Some("Not Found")
        );
    }

    #[test]
    fn extracts_validation_errors_with_their_location() {
        let body = r#"{"detail":[
            {"type":"missing","loc":["body","state"],"msg":"Field required","input":{}},
            {"type":"literal_error","loc":["body","questions","q1","type"],"msg":"Input should be 'noul'"},
            {"loc":["body","questions",0],"msg":"Bad index"},
            {"msg":"No location"}
        ]}"#;
        assert_eq!(
            extract_error_message(body).as_deref(),
            Some(
                "body.state: Field required; \
                 body.questions.q1.type: Input should be 'noul'; \
                 body.questions.0: Bad index; \
                 No location"
            )
        );
    }

    #[test]
    fn gives_up_on_unexpected_bodies() {
        for body in [
            "",
            "Internal Server Error",
            "{}",
            r#"{"detail":[]}"#,
            r#"{"detail":{"error_type":"x"}}"#,
            r#"{"error":{"message":"wrong provider"}}"#,
        ] {
            assert_eq!(extract_error_message(body), None, "body: {body}");
        }
    }
}
