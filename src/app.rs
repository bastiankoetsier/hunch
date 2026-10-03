//! The application core: turn parsed arguments into a request, ask the
//! driver, and decide what to print.
//!
//! Nothing in here touches process-global state. The driver, stdin and
//! "is stdin a terminal?" are all passed in, and output is *returned* rather
//! than printed. That is what lets the tests below run the whole flow with a
//! fake driver and an in-memory stdin, in parallel, with no network.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use serde_json::Value;

use crate::cli::{ChoiceArgs, Cli, Command, NoulArgs, ScoreArgs, StateArgs};
use crate::config::{self, ConfigError, Overrides};
use crate::driver::{self, Driver, DriverError, DriverKind};
use crate::render;
use crate::wire::{NoulCriteria, Question, Request};

/// Every request carries exactly one question, under this id. It is also
/// where the answer sits in `--json` output, so chained calls can reference
/// it as `answers.answer...`.
pub const QUESTION_ID: &str = "answer";

/// Limits from the API reference, checked locally so the user gets a clear
/// message before any network round trip.
pub const MIN_CHOICE_OPTIONS: usize = 2;
pub const MAX_CHOICE_OPTIONS: usize = 255;
pub const MIN_SCORE_LEVELS: usize = 2;
pub const MAX_SCORE_LEVELS: usize = 10;

/// What a successful run wants printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The result, for stdout. Always ends with a newline.
    pub stdout: String,
    /// Diagnostics for stderr (`--verbose`), kept separate so piping stdout
    /// into another program never mixes them in.
    pub stderr: Option<String>,
}

/// Runs one question subcommand end to end.
///
/// `driver` is a `&dyn Driver` (dynamic dispatch) rather than a generic
/// `&impl Driver`: `main` only learns which driver to use at runtime, so it
/// holds a trait object anyway, and one non-generic `run` is simpler to read
/// than a function that gets compiled once per driver type. The cost, one
/// indirect call per request, is invisible next to an HTTP round trip.
///
/// `model` is the configured model, if any; otherwise the driver's default
/// is used. `stdin` is any `Read`er so tests can pass a byte slice.
pub fn run(
    cli: &Cli,
    driver: &dyn Driver,
    model: Option<&str>,
    stdin: impl Read,
    stdin_is_terminal: bool,
) -> Result<Output, AppError> {
    let (question, input) = match &cli.command {
        Command::Noul(args) => (noul_question(args), &args.input),
        Command::Choice(args) => (choice_question(args)?, &args.input),
        Command::Score(args) => (score_question(args)?, &args.input),
        Command::Config => {
            return Err(AppError::Usage(
                "`hunch config` does not ask the model anything".to_string(),
            ));
        }
    };
    // The question is validated (above) before reading the state, so a typo
    // in the options fails fast instead of after the user typed their input.
    let state = read_state(input, stdin, stdin_is_terminal)?;
    let model = model.unwrap_or_else(|| driver.default_model());
    let request = build_request(question, state, model);

    let evaluation = driver.evaluate(&request)?;

    let stdout = if cli.json {
        let mut raw = evaluation.raw;
        if !raw.ends_with('\n') {
            raw.push('\n');
        }
        raw
    } else {
        let answer = evaluation
            .response
            .answers
            .get(QUESTION_ID)
            .ok_or(AppError::MissingAnswer)?;
        render::answer(answer)
    };
    let stderr = cli.verbose.then(|| render::usage(&evaluation.response));
    Ok(Output { stdout, stderr })
}

/// Wraps a single question into a request. Pure: no I/O, no validation left
/// to do (the `*_question` builders already did it).
pub fn build_request(question: Question, state: Value, model: &str) -> Request {
    Request {
        state,
        model: model.to_string(),
        questions: BTreeMap::from([(QUESTION_ID.to_string(), question)]),
    }
}

pub fn noul_question(args: &NoulArgs) -> Question {
    // Only send `criteria` when at least one side was described; the API
    // treats a missing object and an empty one alike, but this keeps the
    // request minimal and readable in logs.
    let criteria = (args.yes.is_some() || args.no.is_some()).then(|| NoulCriteria {
        yes: args.yes.clone(),
        no: args.no.clone(),
    });
    Question::Noul {
        instructions: args.question.clone(),
        criteria,
    }
}

