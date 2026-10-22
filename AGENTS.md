# AGENTS.md

This file contains guidance for AI agents working in this repository. Keep this file high-level:
commands, architecture seams and durable conventions belong here. Module-level details belong in
code or module docs.

## Build, test, lint

```sh
cargo build                                  # both binaries and every workspace member
cargo test                                   # unit tests and the integration suites
cargo test <name>                            # only tests whose name matches <name>
cargo test -p smith-core                     # restrict to one crate
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

All build, test and lint commands go through cargo. There is no wrapper script or second build
system. `smith` is the only binary. `cargo run -q -p smith-cli -- …` runs it during development.
The default test suite is offline. It requires no API key and no external network access. The
only sockets it opens are bound to loopback. Five tests are ignored: they build the sandbox
image and start containers, so they need podman and network. Run them with
`cargo test -p smith-sandbox -- --ignored` after `cargo build --workspace`.

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

smith is a cargo workspace with two binaries: `smith`, the agent, and `smith-worker`, the
sandbox worker. The terminal REPL, the headless one-shot mode and the web server share `smith`'s
session actors.

The data flow is:

`Command → session actor → agent turn → provider stream → UiEvent → frontend`

A tool call adds a branch: `agent turn → smith_core::Tool → smith-sandbox → channel →
smith-worker → container process`. In inherit mode, the `Tool` calls the `smith-worker` library
directly inside the existing container; no supervisor and no new container are created.

A sandboxed `smith -p` child adds a provider branch: `child adapter → 127.0.0.1:3129 → worker
provider listener → ProviderRequest frame → host ProviderProxy → provider`. The child holds no
provider key. The host proxy adds the parent's credential, model and session header.

Subagents are `smith -p` processes started through `run_shell`; there is no subagent tool. The
child's final answer is the `run_shell` result's `stdout`.

The stable extension seams are `smith_core::Provider` (a wire-format adapter), the
`Command`/`UiEventKind` pair (a frontend) and `smith_core::Tool` (a tool implementation).
Frontends do not execute tools, and only the worker touches the container.

Terminology:

- A **turn** is one provider stream round-trip that produces one assistant message and optional
  tool calls. A turn that starts ends with exactly one of `TurnFinished`, `TurnPaused`,
  `TurnFailed` or `Cancelled`.
- A **user turn** is one prompt plus every turn spawned until the model stops requesting tools.
  A pause stops the user turn at the next provider-request boundary. The loop lives in
  `run_agent` (`smith-core/src/agent.rs`).
- `UiEvent.seq` is session-scoped, monotonic and never reused. Resume and `Last-Event-ID` depend
  on it.

Core boundaries:

- Keep shared primitives in small focused modules. There is no catch-all `util`. Extend the
  module whose contract a new helper fits, or give the helper a focused module of its own.
- The session actor owns the canonical conversation state as a provider-independent
  `Vec<Message>`, exposed as `SessionSnapshot.transcript`. Tool calls and results are
  `ContentBlock`s, not roles. Deltas are never persisted. The transcript gains the user and
  assistant messages exactly once, when a turn finishes or pauses. A paused turn keeps the steps
  that completed before the boundary; a failed or cancelled turn leaves the transcript unchanged.
  Frontends keep no copy of their own.
- Provider adapters implement native API protocols and wire formats. Shared transport implements
  HTTP and SSE mechanics. Adapters serialize `ChatRequest` and emit provider-independent
  `ProviderEvent`s. Agent and frontend behavior must not depend on native response shapes. An
  adapter decides whether to replay the reasoning stored on a message; the OpenAI-compatible
  adapter sends it as `reasoning_content`.
- `smith-core/src/agent.rs` is a pure state machine from borrowed stream events to an owned
  message (`TurnAccumulator`). Keep I/O and presentation out of it.
- Behavior shared by the interactive, one-shot and web frontends belongs in the session actor
  (`smith-core/src/session.rs`). Frontends send `Command`s and render `UiEvent`s. They do not
  duplicate the turn loop. Cancellation is `Command::Cancel`, not a frontend-local mechanism.
- A pause is `Command::Pause`. The loop checks it at the top of each provider iteration, so the
  in-flight provider response and the whole tool batch finish first. The terminal watcher
  (`crates/smith-cli/src/watch.rs`) owns stdin in raw mode only while a turn runs and restores it
  on drop; frontends map the first Esc to `Pause` and the second to `Cancel`.
- Tool execution has two compositions. A host process builds the sandbox tool factory and one
  container per session. A process already inside a smith sandbox (`SMITH_IN_SANDBOX=1`) resolves
  `--sandbox=auto` to inherit and uses the `smith-worker` tool functions in the current
  environment. `--sandbox=container` refuses inside the sandbox and `--sandbox=inherit` refuses
  outside it, so a child neither nests a container nor runs tools on the host. The inherit path
  starts no supervisor; the outer worker still owns the container.
- A child's provider is the `sandbox` provider from the image's `/etc/smith/config.toml`. The
  host proxy in `smith-providers/src/proxy.rs` replaces the child's model and sends the parent's
  headers; only the request body comes from the child. The worker inserts
  `SMITH_SANDBOX_TOKEN` after the configured environment, so a configured value cannot replace
  it.
- The sandbox image has an embedded base and a config-keyed cache. The base archive is
  `crates/smith-sandbox/assets/debian-slim.tar.gz`, loaded with `podman load` when the private
  store lacks `localhost/smith-base:debian-13.7`. The final tag hashes the base tag, the sorted
  `[sandbox.image] packages` and both binary digests. The default package list is empty; a
  non-empty list is installed with `--no-install-recommends`. The package list belongs to the
  system or user config; a project file that sets it is an error. The inventory is generated
  from the final image, not a fixed list.
- Declare user-facing settings in `Config` and `FileConfig`, and consume them by field. The
  direct environment reads are the documented config layers (`SMITH_MODEL`, `SMITH_PROVIDER`,
  `SMITH_THEME`), environment-only secrets (`api_key_env`), the binary locations (`SMITH_BIN`,
  `SMITH_WORKER_BIN`) and the process markers (`SMITH_IN_SANDBOX`, `SMITH_SANDBOX_TOKEN`). The
  markers are set by the sandbox and the worker, not by the user. All other settings are read
  from `Config`.
- The session actor owns the only mutable conversation state. Frontends hold `SessionHandle`
  clones. Do not add global mutable state. Spawned tasks must not outlive the session they
  serve.
- The system message is composed in `smith-core/src/prompt.rs` from the base prompt in
  `ChatRequest.system`, `system_prompt_append`, the static sections, and the fragment a tool
  returns from `prepare`. `run_agent` composes it; the session builds the static parts once.
  The order is base, append, background tasks, subagents, sandbox, project context, skills.
  Frontends do not compose prompts.
- Project context and skills are host reads performed when the prompt parts are built. A
  missing or unreadable file is skipped rather than failing the session. The project
  `AGENTS.md` and `.agents` paths are mounted read-only in the sandbox when they exist; the
  global skill roots are mounted read-only when `prompt.skills` is enabled. `read_skill`
  resolves a listed skill name, never a path.
- The sandbox and local tool handles own a `WorkingDir` per session. It starts at the
  workspace, resolves relative file paths and the `run_shell` default directory, and stores
  only a canonical directory inside the workspace. A foreground `run_shell` reports the
  shell's final directory through an EXIT trap file under `/tmp`. The worker reads at most one
  path from a regular file without following a symlink and canonicalizes it in the container;
  the host checks every component without following a symlink.
  A background run, a timeout or cancel, and an `exec`-replaced shell do not report.
  `Tool::cwd` exposes the value and `run_agent` recomposes `# Sandbox` at each provider
  request.
