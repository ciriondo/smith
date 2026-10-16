# AGENTS.md

This file contains guidance for AI agents working in this repository. Keep this file high-level:
commands, architecture seams and durable conventions belong here. Module-level details belong in
code or module docs.

## Build, test, lint

```sh
cargo build                                  # debug build of every workspace member
cargo test                                   # unit tests and the integration suites
cargo test <name>                            # only tests whose name matches <name>
cargo test -p smith-core                     # restrict to one crate
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

All build, test and lint commands go through cargo. There is no wrapper script or second build
system. `smith` is the only binary. `cargo run -q -p smith-cli -- …` runs it during development.
The test suite is offline. It requires no API key and no external network access. The only
socket any test opens is bound to loopback.

Use rustup's cargo. The system `cargo` (version 1.75 on this machine) cannot parse the dependency
graph and fails with `lock file version 4 requires -Znext-lockfile-bump`. That failure is a
toolchain symptom, not a lock file problem.

The formatting and lint checks are `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings`. Clippy warnings are errors. Run both before
reporting done. Format the files you touched with `cargo fmt`.

Cargo builds and tests use two profiles:

| profile | command | for |
| --- | --- | --- |
| debug (default) | `cargo build`, `cargo test` | local builds and tests, overflow checks on |
| release | `cargo build --release` | behavior that depends on optimization |

The default test run uses neither Miri nor a sanitizer. Both require a nightly toolchain.

## Manual checks and debugging

There is no environment switch that runs the mock provider in the binary. `MockProvider`
(`smith-core/src/mock.rs`) is a test fixture. Use it from `cargo test` when you need a provider
without network access or an API key. Manual end-to-end checks use a real provider and require
`OPENCODE_API_KEY`:

```sh
cargo run -q -p smith-cli -- -p "say hi"
```

`SMITH_MODEL` and `SMITH_PROVIDER` override the default model and provider without a config
file. `--config` loads a config file. The one-shot exit codes are part of the contract: `0` for
success, `1` for failure and `130` for cancellation. When you change an error path, check the
exit code as well as the text.

The REPL prints its prompt only when stdin is a terminal. It still reads piped lines, so
scripted checks can pipe stdin. Ctrl-C handling and `smith serve --repl` require a real tty. Use
tmux instead of hand-rolled pty scripts. Send keys, capture the pane and read the result:

```sh
export OPENCODE_API_KEY=oc_...     # tmux sessions inherit the environment they start with
tmux new-session -d -s smithtest -x 110 -y 32 'cargo run -q -p smith-cli'
tmux send-keys -t smithtest 'hello' Enter   # Enter/C-u/Escape as named keys
tmux capture-pane -t smithtest -p           # pane text, escapes already resolved
tmux kill-session -t smithtest
```

Clean up only the processes and sessions you started. The user may be working inside tmux.
Other smith processes, including a `smith serve`, may belong to them. Do not run
`tmux kill-server`, `pkill smith`, `killall smith` or `pkill -f smith`. Kill the session you
named, or the PID you captured at launch.

## Architecture

smith is a cargo workspace with one binary, `smith`. The terminal REPL, the headless one-shot
mode and the web server share its session actors.

The data flow is:

`Command → session actor → agent turn → provider stream → UiEvent → frontend`

The stable extension seams are `smith_core::Provider` (a wire-format adapter) and the
`Command`/`UiEventKind` pair (a frontend). Tool execution will be added behind the session layer.
Frontends do not execute tools.

Terminology:

- A **turn** is one provider stream round-trip that produces one assistant message and, once
  tools exist, optional tool calls. A turn that starts ends with exactly one of `TurnFinished`,
  `TurnFailed` or `Cancelled`.
- A **user turn** will be one prompt plus every turn spawned until the model stops requesting
  tools. Until tools are implemented, one prompt is exactly one turn.
- `UiEvent.seq` is session-scoped, monotonic and never reused. Resume and `Last-Event-ID` depend
  on it.

Core boundaries:

- Keep shared primitives in small focused modules. There is no catch-all `util`. Extend the
  module whose contract a new helper fits, or give the helper a focused module of its own.
- The session actor owns the canonical conversation state as a provider-independent
  `Vec<Message>`, exposed as `SessionSnapshot.transcript`. Tool calls and results are
  `ContentBlock`s, not roles. Deltas are never persisted. The transcript gains the user and
  assistant messages exactly once, when a turn finishes. A failed or cancelled turn leaves the
  transcript unchanged. Frontends keep no copy of their own.
- Provider adapters implement native API protocols and wire formats. Shared transport implements
  HTTP and SSE mechanics. Adapters serialize `ChatRequest` and emit provider-independent
  `ProviderEvent`s. Agent and frontend behavior must not depend on native response shapes.
- `smith-core/src/agent.rs` is a pure state machine from borrowed stream events to an owned
  message (`TurnAccumulator`). Keep I/O and presentation out of it.
- Behavior shared by the interactive, one-shot and web frontends belongs in the session actor
  (`smith-core/src/session.rs`). Frontends send `Command`s and render `UiEvent`s. They do not
  duplicate the turn loop. Cancellation is `Command::Cancel`, not a frontend-local mechanism.
- Declare user-facing settings in `Config` and `FileConfig`, and consume them by field. The only
  direct environment reads are the documented config layers (`SMITH_MODEL`, `SMITH_PROVIDER`)
  and environment-only secrets (`api_key_env`). All other settings are read from `Config`.
- The session actor owns the only mutable conversation state. Frontends hold `SessionHandle`
  clones. Do not add global mutable state. Spawned tasks must not outlive the session they
  serve.
- The transcript is the model-visible conversation. Reasoning is not part of it.
  `ReasoningDelta` is a separate channel, hidden unless `--show-reasoning`, and never added to
  the transcript.
- The CLI's reply goes to stdout. Diagnostics (reasoning, usage and errors) go to stderr. Exit
  codes `0`, `1` and `130` are a scripting contract. `render_turn` takes generic writers so
  tests can check output and codes byte for byte. Keep that signature.
- Frontends subscribe before they take a snapshot and render only events with `seq` above the
  snapshot. With two frontends on one session, a reversed order drops turns or renders another
  frontend's reply as its own.

Extension workflows:

- A provider with an OpenAI-compatible `/chat/completions` endpoint needs no code. Add a
  `[providers.<id>]` block with `kind = "openai_compat"`. A new wire protocol requires a
  `ProviderKind` variant in `smith-core/src/config.rs`, a module in `smith-providers/src/` and
  one arm in `smith-providers::build`. Shipped defaults are in `Config::default`. Prefer
  configuration data over conditional code. Add an adapter hook only for behavior specific to
  one provider.
- A new frontend implements the `Command`/`UiEvent` contract over `SessionManager`. It does not
  own a turn loop and does not call a provider directly.
- Slash commands are part of the frontend. `/exit` and `/quit` are matched in `repl`. A command
  with logic beyond a match belongs in a module in the frontend crate, not in `smith-core`.
- Keep protocol translation and terminal-independent state machines pure and separately
  testable. Do not require HTTP or a TTY to test parsing and state transitions.

## Tests

The test suite runs with `cargo test`. It must stay offline.

- Unit tests are in a `#[cfg(test)] mod tests` in the module they exercise. Integration tests
  are in `crates/<crate>/tests/` and cover behavior the public API exposes across module
  boundaries. Examples include wire parsing over recorded fixtures and HTTP routing.
