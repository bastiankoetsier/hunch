//! End-to-end tests: run the compiled `hunch` binary against a fake provider
//! API and check what goes over the wire and what lands on stdout/stderr.
//!
//! Unlike the unit tests in `src/`, nothing here calls hunch's Rust API.
//! This file is its own crate that only sees the program from the outside,
//! exactly like a user's shell does: arguments, environment, stdin in;
//! HTTP requests, stdout, stderr and an exit code out.

mod support;

use serde_json::{Value, json};
use support::{FakeServer, Hunch, Reply, unused_port};

const TYPESAFE_KEY: &str = "ts-test-key";
const OPENROUTER_KEY: &str = "or-test-key";

// --- canned responses ----------------------------------------------------------

const NOUL_BODY: &str = r#"{"model":"jev-1.13.0","answers":{"answer":{"type":"noul","noul":0.87}},"usage":{"input_tokens":392,"output_tokens":65}}"#;

const CHOICE_BODY: &str = r#"{"model":"jev-1.13.0","answers":{"answer":{"type":"choice","choice":"technical","probabilities":{"billing":0.12,"technical":0.78,"sales":0.10},"confidence":0.78}},"usage":{"input_tokens":410,"output_tokens":70}}"#;

const SCORE_BODY: &str = r#"{"model":"jev-1.13.0","answers":{"answer":{"type":"score","score":1.05,"legend":{"0":"Calm","1":"Frustrated","2":"Furious"},"probabilities":{"0":0.05,"1":0.85,"2":0.10},"confidence":0.92}},"usage":{"input_tokens":388,"output_tokens":61}}"#;

fn ok(body: &str) -> Reply {
    Reply::json(200, body)
}

/// A `hunch` run against `server` with the TypeSafe key set.
fn typesafe(server: &FakeServer) -> Hunch {
    Hunch::new()
        .server(server)
        .env("TYPESAFE_API_KEY", TYPESAFE_KEY)
}

/// A `hunch` run against `server` with *both* keys set, so the test can tell
/// from the `Authorization` header which driver was picked.
fn both_keys(server: &FakeServer) -> Hunch {
    typesafe(server).env("OPENROUTER_API_KEY", OPENROUTER_KEY)
}

// --- the three question types --------------------------------------------------

#[test]
fn noul_sends_the_request_and_prints_a_bar() {
    let server = FakeServer::start([ok(NOUL_BODY)]);

    let outcome = typesafe(&server)
        .args(["--driver", "typesafe", "noul", "Does this convey urgency?"])
        .args(["--yes", "Time-sensitive", "--no", "Can wait"])
        .args(["--state", "Help! Payouts fail."])
        .run();

    assert_eq!(
        outcome.success(),
        "0.87  █████████████████░░░  likely yes\n"
    );
    assert_eq!(outcome.stderr, "");

    let request = server.single_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/systemone");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer ts-test-key"),
        "header lookup is case-insensitive"
    );
    assert_eq!(request.header("Content-Type"), Some("application/json"));
    assert_eq!(
        request.json(),
        json!({
            "state": "Help! Payouts fail.",
            "model": "jev-latest",
            "questions": {
                "answer": {
                    "type": "noul",
                    "instructions": "Does this convey urgency?",
                    "criteria": { "true": "Time-sensitive", "false": "Can wait" }
                }
            }
        })
    );
    // The TypeSafe driver sends no OpenRouter attribution.
    assert_eq!(request.header("X-Title"), None);
}

