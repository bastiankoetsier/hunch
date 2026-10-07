//! Wire types for the System One request format.
//!
//! These mirror TypeSafe's HTTP API reference (<https://docs.typesafe.ai/api>)
//! for `POST /v1/systemone`, and are shared by every driver: OpenRouter's
//! Decisions router (`POST /api/alpha/decisions`) accepts the same format for
//! every decision model it serves. For models with a different native API,
//! such as OpenAI's GPT-6 Luna Decisions, OpenRouter translates on its side.

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

/// What "yes" and "no" mean for a Noul. Both sides, always: TypeSafe would
/// accept one, but OpenRouter's Decisions schema requires both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Response {
    /// The versioned model that answered, e.g. `jev-1.13.0`.
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

/// An answer, discriminated by its `type` field on the wire.
///
/// TypeSafe always sends every field. OpenRouter's Decisions schema, shared
/// by every model it routes to, only requires the chosen value and the
/// score, so the rest may be missing.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: Option<f64>,
    },
    /// The model declined to answer. OpenAI's Decisions API can return this
    /// for any question; TypeSafe's API has no such answer type.
    Refusal,
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
                yes: "Time-sensitive".into(),
                no: "Can wait".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&question).unwrap(),
            json!({
                "type": "noul",
                "instructions": "Urgent?",
                "criteria": { "true": "Time-sensitive", "false": "Can wait" }
            })
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

    #[test]
    fn deserializes_an_openrouter_decisions_response() {
        // OpenRouter adds `id`, `provider` and `usage.cost`; we ignore them.
        let body = r#"{
            "id": "gen-dec-1789738314-X5e5eKGQdvR9rblyX250",
            "model": "openai/gpt-6-luna-decisions-20261006",
            "provider": "OpenAI",
            "answers": { "is_bug": { "type": "noul", "noul": 0.96 } },
            "usage": { "cost": 0.000019992, "input_tokens": 476, "output_tokens": 70 }
        }"#;

        let response: Response = serde_json::from_str(body).unwrap();

        assert_eq!(response.model, "openai/gpt-6-luna-decisions-20261006");
        assert_eq!(response.answers["is_bug"], Answer::Noul { noul: 0.96 });
    }

    #[test]
    fn choice_and_score_need_only_their_value() {
        let choice: Answer =
            serde_json::from_str(r#"{"type":"choice","choice":"billing"}"#).unwrap();
        assert_eq!(
            choice,
            Answer::Choice {
                choice: "billing".into(),
                probabilities: BTreeMap::new(),
                confidence: None,
            }
        );

        let score: Answer = serde_json::from_str(r#"{"type":"score","score":1.1}"#).unwrap();
        assert_eq!(
            score,
            Answer::Score {
                score: 1.1,
                legend: BTreeMap::new(),
                probabilities: BTreeMap::new(),
                confidence: None,
            }
        );
    }

    #[test]
    fn deserializes_a_refusal_ignoring_extra_fields() {
        let refusal: Answer =
            serde_json::from_str(r#"{"type":"refusal","name":"answer"}"#).unwrap();
        assert_eq!(refusal, Answer::Refusal);
    }
}
