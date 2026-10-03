//! Layered configuration: which driver to use, with which model and key.
//!
//! Every setting is looked up in the same order, first hit wins:
//!
//! 1. CLI flag ([`Overrides`])
//! 2. environment variable
//! 3. config file (TOML, see [`config_file_path`] for where it is looked up)
//! 4. built-in default
//!
//! API keys deliberately skip step 1: a key passed as a flag ends up in shell
//! history and in `ps` output, so keys only come from the environment or the
//! config file.
//!
//! The environment is *injected* into [`resolve`] as a closure instead of
//! being read with `std::env::var` directly. That keeps resolution a pure
//! function of its inputs, which is what makes it testable: in edition 2024
//! `std::env::set_var` is `unsafe` (other threads may be reading the
//! environment at the same time), and `cargo test` runs tests in parallel
//! threads, so tests must never mutate the real process environment.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::driver::{DriverKind, UnknownDriver};

/// Selects the driver (`typesafe` / `openrouter`).
pub const ENV_DRIVER: &str = "HUNCH_DRIVER";
/// Overrides the model id for whichever driver is selected.
pub const ENV_MODEL: &str = "HUNCH_MODEL";
/// Explicit path to the config file.
pub const ENV_CONFIG: &str = "HUNCH_CONFIG";

/// Values coming from CLI flags (all optional).
///
/// There is intentionally no `api_key` here: see the module docs.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub driver: Option<DriverKind>,
    pub model: Option<String>,
    pub config_path: Option<PathBuf>,
}

/// Fully resolved settings for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub driver: DriverKind,
    /// `None` means "use the driver's default model".
    pub model: Option<String>,
    pub api_key: String,
    /// `None` means "use the driver's default endpoint".
    pub base_url: Option<String>,
    /// The config file that was actually read, if any.
    pub config_path: Option<PathBuf>,
}

/// Name of the environment variable holding the API key for `driver`.
///
/// These match the names used by each provider's official SDKs, so a key the
/// user already exported for other tools just works.
pub fn api_key_var(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::TypeSafe => "TYPESAFE_API_KEY",
        DriverKind::OpenRouter => "OPENROUTER_API_KEY",
    }
}

/// Name of the environment variable overriding the endpoint for `driver`.
pub fn base_url_var(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::TypeSafe => "TYPESAFE_BASE_URL",
        DriverKind::OpenRouter => "OPENROUTER_BASE_URL",
    }
}

/// Resolves [`Settings`] from CLI flags, the given environment and the config
/// file.
///
/// `env` looks up one environment variable; production code passes
/// `|key| std::env::var(key).ok()` (or uses [`resolve_from_process_env`]),
/// tests pass a closure over a `HashMap`. Empty values count as unset, so
/// `HUNCH_DRIVER= hunch ...` falls through to the next layer.
pub fn resolve(
    overrides: &Overrides,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Settings, ConfigError> {
    // Wrap the injected lookup once so that "empty means unset" is applied to
    // every variable we read, not just the ones we remember to check.
    let var = |key: &str| env(key).filter(|value| !value.is_empty());

    let location = config_file_path(overrides, &var);
    let file = match &location {
        Some(location) => load_file(location)?,
        None => None,
    };
    // Every later lookup can treat "no file" like "empty file".
    let file_config = file.unwrap_or_default();
    let config_path = file_config.path.clone();

    let driver = if let Some(kind) = overrides.driver {
        kind
    } else if let Some(raw) = var(ENV_DRIVER) {
        parse_driver(&raw, format!("${ENV_DRIVER}"))?
    } else if let Some(raw) = &file_config.contents.driver {
        let origin = format!("`driver` in {}", file_config.display_path());
        parse_driver(raw, origin)?
    } else {
        DriverKind::default()
    };

    let section = file_config.contents.section(driver);

    // `or_else` only runs its closure when the previous layer was `None`, so
    // this reads top to bottom exactly like the precedence list.
    let model = overrides
        .model
        .clone()
        .or_else(|| var(ENV_MODEL))
        .or_else(|| section.model.clone());

    let base_url = var(base_url_var(driver)).or_else(|| section.base_url.clone());

    let api_key = var(api_key_var(driver))
        .or_else(|| section.api_key.clone())
        .ok_or_else(|| ConfigError::MissingApiKey {
            driver,
            // Point the user at the file we read or, failing that, at the
            // file we *would* have read, so the hint is copy-pasteable.
            config_path: location.map(ConfigLocation::into_path),
        })?;

    Ok(Settings {
        driver,
        model,
        api_key,
        base_url,
        config_path,
    })
}

/// [`resolve`] against the real process environment.
pub fn resolve_from_process_env(overrides: &Overrides) -> Result<Settings, ConfigError> {
    resolve(overrides, &|key| std::env::var(key).ok())
}

/// Where the config file lives, and whether the user asked for it explicitly.
///
/// The distinction matters for a missing file: if the user pointed us at a
/// path, a typo there should be an error, but nobody is required to create
/// the default file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLocation {
    /// From `--config` or `$HUNCH_CONFIG`: must exist.
    Explicit(PathBuf),
    /// Discovered under the XDG config directory: optional.
    Default(PathBuf),
}

