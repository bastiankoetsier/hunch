# hunch

> [!WARNING]
> **This is a personal learning project.** I built it to learn Rust, and I
> don't promise maintenance, stability or support. Use it at your own risk:
> it sends your input to third-party APIs that bill your API key. hunch is an
> unofficial client and is not affiliated with or endorsed by TypeSafe or
> OpenRouter. If you want the learning notes, see the
> [learning map](docs/learning.md).

A small CLI for [Jev](https://docs.typesafe.ai/introduction), TypeSafe's
"System One" model. You give Jev a *state* (the content to judge) and one typed
question; it answers with calibrated probabilities, not prose. hunch exposes the
three question types as three subcommands: `noul` (yes/no), `choice` (pick one)
and `score` (place on a scale).

```console
$ hunch noul "Does the customer convey urgency?" \
    --state "I was charged twice for my March invoice and nobody answers my emails. Fix this today."
0.93  ███████████████████░  likely yes

$ hunch choice "Which team should handle this?" -o billing -o technical -o sales --state "..."
billing  (confidence 0.81)

  billing    ██████████████████░░   88.0%
  technical  ██░░░░░░░░░░░░░░░░░░   12.0%
  sales      ░░░░░░░░░░░░░░░░░░░░    0.0%
```

The name: "System One" is Kahneman's fast, intuitive mode of thinking, so a
System One answer is a hunch.

- [Install](#install)
- [Configure](#configure)
- [Usage](#usage)
- [Development](#development)
- [License](#license)

## Install

### Prebuilt binaries

Each [GitHub release](https://github.com/bastiankoetsier/hunch/releases)
ships binaries for macOS (Apple Silicon and Intel), Linux (x86_64 and arm64)
and Windows (x86_64), plus an install script that picks the right one and puts
it in `~/.cargo/bin`, without `sudo`:

```sh
# macOS and Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bastiankoetsier/hunch/releases/latest/download/hunch-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/bastiankoetsier/hunch/releases/latest/download/hunch-installer.ps1 | iex"
```

Want to read the script first? Download it from the release page, or skip it
and grab the archive for your platform there; each comes with a `.sha256`
checksum.

### From source

With a Rust toolchain already installed, install straight from GitHub:

```sh
cargo install --git https://github.com/bastiankoetsier/hunch
```

From a checkout, the Rust toolchain (version plus the `rustfmt`/`clippy`
components) is pinned in `mise.toml`:

```sh
mise install              # installs the pinned Rust toolchain
cargo install --path .    # builds hunch and puts it on your PATH (~/.cargo/bin)
```

Or run it from the checkout without installing: `mise run run -- noul ...`
(which is `cargo run -- noul ...`).

## Configure

### API keys

Keys are read from the environment or the config file, never from a flag (a
flag would end up in shell history and `ps` output).

| Driver       | Key variable         | Default endpoint            | Default model          |
| ------------ | -------------------- | --------------------------- | ---------------------- |
| `typesafe`   | `TYPESAFE_API_KEY`   | `https://api.typesafe.ai`   | `jev-latest`           |
| `openrouter` | `OPENROUTER_API_KEY` | `https://openrouter.ai/api` | `~typesafe/jev-latest` |

```sh
export TYPESAFE_API_KEY=...
hunch noul "Is this spam?" --state "You won a prize!"
```

### Drivers and precedence

A *driver* is the route a request takes to Jev: `typesafe` talks to TypeSafe
directly, `openrouter` goes through OpenRouter. Both speak the same
`POST /v1/systemone` protocol. Every setting is resolved left to right, first
hit wins; empty values count as unset.

| Setting     | 1. flag    | 2. environment                              | 3. config file                     | 4. default                   |
| ----------- | ---------- | ------------------------------------------- | ---------------------------------- | ---------------------------- |
| driver      | `--driver` | `HUNCH_DRIVER`                              | top-level `driver`                 | `typesafe`                   |
| model       | `--model`  | `HUNCH_MODEL`                               | `model` in the driver's section    | the driver's default (above) |
| API key     | (none)     | `TYPESAFE_API_KEY` / `OPENROUTER_API_KEY`   | `api_key` in the driver's section  | none: error, exit 2          |
| base URL    | (none)     | `TYPESAFE_BASE_URL` / `OPENROUTER_BASE_URL` | `base_url` in the driver's section | the driver's default (above) |
| config file | `--config` | `HUNCH_CONFIG`                              |                                    | `$XDG_CONFIG_HOME/hunch/config.toml`, else `~/.config/hunch/config.toml` |

Driver names are case-insensitive. A missing config file at the default
location is fine; a missing file you named with `--config` or `HUNCH_CONFIG` is
an error.

### Config file

`~/.config/hunch/config.toml` (on macOS too). Unknown keys are rejected, so a
typo like `api-key` fails loudly instead of being ignored.

```toml
driver = "openrouter"        # optional, default "typesafe"

[typesafe]                   # every key in a section is optional
api_key = "..."
# base_url = "https://api.typesafe.ai"
# model = "jev-latest"

[openrouter]
api_key = "..."
# model = "~typesafe/jev-latest"
```

### Debugging: `hunch config`

Shows what was resolved and where the key comes from, without printing the key.
It does not call the API and succeeds even when the key is missing:

```console
$ hunch config
driver:      typesafe
model:       jev-latest (default)
base URL:    https://api.typesafe.ai (default)
config file: /Users/you/.config/hunch/config.toml (not found)
API key:     missing: export TYPESAFE_API_KEY=..., or add `api_key = "..."` under [typesafe] in the config file

$ hunch --driver openrouter config
driver:      openrouter
model:       ~typesafe/jev-latest (default)
base URL:    https://openrouter.ai/api (default)
...
```

## Usage

```text
Usage: hunch [OPTIONS] <COMMAND>

Commands:
  noul    Probability that a statement about the state is true
  choice  Pick the best of several options
  score   Place the state on an ordered scale of levels
  config  Show the resolved driver, model, endpoint and API key source

Global options:
      --driver <DRIVER>  Provider to send the request through [possible values: typesafe, openrouter]
      --model <ID>       Model id (default: the driver's own default, e.g. jev-latest)
      --config <PATH>    Config file to read instead of ~/.config/hunch/config.toml
      --json             Print the raw JSON response instead of a human-readable summary
  -v, --verbose          Print the model and token usage to stderr
```

Global options work before or after the subcommand (`hunch --json noul ...` and
`hunch noul ... --json` are the same).

**Ask atomic questions.** Each hunch call is one snap judgment about one
property. Instead of "Is this a good support reply?", ask several narrow
questions ("Does it answer the question asked?", "Is the tone polite?") and
combine the numbers yourself. See Jev's
[How to build with System One](https://docs.typesafe.ai/concepts/how-to-build-with-system-one)
guide and the [API reference](https://docs.typesafe.ai/api.md).

The examples below use this state:

```sh
T="I was charged twice for my March invoice and nobody answers my emails. Fix this today."
```

### `noul`: yes/no probability

```text
hunch noul [OPTIONS] <QUESTION>
      --yes <DESC>  What "yes" means (optional)
      --no <DESC>   What "no" means (optional)
```

```console
$ hunch noul "Does the customer convey urgency?" --state "$T"
0.93  ███████████████████░  likely yes
```

The number is P(yes). The hint reads `likely yes` at >= 0.80, `likely no` at
<= 0.20, and `uncertain` in between.

### `choice`: pick one option

```text
hunch choice [OPTIONS] --option <NAME[=DESC]> <QUESTION>
  -o, --option <NAME[=DESC]>  An option, optionally with a description (repeat, at least 2)
```

Between 2 and 255 options, with unique names. The description is split off at
the first `=`.

```console
$ hunch choice "Which team should handle this?" \
    -o billing="Invoices, refunds, payments" -o technical -o sales --state "$T"
billing  (confidence 0.81)

  billing    ██████████████████░░   88.0%
  technical  ██░░░░░░░░░░░░░░░░░░   12.0%
  sales      ░░░░░░░░░░░░░░░░░░░░    0.0%
```

Options are listed most likely first.

### `score`: place on an ordered scale

```text
hunch score [OPTIONS] --level <LEVEL> <QUESTION>
  -l, --level <LEVEL>  A level description, lowest first (repeat, 2 to 10 levels)
```

```console
$ hunch score "How frustrated is the customer?" \
    -l Calm -l Frustrated -l "Very angry" --state "$T"
1.05 on a 0–2 scale → Frustrated  (confidence 0.92)

  0  Calm        ░░░░░░░░░░░░░░░░░░░░    0.0%
  1  Frustrated  ███████████████████░   95.0%
  2  Very angry  █░░░░░░░░░░░░░░░░░░░    5.0%
```

Levels are numbered from 0. The score is a point on that scale; the arrow
names the nearest level.

### State input

The state comes from exactly one of:

| Source              | Example                                    |
| ------------------- | ------------------------------------------ |
| `--state TEXT`      | `hunch noul "..." --state "$T"`            |
| `--state-file PATH` | `hunch noul "..." --state-file ticket.txt` |
| `--state-file -`    | reads stdin explicitly                     |
| stdin (no flag)     | `cat ticket.txt \| hunch noul "..."`       |

`--state` and `--state-file` conflict. With neither, hunch reads stdin, unless
stdin is a terminal: then it exits with an error instead of appearing to hang.
Empty or whitespace-only state is an error.

Text that parses as a JSON object or array is sent as structured JSON, so a
question can point at a field with backticks (`` `ticket.subject` ``). Anything
else is sent as a trimmed string.

### `--json` and `--verbose`

`--json` prints the provider's response body untouched. The answer always sits
under `answers.answer`:

```console
$ hunch --json choice "Which team?" -o billing -o technical -o sales --state "$T"
{"model": "jev-1.13.0", "answers": {"answer": {"type": "choice", "choice": "billing", "probabilities": {"billing": 0.88, "technical": 0.12, "sales": 0.0}, "confidence": 0.81}}, "usage": {"input_tokens": 312, "output_tokens": 24}}
```

`-v/--verbose` writes one extra line to stderr, so it never ends up in a pipe:

```console
$ hunch -v noul "Does the customer convey urgency?" --state "$T"
model jev-1.13.0, 312 input + 24 output tokens
0.93  ███████████████████░  likely yes
```

### Chaining

`--json` output is a JSON object, so piping it into another hunch sends it as
structured state, and the next question can reference the previous answer:

```sh
hunch --json choice "Which team should handle this?" -o billing -o technical -o sales --state "$T" \
  | hunch noul 'Is `answers.answer.choice` billing?'
```

Use single quotes so the shell does not treat the backticks as command
substitution.

For scripts, pull fields out with `jq`:

```sh
team=$(hunch --json choice "Which team?" -o billing -o technical -o sales --state "$T" \
  | jq -r '.answers.answer.choice')

urgency=$(hunch --json noul "Does the customer convey urgency?" --state "$T" \
  | jq '.answers.answer.noul')
```

### Exit codes

| Code | Meaning |
| ---- | ------- |
| 0    | Success, including `--help`, `--version`, and a reader closing the pipe early (`\| head -c1`) |
| 1    | Runtime failure: the provider call failed (after retries), the response had no answer, or output could not be written |
| 2    | Something to fix in the invocation: bad arguments (clap), wrong option/level count, duplicate option, bad config or missing API key, missing/empty/unreadable state |

Rate limits (429) and overload (503/529) are retried automatically: 3 attempts
in total, waiting 0.5s then 1s, or whatever the server's `Retry-After` says
(capped at 8s). Each request times out after 60s.

## Development

```sh
mise run test               # all non-ignored tests (unit + integration)
mise run test-unit          # unit tests only
mise run test-integration   # tests/ only
mise run test-live          # ignored live-API smoke tests; needs real API keys
mise run lint               # cargo fmt --check + clippy -D warnings
mise run fmt                # cargo fmt
mise run run -- <args>      # cargo run -- <args>
mise run clean              # cargo clean: free the disk space used by target/
```

The test pyramid:

| Layer | Where | What it covers |
| ----- | ----- | -------------- |
| Unit | `#[cfg(test)] mod tests` at the bottom of each `src/*.rs` file | Pure logic, no I/O: config resolution with a fake env, request building, rendering, retry timing with a recording sleeper, error mapping. Runs in parallel, no network. |
| Integration | `tests/cli.rs` | The real `hunch` binary end to end, against a small fake HTTP server built on the standard library. |
| Live | `tests/live.rs`, `#[ignore]`d | Smoke tests against the real APIs. Skipped by `cargo test`; run with `mise run test-live`. |

CI (`.github/workflows/ci.yml`) runs on pushes to `main` and on every pull
request: it installs the toolchain through `jdx/mise-action` (the same versions
as locally), caches with `Swatinem/rust-cache`, then runs `mise run lint` and
`mise run test`.

### Releasing

Releases are built by [dist](https://github.com/axodotdev/cargo-dist), pinned
in `mise.toml` like the Rust toolchain. Its config lives in
`dist-workspace.toml` and `[profile.dist]` in `Cargo.toml`; the workflow
`.github/workflows/release.yml` is generated from that config, so don't edit
it by hand: change the config and run `dist init` (or `dist generate`) again.

To cut a release:

1. Bump `version` in `Cargo.toml` (hunch reports it in `hunch --version`) and
   merge that to `main`.
2. Tag the merge commit with the same version and push the tag:
   `git tag v0.2.0 && git push origin v0.2.0`.
3. The release workflow builds every target on its own native runner and
   publishes a GitHub release with the archives, checksums and install scripts.

On pull requests the same workflow only runs `dist plan`, a quick check that
the config and the generated workflow still agree. To try a build locally:

```sh
dist plan                                                 # what a release would contain
dist build --artifacts=local --target aarch64-apple-darwin  # build one target into target/distrib/
```

## License

[MIT](LICENSE).