#[test]
fn choice_sends_options_and_prints_rows_by_probability() {
    let server = FakeServer::start([ok(CHOICE_BODY)]);

    let outcome = typesafe(&server)
        .args(["--driver", "typesafe", "choice", "Which team?"])
        .args(["-o", "billing=Payments and invoices", "-o", "technical"])
        .args(["-o", "sales", "--state", "My API calls time out"])
        .run();

    assert_eq!(
        outcome.success(),
        "technical  (confidence 0.78)\n\
         \n\
         \x20 technical  ████████████████░░░░   78.0%\n\
         \x20 billing    ██░░░░░░░░░░░░░░░░░░   12.0%\n\
         \x20 sales      ██░░░░░░░░░░░░░░░░░░   10.0%\n"
    );

    let request = server.single_request();
    assert_eq!(request.path, "/v1/systemone");
    assert_eq!(request.header("Authorization"), Some("Bearer ts-test-key"));
    assert_eq!(
        request.json(),
        json!({
            "state": "My API calls time out",
            "model": "jev-latest",
            "questions": {
                "answer": {
                    "type": "choice",
                    "instructions": "Which team?",
                    "criteria": {
                        "billing": "Payments and invoices",
                        "technical": null,
                        "sales": null
                    }
                }
            }
        })
    );
}

#[test]
fn score_sends_levels_in_order_and_prints_the_nearest_level() {
    let server = FakeServer::start([ok(SCORE_BODY)]);

    let outcome = typesafe(&server)
        .args(["--driver", "typesafe", "score", "How frustrated?"])
        .args(["-l", "Calm", "-l", "Frustrated", "-l", "Furious"])
        .args(["--state", "Third time I'm writing!"])
        .run();

    assert_eq!(
        outcome.success(),
        "1.05 on a 0–2 scale → Frustrated  (confidence 0.92)\n\
         \n\
         \x20 0  Calm        █░░░░░░░░░░░░░░░░░░░    5.0%\n\
         \x20 1  Frustrated  █████████████████░░░   85.0%\n\
         \x20 2  Furious     ██░░░░░░░░░░░░░░░░░░   10.0%\n"
    );
    assert_eq!(
        server.single_request().json(),
        json!({
            "state": "Third time I'm writing!",
            "model": "jev-latest",
            "questions": {
                "answer": {
                    "type": "score",
                    "instructions": "How frustrated?",
                    "criteria": ["Calm", "Frustrated", "Furious"]
                }
            }
        })
    );
}

// --- choosing the OpenRouter driver -------------------------------------------

/// Checks that a request went through the OpenRouter driver.
fn assert_openrouter(server: &FakeServer) {
    let request = server.single_request();
    assert_eq!(request.path, "/alpha/decisions");
    assert_eq!(request.header("Authorization"), Some("Bearer or-test-key"));
    assert_eq!(request.json()["model"], "~typesafe/jev-latest");
    assert_eq!(request.header("X-Title"), Some("hunch"));
    assert_eq!(
        request.header("HTTP-Referer"),
        Some("https://github.com/bastiankoetsier/hunch")
    );
}

#[test]
fn openrouter_via_flag() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    both_keys(&server)
        .args(["--driver", "openrouter", "noul", "q", "--state", "x"])
        .run()
        .success();
    assert_openrouter(&server);
}

#[test]
fn openrouter_via_env() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    both_keys(&server)
        .env("HUNCH_DRIVER", "openrouter")
        .args(["noul", "q", "--state", "x"])
        .run()
        .success();
    assert_openrouter(&server);
}

#[test]
fn openrouter_via_config_file() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    // The key comes from the file too, so this also covers `api_key` there.
    Hunch::new()
        .server(&server)
        .env("TYPESAFE_API_KEY", TYPESAFE_KEY)
        .config_file("driver = \"openrouter\"\n\n[openrouter]\napi_key = \"or-test-key\"\n")
        .args(["noul", "q", "--state", "x"])
        .run()
        .success();
    assert_openrouter(&server);
}

// --- other decision models through OpenRouter ------------------------------------

/// GPT-6 Luna Decisions as OpenRouter's Decisions router returns it: the
/// System One answer format, plus `id`, `provider` and `usage.cost`.
const LUNA_SCORE_BODY: &str = r#"{"id":"gen-dec-1791321600-abc","model":"openai/gpt-6-luna-decisions-20261006","provider":"OpenAI","answers":{"answer":{"type":"score","score":1.1,"legend":{"0":"Cosmetic","1":"Workaround available","2":"Fully blocked"},"probabilities":{"0":0.1,"1":0.7,"2":0.2},"confidence":0.55}},"usage":{"cost":0.0000412,"input_tokens":412,"output_tokens":0}}"#;

