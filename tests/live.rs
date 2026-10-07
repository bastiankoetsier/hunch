//! Smoke tests against the *real* provider APIs.
//!
//! `#[ignore]` keeps them out of a plain `cargo test` (they need network, a
//! real key, and cost a few tokens). Run them on purpose with
//! `mise run test-live`, i.e. `cargo test --test live -- --ignored`.
//!
//! Each test still runs hunch with an isolated `HOME` (so your config file
//! cannot interfere) but passes the real API key through from your shell.
//! Without that key it skips instead of failing, so `--ignored` runs stay
//! green on machines that only have one of the two keys.

mod support;

use serde_json::Value;
use support::Hunch;

/// The real key from `key_var`, or `None` (after saying so) to skip the test.
fn key(key_var: &str, test: &str) -> Option<String> {
    let key = std::env::var(key_var).ok().filter(|key| !key.is_empty());
    if key.is_none() {
        eprintln!("skipping the live {test} test: ${key_var} is not set");
    }
    key
}

/// Asks one cheap noul question through `driver` and checks the shape of
/// the answer, not its value (the model's judgment is not ours to test).
fn smoke(driver: &str, key_var: &str) {
    let Some(key) = key(key_var, driver) else {
        return;
    };

    let outcome = Hunch::new()
        .env(key_var, &key)
        .args(["--json", "--driver", driver, "noul", "Is this a greeting?"])
        .args(["--state", "Hello there!"])
        .run();

    let response: Value = serde_json::from_str(outcome.success()).expect("--json prints JSON");
    let noul = response["answers"]["answer"]["noul"]
        .as_f64()
        .unwrap_or_else(|| panic!("no answers.answer.noul in {response}"));
    assert!((0.0..=1.0).contains(&noul), "noul out of range: {noul}");
}

#[test]
#[ignore = "hits the live API; run with mise run test-live"]
fn live_typesafe() {
    smoke("typesafe", "TYPESAFE_API_KEY");
}

#[test]
#[ignore = "hits the live API; run with mise run test-live"]
fn live_openrouter() {
    smoke("openrouter", "OPENROUTER_API_KEY");
}

/// OpenAI's GPT-6 Luna Decisions, which OpenRouter translates from OpenAI's
/// own format into the System One one. Every question type, rendered for
/// humans, so a translated answer hunch cannot decode fails here.
#[test]
#[ignore = "hits the live API; run with mise run test-live"]
fn live_openrouter_gpt_6_luna_decisions() {
    let Some(key) = key("OPENROUTER_API_KEY", "GPT-6 Luna Decisions") else {
        return;
    };

    // (subcommand, question, its options or levels)
    let questions: [(&str, &str, &[&str]); 3] = [
        ("noul", "Is this a greeting?", &[]),
        (
            "choice",
            "What kind of message?",
            &["-o", "greeting", "-o", "other"],
        ),
        (
            "score",
            "How friendly?",
            &["-l", "Hostile", "-l", "Neutral", "-l", "Warm"],
        ),
    ];
    for (subcommand, question, options) in questions {
        let outcome = Hunch::new()
            .env("OPENROUTER_API_KEY", &key)
            .args(["--driver", "openrouter"])
            .args(["--model", "openai/gpt-6-luna-decisions"])
            .args([subcommand, question])
            .args(options.iter().copied())
            .args(["--state", "Hello there!"])
            .run();
        // Printed for `--nocapture`; the answer's value is not ours to test.
        eprintln!("{}", outcome.success());
    }
}