pub fn choice_question(args: &ChoiceArgs) -> Result<Question, AppError> {
    let count = args.options.len();
    if !(MIN_CHOICE_OPTIONS..=MAX_CHOICE_OPTIONS).contains(&count) {
        return Err(AppError::Usage(format!(
            "a choice needs {MIN_CHOICE_OPTIONS} to {MAX_CHOICE_OPTIONS} options, got {count} \
             (pass each with -o NAME or -o NAME=DESC)"
        )));
    }
    let mut criteria = BTreeMap::new();
    for option in &args.options {
        // `insert` returns the previous value if the key existed. A silent
        // overwrite would drop one of the user's descriptions, so refuse.
        if criteria
            .insert(option.name.clone(), option.description.clone())
            .is_some()
        {
            return Err(AppError::Usage(format!(
                "option `{}` is given more than once",
                option.name
            )));
        }
    }
    Ok(Question::Choice {
        instructions: args.question.clone(),
        criteria,
    })
}

pub fn score_question(args: &ScoreArgs) -> Result<Question, AppError> {
    let count = args.levels.len();
    if !(MIN_SCORE_LEVELS..=MAX_SCORE_LEVELS).contains(&count) {
        return Err(AppError::Usage(format!(
            "a score needs {MIN_SCORE_LEVELS} to {MAX_SCORE_LEVELS} levels, got {count} \
             (pass each with -l LEVEL, lowest first)"
        )));
    }
    Ok(Question::Score {
        instructions: args.question.clone(),
        criteria: args.levels.clone(),
    })
}

/// Gets the state text from `--state`, `--state-file` or stdin, then parses
/// it with [`parse_state`].
pub fn read_state(
    args: &StateArgs,
    mut stdin: impl Read,
    stdin_is_terminal: bool,
) -> Result<Value, AppError> {
    let text = match (&args.state, &args.state_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) if path == Path::new("-") => read_stdin(&mut stdin)?,
        (None, Some(path)) => fs::read_to_string(path).map_err(|source| AppError::ReadState {
            from: path.display().to_string(),
            source,
        })?,
        // Waiting on an interactive terminal would look like a hang.
        (None, None) if stdin_is_terminal => return Err(AppError::NoState),
        (None, None) => read_stdin(&mut stdin)?,
    };
    if text.trim().is_empty() {
        return Err(AppError::EmptyState);
    }
    Ok(parse_state(&text))
}

fn read_stdin(stdin: &mut impl Read) -> Result<String, AppError> {
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .map_err(|source| AppError::ReadState {
            from: "stdin".to_string(),
            source,
        })?;
    Ok(text)
}

/// Structured input stays structured: text that looks like a JSON object or
/// array *and* parses as one is sent as JSON, so Jev can address its fields
/// (e.g. another hunch's `--json` output, referenced as
/// `` `answers.answer.choice` ``). Anything else is sent as a JSON string,
/// trimmed so the trailing newline from `echo` or a file does not matter.
pub fn parse_state(text: &str) -> Value {
    let trimmed = text.trim();
    if trimmed.starts_with(['{', '['])
        && let Ok(value) = serde_json::from_str(trimmed)
    {
        return value;
    }
    Value::String(trimmed.to_string())
}

