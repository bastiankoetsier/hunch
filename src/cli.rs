//! Command-line arguments, declared with clap's derive API.
//!
//! This module only describes the *shape* of the command line. It does not
//! validate counts (e.g. "a choice needs at least two options") or read any
//! input: that lives in [`crate::app`], where it can produce friendlier
//! errors and is easy to unit-test without going through clap.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::Overrides;
use crate::driver::DriverKind;

/// Ask Jev (TypeSafe's System One model) for calibrated gut-check judgments.
#[derive(Debug, Parser)]
#[command(name = "hunch", version)]
pub struct Cli {
    // A plain `//` comment on purpose: clap turns `///` doc comments into
    // help text, and this note is for readers of the code, not users.
    //
    // No `value_parser` attribute needed: for a type that implements
    // `FromStr` (with an error type that implements `Error`), clap's derive
    // falls back to `FromStr` on its own. So the driver names and the
    // case-insensitive matching stay defined in exactly one place
    // (`DriverKind`), shared with `$HUNCH_DRIVER` and the config file.
    /// Provider to send the request through [possible values: typesafe, openrouter]
    #[arg(long, global = true, value_name = "DRIVER")]
    pub driver: Option<DriverKind>,

    /// Model id (default: the driver's own default, e.g. jev-latest)
    #[arg(long, global = true, value_name = "ID")]
    pub model: Option<String>,

    /// Config file to read instead of ~/.config/hunch/config.toml
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Print the raw JSON response instead of a human-readable summary
    #[arg(long, global = true)]
    pub json: bool,

    /// Print the model and token usage to stderr
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// The subset of flags that feed into layered configuration.
    pub fn overrides(&self) -> Overrides {
        Overrides {
            driver: self.driver,
            model: self.model.clone(),
            config_path: self.config.clone(),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Probability that a statement about the state is true
    Noul(NoulArgs),
    /// Pick the best of several options
    Choice(ChoiceArgs),
    /// Place the state on an ordered scale of levels
    Score(ScoreArgs),
    /// Show the resolved driver, model, endpoint and API key source
    Config,
}

#[derive(Debug, Args)]
pub struct NoulArgs {
    /// The yes/no question, e.g. "Does this convey urgency?"
    pub question: String,

    /// What "yes" means (optional)
    #[arg(long, value_name = "DESC")]
    pub yes: Option<String>,

    /// What "no" means (optional)
    #[arg(long, value_name = "DESC")]
    pub no: Option<String>,

    #[command(flatten)]
    pub input: StateArgs,
}

#[derive(Debug, Args)]
pub struct ChoiceArgs {
    /// The question, e.g. "Which team should handle this?"
    pub question: String,

    /// An option, optionally with a description (repeat, at least 2)
    #[arg(
        short = 'o',
        long = "option",
        value_name = "NAME[=DESC]",
        required = true,
        value_parser = parse_choice_option,
    )]
    pub options: Vec<ChoiceOption>,

    #[command(flatten)]
    pub input: StateArgs,
}

#[derive(Debug, Args)]
pub struct ScoreArgs {
    /// The question, e.g. "How frustrated is the customer?"
    pub question: String,

    /// A level description, lowest first (repeat, 2 to 10 levels)
    #[arg(short = 'l', long = "level", value_name = "LEVEL", required = true)]
    pub levels: Vec<String>,

    #[command(flatten)]
    pub input: StateArgs,
}

/// Where the state (the content to judge) comes from. Shared by every
/// question subcommand through `#[command(flatten)]`, which splices these
/// fields into the parent's arguments as if they were declared there.
///
/// With neither flag, the state is read from stdin, unless stdin is a
/// terminal (then hunch would just sit there waiting, so it errors instead).
#[derive(Debug, Default, Args)]
pub struct StateArgs {
    /// The state as inline text (JSON objects/arrays are sent as structured data)
    #[arg(long, value_name = "TEXT", conflicts_with = "state_file")]
    pub state: Option<String>,

    /// Read the state from a file; `-` reads stdin
    #[arg(long, value_name = "PATH")]
    pub state_file: Option<PathBuf>,
}

/// One `--option NAME[=DESC]` of a choice question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOption {
    pub name: String,
    pub description: Option<String>,
}

/// Custom clap `value_parser`: splits `NAME=DESC` at the *first* `=`, so
/// descriptions may themselves contain `=`. clap shows the returned `String`
/// as the error message, prefixed with the offending flag and value.
pub fn parse_choice_option(raw: &str) -> Result<ChoiceOption, String> {
    let (name, description) = match raw.split_once('=') {
        Some((name, description)) => (name, Some(description.trim())),
        None => (raw, None),
    };
    let name = name.trim();
    if name.is_empty() {
        return Err("option name must not be empty (expected NAME or NAME=DESC)".to_string());
    }
    Ok(ChoiceOption {
        name: name.to_string(),
        // `name=` with nothing after it means "no description", not "".
        description: description
            .filter(|description| !description.is_empty())
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap checks its own configuration (conflicting ids, bad defaults, ...)
    /// lazily at runtime. `debug_assert` runs all of those checks up front,
    /// so a mistake in the attributes above fails here, not in a user's hands.
    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_bare_option_name() {
        assert_eq!(
            parse_choice_option("sales"),
            Ok(ChoiceOption {
                name: "sales".into(),
                description: None
            })
        );
    }

    #[test]
    fn splits_option_at_first_equals_sign() {
        assert_eq!(
            parse_choice_option(" billing = Payments, a=b "),
            Ok(ChoiceOption {
                name: "billing".into(),
                description: Some("Payments, a=b".into())
            })
        );
    }

    #[test]
    fn empty_description_means_none() {
        assert_eq!(
            parse_choice_option("sales="),
            Ok(ChoiceOption {
                name: "sales".into(),
                description: None
            })
        );
    }

    #[test]
    fn rejects_empty_option_name() {
        assert!(parse_choice_option("=Payments").is_err());
        assert!(parse_choice_option("  ").is_err());
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "hunch",
            "noul",
            "Urgent?",
            "--state",
            "x",
            "--driver",
            "OpenRouter",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.driver, Some(DriverKind::OpenRouter));
        assert!(cli.json);
    }

    #[test]
    fn rejects_unknown_driver() {
        let err = Cli::try_parse_from(["hunch", "--driver", "openai", "config"]).unwrap_err();
        assert!(err.to_string().contains("unknown driver `openai`"));
    }

    #[test]
    fn state_and_state_file_conflict() {
        let result =
            Cli::try_parse_from(["hunch", "noul", "q", "--state", "x", "--state-file", "f"]);
        assert!(result.is_err());
    }

    #[test]
    fn collects_repeated_options() {
        let cli = Cli::try_parse_from([
            "hunch",
            "choice",
            "Team?",
            "-o",
            "billing=Payments",
            "-o",
            "sales",
        ])
        .unwrap();
        let Command::Choice(args) = cli.command else {
            panic!("expected the choice subcommand");
        };
        assert_eq!(args.options.len(), 2);
        assert_eq!(args.options[0].description.as_deref(), Some("Payments"));
    }
}