#[test]
fn openai_luna_decisions_through_openrouter() {
    let server = FakeServer::start([ok(LUNA_SCORE_BODY)]);

    let outcome = both_keys(&server)
        .args([
            "--driver",
            "openrouter",
            "--model",
            "openai/gpt-6-luna-decisions",
        ])
        .args(["score", "How severe is this issue?"])
        .args([
            "-l",
            "Cosmetic",
            "-l",
            "Workaround available",
            "-l",
            "Fully blocked",
        ])
        .args(["--state", "Export fails in Safari but works in Chrome."])
        .run();

    assert_eq!(
        outcome.success(),
        [
            "1.10 on a 0–2 scale → Workaround available  (confidence 0.55)",
            "",
            "  0  Cosmetic              ██░░░░░░░░░░░░░░░░░░   10.0%",
            "  1  Workaround available  ██████████████░░░░░░   70.0%",
            "  2  Fully blocked         ████░░░░░░░░░░░░░░░░   20.0%",
            "",
        ]
        .join("\n")
    );

    // Same driver, same request format as for Jev; only the model differs.
    let request = server.single_request();
    assert_eq!(request.path, "/alpha/decisions");
    assert_eq!(request.header("Authorization"), Some("Bearer or-test-key"));
    assert_eq!(
        request.json(),
        json!({
            "state": "Export fails in Safari but works in Chrome.",
            "model": "openai/gpt-6-luna-decisions",
            "questions": { "answer": {
                "type": "score",
                "instructions": "How severe is this issue?",
                "criteria": ["Cosmetic", "Workaround available", "Fully blocked"]
            } }
        })
    );
}

#[test]
fn a_refusal_exits_1_and_prints_nothing() {
    let server = FakeServer::start([ok(
        r#"{"model":"openai/gpt-6-luna-decisions-20261006","answers":{"answer":{"type":"refusal"}},"usage":{"input_tokens":9,"output_tokens":0}}"#,
    )]);
    let outcome = both_keys(&server)
        .args([
            "--driver",
            "openrouter",
            "--model",
            "openai/gpt-6-luna-decisions",
        ])
        .args(["noul", "q", "--state", "x"])
        .run();
    let stderr = outcome.failure(1);
    assert!(stderr.contains("declined to answer"), "{stderr}");
    assert_eq!(outcome.stdout, "");
}

// --- precedence: flag > env > file > default ----------------------------------

#[test]
fn driver_precedence_is_flag_then_env_then_file() {
    // (flag, env, file, expected key in the Authorization header)
    let cases = [
        (
            Some("typesafe"),
            Some("openrouter"),
            "openrouter",
            TYPESAFE_KEY,
        ),
        (None, Some("typesafe"), "openrouter", TYPESAFE_KEY),
        (None, Some("openrouter"), "typesafe", OPENROUTER_KEY),
        (None, None, "openrouter", OPENROUTER_KEY),
        (
            Some("openrouter"),
            Some("typesafe"),
            "typesafe",
            OPENROUTER_KEY,
        ),
    ];
    for (flag, env, file, expected_key) in cases {
        let server = FakeServer::start([ok(NOUL_BODY)]);
        let mut hunch = both_keys(&server).config_file(&format!("driver = \"{file}\"\n"));
        if let Some(env) = env {
            hunch = hunch.env("HUNCH_DRIVER", env);
        }
        if let Some(flag) = flag {
            hunch = hunch.args(["--driver", flag]);
        }
        hunch.args(["noul", "q", "--state", "x"]).run().success();

        let auth = server
            .single_request()
            .header("Authorization")
            .map(String::from);
        assert_eq!(
            auth,
            Some(format!("Bearer {expected_key}")),
            "flag={flag:?} env={env:?} file={file:?}"
        );
    }
}