- The transcript is the model-visible conversation. An assistant message keeps the reasoning
  the provider streamed with it in `Message.reasoning`, because thinking-mode models require it
  on the next request. No frontend renders the field and `Message::text()` ignores it.
  `ReasoningDelta` remains a separate channel, hidden unless `--show-reasoning`, and the
  adapter decides whether to replay the stored value.
- The CLI's reply goes to stdout. Diagnostics (reasoning, usage, tool blocks and errors) go to
  stderr. Exit codes `0`, `1` and `130` are a scripting contract. `render_turn` takes a `Theme`
  and generic writers so tests can check output and codes byte for byte. Keep that signature.
- The terminal theme follows hax: `auto`, `dark`, `light`, `ansi` or `off` from `[ui] theme`,
  `SMITH_THEME`, `NO_COLOR`, `TERM`, `COLORTERM` and `COLORFGBG`. The roles are accent, chrome,
  dim, heading, code, code block, link, add, remove, ok, warn and error. `smith-cli/src/theme.rs`
  holds the palettes; the provisional values are reviewed with the web palette.
- Assistant text is rendered through a line-oriented Markdown highlighter in the terminal
  (`smith-cli/src/markdown.rs`) and a DOM writer in the web UI. The subset is headings, fenced
  and inline code, links, bold, italic, list and quote markers and rules; tables, reflow and
  nested emphasis are out. The markers are removed. The web writer never uses `innerHTML` and
  links only `http` and `https`.