/// Text for `hunch config`: where every setting comes from, without ever
/// revealing the API key itself.
///
/// Like [`config::resolve`], the environment is injected, so this stays pure.
/// A missing key is *not* an error here: explaining that situation is the
/// main reason to run `hunch config` in the first place.
pub fn describe_config(
    overrides: &Overrides,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<String, AppError> {
    let (settings, key_status) = match config::resolve(overrides, env) {
        Ok(settings) => {
            let var = config::api_key_var(settings.driver);
            let status = if env(var).is_some_and(|key| !key.is_empty()) {
                format!("set (from ${var})")
            } else {
                "set (from config file)".to_string()
            };
            (settings, status)
        }
        Err(ConfigError::MissingApiKey { driver, .. }) => {
            // Resolve once more with a placeholder key so we can still show
            // every other setting. The placeholder never leaves this function.
            let var = config::api_key_var(driver);
            let with_placeholder = |key: &str| {
                if key == var {
                    Some("placeholder".to_string())
                } else {
                    env(key)
                }
            };
            let settings = config::resolve(overrides, &with_placeholder)?;
            let status = format!(
                "missing: export {var}=..., or add `api_key = \"...\"` under [{driver}] in the config file"
            );
            (settings, status)
        }
        Err(other) => return Err(other.into()),
    };

    let model = match &settings.model {
        Some(model) => model.clone(),
        None => format!("{} (default)", default_model(settings.driver)),
    };
    let base_url = match &settings.base_url {
        Some(url) => url.clone(),
        None => format!("{} (default)", default_base_url(settings.driver)),
    };
    let config_file = match (
        &settings.config_path,
        config::config_file_path(overrides, env),
    ) {
        (Some(path), _) => path.display().to_string(),
        (None, Some(location)) => format!("{} (not found)", location.path().display()),
        (None, None) => "none".to_string(),
    };

    Ok(format!(
        "driver:      {}\n\
         model:       {model}\n\
         base URL:    {base_url}\n\
         config file: {config_file}\n\
         API key:     {key_status}\n",
        settings.driver
    ))
}

fn default_model(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::TypeSafe => driver::typesafe::DEFAULT_MODEL,
        DriverKind::OpenRouter => driver::openrouter::DEFAULT_MODEL,
    }
}

fn default_base_url(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::TypeSafe => driver::typesafe::DEFAULT_BASE_URL,
        DriverKind::OpenRouter => driver::openrouter::DEFAULT_BASE_URL,
    }
}

/// Everything that can make a run fail, with the exit code each maps to.
#[derive(Debug)]
pub enum AppError {
    /// Bad configuration (file, env, missing key).
    Config(ConfigError),
    /// Arguments that clap accepted but that make no sense (counts, dupes).
    Usage(String),
    /// No `--state`/`--state-file`, and stdin is an interactive terminal.
    NoState,
    /// The state was given but is empty or whitespace only.
    EmptyState,
    /// The state file (or stdin) could not be read.
    ReadState { from: String, source: io::Error },
    /// The provider call failed (after retries).
    Driver(DriverError),
    /// The response did not contain an answer for our question.
    MissingAnswer,
    /// Writing the output failed.
    Output(io::Error),
}