impl ConfigLocation {
    pub fn path(&self) -> &Path {
        match self {
            ConfigLocation::Explicit(path) | ConfigLocation::Default(path) => path,
        }
    }

    pub fn into_path(self) -> PathBuf {
        match self {
            ConfigLocation::Explicit(path) | ConfigLocation::Default(path) => path,
        }
    }
}

/// Finds the config file path, first hit wins:
///
/// 1. `--config <path>`
/// 2. `$HUNCH_CONFIG`
/// 3. `$XDG_CONFIG_HOME/hunch/config.toml`
/// 4. `$HOME/.config/hunch/config.toml` (also on macOS, on purpose: CLI tools
///    are easier to manage when their config lives next to everyone else's
///    in `~/.config` rather than in `~/Library/Application Support`)
///
/// Returns `None` only if none of these variables are set. The file itself
/// is not touched here.
pub fn config_file_path(
    overrides: &Overrides,
    env: &impl Fn(&str) -> Option<String>,
) -> Option<ConfigLocation> {
    let var = |key: &str| env(key).filter(|value| !value.is_empty());

    if let Some(path) = &overrides.config_path {
        return Some(ConfigLocation::Explicit(path.clone()));
    }
    if let Some(path) = var(ENV_CONFIG) {
        return Some(ConfigLocation::Explicit(PathBuf::from(path)));
    }

    let config_home = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        // The XDG spec says relative values must be ignored.
        .filter(|path| path.is_absolute())
        .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    Some(ConfigLocation::Default(
        config_home.join("hunch").join("config.toml"),
    ))
}

/// The on-disk format. `deny_unknown_fields` turns a typo like `api-key` or
/// `[open_router]` into an error instead of a silently ignored setting.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContents {
    /// Kept as a string so an invalid value gets our own error message
    /// (via `DriverKind::from_str`) rather than serde's generic one.
    driver: Option<String>,
    #[serde(default)]
    typesafe: DriverSection,
    #[serde(default)]
    openrouter: DriverSection,
}

impl FileContents {
    fn section(&self, driver: DriverKind) -> &DriverSection {
        match driver {
            DriverKind::TypeSafe => &self.typesafe,
            DriverKind::OpenRouter => &self.openrouter,
        }
    }
}

/// One `[typesafe]` / `[openrouter]` table. `Option` fields are optional in
/// the file without any extra serde attributes.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverSection {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
}

impl DriverSection {
    /// Treat `api_key = ""` like a missing key, consistent with empty env
    /// vars, so the user gets the helpful "missing key" error instead of a
    /// confusing 401 later.
    fn clear_empty(&mut self) {
        for field in [&mut self.api_key, &mut self.base_url, &mut self.model] {
            // `take_if` replaces the value with `None` when the predicate holds.
            field.take_if(|value| value.is_empty());
        }
    }
}

/// A parsed config file plus where it came from (`None` for "no file").
#[derive(Debug, Default)]
struct FileConfig {
    path: Option<PathBuf>,
    contents: FileContents,
}

impl FileConfig {
    fn display_path(&self) -> String {
        match &self.path {
            Some(path) => path.display().to_string(),
            None => "the config file".to_string(),
        }
    }
}