#[test]
fn model_precedence_is_flag_then_env_then_file_then_default() {
    let cases = [
        (
            Some("flag-model"),
            Some("env-model"),
            Some("file-model"),
            "flag-model",
        ),
        (None, Some("env-model"), Some("file-model"), "env-model"),
        (None, None, Some("file-model"), "file-model"),
        (None, None, None, "jev-latest"),
    ];
    for (flag, env, file, expected) in cases {
        let server = FakeServer::start([ok(NOUL_BODY)]);
        let mut hunch = typesafe(&server);
        if let Some(file) = file {
            hunch = hunch.config_file(&format!("[typesafe]\nmodel = \"{file}\"\n"));
        }
        if let Some(env) = env {
            hunch = hunch.env("HUNCH_MODEL", env);
        }
        if let Some(flag) = flag {
            hunch = hunch.args(["--model", flag]);
        }
        hunch.args(["noul", "q", "--state", "x"]).run().success();

        assert_eq!(
            server.single_request().json()["model"],
            expected,
            "flag={flag:?} env={env:?} file={file:?}"
        );
    }
}

// --- output modes --------------------------------------------------------------

#[test]
fn json_flag_prints_the_raw_body_verbatim() {
    // Odd spacing and no trailing newline: re-serializing would change it.
    let body = r#"{"model": "jev-1.13.0",   "answers": {"answer": {"type": "noul", "noul": 0.87}}, "usage": {"input_tokens": 392, "output_tokens": 65}}"#;
    let server = FakeServer::start([ok(body)]);

    let outcome = typesafe(&server)
        .args(["--json", "noul", "q", "--state", "x"])
        .run();

    assert_eq!(outcome.success(), format!("{body}\n"));
    assert_eq!(outcome.stderr, "");
}

#[test]
fn verbose_writes_usage_to_stderr_only() {
    let server = FakeServer::start([ok(NOUL_BODY)]);

    let outcome = typesafe(&server)
        .args(["-v", "noul", "q", "--state", "x"])
        .run();

    assert_eq!(
        outcome.success(),
        "0.87  █████████████████░░░  likely yes\n"
    );
    assert_eq!(
        outcome.stderr,
        "model jev-1.13.0, 392 input + 65 output tokens\n"
    );
}

// --- where the state comes from -------------------------------------------------

#[test]
fn state_from_piped_stdin_is_trimmed() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    typesafe(&server)
        .stdin("  my payouts fail\n")
        .args(["noul", "q"])
        .run()
        .success();
    assert_eq!(server.single_request().json()["state"], "my payouts fail");
}

#[test]
fn state_from_a_file() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    let hunch = typesafe(&server);
    let path = hunch.write_file("state.txt", "from a file\n");
    hunch
        .args(["noul", "q", "--state-file"])
        .args([path.to_str().unwrap()])
        .run()
        .success();
    assert_eq!(server.single_request().json()["state"], "from a file");
}

#[test]
fn json_state_is_sent_structured() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    let hunch = typesafe(&server);
    let path = hunch.write_file(
        "ticket.json",
        "{\"ticket\": {\"id\": 7, \"tags\": [\"vip\"]}}\n",
    );
    hunch
        .args(["noul", "q", "--state-file"])
        .args([path.to_str().unwrap()])
        .run()
        .success();
    assert_eq!(
        server.single_request().json()["state"],
        json!({ "ticket": { "id": 7, "tags": ["vip"] } })
    );
}

#[test]
fn state_file_dash_reads_stdin() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    typesafe(&server)
        .stdin("[1, 2, 3]")
        .args(["noul", "q", "--state-file", "-"])
        .run()
        .success();
    assert_eq!(server.single_request().json()["state"], json!([1, 2, 3]));
}

#[test]
fn empty_state_is_a_usage_error_and_sends_nothing() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    typesafe(&server)
        .stdin(" \n")
        .args(["noul", "q"])
        .run()
        .failure(2);
    assert!(server.requests().is_empty());
}