- A finished tool emits its bounded `ToolDisplay` in `ToolFinished`: `Collapsed`, `Head`,
  `HeadTail` or `Diff`. `smith-core/src/tools.rs` computes it from the call arguments before
  the call and from the result after it; a failed call shows the error head. The CLI prints the
  call summary, the display rows and the outcome to stderr. The web UI renders the same value.
  Reads, background starts and unknown tools are collapsed. The preview is settled when the
  tool finishes. Live streaming needs a new UI event and is not implemented.
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

The default test run is `cargo test` and must stay offline. The sandbox's five container tests
are ignored; they need podman, the built binaries and network.

- Unit tests are in a `#[cfg(test)] mod tests` in the module they exercise. Integration tests
  are in `crates/<crate>/tests/` and cover behavior the public API exposes across module
  boundaries. Examples include wire parsing over recorded fixtures and HTTP routing.
- `MockProvider` is the scripted provider. Queue turns with `push_text`, `push_events`,
  `push_stalling_text` or `push_error`, then assert on `requests()`. Tests use it to exercise
  the session, CLI rendering and web API without a network. `MockTool` is the scripted tool; it
  records its arguments and returns a fixed result or message.
- Wire-format tests replay the recorded SSE captures in
  `crates/smith-providers/tests/fixtures/` through the same parser the live stream uses. Do not
  hand-write provider JSON. Record a new fixture when the real format changes.
- Exercise the router through `tower::ServiceExt::oneshot` rather than a socket. One web test
  opens a real socket to prove the bound address is loopback. The worker provider listener tests
  bind loopback on an ephemeral port because that listener parses HTTP itself. Do not add a
  further socket test for behavior a pure function can show.
- `Config::load` reads the process environment. Cargo runs tests in parallel threads. Any test
  that sets `SMITH_*` must take a shared lock and restore the previous value, as
  `env_overrides_model` does. Prefer passing a path or value explicitly. The suite must pass
  with no `SMITH_*` variables and no API key set.
- Prompt tests build temporary global and project directories and pass them as `PromptPaths`.
  They do not read the real config home or the workspace's `AGENTS.md`.

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

The workspace lints set `unsafe_code = "deny"`. Every crate sets `[lints] workspace = true`.
Unsafe code is an error unless the item carries an explicit `#[allow(unsafe_code)]`.

There is one approved exception, in `crates/smith-worker/src/main.rs`: adopting descriptor 3,
the inherited end of the channel socketpair. It exists because Rust has no safe conversion from
a descriptor number received across `exec` to an owner, and it is written as a borrow followed
immediately by `dup`, so nothing owns descriptor 3 and no drop can close a recycled number. The
item carries a `// SAFETY:` comment stating the contract, and binary tests start the worker
with descriptor 3 closed, with descriptor 3 not a socket, and with a socket on descriptor 3.

