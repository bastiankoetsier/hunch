# Learning map

hunch is a learning project: I built it to learn Rust. This page maps the Rust
concepts it uses to the code and to the pull request that introduced them, so
you can read each one in context. Paths are relative to `src/`. For how the
pieces fit together, see [Architecture](architecture.md).

| Concept | Where to look | PR |
| ------- | ------------- | -- |
| Trait objects: `Box<dyn Driver>` picked at runtime; `&dyn Driver` vs `impl Driver` | `driver/mod.rs` (`build`), `app.rs` (`run` doc comment) | [#3](https://github.com/bastiankoetsier/hunch/pull/3), [#6](https://github.com/bastiankoetsier/hunch/pull/6) |
| Generic decorator with a defaulted type parameter (`Retry<D, S = fn(Duration)>`) | `driver/retry.rs` | [#2](https://github.com/bastiankoetsier/hunch/pull/2) |
| `?Sized` blanket impl so `Box<dyn Driver>` is itself a `Driver` | `driver/mod.rs` (`impl<D: Driver + ?Sized> Driver for Box<D>`) | [#2](https://github.com/bastiankoetsier/hunch/pull/2) |
| Serde internally tagged enums (`#[serde(tag = "type")]`), `rename`, `skip_serializing_if` | `wire.rs` | [#1](https://github.com/bastiankoetsier/hunch/pull/1) |
| Serde `untagged` enum for three error-body shapes | `driver/typesafe.rs` | [#3](https://github.com/bastiankoetsier/hunch/pull/3) |
| Error enums: `Display`, `Error::source`, `From` so `?` converts | `driver/mod.rs`, `config.rs`, `app.rs` | [#1](https://github.com/bastiankoetsier/hunch/pull/1), [#4](https://github.com/bastiankoetsier/hunch/pull/4), [#6](https://github.com/bastiankoetsier/hunch/pull/6) |
| Injecting the environment as a closure; why edition 2024 makes `std::env::set_var` `unsafe` | `config.rs` (module docs, `resolve`) | [#4](https://github.com/bastiankoetsier/hunch/pull/4) |
| Precedence as an `Option::or_else` chain | `config.rs` (`resolve`) | [#4](https://github.com/bastiankoetsier/hunch/pull/4) |
| clap derive: `#[command(flatten)]`, `global = true`, `help_heading`, custom `value_parser`, `FromStr` fallback | `cli.rs` | [#6](https://github.com/bastiankoetsier/hunch/pull/6) |
| `impl Read` for testable input, `IsTerminal`, returning `ExitCode` from `main`, ignoring `BrokenPipe` | `app.rs` (`run`, `read_state`), `main.rs` | [#6](https://github.com/bastiankoetsier/hunch/pull/6) |
| Let chains (`if a && let Ok(x) = ...`, edition 2024) | `app.rs` (`parse_state`) | [#6](https://github.com/bastiankoetsier/hunch/pull/6) |
| Sorting floats with `total_cmp`; a struct borrowing with lifetimes (`Level<'a>`) | `render.rs` | [#6](https://github.com/bastiankoetsier/hunch/pull/6) |
| Toolchain components drift: rustup's minimal profile on CI lacks `rustfmt`/`clippy`, so mise pins them | `mise.toml` | [#5](https://github.com/bastiankoetsier/hunch/pull/5) |