impl AppError {
    /// The process exit code: 2 for problems the user can fix in how they
    /// invoked hunch (like clap's own usage errors), 1 for failures at run
    /// time. Returned as `u8` rather than `ExitCode` because `u8` can be
    /// compared in tests; `main` converts with `ExitCode::from`.
    pub fn exit_code(&self) -> u8 {
        match self {
            AppError::Config(_)
            | AppError::Usage(_)
            | AppError::NoState
            | AppError::EmptyState
            | AppError::ReadState { .. } => 2,
            AppError::Driver(_) | AppError::MissingAnswer | AppError::Output(_) => 1,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Config(err) => err.fmt(f),
            AppError::Usage(message) => f.write_str(message),
            AppError::NoState => f.write_str(
                "no state given: pass --state TEXT, --state-file PATH, \
                 or pipe it in (e.g. `echo \"...\" | hunch noul ...`)",
            ),
            AppError::EmptyState => f.write_str("the state is empty"),
            AppError::ReadState { from, source } => {
                write!(f, "could not read state from {from}: {source}")
            }
            AppError::Driver(err) => err.fmt(f),
            AppError::MissingAnswer => {
                write!(f, "the response has no answer for question `{QUESTION_ID}`")
            }
            AppError::Output(err) => write!(f, "could not write output: {err}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppError::Config(err) => Some(err),
            AppError::Driver(err) => Some(err),
            AppError::ReadState { source, .. } | AppError::Output(source) => Some(source),
            AppError::Usage(_)
            | AppError::NoState
            | AppError::EmptyState
            | AppError::MissingAnswer => None,
        }
    }
}

/// `From` impls let `?` convert errors automatically: inside a function
/// returning `Result<_, AppError>`, `driver.evaluate(..)?` turns a
/// `DriverError` into `AppError::Driver` without a `map_err`.
impl From<ConfigError> for AppError {
    fn from(err: ConfigError) -> Self {
        AppError::Config(err)
    }
}

impl From<DriverError> for AppError {
    fn from(err: DriverError) -> Self {
        AppError::Driver(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    use clap::Parser;
    use serde_json::json;

    use crate::wire::{Answer, Evaluation, Response, Usage};

    /// A driver that never touches the network: it records the request it
    /// was given and replies with a canned result.
    ///
    /// `evaluate` takes `&self`, yet we want to store the request. `RefCell`
    /// moves Rust's borrow check for its contents from compile time to run
    /// time, which allows mutation through a shared reference ("interior
    /// mutability"). Fine for single-threaded test code.
    struct FakeDriver {
        reply: Result<Evaluation, DriverError>,
        seen: RefCell<Option<Request>>,
    }

    impl FakeDriver {
        fn answering(answer: Answer) -> Self {
            let response = Response {
                model: "jev-test".into(),
                answers: BTreeMap::from([(QUESTION_ID.to_string(), answer)]),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                },
            };
            Self {
                reply: Ok(Evaluation {
                    response,
                    raw: r#"{"canned":   "raw body"}"#.to_string(),
                }),
                seen: RefCell::new(None),
            }
        }

        fn failing(error: DriverError) -> Self {
            Self {
                reply: Err(error),
                seen: RefCell::new(None),
            }
        }

        /// The request the driver received; panics if it was never called.
        fn request(&self) -> Request {
            self.seen.borrow().clone().expect("driver was not called")
        }
    }

    impl Driver for FakeDriver {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn default_model(&self) -> &str {
            "fake-default"
        }

        fn evaluate(&self, request: &Request) -> Result<Evaluation, DriverError> {
            *self.seen.borrow_mut() = Some(request.clone());
            self.reply.clone()
        }
    }

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("hunch").chain(args.iter().copied())).unwrap()
    }

    /// Runs `args` with no stdin (an empty, non-terminal reader).
    fn run_args(args: &[&str], driver: &FakeDriver) -> Result<Output, AppError> {
        run(&cli(args), driver, None, io::empty(), false)
    }

    fn noul_answer() -> Answer {
        Answer::Noul { noul: 0.9 }
    }

    // --- request building ---------------------------------------------------