// --- chaining: one hunch's --json output is the next one's state ---------------

#[test]
fn json_output_chains_into_the_next_question_as_structured_state() {
    let server = FakeServer::start([ok(CHOICE_BODY), ok(NOUL_BODY)]);

    let first = typesafe(&server)
        .args(["--json", "choice", "Which team?", "-o", "billing"])
        .args(["-o", "technical", "-o", "sales", "--state", "API times out"])
        .run();
    let second = typesafe(&server)
        .stdin(first.success())
        .args(["noul", "Is `answers.answer.choice` technical?"])
        .run();
    second.success();

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let first_response: Value = serde_json::from_str(CHOICE_BODY).unwrap();
    assert_eq!(
        requests[1].json()["state"],
        first_response,
        "the second request's state is the first response, as an object"
    );
}

// --- errors ----------------------------------------------------------------------

#[test]
fn typesafe_401_shows_the_providers_message() {
    let server = FakeServer::start([Reply::json(
        401,
        r#"{"detail":{"error_type":"authentication_error","message":"Invalid API key provided"}}"#,
    )]);
    let outcome = typesafe(&server).args(["noul", "q", "--state", "x"]).run();
    let stderr = outcome.failure(1);
    assert!(stderr.contains("Invalid API key provided"), "{stderr}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(server.requests().len(), 1, "a 401 is not retried");
}

#[test]
fn openrouter_401_shows_the_providers_message() {
    let server = FakeServer::start([Reply::json(
        401,
        r#"{"error":{"message":"No auth credentials found","code":401}}"#,
    )]);
    let outcome = both_keys(&server)
        .args(["--driver", "openrouter", "noul", "q", "--state", "x"])
        .run();
    let stderr = outcome.failure(1);
    assert!(stderr.contains("No auth credentials found"), "{stderr}");
}

#[test]
fn validation_error_422_exits_1() {
    let server = FakeServer::start([Reply::json(
        422,
        r#"{"detail":[{"loc":["body","model"],"msg":"Unknown model 'jev-nope'"}]}"#,
    )]);
    let outcome = typesafe(&server)
        .args(["--model", "jev-nope", "noul", "q", "--state", "x"])
        .run();
    let stderr = outcome.failure(1);
    assert!(stderr.contains("Unknown model 'jev-nope'"), "{stderr}");
}

#[test]
fn rate_limit_is_retried_then_succeeds() {
    // `Retry-After: 0` keeps the test fast: the retry decorator honors it
    // instead of its own (half-second) backoff.
    let server = FakeServer::start([
        Reply::json(
            429,
            r#"{"detail":{"error_type":"rate_limit","message":"slow down"}}"#,
        )
        .header("Retry-After", "0"),
        ok(NOUL_BODY),
    ]);
    let outcome = typesafe(&server).args(["noul", "q", "--state", "x"]).run();
    outcome.success();

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one 429, one retry");
    assert_eq!(requests[0].body, requests[1].body, "the retry is identical");
}

#[test]
fn missing_api_key_exits_2_without_a_request() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    let outcome = Hunch::new()
        .server(&server)
        .args(["noul", "q", "--state", "x"])
        .run();
    let stderr = outcome.failure(2);
    assert!(stderr.contains("TYPESAFE_API_KEY"), "{stderr}");
    // hunch has exited, so a request it had sent would already be recorded
    // (or, at worst, be about to be: this check cannot fail spuriously, it
    // could only miss a bug in a very unlucky scheduling).
    assert!(server.requests().is_empty());
}

#[test]
fn unreachable_server_exits_1() {
    let outcome = Hunch::new()
        .env("TYPESAFE_API_KEY", TYPESAFE_KEY)
        .env(
            "TYPESAFE_BASE_URL",
            &format!("http://127.0.0.1:{}", unused_port()),
        )
        .args(["noul", "q", "--state", "x"])
        .run();
    outcome.failure(1);
    assert_eq!(outcome.stdout, "");
}