/// Reads and parses the file at `location`. `Ok(None)` means "no file, and
/// that is fine" (a missing file at a discovered default path).
fn load_file(location: &ConfigLocation) -> Result<Option<FileConfig>, ConfigError> {
    let path = location.path();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return match location {
                ConfigLocation::Default(_) => Ok(None),
                ConfigLocation::Explicit(_) => Err(ConfigError::NotFound {
                    path: path.to_path_buf(),
                }),
            };
        }
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let mut contents: FileContents =
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    contents.typesafe.clear_empty();
    contents.openrouter.clear_empty();
    contents.driver.take_if(|driver| driver.is_empty());

    Ok(Some(FileConfig {
        path: Some(path.to_path_buf()),
        contents,
    }))
}

fn parse_driver(raw: &str, origin: String) -> Result<DriverKind, ConfigError> {
    raw.parse()
        .map_err(|source| ConfigError::InvalidDriver { origin, source })
}

/// Everything that can go wrong while resolving [`Settings`]. Each message
/// says *where* the problem is so the user can fix it without guessing.
#[derive(Debug)]
pub enum ConfigError {
    /// An explicitly requested config file (`--config` / `$HUNCH_CONFIG`)
    /// does not exist.
    NotFound { path: PathBuf },
    /// The config file exists but could not be read (permissions, ...).
    Read { path: PathBuf, source: io::Error },
    /// The config file is not valid TOML or has unknown/mistyped fields.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A driver name (from env or file) is not one we know. `origin`
    /// describes where it came from, e.g. "$HUNCH_DRIVER".
    InvalidDriver {
        origin: String,
        source: UnknownDriver,
    },
    /// No API key for the selected driver in env or file.
    MissingApiKey {
        driver: DriverKind,
        /// Config file to suggest in the hint, if one could be determined.
        config_path: Option<PathBuf>,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NotFound { path } => {
                write!(f, "config file {} does not exist", path.display())
            }
            ConfigError::Read { path, source } => {
                write!(f, "could not read config file {}: {source}", path.display())
            }
            ConfigError::Parse { path, source } => {
                write!(f, "invalid config file {}: {source}", path.display())
            }
            ConfigError::InvalidDriver { origin, source } => write!(f, "{origin}: {source}"),
            ConfigError::MissingApiKey {
                driver,
                config_path,
            } => {
                let file = match config_path {
                    Some(path) => path.display().to_string(),
                    None => "your hunch config file".to_string(),
                };
                write!(
                    f,
                    "no API key for the {driver} driver: set {var}, \
                     or add `api_key = \"...\"` under [{driver}] in {file}",
                    var = api_key_var(*driver),
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    /// Exposes the wrapped error so callers (or an error reporter) can walk
    /// the chain. Our `Display` already includes the source's message.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
            ConfigError::InvalidDriver { source, .. } => Some(source),
            ConfigError::NotFound { .. } | ConfigError::MissingApiKey { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::error::Error as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An injected environment backed by a `HashMap`, so tests never touch
    /// the real (process-global, shared between test threads) environment.
    ///
    /// `+ use<>` says the returned closure borrows nothing from `pairs` (it
    /// owns its `HashMap`). Without it, edition 2024 assumes the closure may
    /// capture the input lifetimes, and `env_from(&[...])` on a temporary
    /// array would not compile.
    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    /// A unique scratch directory under the system temp dir, deleted on drop
    /// (also when the test panics), so tests clean up after themselves and
    /// can run in parallel without seeing each other's files.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("hunch-config-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        /// Writes `contents` to `relative` inside the dir, creating parents.
        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        fn path_str(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn explicit(path: &Path) -> Overrides {
        Overrides {
            config_path: Some(path.to_path_buf()),
            ..Overrides::default()
        }
    }

    #[test]
    fn defaults_with_only_an_api_key() {
        let env = env_from(&[("TYPESAFE_API_KEY", "ts-key")]);
        let settings = resolve(&Overrides::default(), &env).unwrap();
        assert_eq!(
            settings,
            Settings {
                driver: DriverKind::TypeSafe,
                model: None,
                api_key: "ts-key".into(),
                base_url: None,
                config_path: None,
            }
        );
    }

    #[test]
    fn driver_precedence_is_flag_then_env_then_file() {
        let dir = TempDir::new();
        let path = dir.write(
            "c.toml",
            "driver = \"openrouter\"\n[typesafe]\napi_key = \"t\"\n[openrouter]\napi_key = \"o\"\n",
        );

        let file_only = resolve(&explicit(&path), &env_from(&[])).unwrap();
        assert_eq!(file_only.driver, DriverKind::OpenRouter);

        let env = env_from(&[(ENV_DRIVER, "typesafe")]);
        let env_wins = resolve(&explicit(&path), &env).unwrap();
        assert_eq!(env_wins.driver, DriverKind::TypeSafe);

        let flag = Overrides {
            driver: Some(DriverKind::OpenRouter),
            ..explicit(&path)
        };
        let flag_wins = resolve(&flag, &env).unwrap();
        assert_eq!(flag_wins.driver, DriverKind::OpenRouter);
    }

    #[test]
    fn model_precedence_is_flag_then_env_then_file_then_none() {
        let dir = TempDir::new();
        let with_model = dir.write(
            "with.toml",
            "[typesafe]\napi_key = \"t\"\nmodel = \"from-file\"\n",
        );
        let without_model = dir.write("without.toml", "[typesafe]\napi_key = \"t\"\n");

        let none = resolve(&explicit(&without_model), &env_from(&[])).unwrap();
        assert_eq!(none.model, None);

        let file = resolve(&explicit(&with_model), &env_from(&[])).unwrap();
        assert_eq!(file.model.as_deref(), Some("from-file"));

        let env = env_from(&[(ENV_MODEL, "from-env")]);
        let env_wins = resolve(&explicit(&with_model), &env).unwrap();
        assert_eq!(env_wins.model.as_deref(), Some("from-env"));

        let flag = Overrides {
            model: Some("from-flag".into()),
            ..explicit(&with_model)
        };
        let flag_wins = resolve(&flag, &env).unwrap();
        assert_eq!(flag_wins.model.as_deref(), Some("from-flag"));
    }

    #[test]
    fn api_key_and_base_url_env_beat_file() {
        let dir = TempDir::new();
        let path = dir.write(
            "c.toml",
            "[typesafe]\napi_key = \"file-key\"\nbase_url = \"https://file\"\n",
        );

        let from_file = resolve(&explicit(&path), &env_from(&[])).unwrap();
        assert_eq!(from_file.api_key, "file-key");
        assert_eq!(from_file.base_url.as_deref(), Some("https://file"));
        assert_eq!(from_file.config_path.as_deref(), Some(path.as_path()));

        let env = env_from(&[
            ("TYPESAFE_API_KEY", "env-key"),
            ("TYPESAFE_BASE_URL", "https://env"),
        ]);
        let from_env = resolve(&explicit(&path), &env).unwrap();
        assert_eq!(from_env.api_key, "env-key");
        assert_eq!(from_env.base_url.as_deref(), Some("https://env"));
    }

    #[test]
    fn empty_env_values_count_as_unset() {
        let dir = TempDir::new();
        let path = dir.write(
            "c.toml",
            "driver = \"openrouter\"\n[openrouter]\napi_key = \"file-key\"\nmodel = \"m\"\n",
        );
        let env = env_from(&[
            (ENV_DRIVER, ""),
            (ENV_MODEL, ""),
            ("OPENROUTER_API_KEY", ""),
        ]);

        let settings = resolve(&explicit(&path), &env).unwrap();
        assert_eq!(settings.driver, DriverKind::OpenRouter);
        assert_eq!(settings.model.as_deref(), Some("m"));
        assert_eq!(settings.api_key, "file-key");
    }

    #[test]
    fn empty_hunch_config_falls_back_to_discovery() {
        let env = env_from(&[(ENV_CONFIG, ""), ("HOME", "/home/me")]);
        assert_eq!(
            config_file_path(&Overrides::default(), &env),
            Some(ConfigLocation::Default(PathBuf::from(
                "/home/me/.config/hunch/config.toml"
            )))
        );
    }

    #[test]
    fn xdg_config_home_beats_home() {
        let xdg = TempDir::new();
        let home = TempDir::new();
        let xdg_file = xdg.write("hunch/config.toml", "[typesafe]\napi_key = \"xdg\"\n");
        home.write(
            ".config/hunch/config.toml",
            "[typesafe]\napi_key = \"home\"\n",
        );

        let env = env_from(&[
            ("XDG_CONFIG_HOME", xdg.path_str()),
            ("HOME", home.path_str()),
        ]);
        let settings = resolve(&Overrides::default(), &env).unwrap();
        assert_eq!(settings.api_key, "xdg");
        assert_eq!(settings.config_path, Some(xdg_file));
    }

    #[test]
    fn home_dot_config_is_used_without_xdg() {
        let home = TempDir::new();
        let file = home.write(
            ".config/hunch/config.toml",
            "[typesafe]\napi_key = \"home\"\n",
        );

        // A relative XDG_CONFIG_HOME is invalid per the spec and ignored.
        let env = env_from(&[("XDG_CONFIG_HOME", "relative"), ("HOME", home.path_str())]);
        let settings = resolve(&Overrides::default(), &env).unwrap();
        assert_eq!(settings.api_key, "home");
        assert_eq!(settings.config_path, Some(file));
    }

    #[test]
    fn config_flag_beats_hunch_config_env() {
        let env = env_from(&[(ENV_CONFIG, "/from/env.toml"), ("HOME", "/home/me")]);
        assert_eq!(
            config_file_path(&Overrides::default(), &env),
            Some(ConfigLocation::Explicit("/from/env.toml".into()))
        );
        assert_eq!(
            config_file_path(&explicit(Path::new("/from/flag.toml")), &env),
            Some(ConfigLocation::Explicit("/from/flag.toml".into()))
        );
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let dir = TempDir::new();
        let missing = dir.0.join("nope.toml");

        let err = resolve(&explicit(&missing), &env_from(&[])).unwrap_err();
        assert!(matches!(&err, ConfigError::NotFound { path } if *path == missing));

        let env = env_from(&[(ENV_CONFIG, missing.to_str().unwrap())]);
        let err = resolve(&Overrides::default(), &env).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound { .. }));
        assert!(err.to_string().contains("nope.toml"));
    }

    #[test]
    fn missing_default_config_is_fine() {
        let home = TempDir::new(); // exists, but has no .config/hunch inside
        let env = env_from(&[("HOME", home.path_str()), ("TYPESAFE_API_KEY", "k")]);
        let settings = resolve(&Overrides::default(), &env).unwrap();
        assert_eq!(settings.config_path, None);
        assert_eq!(settings.api_key, "k");
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let dir = TempDir::new();
        let path = dir.write("broken.toml", "driver = \n");

        let err = resolve(&explicit(&path), &env_from(&[])).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains(&path.display().to_string()));
        assert!(err.source().is_some(), "toml error should be the source");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let dir = TempDir::new();
        let path = dir.write("typo.toml", "[typesafe]\napi_kee = \"k\"\n");

        let err = resolve(&explicit(&path), &env_from(&[])).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains("api_kee"), "{err}");
    }

    #[test]
    fn invalid_driver_names_its_origin() {
        let dir = TempDir::new();
        let path = dir.write("c.toml", "driver = \"openai\"\n");

        let err = resolve(&explicit(&path), &env_from(&[])).unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, ConfigError::InvalidDriver { .. }));
        assert!(message.contains("openai"), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");

        let env = env_from(&[(ENV_DRIVER, "gpt")]);
        let err = resolve(&Overrides::default(), &env).unwrap_err();
        assert!(err.to_string().contains("$HUNCH_DRIVER"), "{err}");
    }