A new exception is a policy change, and it does not belong in an unrelated change. If a change
appears to require `unsafe`, stop and discuss it with the developer first. Document the
invariant with a `// SAFETY:` comment and add a test that exercises the block. Run what can run
under Miri or a sanitizer on a scratch nightly toolchain rather than changing
`rust-toolchain.toml`. Descriptor adoption across `exec` is outside what Miri models, so that
exception is covered by tests.

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

Tracked changes are made in a git worktree, never in the primary checkout. The primary
checkout is the integration point: it stays on `main`, and the developer merges there.

- Check the current root before editing. `git rev-parse --show-toplevel` names it, and
  `git worktree list` shows every worktree; the primary checkout is the first entry. If the
  root is the primary checkout, create a worktree instead of editing.
- Create one worktree per task, from the primary checkout:
  `git worktree add .worktrees/<task> -b <task>`. Worktrees live under `.worktrees/` in the
  repository root, which `.gitignore` excludes, so `git status` and tools that respect ignore
  files skip them. Omit `-b` when the branch already exists.
- One branch and one task per worktree. Do not switch branches inside a worktree, and do not
  check out a branch that another worktree already holds. The sandbox mounts the workspace
  root, so a session started in the primary checkout can also see the worktrees under
  `.worktrees/`; do not treat that visibility as permission to edit them.
- `plans/` is ignored by git and exists only in the primary checkout. Design documents are
  read and written there, not in a worktree. This is the one exception to editing the primary
  checkout, and it touches no tracked file.
- Do not commit, amend, rebase, cherry-pick, merge or push unless the user explicitly asks.
  When they ask, commit on the worktree's branch. Git history is the developer's decision.
- When a task is finished, leave the worktree in place for review. Remove only a worktree you
  created, and only after its branch is merged or abandoned:
  `git worktree remove .worktrees/<task>` and `git branch -d <task>`.
- `git worktree prune` clears records for worktrees whose directory was deleted by hand. It is
  not routine cleanup.
- Commit messages use sentence case with a present-tense verb in the subject line, no trailing
  period, and a short body that says why. Write a non-trivial message to a temporary file
  first to check its formatting.

## Dependencies

Dependencies are declared once in the workspace `[workspace.dependencies]` and consumed with
`.workspace = true`. Keep the dependency set small. Use std or an existing dependency before
adding a new one. Check a crate's license before adding it (the project is MIT and excludes GPL
dependencies).

The dependency direction is an architecture rule:

- `smith-core` must never gain a network, terminal or HTTP crate. Check with
  `cargo tree -p smith-core -e normal | grep -E 'reqwest|hyper|axum|clap|rustyline'`, which must
  print nothing.
- `smith-worker` must never gain a network client, an HTTP server, a terminal library or the
  sandbox supervisor. Check with
  `cargo tree -p smith-worker -e normal | grep -E 'reqwest|hyper|axum|clap|rustyline'`, which
  must print nothing. `smith-cli` links the worker library for inherit mode; the dependency is
  one-way and the worker rule is unchanged.
- `smith-sandbox` talks to podman through `tokio::process`, maps the channel descriptor with
  `command-fds` and leaves the descriptor arithmetic to `nix`. `smith-channel` depends only on
  `serde` and `serde_json`, so the worker links a small static binary.
- `smith-providers` is the only crate that performs network I/O in production. It uses
  `reqwest`. `smith-web` uses `reqwest` only as a dev-dependency for its HTTP tests.
- `smith-web` is the only crate with a browser-facing stack (`axum`).

TLS is rustls only. The development machine has no OpenSSL headers, so do not add `native-tls`
or a crate that depends on it. The toolchain channel is pinned to stable because transitive
manifests require `edition2024`. The system `cargo` (1.75) cannot build this workspace.
Downgrading the lock file only delays the same failure.