    #[test]
    fn builds_noul_request_without_criteria() {
        let driver = FakeDriver::answering(noul_answer());
        run_args(&["noul", "Urgent?", "--state", "help"], &driver).unwrap();

        let request = driver.request();
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "state": "help",
                "model": "fake-default",
                "questions": { "answer": { "type": "noul", "instructions": "Urgent?" } }
            })
        );
    }

    #[test]
    fn builds_noul_request_with_criteria() {
        let driver = FakeDriver::answering(noul_answer());
        run_args(
            &["noul", "Urgent?", "--yes", "Time-sensitive", "--state", "x"],
            &driver,
        )
        .unwrap();

        assert_eq!(
            driver.request().questions[QUESTION_ID],
            Question::Noul {
                instructions: "Urgent?".into(),
                criteria: Some(NoulCriteria {
                    yes: Some("Time-sensitive".into()),
                    no: None,
                }),
            }
        );
    }

    #[test]
    fn builds_choice_request_with_and_without_descriptions() {
        let driver = FakeDriver::answering(noul_answer());
        run_args(
            &[
                "choice",
                "Team?",
                "-o",
                "billing=Payments",
                "-o",
                "sales",
                "--state",
                "x",
            ],
            &driver,
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(&driver.request().questions[QUESTION_ID]).unwrap(),
            json!({
                "type": "choice",
                "instructions": "Team?",
                "criteria": { "billing": "Payments", "sales": null }
            })
        );
    }

    #[test]
    fn builds_score_request_keeping_level_order() {
        let driver = FakeDriver::answering(noul_answer());
        run_args(
            &[
                "score", "Mood?", "-l", "Calm", "-l", "Angry", "--state", "x",
            ],
            &driver,
        )
        .unwrap();

        assert_eq!(
            driver.request().questions[QUESTION_ID],
            Question::Score {
                instructions: "Mood?".into(),
                criteria: vec!["Calm".into(), "Angry".into()],
            }
        );
    }

    #[test]
    fn configured_model_wins_over_driver_default() {
        let driver = FakeDriver::answering(noul_answer());
        run(
            &cli(&["noul", "q", "--state", "x"]),
            &driver,
            Some("jev-1.13.0"),
            io::empty(),
            false,
        )
        .unwrap();
        assert_eq!(driver.request().model, "jev-1.13.0");
    }

    #[test]
    fn rejects_bad_counts_before_calling_the_driver() {
        let cases: [&[&str]; 3] = [
            &["choice", "q", "-o", "only", "--state", "x"],
            &["score", "q", "-l", "only", "--state", "x"],
            &["choice", "q", "-o", "a", "-o", "a=dupe", "--state", "x"],
        ];
        for args in cases {
            let driver = FakeDriver::answering(noul_answer());
            let err = run_args(args, &driver).unwrap_err();
            assert!(matches!(err, AppError::Usage(_)), "{args:?}: {err:?}");
            assert_eq!(err.exit_code(), 2);
            assert!(driver.seen.borrow().is_none(), "driver called for {args:?}");
        }
    }

    #[test]
    fn score_accepts_ten_levels_but_not_eleven() {
        let levels = |n: usize| ScoreArgs {
            question: "q".into(),
            levels: (0..n).map(|i| i.to_string()).collect(),
            input: StateArgs::default(),
        };
        assert!(score_question(&levels(10)).is_ok());
        assert!(score_question(&levels(11)).is_err());
    }

    // --- state input ----------------------------------------------------------

    #[test]
    fn plain_text_state_becomes_a_trimmed_json_string() {
        assert_eq!(parse_state("  my payouts fail\n"), json!("my payouts fail"));
    }

    #[test]
    fn json_object_and_array_state_stay_structured() {
        assert_eq!(
            parse_state("{\"answers\": {\"answer\": {\"choice\": \"billing\"}}}\n"),
            json!({ "answers": { "answer": { "choice": "billing" } } })
        );
        assert_eq!(parse_state(" [1, 2] "), json!([1, 2]));
    }

    #[test]
    fn text_that_only_looks_like_json_stays_a_string() {
        assert_eq!(parse_state("{not json}"), json!("{not json}"));
        // Valid JSON, but not an object/array: still just text.
        assert_eq!(parse_state("42"), json!("42"));
    }

    #[test]
    fn reads_state_from_stdin_when_not_a_terminal() {
        let state = read_state(&StateArgs::default(), "piped text\n".as_bytes(), false);
        assert_eq!(state.unwrap(), json!("piped text"));
    }

    #[test]
    fn refuses_to_wait_on_a_terminal() {
        let err = read_state(&StateArgs::default(), io::empty(), true).unwrap_err();
        assert!(matches!(err, AppError::NoState));
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--state"));
    }

    #[test]
    fn state_flag_wins_over_stdin() {
        let args = StateArgs {
            state: Some("from flag".into()),
            state_file: None,
        };
        let state = read_state(&args, "from stdin".as_bytes(), false).unwrap();
        assert_eq!(state, json!("from flag"));
    }

    #[test]
    fn state_file_dash_reads_stdin_even_on_a_terminal() {
        let args = StateArgs {
            state: None,
            state_file: Some("-".into()),
        };
        let state = read_state(&args, "{\"a\": 1}".as_bytes(), true).unwrap();
        assert_eq!(state, json!({ "a": 1 }));
    }

    #[test]
    fn reads_state_from_a_file() {
        // A per-process file name, so parallel test runs never collide.
        let path = std::env::temp_dir().join(format!("hunch-state-{}.txt", std::process::id()));
        fs::write(&path, "from a file\n").unwrap();
        let args = StateArgs {
            state: None,
            state_file: Some(path.clone()),
        };
        let state = read_state(&args, io::empty(), true);
        fs::remove_file(&path).unwrap();
        assert_eq!(state.unwrap(), json!("from a file"));
    }

    #[test]
    fn missing_state_file_is_an_input_error() {
        let args = StateArgs {
            state: None,
            state_file: Some("/nonexistent/hunch-state".into()),
        };
        let err = read_state(&args, io::empty(), false).unwrap_err();
        assert!(matches!(err, AppError::ReadState { .. }));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn empty_state_is_rejected() {
        let err = read_state(&StateArgs::default(), " \n".as_bytes(), false).unwrap_err();
        assert!(matches!(err, AppError::EmptyState));
    }

    // --- output ---------------------------------------------------------------

    #[test]
    fn json_flag_prints_the_raw_body_verbatim() {
        let driver = FakeDriver::answering(noul_answer());
        let output = run_args(&["--json", "noul", "q", "--state", "x"], &driver).unwrap();
        assert_eq!(output.stdout, "{\"canned\":   \"raw body\"}\n");
        assert_eq!(output.stderr, None);
    }

    #[test]
    fn human_output_renders_the_answer() {
        let driver = FakeDriver::answering(noul_answer());
        let output = run_args(&["noul", "q", "--state", "x"], &driver).unwrap();
        assert_eq!(output.stdout, render::noul(0.9));
    }

    #[test]
    fn verbose_puts_usage_on_stderr_only() {
        let driver = FakeDriver::answering(noul_answer());
        let output = run_args(&["noul", "q", "--state", "x", "-v"], &driver).unwrap();
        assert_eq!(
            output.stderr.as_deref(),
            Some("model jev-test, 10 input + 2 output tokens\n")
        );
        assert!(!output.stdout.contains("tokens"));
    }

    #[test]
    fn driver_errors_exit_with_1() {
        let driver = FakeDriver::failing(DriverError::Transport("refused".into()));
        let err = run_args(&["noul", "q", "--state", "x"], &driver).unwrap_err();
        assert!(matches!(err, AppError::Driver(_)));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn missing_answer_is_a_runtime_error() {
        let mut driver = FakeDriver::answering(noul_answer());
        if let Ok(evaluation) = &mut driver.reply {
            evaluation.response.answers.clear();
        }
        let err = run_args(&["noul", "q", "--state", "x"], &driver).unwrap_err();
        assert!(matches!(err, AppError::MissingAnswer));
        assert_eq!(err.exit_code(), 1);
    }

    // --- hunch config -------------------------------------------------------

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn config_shows_key_source_but_never_the_key() {
        let env = env_from(&[("TYPESAFE_API_KEY", "sk-secret-123")]);
        let text = describe_config(&Overrides::default(), &env).unwrap();
        assert!(text.contains("driver:      typesafe\n"), "{text}");
        assert!(
            text.contains("model:       jev-latest (default)\n"),
            "{text}"
        );
        assert!(
            text.contains("API key:     set (from $TYPESAFE_API_KEY)\n"),
            "{text}"
        );
        assert!(!text.contains("sk-secret-123"));
    }

    #[test]
    fn config_explains_a_missing_key_instead_of_failing() {
        let env = env_from(&[
            ("HUNCH_DRIVER", "openrouter"),
            ("OPENROUTER_BASE_URL", "http://127.0.0.1:9"),
        ]);
        let text = describe_config(&Overrides::default(), &env).unwrap();
        assert!(text.contains("driver:      openrouter\n"), "{text}");
        assert!(text.contains("base URL:    http://127.0.0.1:9\n"), "{text}");
        assert!(
            text.contains("API key:     missing: export OPENROUTER_API_KEY"),
            "{text}"
        );
        assert!(!text.contains("placeholder"));
    }

    #[test]
    fn config_errors_still_fail_with_exit_2() {
        let env = env_from(&[("HUNCH_CONFIG", "/nonexistent/hunch.toml")]);
        let err = describe_config(&Overrides::default(), &env).unwrap_err();
        assert!(matches!(
            err,
            AppError::Config(ConfigError::NotFound { .. })
        ));
        assert_eq!(err.exit_code(), 2);
    }
}