#[test]
fn a_server_that_never_answers_times_out_after_timeout_seconds() {
    // Bound but never accepted: the OS still completes the TCP handshake
    // (the connection waits in the listen backlog), so hunch connects, sends
    // its request, and then waits for a reply that never comes.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", silent.local_addr().unwrap());

    let started = std::time::Instant::now();
    let outcome = Hunch::new()
        .env("TYPESAFE_API_KEY", TYPESAFE_KEY)
        .env("TYPESAFE_BASE_URL", &url)
        .args(["--timeout", "1", "noul", "q", "--state", "x"])
        .run();
    let elapsed = started.elapsed();

    let stderr = outcome.failure(1);
    assert!(
        stderr.contains("did not answer within 1s (wait longer with --timeout SECS)"),
        "{stderr}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "took {elapsed:?}, so --timeout 1 was not applied"
    );
}

#[test]
fn choice_with_one_option_exits_2_without_a_request() {
    let server = FakeServer::start([ok(CHOICE_BODY)]);
    let outcome = typesafe(&server)
        .args(["choice", "Which team?", "-o", "billing", "--state", "x"])
        .run();
    let stderr = outcome.failure(2);
    assert!(stderr.contains("2 to 255 options"), "{stderr}");
    assert!(server.requests().is_empty());
}

#[test]
fn noul_yes_without_no_exits_2_without_a_request() {
    let server = FakeServer::start([ok(NOUL_BODY)]);
    let outcome = both_keys(&server)
        .args(["--driver", "openrouter", "noul", "Is this a greeting?"])
        .args(["--yes", "Says hello", "--state", "Hello there!"])
        .run();
    // A clap usage error, so no `hunch: error:` prefix (`failure` expects one).
    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert!(outcome.stderr.contains("--no <DESC>"), "{outcome:?}");
    assert!(server.requests().is_empty());
}

// --- hunch config ------------------------------------------------------------------

#[test]
fn config_reports_the_key_source_but_never_the_key() {
    let secret = "sk-very-secret-123";
    let outcome = Hunch::new()
        .env("TYPESAFE_API_KEY", secret)
        .args(["config"])
        .run();
    let stdout = outcome.success();
    assert!(
        stdout.contains("API key:     set (from $TYPESAFE_API_KEY)"),
        "{stdout}"
    );
    assert!(!stdout.contains(secret));
    assert!(!outcome.stderr.contains(secret));
}

#[test]
fn config_never_prints_a_key_from_the_config_file() {
    let secret = "or-very-secret-456";
    let outcome = Hunch::new()
        .config_file(&format!(
            "driver = \"openrouter\"\n[openrouter]\napi_key = \"{secret}\"\n"
        ))
        .args(["config"])
        .run();
    let stdout = outcome.success();
    assert!(stdout.contains("driver:      openrouter\n"), "{stdout}");
    assert!(
        stdout.contains("API key:     set (from config file)"),
        "{stdout}"
    );
    assert!(!stdout.contains(secret));
}

// --- the test helper itself ----------------------------------------------------------

/// ureq sends a `Content-Length` for our small bodies, so the chunked branch
/// of the fake server is not exercised above. A hand-written request makes
/// sure it works before some future client change relies on it.
#[test]
fn fake_server_reads_chunked_request_bodies() {
    use std::io::{Read, Write};

    let server = FakeServer::start([ok("{}")]);
    let mut stream =
        std::net::TcpStream::connect(server.url().trim_start_matches("http://")).unwrap();
    stream
        .write_all(
            b"POST /v1/systemone HTTP/1.1\r\nHost: x\r\ntransfer-encoding: chunked\r\n\r\n\
              7\r\n{\"a\": 1\r\n1;ext=1\r\n}\r\n0\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Connection: close\r\n"), "{response}");
    assert_eq!(server.single_request().json(), json!({ "a": 1 }));
}
