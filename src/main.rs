//! The `hunch` binary: everything that touches the real world (process
//! environment, terminal, stdout/stderr, exit code) lives here, and nothing
//! else does. The logic it wires together is unit-tested in the library.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use clap::Parser;

use hunch::app::{self, AppError};
use hunch::cli::{Cli, Command};
use hunch::config;
use hunch::driver::{self, DriverConfig, retry::Retry};

/// Returning `ExitCode` (instead of calling `std::process::exit`) lets
/// `main` end normally, so destructors run and buffered output is flushed.
fn main() -> ExitCode {
    // On invalid arguments or `--help`, clap prints and exits by itself
    // (with code 2 for usage errors, 0 for help).
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hunch: error: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run(cli: &Cli) -> Result<(), AppError> {
    let overrides = cli.overrides();

    if let Command::Config = cli.command {
        let env = |key: &str| std::env::var(key).ok();
        return write_stdout(&app::describe_config(&overrides, &env)?);
    }

    let settings = config::resolve_from_process_env(&overrides)?;
    // The concrete driver is only known at runtime (`Box<dyn Driver>`); the
    // retry decorator wraps it like any other driver.
    let driver = Retry::new(driver::build(
        settings.driver,
        DriverConfig {
            api_key: settings.api_key,
            base_url: settings.base_url,
            timeout: cli.timeout(),
        },
    ));

    let stdin = io::stdin();
    let stdin_is_terminal = stdin.is_terminal();
    let output = app::run(
        cli,
        &driver,
        settings.model.as_deref(),
        stdin.lock(),
        stdin_is_terminal,
    )?;

    if let Some(diagnostics) = &output.stderr {
        eprint!("{diagnostics}");
    }
    write_stdout(&output.stdout)
}

/// Like `print!`, but a closed pipe (`hunch ... | head -c1`) ends quietly
/// instead of panicking, and other write errors become a normal error.
fn write_stdout(text: &str) -> Result<(), AppError> {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(err) if err.kind() != io::ErrorKind::BrokenPipe => Err(AppError::Output(err)),
        _ => Ok(()),
    }
}
