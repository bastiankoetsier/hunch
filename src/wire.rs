//! Wire types for Jev's `POST /v1/systemone` endpoint.
//!
//! These mirror the HTTP API reference (<https://docs.typesafe.ai/api>) and are
//! shared by every driver: TypeSafe and OpenRouter speak the same protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One evaluation request: a state plus named, typed questions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    /// The content to judge. Plain text becomes a JSON string; structured
    /// input (e.g. another hunch's `--json` output) stays an object/array.
    pub state: Value,
    pub model: String,
    pub questions: BTreeMap<String, Question>,
}

/// A question, discriminated by its `type` field on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Yes/no: returns the probability that the answer is yes.
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Pick one option. Map of option name to optional description.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, Option<String>>,
    },
    /// Rate against ordered levels, lowest first (2 to 10 levels).
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

/// Optional descriptions of what "yes" and "no" mean for a Noul.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    pub yes: Option<String>,
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    pub no: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Response {
    /// The versioned model that answered, e.g. `jev-1.13.0`.
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// What a driver hands back: the parsed response plus the exact body the
/// server sent, so `--json` can print it untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub response: Response,
    pub raw: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_request_like_the_api_reference() {
        let request = Request {
            state: json!("Help! My payouts have been failing for 3 days."),
            model: "jev-latest".into(),
            questions: BTreeMap::from([
                (
                    "is_urgent".to_string(),
                    Question::Noul {
                        instructions: "Does this convey urgency?".into(),
                        criteria: None,
                    },
                ),
                (
                    "department".to_string(),
                    Question::Choice {
                        instructions: "Which team should handle this?".into(),
                        criteria: BTreeMap::from([
                            ("billing".to_string(), Some("Payments".to_string())),
                            ("sales".to_string(), None),
                        ]),
                    },
                ),
            ]),
        };

        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "state": "Help! My payouts have been failing for 3 days.",
                "model": "jev-latest",
                "questions": {
                    "is_urgent": { "type": "noul", "instructions": "Does this convey urgency?" },
                    "department": {
                        "type": "choice",
                        "instructions": "Which team should handle this?",
                        "criteria": { "billing": "Payments", "sales": null }
                    }
                }
            })
        );
    }

    #[test]
    fn noul_criteria_use_true_and_false_keys() {
        let question = Question::Noul {
            instructions: "Urgent?".into(),
            criteria: Some(NoulCriteria {
                yes: Some("Time-sensitive".into()),
                no: None,
            }),
        };
        assert_eq!(
            serde_json::to_value(&question).unwrap(),
            json!({ "type": "noul", "instructions": "Urgent?", "criteria": { "true": "Time-sensitive" } })
        );
    }

    #[test]
    fn deserializes_every_answer_type() {
        let body = r#"{
            "model": "jev-1.13.0",
            "answers": {
                "department": { "type": "choice", "choice": "technical", "confidence": 0.78,
                                "probabilities": { "technical": 0.85, "sales": 0.0, "billing": 0.15 } },
                "frustration": { "type": "score", "score": 1.0, "confidence": 1.0,
                                 "legend": { "0": "Calm", "1": "Frustrated", "2": "Very angry" },
                                 "probabilities": { "0": 0.0, "1": 1.0, "2": 0.0 } },
                "is_urgent": { "type": "noul", "noul": 1.0 }
            },
            "usage": { "input_tokens": 392, "output_tokens": 65 }
        }"#;

        let response: Response = serde_json::from_str(body).unwrap();

        assert_eq!(response.model, "jev-1.13.0");
        assert_eq!(response.answers["is_urgent"], Answer::Noul { noul: 1.0 });
        assert!(matches!(
            &response.answers["department"],
            Answer::Choice { choice, .. } if choice == "technical"
        ));
        assert!(matches!(
            &response.answers["frustration"],
            Answer::Score { score, legend, .. } if *score == 1.0 && legend["2"] == "Very angry"
        ));
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 392,
                output_tokens: 65
            }
        );
    }
}
