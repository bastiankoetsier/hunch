//! The driver abstraction: *how* a request reaches Jev.
//!
//! Everything above this module (CLI, rendering) talks to `dyn Driver` and never
//! knows whether it is hitting TypeSafe directly, going through OpenRouter, or
//! talking to a test fake.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::wire::{Evaluation, Request};

/// Something that can evaluate a [`Request`] against Jev.
pub trait Driver {
    /// Short identifier, e.g. `"typesafe"`. Used in error messages.
    fn name(&self) -> &'static str;

    /// Model id used when the user did not pick one. Providers name the same
    /// model differently (`jev-latest` vs `~typesafe/jev-latest`).
    fn default_model(&self) -> &str;

    fn evaluate(&self, request: &Request) -> Result<Evaluation, DriverError>;
}

/// The drivers hunch knows how to build. Selected via flag, env or config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DriverKind {
    #[default]
    TypeSafe,
    OpenRouter,
}

impl DriverKind {
    pub const ALL: [DriverKind; 2] = [DriverKind::TypeSafe, DriverKind::OpenRouter];

    pub fn as_str(self) -> &'static str {
        match self {
            DriverKind::TypeSafe => "typesafe",
            DriverKind::OpenRouter => "openrouter",
        }
    }
}

impl fmt::Display for DriverKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DriverKind {
    type Err = UnknownDriver;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| UnknownDriver(s.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownDriver(pub String);

impl fmt::Display for UnknownDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown driver `{}` (expected one of: typesafe, openrouter)",
            self.0
        )
    }
}

impl std::error::Error for UnknownDriver {}

/// Everything that can go wrong talking to a provider, normalised across
/// drivers so callers can react (e.g. retry on `RateLimited`) without caring
/// which provider produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum DriverError {
    /// 401/403: missing or invalid API key.
    Unauthorized { message: String },
    /// 422 (or 400): the request failed validation.
    InvalidRequest { message: String },
    /// 429: back off, optionally for the time the server asked for.
    RateLimited { retry_after: Option<Duration> },
    /// 529 / 503: provider temporarily overloaded.
    Overloaded,
    /// Any other non-success status.
    Api { status: u16, message: String },
    /// Could not reach the server (DNS, TLS, connection refused, timeout).
    Transport(String),
    /// The server answered 2xx but the body was not a valid response.
    Decode(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::Unauthorized { message } => write!(f, "unauthorized: {message}"),
            DriverError::InvalidRequest { message } => write!(f, "invalid request: {message}"),
            DriverError::RateLimited {
                retry_after: Some(wait),
            } => write!(f, "rate limited, retry after {}s", wait.as_secs()),
            DriverError::RateLimited { retry_after: None } => f.write_str("rate limited"),
            DriverError::Overloaded => f.write_str("provider is overloaded, try again shortly"),
            DriverError::Api { status, message } => write!(f, "HTTP {status}: {message}"),
            DriverError::Transport(message) => write!(f, "could not reach provider: {message}"),
            DriverError::Decode(message) => write!(f, "unexpected response body: {message}"),
        }
    }
}

impl std::error::Error for DriverError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_driver_names_case_insensitively() {
        assert_eq!("typesafe".parse(), Ok(DriverKind::TypeSafe));
        assert_eq!(" OpenRouter ".parse(), Ok(DriverKind::OpenRouter));
        assert_eq!(
            "openai".parse::<DriverKind>(),
            Err(UnknownDriver("openai".into()))
        );
    }

    #[test]
    fn display_round_trips_through_from_str() {
        for kind in DriverKind::ALL {
            assert_eq!(kind.to_string().parse(), Ok(kind));
        }
    }
}