    #[test]
    fn missing_api_key_explains_both_fixes() {
        let env = env_from(&[(ENV_DRIVER, "openrouter"), ("HOME", "/home/me")]);
        let err = resolve(&Overrides::default(), &env).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no API key for the openrouter driver: set OPENROUTER_API_KEY, \
             or add `api_key = \"...\"` under [openrouter] in \
             /home/me/.config/hunch/config.toml"
        );
    }

    #[test]
    fn each_driver_reads_its_own_section() {
        let dir = TempDir::new();
        let path = dir.write(
            "c.toml",
            "[typesafe]\napi_key = \"ts\"\nmodel = \"jev-1.13.0\"\n\
             [openrouter]\napi_key = \"or\"\nmodel = \"~typesafe/jev-latest\"\n",
        );

        let ts = resolve(&explicit(&path), &env_from(&[])).unwrap();
        assert_eq!(
            (ts.api_key.as_str(), ts.model.as_deref()),
            ("ts", Some("jev-1.13.0"))
        );

        let or_flag = Overrides {
            driver: Some(DriverKind::OpenRouter),
            ..explicit(&path)
        };
        let or = resolve(&or_flag, &env_from(&[("TYPESAFE_API_KEY", "ignored")])).unwrap();
        assert_eq!(
            (or.api_key.as_str(), or.model.as_deref()),
            ("or", Some("~typesafe/jev-latest"))
        );
    }
}