- `MockProvider` is the scripted provider. Queue turns with `push_text`, `push_events`,
  `push_stalling_text` or `push_error`, then assert on `requests()`. Tests use it to exercise
  the session, CLI rendering and web API without a network.
- Wire-format tests replay the recorded SSE captures in
  `crates/smith-providers/tests/fixtures/` through the same parser the live stream uses. Do not
  hand-write provider JSON. Record a new fixture when the real format changes.
- Exercise the router through `tower::ServiceExt::oneshot` rather than a socket. Exactly one test
  opens a real socket to prove the bound address is loopback. A second socket test would add
  port allocation and timing without testing different code.
- `Config::load` reads the process environment. Cargo runs tests in parallel threads. Any test
  that sets `SMITH_*` must take a shared lock and restore the previous value, as
  `env_overrides_model` does. Prefer passing a path or value explicitly. The suite must pass
  with no `SMITH_*` variables and no API key set.

Where a test goes:

- A test file mirrors the production module it exercises. Behavior is tested in the module that
  owns it. When a change extends a shared module and adds a consumer of the extension, test the
  extension in the shared module with the smallest input that exercises it. Test only the
  consumer's own code in the consumer's tests. Shipped defaults and their ordering are tested
  where the table is defined.
- Use the lowest level that can observe the behavior: a pure function over an assembled object,
  a session against `MockProvider` or an adapter against a fixture, rather than the built
  binary. Spawn the `smith` binary only for behavior the library cannot show: argv parsing,
  process exit codes, stdout and stderr shape, signals and terminal interaction.
- Do not assert the same behavior at two levels. Once a unit test covers it, an integration test
  that repeats the check adds runtime without covering new behavior.
- Before writing a fixture (scratch config, recorded stream, fake endpoint), look for one in
  sibling tests or the existing helpers (`replay` in `wire_format.rs`, the request builders in
  `http.rs`). Reuse or extract it instead of copying it.

## Code style and conventions

- The workspace uses Rust 2021 and current stable, pinned by `rust-toolchain.toml`. Code is
  formatted by rustfmt's defaults. It is lint-free under clippy `-D warnings`. There are no
  `rustfmt.toml` or `clippy.toml` overrides. Do not add one without migrating the whole tree.
- Library code returns typed errors (`SmithError`, `ProviderError`). User-facing text is in the
  `#[error(...)]` attributes. Do not introduce `anyhow` or `Box<dyn Error>`. Do not panic on
  config, provider or I/O input.
- Prefer ownership and the type system over runtime checks. Do not add global mutable state.
  Shared mutable state belongs to the session layer.
