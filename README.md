# smith

smith is a barebones coding agent in Rust. One shared core serves a terminal REPL, a headless
CLI and a localhost web server over a pluggable LLM provider layer.

---

## Status: early

smith is a streaming client with a provider abstraction, a multi-turn REPL and a browser
frontend over the same session layer. Its tools run inside a podman container that has no
network interface, a read-only root filesystem and explicit mounts.

| | |
|---|---|
| **Works** | Streaming replies, multi-turn conversations, headless one-shot mode, provider registry, config, `smith serve` (localhost-only browser UI over the same sessions), `read_file`, `write_file`, `edit_file`, `run_shell` and background `tool_wait` in a sandbox, and sandboxed `smith -p` subagents whose provider calls are proxied by the host |
| **Not yet** | Session persistence across restarts; resource limits; the vendored podman fallback |
| **Milestones** | M0/M1 (core, CLI), M2 (tools and sandbox) and M4 (web) done |

The project is at an early stage. It is not ready for use on an arbitrary repository.

---

## Requirements

- A current stable Rust toolchain, built and tested with rustc 1.99.1, and a `cargo` from that
  toolchain. `rust-toolchain.toml` declares the channel, so rustup selects it per directory. The
  system package manager provides rustc 1.75 on this machine. That toolchain is too old for the
  dependency graph and fails before any test runs. See [Toolchain](#toolchain).
- An OpenCode Go subscription and its API key.
- Network access to `opencode.ai`.
- Rootless podman 4.9 or newer for the sandbox. The base image ships inside `smith`. A
  non-empty `[sandbox.image] packages` list needs network access on the first build of that
  package set.
- The `smith-worker` binary next to `smith`, or `SMITH_WORKER_BIN` naming it. `cargo build`
  produces both. The container runs tools, so smith refuses to start without a worker.

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

`--sandbox` selects where tool calls run. It applies to the one-shot, interactive and `serve`
modes:

| mode | behavior |
|---|---|
| `auto` (default) | Creates a container on the host. Inside a smith sandbox, inherits the existing sandbox. |
| `container` | Always creates a container. Refused inside a smith sandbox. |
| `inherit` | Runs tools in the current environment. Refused outside a smith sandbox. |

Inside a smith sandbox, the environment contains `SMITH_IN_SANDBOX=1`. `auto` therefore makes a
one-shot child use the parent container instead of starting a nested one. `inherit` does not run
tools on the host: the guard refuses it unless that marker is set.

A one-shot run inside an existing sandbox can serve as a subagent process. The image contains
the `smith` binary, so the child runs the same code as its parent. The provider credential stays
on the host: the child's provider calls go to the worker's loopback listener, travel over the
channel, and are sent by the host with the parent's key. The child receives a per-container
token, not the key. Invoke one through `run_shell`: `smith -p "<task>"`. The child's final answer
is the command's stdout, and its tool lines and diagnostics are on stderr.

### Interactive REPL

```sh
smith
```

The REPL reads a line, sends it, prints the reply and repeats. The transcript accumulates across
turns, so follow-up questions have context. `/exit`, `/quit` or Ctrl-D exits the REPL.

While a reply streams, Esc pauses after the current step. The in-flight provider response and
the current tool batch finish, the completed steps are recorded, and the prompt returns. The next
message steers the model from that point. Esc again terminates the turn immediately. Ctrl-C also
terminates. Termination drops the turn from the transcript, so the model does not see a partial
answer; a paused turn keeps the steps that completed. The startup line names both shortcuts.

The reply is rendered as a Markdown subset in the terminal and the web UI: headings, fenced
and inline code, links, bold, italic, list bullets, quotes and rules. The markers are removed.
The terminal applies a theme; the web UI applies its own palette.

Three kinds of output go to stderr, so stdout contains only the reply:
- `--show-reasoning` prints reasoning tokens, dimmed, for models that emit them.
- A token-usage line after each turn, for example `smith: 19 in / 17 out tokens`.
- A tool block after each call. The call summary is one line, the preview is bounded and the
  outcome closes the block:

```
smith: run_shell cargo test
  │ test result: ok. 42 passed
  │ ... (18 more lines) ...
  └ exit 0 (412 ms)
```

Shell output shows a head and a tail with an omitted-line row. An `edit_file` call shows a
diff with `-` and `+` rows. A `write_file` call shows the head of the content. A `read_file`
call and a background start are collapsed to the call and outcome. The preview limits are 8
head lines, 4 head plus 4 tail lines, and 400 characters per row. The block is settled when the
tool finishes; there is no live update while it runs.

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

The browser lists and creates sessions and streams replies token by token. While a turn runs,
the `Pause` button and Esc pause after the current step; the button becomes `Terminate` and a
second Esc terminates immediately. A paused turn keeps its completed steps and accepts a steering
message. smith stores sessions in memory. Restarting smith loses them.

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

The browser renders the same tool block as the terminal: the call summary, the bounded preview
and the outcome. Diff rows use the add and remove colours; an omitted-lines row appears when
the preview is capped. Reads and background starts are collapsed.

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
paused or terminated from either. The browser renders every turn in the session. The terminal
renders only the turns it started.

Reaching end-of-input (Ctrl-D) detaches the terminal and leaves the server listening. `--repl`
adds a frontend. It does not own the process. Ctrl-C terminates the running turn and stops the
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

smith has a built-in system prompt. `--system` replaces the base prompt for the run; the
procedural sections described below are still appended.

```sh
smith --system "Answer only in JSON." -p "List three colours."
```

`system_prompt_append` adds rules after the base prompt without replacing it. The
`[prompt]` switches turn individual sections off.

### System prompt composition

smith sends the base prompt followed by these sections, in order:

1. `system_prompt_append`, when set.
2. `# Background tasks`: how a `run_shell` background task behaves and how `tool_wait`
   collects or kills it.
3. `# Subagents`: how to run `smith -p` through `run_shell`, and when to use `background`.
4. `# Sandbox`: working directory, home directory, operating system, command shell, model,
   git repository root, network state, the commands the image provides and the replacements
   worth preferring when both commands exist.
5. `# Project Context`: `$XDG_CONFIG_HOME/smith/AGENTS.md`, then `AGENTS.md` from the
   repository root down to the workspace. Each file is capped at 64 KiB and marked
   `[truncated]` when cut. The section states that it overrides the assistant defaults.
6. `# Skills`: `SKILL.md` descriptions from project `.agents/skills` directories, then
   `$XDG_CONFIG_HOME/smith/skills` and `~/.agents/skills`. The listing gives the name,
   description and path; the model reads the body with `read_file`.

The skills roots outside the workspace are mounted read-only when the skills section is
enabled, so the listed paths are readable inside the sandbox. Empty sections are omitted. The
`[prompt]` table and the order are described under configuration.

## Sandbox

Tool calls run inside a podman container. On the host, smith always creates one, and there is
no host-tool mode. A process already inside a smith sandbox can inherit that sandbox instead of
nesting a new container; its tools execute in the current environment. When the container cannot
start, the turn fails and smith prints why.

- The container has no network interface. Egress is a `CONNECT` request that the worker sends
  over the channel; the host checks it against the allowlist, resolves the name and dials. TLS
  is end to end, and there is no DNS inside the sandbox.
- A child `smith` reaches its provider through the worker's second loopback listener. The worker
  sends the request body over the channel; the host proxy adds the parent's provider key, user
  agent, session header and model, and forwards the response. The sandbox environment contains
  `SMITH_PROVIDER=sandbox`, `SMITH_MODEL=<parent model>`, `NO_PROXY` for loopback and a
  per-container `SMITH_SANDBOX_TOKEN`. It does not contain the provider key. The image's
  `/etc/smith/config.toml` defines the `sandbox` provider.
- The root filesystem is read-only, `/tmp` is a tmpfs, the workspace is mounted at its own
  path, and the caches live under `~/.local/share/smith/cache/{cargo,npm,pip}`. Existing
  `$XDG_CONFIG_HOME/smith/skills` and `~/.agents/skills` directories are mounted read-only at
  their own paths when `prompt.skills` is enabled.
- The environment is computed rather than inherited. Values travel over the channel and never
  appear in the container's command line.
- The base image ships inside the `smith` binary as a compressed docker archive and is loaded
  into the private store on first use. The final image builds from a generated Containerfile
  and contains `smith`, `smith-worker`, the child provider config and a generated inventory.
  The default image is about 123 MB and has no compiler, Git or CA certificates.
- Extra tools come from `[sandbox.image] packages` in the system or user config. The names are
  Debian package names; the build uses `--no-install-recommends`. Adding `git` installs Perl
  through Git's package dependency; add `ca-certificates` for HTTPS. A package set is cached
  by a tag that hashes the base, the package list and both binary digests, so sessions with
  the same resolved settings reuse the image. The default list is empty.
- A session's container stops after `[sandbox] idle_timeout` without activity, on disconnect
  and on process exit. Process state does not survive a stop; the filesystem does, through the
  workspace and the caches.
- `run_shell` with `background: true` starts a long command and returns a task id. `tool_wait`
  returns the output produced since the previous wait, reports whether the task is still
  running, and can stop it. A container with an open background run is not stopped by the idle
  timer.
- `--sandbox=container` refuses inside a sandbox, and `--sandbox=inherit` refuses outside one.
  The two guards keep a child from creating a nested container and keep inherit mode off the
  host. The marker is a policy signal, not authentication against the machine's owner.

The allowlist is empty by default. A `cargo fetch` through the proxy needs these entries:

```toml
[sandbox.network]
allow = ["index.crates.io:443", "static.crates.io:443", "github.com:443"]
```

An entry without a port matches every port on that host. A `host:port` entry matches that port
only. IP addresses, loopback targets and SOCKS are refused. See [Sandbox settings](#sandbox-settings).

Inspect and clean up containers:

```sh
smith sandbox ps            # containers in smith's private store
smith sandbox stop <id>     # stop one
smith sandbox rm <id>       # remove one, including after a crash
```

A `SIGKILL` of smith leaves the container behind. `smith sandbox ps` lists it, and
`smith sandbox rm` removes it. Nothing is reaped silently.

---

## Configuration

The precedence order, highest first, is CLI flags, then `SMITH_*` environment variables, then
three config files, then built-in defaults. Within the files the system file outranks the user
file, which outranks the project file. Each layer changes only the fields it states.

### Environment variables

| variable | effect |
|---|---|
| `OPENCODE_API_KEY` | This variable is required. It holds the credential. The variable name is configurable per provider (`api_key_env`). The value is read only from the environment. |
| `SMITH_MODEL` | Overrides the default model, for example `opencode-go/glm-5.3`. |
| `SMITH_PROVIDER` | Overrides the default provider. |
| `SMITH_THEME` | Overrides the terminal theme: `auto`, `dark`, `light`, `ansi` or `off`. |

### Config file

```sh
smith --config ./smith.toml -p "hi"
```

smith loads three files, in increasing priority:

| tier | path |
|---|---|
| project | `$XDG_CONFIG_HOME/smith/projects/<hash>.toml` (default `~/.config`) |
| user | `$XDG_CONFIG_HOME/smith/config.toml` |
| system | `/etc/smith/config.toml` |

The project file is keyed by the first 16 hex digits of the sha256 of the canonical workspace
root. It records `[project] path`, and smith ignores it with a warning when it names another
directory. `--config PATH` loads exactly that file and skips the three tiers.

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
`system_prompt_append`, `prompt`, `max_tokens`, `temperature`, `serve`, `ui`, `project`,
`sandbox`, `policy`, `providers`
```

Every top-level key is optional:

| key | meaning |
|---|---|
| `default_provider` | Provider used when a model is addressed without a `provider/` prefix |
| `model` | `provider/model`, or a bare id resolved against `default_provider` |
| `system_prompt` | Replaces the built-in base prompt for every session |
| `system_prompt_append` | Appended after the base prompt and before the procedural sections |
| `prompt.agents_md` | Include `# Project Context`. Default `true` |
| `prompt.skills` | Include `# Skills` and mount the global skill roots. Default `true` |
| `prompt.tasks` | Include `# Background tasks`. Default `true` |
| `prompt.subagents` | Include `# Subagents`. Default `true` |
| `prompt.sandbox` | Include `# Sandbox`. Default `true` |
| `max_tokens`, `temperature` | Passed through to the provider |
| `serve.bind`, `serve.port` | What `smith serve` binds when `--bind` or `--port` are not given. `bind` must be loopback |
| `ui.theme` | Terminal theme: `auto`, `dark`, `light`, `ansi` or `off`. Default `auto` |
| `sandbox` | Sandbox settings, described below |
| `policy.managed` | When true, a project file that sets a sandbox list is an error |
| `project.path` | The canonical workspace root a project file belongs to |

### Sandbox settings

```toml
[sandbox]
idle_timeout = "5m"

[sandbox.network]
allow = ["index.crates.io:443", "static.crates.io:443"]

[sandbox.mounts]
ro = ["/home/chris/.rustup", "/home/chris/.cargo/bin"]

[sandbox.env]
inherit = ["RUST_BACKTRACE"]
set = { CARGO_TERM_COLOR = "never" }

[sandbox.image]
packages = ["git", "ca-certificates", "gcc", "libc6-dev", "binutils"]

[policy]
managed = true
```

| key | meaning |
|---|---|
| `sandbox.idle_timeout` | How long an idle session keeps its container. Default `5m`, minimum 10 seconds |
| `sandbox.network.allow` | Egress allowlist. Default empty |
| `sandbox.mounts.ro` | Host paths mounted read-only at the same path. Defaults to `~/.rustup` and `~/.cargo/bin` when they exist |
| `sandbox.mounts.rw` | Extra writable mounts. Default empty |
| `sandbox.env.inherit` | Host variable names copied into the sandbox |
| `sandbox.env.set` | Literal variables |
| `sandbox.image.packages` | Debian packages installed into the session image. Default empty. The base provides the shell and coreutils |
| `policy.managed` | Reject project files that set a sandbox list |

`allow` and `inherit` intersect across the tiers that set them, so a project file can narrow a
list and never widen it. `mounts`, `image` and `policy` belong to the system or user file; a
project file that sets any of them is an error. Package lists from the system and user tiers
are unioned, sorted and deduplicated.

### Terminal and web highlighting

The terminal theme is `auto` by default. It follows the terminal environment:

- `NO_COLOR` non-empty, or `TERM` unset, empty or `dumb`, selects `off`.
- No `256color` in `TERM` and no `COLORTERM` selects `ansi`.
- `COLORFGBG` with a background of `7` or `15` selects `light`; otherwise `dark`.

`dark` and `light` use indexed 256 colours, `ansi` uses the 16 basic colours, and `off` uses no
colour but keeps bold headings, dim code blocks and underlined links. `[ui] theme` or
`SMITH_THEME` overrides the automatic choice.

The web UI uses its own dark palette and a light palette under `prefers-color-scheme: light`.
The roles are shared: accent for the user, chrome for tool headers, dim for gutters and notes,
heading, code, code block, link, add, remove, ok, warn and error. The browser renders the same
Markdown subset with DOM text nodes and classes; links are clickable only for `http` and
`https`. The credential deny list (`SSH_AUTH_SOCK`, `PODMAN_*`, `AWS_*`,
`*_TOKEN`, `EDITOR` and others) is enforced at load time, and a refused name is an error naming
the reason.

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

An assistant message keeps the reasoning the provider streamed with it, and the adapter sends
it back as `reasoning_content` on the next request. Thinking-mode models reject the request that
continues a tool call without it. Reasoning is printed only with `--show-reasoning`; it is never
part of the reply text.

---

## Troubleshooting

The messages below are the text smith prints.

| message | cause and fix |
|---|---|
| `no API key: environment variable OPENCODE_API_KEY is unset` | Export the key. smith checks for the key before any request, so the failure is immediate rather than a 401 during a session. |
| `authentication rejected by opencode-go: FreeTierError: ...` | The request is reaching the Console endpoint with a free-tier model, or the key has no OpenCode Go entitlement. Check `base_url` and the subscription. |
| `... upstream failure: ... Insufficient account funds` | The key is valid, but the account lacks credit for that model. |
| `cannot find the worker binary at ...; run cargo build -p smith-worker or set SMITH_WORKER_BIN` | The sandbox needs `smith-worker` next to `smith`. `cargo build` builds both. |
| `cannot find the smith binary at ...; run cargo build -p smith-cli or set SMITH_BIN` | The image needs `smith` to run a sandboxed child. `cargo build` builds it. |
| `unknown field `provider`` | A config file from an older schema. The key is `[providers.<id>]`; smith rejects the file rather than migrating it. |
| `the sandbox worker did not answer within 10s` | podman started but the channel did not complete. Check that the image starts (`podman --root ~/.local/share/smith/containers/storage images` shows the `smith-sandbox:<hash>` tag) and that the worker matches the smith version. |
| `invalid sandbox image package 'x': ...` | `[sandbox.image] packages` contains a name that is not a Debian package name. Use lowercase letters, digits, `+`, `-` and `.`. |
| `[sandbox.image] belongs in the system or user config` | A project file set the image package list. Move it to the system or user file. |
| `base image load failed: ...` | podman could not load the embedded base archive. Check podman and the private store. |
| `image build failed: ...` | The package install or the final image step failed. Check the package names and the host network. |
| `... Model is unavailable.` | The model id is wrong or not included in the plan. Run `smith providers list`, or check the model table in the OpenCode Go documentation. |
| `configuration error: unknown provider 'x'; configured providers: opencode-go` | The error indicates a typo in `-m` or `SMITH_MODEL`. It lists the configured providers. |
| `configuration error: refusing to bind 0.0.0.0: smith serve is reachable from localhost only...` | `--bind` was not a loopback address. Use `127.0.0.1` or `::1`. This is a hard error because a routable bind would expose the service. |
| `unknown field 'temperatur', expected one of ...` | The config file contains a typo. Unknown keys are errors. |
| `provider 'x' is new, so it has to set base_url` | A `[providers.x]` entry for a provider smith does not know must be complete because there is nothing to inherit from. |
| `... sent a malformed response: ...` | The stream contained data that could not be parsed. This is usually a proxy or a changed upstream format. The message includes the offending payload. |
| `configuration error: SMITH_THEME: unknown theme 'x' ...` | `SMITH_THEME` is not one of `auto`, `dark`, `light`, `ansi` or `off`. |
| `smith: configuration error: refusing to create a nested sandbox: already inside a smith sandbox; use --sandbox=inherit or auto` | `--sandbox=container` ran inside an existing smith sandbox. Use `inherit` or leave the mode on `auto`. |
| `smith: configuration error: refusing to run tools outside a sandbox: --sandbox=inherit requires SMITH_IN_SANDBOX=1` | `--sandbox=inherit` ran on the host. Use `auto` or `container`, or run the process inside a smith sandbox. |
| `smith: cancelled` | Ctrl-C, Esc Esc, or a web terminate stopped the turn. Not an error. |
| `smith: paused after the current step` | Esc paused the turn. Send a message to continue. |
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
  smith-channel/    frame codec and tool types shared by host and worker
  smith-core/       domain, agent loop, session actor, provider trait, config, tool seam
  smith-providers/  provider adapters + registry                 (the only crate doing HTTP)
  smith-sandbox/    podman supervision, image, allowlist, proxy dialer (host side)
  smith-worker/     pid 1 inside the container: handshake, tools, proxy listener
  smith-cli/        the `smith` binary                           (headless, REPL, serve)
  smith-web/        the localhost web server                     (a frontend, no agent logic)
  smith-web/ui/     index.html, app.js, style.css                (no build step, no npm)
```

The split is enforced by dependency direction. `smith-core` must not gain a network, terminal or
HTTP dependency, including from the web server. The session actor is in core, and the HTTP layer
only renders its events. To check this, run:

```sh
cargo tree -p smith-core -e normal | grep -E 'reqwest|hyper|axum|clap|rustyline'
cargo tree -p smith-worker -e normal | grep -E 'reqwest|hyper|axum|clap|rustyline'
# both must print nothing
```

The same core serves every frontend. The REPL, the headless one-shot mode and the web server are
three renderers over one session actor, not three implementations.

---

## Development

```sh
cargo test                        # 246 offline tests; no network, no API key needed
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Tests replay recorded SSE streams from the live endpoint
(`crates/smith-providers/tests/fixtures/`) rather than hand-written JSON. The fixtures cover the
empty-`choices` usage chunk, the separate `reasoning_content` channel and a thinking-mode tool
call. A scripted `MockProvider` and `MockTool` (`smith-core/src/mock.rs`) exercise the session
layer, the tool loop, CLI rendering and the web API. One web test opens a real socket, which
proves the bound address is loopback. The worker's provider-listener tests bind loopback on an
ephemeral port, because that listener parses HTTP itself.

Five tests are ignored because they build the image and start containers:

```sh
cargo build --workspace
cargo test -p smith-sandbox -- --ignored
```

The first runs `run_shell` and `write_file` against the default image and checks the generated
inventory and the missing package manager. The second runs `cargo fetch` and `git ls-remote`
through the proxy with a package list that adds Git and CA certificates, and checks the Perl-based
Git tools are present. The third runs `smith -p` inside the container with a fake host proxy: the
child's answer comes back through the channel, and the container environment holds the proxy
token but not the provider key. The fourth installs the compiler packages and builds a C program
and a Rust crate. The fifth starts two sessions with the same package list and checks that the
image tag and image are reused.

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
