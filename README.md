# smith

smith is a barebones coding agent in Rust. One shared core serves a terminal REPL, a headless
CLI and a localhost web server over a pluggable LLM provider layer.

---

## Status: early

smith is a streaming client with a provider abstraction, a multi-turn REPL and a browser
frontend over the same session layer. It is not yet an agent: it has no tools and cannot read,
write or execute anything.

| | |
|---|---|
| **Works** | Streaming replies, multi-turn conversations, headless one-shot mode, provider registry, config, `smith serve` (localhost-only browser UI over the same sessions) |
| **Not yet** | Tools (`read_file`, `write_file`, `edit_file`, `run_shell`), session persistence across restarts |
| **Milestones** | M0/M1 (core, CLI) and M4 (web) done; M2 (tools) is next |

The project is at an early stage. It is not ready for use on an arbitrary repository.

---

## Requirements

- A current stable Rust toolchain, built and tested with rustc 1.99.1, and a `cargo` from that
  toolchain. `rust-toolchain.toml` declares the channel, so rustup selects it per directory. The
  system package manager provides rustc 1.75 on this machine. That toolchain is too old for the
  dependency graph and fails before any test runs. See [Toolchain](#toolchain).
- An OpenCode Go subscription and its API key.
- Network access to `opencode.ai`.

> OpenCode Go and OpenCode Console are separate products with separate billing. smith targets
> OpenCode Go (`https://opencode.ai/inference/go/openai/v1`), the subscription. The
> pay-as-you-go OpenCode Console at `opencode.ai/zen` is a different endpoint and returns
> `Insufficient account funds` or a `FreeTierError` for the same key.

---

## Setup

```sh
git clone <this repo> smith
cd smith

export OPENCODE_API_KEY=oc_...      # from the OpenCode Console, after subscribing to OpenCode Go
cargo build --release
```

Place the built `smith` binary on `PATH`, or use `cargo run -p smith-cli --` during development.

smith reads the key from the environment on every run. It never reads a key from a config file,
writes one to disk or prints it.

---

## Usage

### One-shot (headless)

```sh
smith -p "Explain what a borrow checker is in two sentences."
```

The command streams the reply to stdout and diagnostics to stderr, so it composes:

```sh
smith -p "Write a haiku about Rust" > haiku.txt
```

Exit codes are part of the scripting contract:

| code | meaning |
|---|---|
| `0` | success |
| `1` | provider, network or configuration failure (reason on stderr) |
| `130` | cancelled before completion |

### Interactive REPL

```sh
smith
```

The REPL reads a line, sends it, prints the reply and repeats. The transcript accumulates across
turns, so follow-up questions have context. `/exit`, `/quit` or Ctrl-D exits the REPL.

While a reply streams, Ctrl-C cancels the turn and returns to the prompt. It does not end the
session. The session drops the cancelled turn from the transcript, so the model does not see a
partial answer.

Two kinds of output go to stderr, so stdout contains only the reply:
- `--show-reasoning` prints reasoning tokens, dimmed, for models that emit them.
- A token-usage line after each turn, for example `smith: 19 in / 17 out tokens`.

### Web UI (localhost only)

```sh
smith serve                 # http://127.0.0.1:7878
smith serve --port 0        # let the OS pick a free port
```

Startup prints the URL to open, with this process's token in the fragment:

```
smith: listening on http://127.0.0.1:7878 (localhost only)
smith: open this URL: http://127.0.0.1:7878/#token=9f3c1b…
smith: the token is this process's only credential and is never written to disk
```

The browser lists and creates sessions, streams replies token by token and cancels a running
turn. smith stores sessions in memory. Restarting smith loses them.

The server is reachable from localhost only, and the bind address is not the only control.
`--bind` accepts loopback and refuses any other address at startup, as a hard error rather than a
warning:

```
smith: configuration error: refusing to bind 0.0.0.0: smith serve is reachable from localhost only. It can run the agent's tools and has no real authentication, so binding a routable address would expose this machine. Use 127.0.0.1 or ::1
```

Binding to loopback blocks other machines. It does not block a page in a local browser from
being tricked into sending a request to this server, so every request is checked as well:

| check | why |
|---|---|
| `Host` is `127.0.0.1:PORT`, `localhost:PORT` or `[::1]:PORT` | This check blocks DNS rebinding. `evil.com` resolves to `127.0.0.1`, and the browser then treats its request as same-origin, so CORS does not apply. Any other value receives `421`. |
| `Authorization: Bearer <token>` on every `/api` route | The token is 32 random bytes per process. It never appears in a cookie, which would allow CSRF, or in a query string, which would reach logs and `Referer`. A missing or wrong token receives `401`. |
| `Origin`, when a mutating request carries one | The origin must be the server's own origin, or the request receives `403`. This is defence in depth behind the `Host` check, not a substitute for it. |
| no CORS layer | The UI is same-origin, so a CORS layer would add exposure. |

This is not authentication, TLS or multi-user support. It is intended for one local user on a
trusted machine. It is not suitable for a shared host.

The client consumes the event stream as Server-Sent Events (SSE) with `fetch` and
`ReadableStream`, not `EventSource`, because `EventSource` cannot set an `Authorization` header.
Reconnection is manual. The client re-fetches the transcript and resumes from the last `seq` it
saw, so the UI does not render a gap as a continuous conversation.

The API is plain JSON for scripting:

```sh
TOKEN=...                                    # from the startup URL
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7878/api/health
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7878/api/sessions
```

### Two frontends on one session

```sh
smith serve --repl
```

`--repl` attaches a terminal to the same session manager the server uses. The session in the
terminal also appears in the browser's session list, so one run can be watched in both places and
cancelled from either. The browser renders every turn in the session. The terminal renders only
the turns it started.

Reaching end-of-input (Ctrl-D) detaches the terminal and leaves the server listening. `--repl`
adds a frontend. It does not own the process. Ctrl-C cancels the running turn and stops the
process when nothing is running.

### Choosing a model

```sh
smith -m glm-5.3-flash -p "hi"                      # bare id -> default provider
smith -m opencode-go/glm-5.3 -p "hi"                # explicit provider
```

Model addressing is always `provider/model`. A bare id resolves against the default provider. An
unknown provider is an error, not a silent fallback.

### Inspecting providers

```sh
smith providers list
```

The command prints each configured provider, its endpoint, the environment variable that holds
its key, and its advertised models (`*` marks the default).

### Setting a system prompt

```sh
smith --system "Answer only in JSON." -p "List three colours."
```

---

## Configuration

The precedence order, highest first, is CLI flags, then `SMITH_*` environment variables, then the
config file, then built-in defaults. Each layer overrides only the fields it states.

### Environment variables

| variable | effect |
|---|---|
| `OPENCODE_API_KEY` | This variable is required. It holds the credential. The variable name is configurable per provider (`api_key_env`). The value is read only from the environment. |
| `SMITH_MODEL` | Overrides the default model, for example `opencode-go/glm-5.3`. |
| `SMITH_PROVIDER` | Overrides the default provider. |

### Config file

```sh
smith --config ./smith.toml -p "hi"
```

There is no automatic discovery. `./smith.toml` and `~/.config/smith/config.toml` are not loaded
unless `--config` names them.

A config file is an overlay: it changes the fields it names and leaves every other field at its
default. The following file is a complete configuration:

```toml
[serve]
port = 7878

[providers.opencode-go]
default_model = "glm-5.3"
```

Naming a configured provider changes only the fields listed. The remaining fields (`base_url`,
`api_key_env`, …) keep their configured values. A new provider has nothing to inherit from, so it
must be complete, and smith names the missing field:

```
smith: configuration error: ./smith.toml: provider 'other' is new, so it has to set api_key_env
```

smith rejects unknown fields, so a typo is an error rather than a setting that does nothing:

```
unknown field `temperatur`, expected one of `default_provider`, `model`, `system_prompt`,
`max_tokens`, `temperature`, `serve`, `providers`
```

Every top-level key is optional:

| key | meaning |
|---|---|
| `default_provider` | Provider used when a model is addressed without a `provider/` prefix |
| `model` | `provider/model`, or a bare id resolved against `default_provider` |
| `system_prompt` | System message for every session |
| `max_tokens`, `temperature` | Passed through to the provider |
| `serve.bind`, `serve.port` | What `smith serve` binds when `--bind` or `--port` are not given. `bind` must be loopback |

The fields inside `[providers.<id>]` are:

- `kind` selects the adapter. The only current value is `openai_compat`, which sends
  `POST {base_url}/chat/completions` with SSE streaming. Any OpenAI-compatible endpoint can
  therefore be added without new code.
- `base_url` includes the version segment. The adapter appends `/chat/completions`.
- `session_header` names a header carrying a stable per-conversation id. OpenCode Go requires
  `x-opencode-session` for routing and prompt caching. Other servers can set their own or omit
  it.
- `user_agent` identifies the client. OpenCode Go requires clients to identify themselves
  instead of using a generic HTTP library name.
- `api_key_env` names the variable, never the secret.
- `default_model` is used when only the provider is named.
- `models` is a hand-maintained list. OpenCode Go has no `/models` endpoint (requests return
  404), so the list is configuration rather than discovery.

---

## Troubleshooting

The messages below are the text smith prints.

| message | cause and fix |
|---|---|
| `no API key: environment variable OPENCODE_API_KEY is unset` | Export the key. smith checks for the key before any request, so the failure is immediate rather than a 401 during a session. |
| `authentication rejected by opencode-go: FreeTierError: ...` | The request is reaching the Console endpoint with a free-tier model, or the key has no OpenCode Go entitlement. Check `base_url` and the subscription. |
| `... upstream failure: ... Insufficient account funds` | The key is valid, but the account lacks credit for that model. |
| `... Model is unavailable.` | The model id is wrong or not included in the plan. Run `smith providers list`, or check the model table in the OpenCode Go documentation. |
| `configuration error: unknown provider 'x'; configured providers: opencode-go` | The error indicates a typo in `-m` or `SMITH_MODEL`. It lists the configured providers. |
| `configuration error: refusing to bind 0.0.0.0: smith serve is reachable from localhost only...` | `--bind` was not a loopback address. Use `127.0.0.1` or `::1`. This is a hard error because a routable bind would expose the service. |
| `unknown field 'temperatur', expected one of ...` | The config file contains a typo. Unknown keys are errors. |
| `provider 'x' is new, so it has to set base_url` | A `[providers.x]` entry for a provider smith does not know must be complete because there is nothing to inherit from. |
| `... sent a malformed response: ...` | The stream contained data that could not be parsed. This is usually a proxy or a changed upstream format. The message includes the offending payload. |
| `smith: cancelled` | The user pressed Ctrl-C. This is not an error. |
| `401` from the web server | The bearer token is missing or wrong. Open the URL smith printed at startup. The token arrives only in that fragment. |
| `403` from the web server | A mutating request carried a different origin in its `Origin` header. |
| `421` from the web server | The `Host` header is not one smith listens on. This is either an unknown name or DNS rebinding. |
| `no token: open the URL smith printed when it started` | The user opened `http://127.0.0.1:7878/` instead of the URL printed at startup. The token arrives only in that fragment. |

Add `--show-reasoning` when a reply is unclear. Reasoning models often show where the reply went
wrong.

---

## Project layout

```
crates/
  smith-core/       domain, agent loop, session actor, provider trait, config
  smith-providers/  provider adapters + registry                 (the only crate doing HTTP)
  smith-cli/        the `smith` binary                           (headless, REPL, serve)
  smith-web/        the localhost web server                     (a frontend, no agent logic)
  smith-web/ui/     index.html, app.js, style.css                (no build step, no npm)
```

The split is enforced by dependency direction. `smith-core` must not gain a network, terminal or
HTTP dependency, including from the web server. The session actor is in core, and the HTTP layer
only renders its events. To check this, run:

```sh
cargo tree -p smith-core -e normal | grep -E 'reqwest|hyper|axum|clap|rustyline'
# must print nothing
```

The same core serves every frontend. The REPL, the headless one-shot mode and the web server are
three renderers over one session actor, not three implementations.

---

## Development

```sh
cargo test                        # 77 offline tests; no network, no API key needed
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Tests replay recorded SSE streams from the live endpoint
(`crates/smith-providers/tests/fixtures/`) rather than hand-written JSON. The fixtures cover the
empty-`choices` usage chunk and the separate `reasoning_content` channel. A scripted
`MockProvider` (`smith-core/src/mock.rs`) exercises the session layer, CLI rendering and web API.
Exactly one test opens a real socket, which proves the bound address is loopback.

### Toolchain

`rust-toolchain.toml` sets the channel to `stable` and includes `rustfmt` and `clippy`, because
the project uses `cargo fmt` and `cargo clippy` as checks. A rustup-managed `cargo` reads the file
and uses that toolchain for this directory. A per-project override elsewhere still takes
precedence, because the rustup binaries are dispatchers rather than one fixed compiler.

If `cargo test` stops before running any test with:

```
error: failed to parse lock file at: .../Cargo.lock
Caused by:
  lock file version 4 requires `-Znext-lockfile-bump`
```

Then the `cargo` on `PATH` is not rustup's. The lock file is not the problem. The dependency graph
contains manifests that require `edition2024` (`clap_derive`, `hyper-util`), and rustc 1.75
cannot parse them. Downgrading the lock file only moves the failure one step later. Point `PATH`
at rustup's shims:

```sh
export PATH="$HOME/.cargo/bin:$PATH"     # what rustup adds to ~/.bashrc and ~/.profile
```

Shell startup files cover interactive shells only. Other contexts, such as a non-interactive
`bash -c`, an editor launched from a menu or CI, read neither startup file. Link the shims into a
directory ahead of `/usr/bin`:

```sh
ln -sfn ~/.cargo/bin/{cargo,rustc,rustdoc,rustfmt,cargo-fmt,cargo-clippy,clippy-driver} ~/.local/bin/
```

### Dependencies

The toolchain is current stable, so `cargo update` is safe and `Cargo.lock` is an ordinary lock
file. No dependency is held back by hand.

The development machine has no OpenSSL development headers, so `native-tls` is not an option.
`reqwest` uses `rustls-tls`, which is the only TLS path in use.