- Module-level `//!` docs state the contract and the reason for it. Comments explain non-obvious
  decisions. Match the surrounding module rather than a separate style guide.
- The license is MIT, declared once in `[workspace.package]`. Do not add per-file license
  headers.

## Unsafe code

The workspace lints set `unsafe_code = "forbid"`. Every crate sets `[lints] workspace = true`.
Do not add `unsafe`. Do not remove the workspace lint or override its level in a crate.

If a change appears to require `unsafe`, stop and discuss it with the developer before making
the change. An approved exception is an explicit policy change. It does not belong in an
unrelated change. If the developer approves one, document the invariant with a `// SAFETY:`
comment and add a test that exercises the block. Run that test under Miri or a sanitizer, using
a scratch nightly toolchain rather than changing `rust-toolchain.toml`.

## Technical writing

Prose in this repository is dry. It records behavior, constraints and reasons for a reader who
is checking a claim against the code. Avoid sentences which have an effect that comes from rhythm,
contrast or a closing fragment rather than from the information.

- State behavior in the present tense and name the actor: "the session actor rejects a second
  `Send`", not "a second send is not allowed". Future tense is for work not yet done.
- Write one claim per sentence. If a sentence exists for rhythm, emphasis or a transition rather
  than information, delete it.
- Do not use sentence fragments, one-line paragraphs, rhetorical questions, exclamations or
  italic emphasis to make a point. Do not shorten a sentence for emphasis. Length follows the
  content.
- Name the type, function, flag, file or command. Write statements that can be checked against
  the code. Avoid general claims such as "turns are handled correctly".
- Do not evaluate. Words such as "simple", "powerful", "seamless", "robust", "elegant" and
  "fast" state an impression rather than a property. State the property, and give the
  measurement or the test that pins it when it matters.
- Avoid persuasive framing: "not just", "the real story", "the honest claim", "this is the
  point", "what makes X work". State the constraint and its consequence.
- Avoid humor, irony, metaphor, personification and scare quotes. Define a term on first use
  and use the same word afterward.
- Use the names that appear in the code, in code font. Do not invent a synonym for an existing
  type, command, flag or setting.
- Error messages state what failed, why and what the reader can do. They contain no blame,
  greeting or exclamation. They stay short enough to quote in a test.
- Documentation describes current behavior. Keep historical and changelog phrasing out of it.
- Markdown is hard-wrapped around 100 columns, as code is. Headings name their subject.

These rules cover the README, module and item docs, comments, error messages and this file.

Two examples show the target:

- "The session actor is the heart of the system — everything flows through it." becomes
  "Frontends send `Command`s to the session actor and render the `UiEvent`s it emits."
- "This is safe enough for your laptop — and nothing more." becomes "The server is intended for
  one local user on a trusted machine. It is not authentication, TLS or multi-user support."

The rules apply to new and edited prose. Do not rewrite unrelated text to match them.

## Documentation

There is no `CHANGELOG.md`. The README is the user-facing contract: it states status, usage, the
config schema, exact error messages and exit codes. When a change alters what the README states,
update the README in the same change. Keep quoted error text identical to what the code prints.

## Git conventions

Do not create commits or perform any other git history manipulation unless the user explicitly
prompts for it. This includes `git commit`, `git commit --amend`, `git rebase`, `git reset`,
`git cherry-pick` and `git merge`. Do not switch or create branches. Do not push unless
explicitly prompted.

The repository is kept at a single commit (`initial commit`). When the user asks for a change to
be recorded, amend that commit instead of adding history.

Commit messages follow these patterns:
- For the subject line, use sentence case with a present-tense verb (for example, "Add" or
  "Fix"). The subject line does not end with a period.
- Prefer a brief explanatory body after the subject line. Describe why the change was made.
  Summarize what changed. Keep the body concise and free-form.
- For non-trivial commits, write the message to a temporary file first to check formatting
  before committing. Aim for approximately 80-90 columns in commit message prose.

## Dependencies

Dependencies are declared once in the workspace `[workspace.dependencies]` and consumed with
`.workspace = true`. Keep the dependency set small. Use std or an existing dependency before
adding a new one. Check a crate's license before adding it (the project is MIT and excludes GPL
dependencies).

The dependency direction is an architecture rule:

- `smith-core` must never gain a network, terminal or HTTP crate. Check with
  `cargo tree -p smith-core -e normal | grep -E 'reqwest|hyper|axum|clap|rustyline'`, which must
  print nothing.
- `smith-providers` is the only crate that performs network I/O in production. It uses
  `reqwest`. `smith-web` uses `reqwest` only as a dev-dependency for its HTTP tests.
- `smith-web` is the only crate with a browser-facing stack (`axum`).

TLS is rustls only. The development machine has no OpenSSL headers, so do not add `native-tls`
or a crate that depends on it. The toolchain channel is pinned to stable because transitive
manifests require `edition2024`. The system `cargo` (1.75) cannot build this workspace.
Downgrading the lock file only delays the same failure.
