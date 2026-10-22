//! smith: a barebones coding agent.
//!
//! The terminal frontend. It is a renderer: it sends [`Command`]s to a session and prints the
//! [`UiEvent`]s it receives. It holds no conversation state. The same session layer supports
//! the web server.

mod local;
mod markdown;
mod theme;
mod watch;

use std::io::{IsTerminal, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};

use smith_core::config::Config;
use smith_core::session::{
    SessionConfig, SessionHandle, SessionManager, SubError, Subscription, ToolSet,
};
use smith_core::{SmithError, ToolDisplay, UiEventKind};
use theme::{Role, Theme};

/// Exit codes. `-p` mode is used by scripts, so these values are part of the contract.
const EXIT_OK: u8 = 0;
const EXIT_FAILED: u8 = 1;
const EXIT_CANCELLED: u8 = 130;

/// How tool calls obtain their execution environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SandboxMode {
    /// Inherit an existing smith sandbox, otherwise create a container.
    Auto,
    /// Always create a container. Refused inside a smith sandbox.
    Container,
    /// Run tools in the current environment. Refused outside a smith sandbox.
    Inherit,
}

/// The resolved tool environment. `Auto` never reaches `build_manager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSandbox {
    Container,
    Inherit,
}

fn inside_sandbox() -> bool {
    std::env::var("SMITH_IN_SANDBOX").is_ok_and(|value| value == "1")
}

fn resolve_sandbox_mode(mode: SandboxMode, inside: bool) -> Result<ToolSandbox, SmithError> {
    match (mode, inside) {
        (SandboxMode::Auto, true) | (SandboxMode::Inherit, true) => Ok(ToolSandbox::Inherit),
        (SandboxMode::Auto, false) | (SandboxMode::Container, false) => Ok(ToolSandbox::Container),
        (SandboxMode::Container, true) => Err(SmithError::Config(
            "refusing to create a nested sandbox: already inside a smith sandbox; \
             use --sandbox=inherit or auto"
                .to_string(),
        )),
        (SandboxMode::Inherit, false) => Err(SmithError::Config(
            "refusing to run tools outside a sandbox: \
             --sandbox=inherit requires SMITH_IN_SANDBOX=1"
                .to_string(),
        )),
    }
}

#[derive(Parser, Debug)]
#[command(name = "smith", version, about = "a barebones coding agent")]
struct Cli {
    /// Run one prompt headlessly, print the reply, and exit.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,

    /// Model to use, as `provider/model` or a bare model id.
    #[arg(short = 'm', long, value_name = "MODEL")]
    model: Option<String>,

    /// Config file to load instead of the built-in defaults.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// System prompt, replacing the built-in one.
    #[arg(long, value_name = "PROMPT")]
    system: Option<String>,

    /// Print reasoning tokens (providers that emit them) to stderr.
    #[arg(long)]
    show_reasoning: bool,

    /// Tool sandbox: auto creates a container on the host and inherits an existing sandbox.
    #[arg(
        long,
        value_enum,
        default_value = "auto",
        value_name = "MODE",
        global = true
    )]
    sandbox: SandboxMode,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect configured providers and models.
    Providers {
        #[command(subcommand)]
        command: ProvidersCommand,
    },
    /// Inspect and clean up smith's sandbox containers.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },

    /// Serve the web UI on localhost: the same sessions, rendered in a browser.
    Serve(ServeArgs),
}

/// `smith serve` flags. `bind` and `port` are optional so the config file's `[serve]` section
/// can supply them. A flag overrides the config, and the config overrides the built-in default.
#[derive(Args, Debug)]
struct ServeArgs {
    /// Address to bind. Loopback only: anything else is refused.
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,

    /// Port to bind. 0 asks the operating system to choose one.
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,

    /// Attach this terminal to the same sessions, so one run can be watched in a terminal and
    /// a browser at once. Off by default, so `smith serve` can run as a background process.
    #[arg(long)]
    repl: bool,
}

#[derive(Subcommand, Debug)]
enum ProvidersCommand {
    /// List providers and the models smith will advertise for each.
    List,
}

#[derive(Subcommand, Debug)]
enum SandboxCommand {
    /// List smith's containers, including ones left by a crashed process.
    Ps,
    /// Stop a container by id or name.
    Stop { container: String },
    /// Remove a container by id or name.
    Rm { container: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("smith: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("smith: cannot resolve the workspace: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = match cli.config.as_deref() {
        Some(path) => Config::load(Some(path)),
        None => Config::load_for_workspace(&workspace),
    };
    let config = match config {
        Ok(c) => c,
        Err(e) => {
            eprintln!("smith: {e}");
            return ExitCode::FAILURE;
        }
    };

    match cli.command {
        Some(Command::Providers {
            command: ProvidersCommand::List,
        }) => {
            list_providers(&config);
            return ExitCode::SUCCESS;
        }
        Some(Command::Sandbox { command }) => {
            return run_sandbox(command).await;
        }
        Some(Command::Serve(args)) => {
            return serve(
                &config,
                cli.model.as_deref(),
                cli.system.clone(),
                args,
                cli.show_reasoning,
                cli.sandbox,
            )
            .await
        }
        None => {}
    }

    let manager = match build_manager(
        &config,
        cli.model.as_deref(),
        cli.system.clone(),
        cli.sandbox,
    ) {
        Ok(manager) => manager,
        Err(e) => {
            eprintln!("smith: {e}");
            return ExitCode::FAILURE;
        }
    };

    // One session for the entire process, so the conversation accumulates across REPL turns.
    // The session id is also the provider's conversation id.
    let session = manager.create();
    let show_reasoning = cli.show_reasoning;
    let theme = Theme::from_setting(config.ui.theme);

    // One listener covers both modes: Ctrl-C cancels the running turn rather than the
    // process, and the session ignores a cancel when nothing is running.
    watch_ctrl_c(&session);

    match cli.print {
        Some(prompt) => {
            let code = prompt_and_render(&session, prompt, show_reasoning, theme).await;
            ExitCode::from(code)
        }
        None => repl(&session, show_reasoning, theme).await,
    }
}

/// Resolve the provider, model and system prompt into a `SessionManager`.
fn build_manager(
    config: &Config,
    model_flag: Option<&str>,
    system_flag: Option<String>,
    sandbox: SandboxMode,
) -> Result<SessionManager, SmithError> {
    // Resolve the mode before the provider, so a guard failure does not require a key.
    let tool_sandbox = resolve_sandbox_mode(sandbox, inside_sandbox())?;

    let model_spec = model_flag.unwrap_or(&config.model);
    let model = config.resolve_model(model_spec)?;
    let provider_cfg = config.provider(&model.provider)?;
    let provider = smith_providers::build(&model.provider, provider_cfg)?;

    let workspace = std::env::current_dir()
        .map_err(|e| SmithError::Config(format!("cannot resolve the workspace: {e}")))?;

    let tools = match tool_sandbox {
        ToolSandbox::Container => {
            let supervisor = Arc::new(
                smith_sandbox::Supervisor::new().map_err(|e| SmithError::Config(e.to_string()))?,
            );
            let settings = config.sandbox.clone();
            let sandbox_workspace = workspace.clone();
            let provider_id = model.provider.clone();
            let provider_cfg = provider_cfg.clone();
            let model_id = model.model.clone();
            let factory = move |id: &smith_core::SessionId| {
                // The child's provider is the host proxy for this session. A failure here
                // leaves the sandbox without a provider, which the child reports as a 502.
                let proxy = match smith_providers::build_proxy(
                    &provider_id,
                    &provider_cfg,
                    id.as_str(),
                    &model_id,
                ) {
                    Ok(proxy) => Some(proxy),
                    Err(e) => {
                        eprintln!("smith: sandbox provider proxy: {e}");
                        None
                    }
                };
                let mut env = smith_sandbox::standard_env(&settings);
                env.insert("SMITH_PROVIDER".to_string(), "sandbox".to_string());
                env.insert("SMITH_MODEL".to_string(), model_id.clone());
                let sandbox = smith_sandbox::SandboxConfig {
                    workspace: sandbox_workspace.clone(),
                    session: id.as_str().to_string(),
                    allow: settings.allow.clone(),
                    ro_mounts: settings.ro_mounts.clone(),
                    env,
                    idle_timeout: settings.idle_timeout,
                    image: settings.image.clone(),
                    provider: proxy,
                };
                smith_sandbox::sandbox_tools(supervisor.clone(), sandbox)
            };
            ToolSet::Factory(Arc::new(factory))
        }
        ToolSandbox::Inherit => ToolSet::Fixed(local::local_tools(&workspace)?),
    };

    Ok(SessionManager::new(SessionConfig {
        provider,
        model: model.model,
        system: system_flag.or_else(|| config.system_prompt.clone()),
        tools,
        workspace,
        max_tokens: config.max_tokens,
        temperature: config.temperature,
    }))
}

/// `smith sandbox ps|stop|rm`. These commands do not need a model or a worker binary.
async fn run_sandbox(command: SandboxCommand) -> ExitCode {
    let supervisor = match smith_sandbox::Supervisor::new() {
        Ok(supervisor) => supervisor,
        Err(e) => {
            eprintln!("smith: sandbox: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = match command {
        SandboxCommand::Ps => match supervisor.list().await {
            Ok(list) if list.trim().is_empty() => {
                println!("no containers in {}", supervisor.store().display());
                Ok(())
            }
            Ok(list) => {
                println!("{}", list.trim_end());
                println!("store: {}", supervisor.store().display());
                Ok(())
            }
            Err(e) => Err(e),
        },
        SandboxCommand::Stop { container } => supervisor.stop(&container).await,
        SandboxCommand::Rm { container } => supervisor.remove(&container).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("smith: sandbox: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run the web frontend: the same core rendered in a browser instead of a terminal.
async fn serve(
    config: &Config,
    model_flag: Option<&str>,
    system_flag: Option<String>,
    args: ServeArgs,
    show_reasoning: bool,
    sandbox: SandboxMode,
) -> ExitCode {
    // Precedence, highest first: the flag, the `[serve]` section, the built-in default.
    let bind: IpAddr = match args.bind.as_deref() {
        Some(text) => match text.parse() {
            Ok(ip) => ip,
            Err(_) => {
                eprintln!("smith: --bind expects an IP address, not '{text}'");
                return ExitCode::FAILURE;
            }
        },
        None => config.serve.bind,
    };
    let port = args.port.unwrap_or(config.serve.port);

    // The bind check runs first: a routable address is a security error, and reporting it
    // before any provider request keeps the failure local. `smith_web::bind` checks it again
    // so the rule holds for every caller.
    if let Err(e) = smith_web::ensure_loopback(bind) {
        eprintln!("smith: {e}");
        return ExitCode::FAILURE;
    }

    let manager = match build_manager(config, model_flag, system_flag, sandbox) {
        Ok(manager) => manager,
        Err(e) => {
            eprintln!("smith: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Shared rather than moved: with `--repl`, the terminal holds a session handle from the
    // same set the browser uses.
    let manager = Arc::new(manager);
    let theme = Theme::from_setting(config.ui.theme);

    let bound = match smith_web::bind(smith_web::ServerConfig {
        sessions: manager.clone(),
        bind,
        port,
        token: smith_web::new_token(),
    })
    .await
    {
        Ok(bound) => bound,
        Err(e) => {
            eprintln!("smith: {e}");
            return ExitCode::FAILURE;
        }
    };

    if args.repl {
        attach_repl(&manager.create(), show_reasoning, theme);
    }

    match smith_web::serve(bound).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("smith: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Attach a terminal frontend to the session manager the server uses.
///
/// The session outlives the terminal: `--repl` adds a frontend to a server, so reaching
/// end-of-input stops reading input rather than shutting the server down.
fn attach_repl(session: &SessionHandle, show_reasoning: bool, theme: Theme) {
    eprintln!(
        "smith: terminal attached to session {}. It is also in the browser's session list.",
        session.id()
    );
    watch_ctrl_c_stopping_when_idle(session);

    let session = session.clone();
    tokio::spawn(async move {
        repl(&session, show_reasoning, theme).await;
        eprintln!("smith: terminal detached. The server is still listening.");
    });
}

fn list_providers(config: &Config) {
    for (id, cfg) in &config.providers {
        let default = if *id == config.default_provider {
            " (default)"
        } else {
            ""
        };
        println!("{id}{default}");
        println!("  endpoint: {}", cfg.base_url);
        println!("  api key:  ${}", cfg.api_key_env);
        for model in &cfg.models {
            let star = if *model == cfg.default_model {
                "*"
            } else {
                " "
            };
            println!("  {star} {id}/{model}");
        }
    }
}

/// Send one prompt and render the turn it starts.
async fn prompt_and_render(
    session: &SessionHandle,
    prompt: String,
    show_reasoning: bool,
    theme: Theme,
) -> u8 {
    // Subscribe before snapshotting, and snapshot before sending: a subscription is a live
    // view, so one created afterwards would miss what had already been emitted, and the
    // snapshot is what marks the turn that follows as this frontend's own.
    let mut subscription = session.subscribe();
    let since = match session.snapshot().await {
        Ok(snapshot) => snapshot.seq,
        Err(e) => {
            eprintln!("smith: {e}");
            return EXIT_FAILED;
        }
    };

    let sent = session.send(prompt.clone()).await;
    if let Err(e) = sent {
        eprintln!("smith: {e}");
        return EXIT_FAILED;
    }

    render_turn(
        &mut subscription,
        &prompt,
        since,
        show_reasoning,
        theme,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .await
}

/// Render a turn's events until it ends, returning the exit code for it.
///
/// `prompt` and `since` identify the turn. Events with `seq` at or below `since` belong to an
/// earlier turn or another frontend and are skipped. Rendering them would print another
/// frontend's reply next to this prompt, and their terminal event would end this loop before
/// the reply to this prompt arrives.
///
/// The writers are parameters rather than `stdout` and `stderr`, so tests can check the output
/// and exit codes. See the tests at the bottom of this file.
async fn render_turn<W: Write, E: Write>(
    subscription: &mut Subscription,
    prompt: &str,
    since: u64,
    show_reasoning: bool,
    theme: Theme,
    out: &mut W,
    err: &mut E,
) -> u8 {
    let mut reasoning_shown = false;
    let mut markdown = markdown::Highlighter::new();

    loop {
        let event = match subscription.recv().await {
            Ok(event) => event,
            // The transcript arrives complete with `TurnFinished`, so a lag costs display, not
            // correctness. Report the gap rather than printing silently.
            Err(SubError::Lagged(n)) => {
                write_styled(
                    err,
                    theme,
                    Role::Warn,
                    &format!("smith: fell {n} events behind. Some output was skipped."),
                );
                continue;
            }
            Err(SubError::Closed) => {
                write_styled(
                    err,
                    theme,
                    Role::Error,
                    "smith: the session stopped unexpectedly",
                );
                return EXIT_FAILED;
            }
        };

        if event.seq <= since {
            continue;
        }

        match event.kind {
            UiEventKind::TextDelta { text } => {
                markdown.feed(&text, theme, out);
            }
            UiEventKind::ReasoningDelta { text } if show_reasoning => {
                if !reasoning_shown {
                    let _ = write!(err, "{}", theme.open(Role::Dim));
                    reasoning_shown = true;
                }
                let _ = write!(err, "{text}");
            }
            // Reasoning is not part of the reply. It is hidden unless asked for.
            UiEventKind::ReasoningDelta { .. } => {}
            UiEventKind::TurnFinished {
                usage,
                tool_arg_errors,
                ..
            } => {
                markdown.finish(theme, out);
                finish_reasoning(err, theme, &mut reasoning_shown);
                for failure in &tool_arg_errors {
                    write_styled(
                        err,
                        theme,
                        Role::Error,
                        &format!(
                            "smith: model sent unparseable arguments for {}: {}",
                            failure.tool, failure.message
                        ),
                    );
                }
                if usage.input_tokens > 0 || usage.output_tokens > 0 {
                    write_styled(
                        err,
                        theme,
                        Role::Dim,
                        &format!(
                            "smith: {} in / {} out tokens",
                            usage.input_tokens, usage.output_tokens
                        ),
                    );
                }
                flush(out, err);
                return EXIT_OK;
            }
            UiEventKind::TurnPaused {
                usage,
                tool_arg_errors,
            } => {
                markdown.finish(theme, out);
                finish_reasoning(err, theme, &mut reasoning_shown);
                for failure in &tool_arg_errors {
                    write_styled(
                        err,
                        theme,
                        Role::Error,
                        &format!(
                            "smith: model sent unparseable arguments for {}: {}",
                            failure.tool, failure.message
                        ),
                    );
                }
                if usage.input_tokens > 0 || usage.output_tokens > 0 {
                    write_styled(
                        err,
                        theme,
                        Role::Dim,
                        &format!(
                            "smith: {} in / {} out tokens",
                            usage.input_tokens, usage.output_tokens
                        ),
                    );
                }
                write_styled(
                    err,
                    theme,
                    Role::Warn,
                    "smith: paused after the current step",
                );
                flush(out, err);
                return EXIT_OK;
            }
            UiEventKind::TurnFailed { message } => {
                markdown.finish(theme, out);
                finish_reasoning(err, theme, &mut reasoning_shown);
                write_styled(err, theme, Role::Error, &format!("smith: {message}"));
                flush(out, err);
                return EXIT_FAILED;
            }
            UiEventKind::PauseRequested => {
                write_styled(
                    err,
                    theme,
                    Role::Warn,
                    "smith: pausing... (esc again to terminate)",
                );
                let _ = err.flush();
            }
            UiEventKind::Cancelled => {
                markdown.finish(theme, out);
                finish_reasoning(err, theme, &mut reasoning_shown);
                write_styled(err, theme, Role::Warn, "smith: cancelled");
                flush(out, err);
                return EXIT_CANCELLED;
            }
            // This frontend's prompt was refused because another frontend started a turn
            // first. Return because this turn will not happen. Waiting would hang, since the
            // reply was never requested.
            UiEventKind::SendRejected { prompt: rejected } if rejected == prompt => {
                write_styled(
                    err,
                    theme,
                    Role::Error,
                    &format!("smith: a turn is already running. {rejected:?} was dropped."),
                );
                flush(out, err);
                return EXIT_FAILED;
            }
            // Another frontend's prompt was refused. Report it because the turn it belongs to
            // is unaffected.
            UiEventKind::SendRejected { prompt: rejected } => {
                write_styled(
                    err,
                    theme,
                    Role::Warn,
                    &format!("smith: another frontend sent {rejected:?} while a turn was running"),
                );
            }
            // The call header, then its bounded output block and the outcome. The model
            // reply stays on stdout; all of this is diagnostics.
            UiEventKind::ToolStarted { summary, .. } => {
                let _ = write!(
                    err,
                    "{}smith:{} ",
                    theme.open(Role::Dim),
                    theme.close(Role::Dim)
                );
                let _ = write!(
                    err,
                    "{}{}{}",
                    theme.open(Role::Chrome),
                    summary,
                    theme.close(Role::Chrome)
                );
                let _ = writeln!(err);
            }
            UiEventKind::ToolFinished {
                ok,
                duration_ms,
                summary,
                display,
                ..
            } => {
                write_display(err, theme, &display, ok);
                let role = if ok { Role::Ok } else { Role::Error };
                let _ = write!(
                    err,
                    "{}  └{} ",
                    theme.open(Role::Dim),
                    theme.close(Role::Dim)
                );
                let _ = write!(
                    err,
                    "{}{}{} ({duration_ms} ms)",
                    theme.open(role),
                    summary,
                    theme.close(role)
                );
                let _ = writeln!(err);
            }
            UiEventKind::TurnStarted { .. } | UiEventKind::Usage { .. } => {}
        }
    }
}

fn finish_reasoning<W: Write>(err: &mut W, theme: Theme, shown: &mut bool) {
    if *shown {
        let _ = write!(err, "{}", theme.close(Role::Dim));
        *shown = false;
    }
}

fn flush<W: Write, E: Write>(out: &mut W, err: &mut E) {
    let _ = out.flush();
    let _ = err.flush();
}

fn write_styled<W: Write>(err: &mut W, theme: Theme, role: Role, text: &str) {
    let _ = writeln!(err, "{}{}{}", theme.open(role), text, theme.close(role));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayKind {
    Head,
    HeadTail,
    Diff,
}

/// Print a finished tool's preview rows. The first row shares the call header; the last row
/// shares the outcome line, so the block is one visual unit.
fn write_display<W: Write>(err: &mut W, theme: Theme, display: &ToolDisplay, ok: bool) {
    let (head, tail, omitted, kind): (&[String], &[String], usize, DisplayKind) = match display {
        ToolDisplay::Collapsed => return,
        ToolDisplay::Head { lines, omitted } => (lines, &[], *omitted, DisplayKind::Head),
        ToolDisplay::HeadTail {
            head,
            tail,
            omitted,
        } => (head, tail, *omitted, DisplayKind::HeadTail),
        ToolDisplay::Diff { lines, omitted } => (lines, &[], *omitted, DisplayKind::Diff),
    };
    for line in head {
        write_display_row(err, theme, line, ok, kind);
    }
    if omitted > 0 {
        write_display_row(
            err,
            theme,
            &format!("... ({omitted} more lines) ..."),
            ok,
            DisplayKind::Head,
        );
    }
    for line in tail {
        write_display_row(err, theme, line, ok, kind);
    }
}

fn write_display_row<W: Write>(err: &mut W, theme: Theme, line: &str, ok: bool, kind: DisplayKind) {
    let role = if !ok {
        Role::Error
    } else {
        match kind {
            DisplayKind::Diff if line.starts_with('+') => Role::Add,
            DisplayKind::Diff if line.starts_with('-') => Role::Remove,
            DisplayKind::HeadTail if line.starts_with("! ") => Role::Warn,
            _ => Role::Dim,
        }
    };
    let _ = write!(
        err,
        "{}  │{} ",
        theme.open(Role::Dim),
        theme.close(Role::Dim)
    );
    let _ = write!(err, "{}{}{}", theme.open(role), line, theme.close(role));
    let _ = writeln!(err);
}

async fn repl(session: &SessionHandle, show_reasoning: bool, theme: Theme) -> ExitCode {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let interactive = std::io::stdin().is_terminal();
    if interactive && std::io::stdout().is_terminal() {
        eprintln!(
            "{}smith: esc pauses after the current step; esc esc terminates{}",
            theme.open(Role::Dim),
            theme.close(Role::Dim)
        );
    }
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut subscription = session.subscribe();

    loop {
        if interactive {
            print!(
                "{}> {}",
                theme.open(Role::Accent),
                theme.close(Role::Accent)
            );
            let _ = std::io::stdout().flush();
        }

        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("smith: {e}");
                return ExitCode::FAILURE;
            }
        };

        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt == "/exit" || prompt == "/quit" {
            return ExitCode::SUCCESS;
        }

        // Snapshot before sending, so the reply rendered next is this prompt's and not
        // something another frontend started in the meantime.
        let since = match session.snapshot().await {
            Ok(snapshot) => snapshot.seq,
            Err(e) => {
                eprintln!("smith: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = session.send(prompt.to_string()).await {
            eprintln!("smith: {e}");
            return ExitCode::FAILURE;
        }

        // The watcher owns stdin only while the turn runs. Drop it before the next line read.
        let watch = watch::KeyWatch::start(session);
        // A failed or cancelled turn never entered the transcript, so there is nothing to
        // undo here. The session enforces that rule, not the REPL.
        let _ = render_turn(
            &mut subscription,
            prompt,
            since,
            show_reasoning,
            theme,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )
        .await;
        drop(watch);
    }
}

/// Ctrl-C cancels the running turn instead of the process, so a long reply can be abandoned
/// without losing the conversation. One listener covers the entire process, and the session
/// ignores a cancel when nothing is running.
fn watch_ctrl_c(session: &SessionHandle) {
    let session = session.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            let _ = session.cancel().await;
        }
    });
}

/// Ctrl-C while a terminal is attached to a server: cancel the running turn, and stop the
/// process when nothing is running.
///
/// The plain REPL does not exit on Ctrl-C because the key cancels a reply. In the `--repl`
/// case that would leave no way to stop the server from the terminal it was started in.
fn watch_ctrl_c_stopping_when_idle(session: &SessionHandle) {
    let session = session.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            if session.is_busy() {
                let _ = session.cancel().await;
            } else {
                std::process::exit(i32::from(EXIT_OK));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use smith_core::config::ThemeName;
    use smith_core::message::StopReason;
    use smith_core::message::Usage;
    use smith_core::mock::{MockProvider, MockTool};
    use smith_core::provider::ProviderEvent;
    use smith_core::{Tool, ToolContext, ToolError, ToolSpec};
    use tokio::sync::Semaphore;

    #[test]
    fn sandbox_mode_resolution_guards_nesting_and_host_tools() {
        assert_eq!(
            resolve_sandbox_mode(SandboxMode::Auto, true).unwrap(),
            ToolSandbox::Inherit
        );
        assert_eq!(
            resolve_sandbox_mode(SandboxMode::Auto, false).unwrap(),
            ToolSandbox::Container
        );
        assert_eq!(
            resolve_sandbox_mode(SandboxMode::Container, false).unwrap(),
            ToolSandbox::Container
        );
        assert_eq!(
            resolve_sandbox_mode(SandboxMode::Inherit, true).unwrap(),
            ToolSandbox::Inherit
        );

        let error = resolve_sandbox_mode(SandboxMode::Container, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("nested sandbox"), "{error}");

        let error = resolve_sandbox_mode(SandboxMode::Inherit, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("SMITH_IN_SANDBOX"), "{error}");
    }

    fn session_running(provider: Arc<MockProvider>) -> SessionHandle {
        SessionManager::new(SessionConfig {
            provider,
            model: "mock-1".to_string(),
            system: None,
            tools: Vec::new().into(),
            workspace: std::env::temp_dir(),
            max_tokens: None,
            temperature: None,
        })
        .create()
    }

    fn session_with_tools(provider: Arc<MockProvider>, tools: Vec<Arc<dyn Tool>>) -> SessionHandle {
        SessionManager::new(SessionConfig {
            provider,
            model: "mock-1".to_string(),
            system: None,
            tools: tools.into(),
            workspace: std::env::temp_dir(),
            max_tokens: None,
            temperature: None,
        })
        .create()
    }

    /// A tool that blocks until the test releases it, so the pause lands mid-batch.
    struct BlockingTool {
        started: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }

    impl BlockingTool {
        fn new() -> Self {
            Self {
                started: Arc::new(Semaphore::new(0)),
                release: Arc::new(Semaphore::new(0)),
            }
        }
    }

    impl Tool for BlockingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "blocking".to_string(),
                description: "a tool that blocks until released".to_string(),
                schema: serde_json::json!({"type": "object"}),
            }
        }

        fn call<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a ToolContext,
        ) -> futures_util::future::BoxFuture<'a, Result<String, ToolError>> {
            let started = self.started.clone();
            let release = self.release.clone();
            Box::pin(async move {
                started.add_permits(1);
                let _permit = release.acquire().await.map_err(|_| {
                    ToolError::Message("the test tool lost its release channel".to_string())
                })?;
                Ok("{\"content\":\"blocked\"}".to_string())
            })
        }
    }

    /// Render one turn and return the exit code and the output written to each stream.
    ///
    /// The subscription is the caller's, as it is for a real frontend. A subscription is a
    /// view of the session from its creation point, so the REPL keeps one for the process
    /// rather than making a fresh one per prompt, which would replay earlier turns.
    async fn run_turn_rendered(
        session: &SessionHandle,
        subscription: &mut Subscription,
        show_reasoning: bool,
        cancel_after: Option<Duration>,
    ) -> (u8, String, String) {
        // Exactly what the REPL does: snapshot, send, then render only what followed.
        let since = session.snapshot().await.unwrap().seq;
        session.send("go").await.unwrap();

        if let Some(delay) = cancel_after {
            let session = session.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = session.cancel().await;
            });
        }

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = render_turn(
            subscription,
            "go",
            since,
            show_reasoning,
            Theme::off(),
            &mut out,
            &mut err,
        )
        .await;
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// This test pins the streaming contract: the reply on stdout with one trailing newline,
    /// diagnostics on stderr, exit code 0.
    #[tokio::test]
    async fn a_finished_turn_prints_the_reply_then_the_usage_line() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::TextDelta("Hel".to_string()),
            ProviderEvent::TextDelta("lo".to_string()),
            ProviderEvent::Usage(Usage {
                input_tokens: 19,
                output_tokens: 17,
            }),
            ProviderEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ]);
        let session = session_running(provider.clone());
        let mut subscription = session.subscribe();

        let (code, out, err) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "Hello\n");
        assert_eq!(
            err,
            "\x1b[2msmith: 19 in / 17 out tokens\x1b[0m\n".to_string()
        );
    }

    #[tokio::test]
    async fn reasoning_is_hidden_unless_asked_for() {
        let script = || {
            vec![
                ProviderEvent::ReasoningDelta("thinking".to_string()),
                ProviderEvent::TextDelta("answer".to_string()),
                ProviderEvent::Done {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };

        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(script());
        let session = session_running(provider.clone());
        let mut subscription = session.subscribe();
        let (_, out, err) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(out, "answer\n");
        assert_eq!(err, "", "reasoning must not leak into stderr by default");

        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(script());
        let session = session_running(provider.clone());
        let mut subscription = session.subscribe();
        let (_, out, err) = run_turn_rendered(&session, &mut subscription, true, None).await;
        assert_eq!(out, "answer\n");
        assert_eq!(err, "\x1b[2mthinking\x1b[0m", "dimmed, then reset");
    }

    /// Exit code 130, and the cancelled turn is dropped from the transcript: the next request
    /// contains only the follow-up prompt.
    #[tokio::test]
    async fn ctrl_c_cancels_the_turn_without_leaving_it_in_the_transcript() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_stalling_text("half an ans");
        provider.push_text("recovered");
        let session = session_running(provider.clone());
        let mut subscription = session.subscribe();

        let (code, out, err) = run_turn_rendered(
            &session,
            &mut subscription,
            false,
            Some(Duration::from_millis(20)),
        )
        .await;
        assert_eq!(code, EXIT_CANCELLED);
        assert_eq!(out, "half an ans\n", "partial output is still flushed");
        assert_eq!(err, "smith: cancelled\n");

        // The session survives and is usable.
        let (code, out, _) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "recovered\n");

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].messages.len(),
            1,
            "the cancelled turn must not appear in the next request"
        );
        assert_eq!(requests[1].messages[0].text(), "go");
    }

    /// With two frontends on one session, the terminal must render the turn it started.
    ///
    /// A queued event from another frontend must not end this frontend's render loop before
    /// the reply to its own prompt arrives.
    #[tokio::test]
    async fn a_turn_from_another_frontend_is_not_rendered_as_this_ones_reply() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("theirs");
        provider.push_text("mine");
        let session = session_running(provider);

        // The terminal's view and a second frontend that sends first.
        let mut terminal = session.subscribe();
        let mut other_frontend = session.subscribe();

        session.send("theirs").await.unwrap();
        loop {
            if matches!(
                other_frontend.recv().await.unwrap().kind,
                UiEventKind::TurnFinished { .. }
            ) {
                break;
            }
        }

        // The terminal sends now. Its queued backlog already holds the other turn.
        let since = session.snapshot().await.unwrap().seq;
        session.send("go").await.unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = render_turn(
            &mut terminal,
            "go",
            since,
            false,
            Theme::off(),
            &mut out,
            &mut err,
        )
        .await;

        assert_eq!(code, EXIT_OK);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "mine\n",
            "the other frontend's reply must not be rendered as this prompt's answer"
        );
        assert_eq!(String::from_utf8(err).unwrap(), "");

        // Both turns are in the transcript, which the two frontends share.
        let transcript = session.snapshot().await.unwrap().transcript;
        assert_eq!(transcript.len(), 4);
        assert_eq!(transcript[3].text(), "mine");
    }

    /// A prompt is refused because another frontend's turn is running. The reply it waits for
    /// is never requested, so waiting would hang. The frontend must fail instead.
    #[tokio::test]
    async fn a_prompt_rejected_while_another_turn_runs_fails_instead_of_waiting() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_stalling_text("working");
        let session = session_running(provider);

        let mut terminal = session.subscribe();
        let mut other_frontend = session.subscribe();

        session.send("first").await.unwrap();
        // Wait until the turn is in flight before sending.
        loop {
            if let UiEventKind::TextDelta { .. } = other_frontend.recv().await.unwrap().kind {
                break;
            }
        }

        let since = session.snapshot().await.unwrap().seq;
        session.send("go").await.unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            render_turn(
                &mut terminal,
                "go",
                since,
                false,
                Theme::off(),
                &mut out,
                &mut err,
            ),
        )
        .await
        .expect("a rejected prompt must not leave the frontend waiting");

        assert_eq!(code, EXIT_FAILED);
        assert_eq!(String::from_utf8(out).unwrap(), "");
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("a turn is already running"), "{err}");
        assert!(
            err.contains("\"go\""),
            "it must name the dropped prompt: {err}"
        );
    }

    #[tokio::test]
    async fn a_provider_failure_is_exit_1_with_the_reason_on_stderr() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_error(smith_core::ProviderError::Transport {
            provider: "mock".to_string(),
            message: "connection reset".to_string(),
        });
        let session = session_running(provider.clone());
        let mut subscription = session.subscribe();

        let (code, out, err) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(out, "", "no reply to print");
        assert!(err.starts_with("smith: transport failure"), "{err}");
    }

    #[tokio::test]
    async fn unparseable_tool_arguments_are_reported_but_the_turn_still_succeeds() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
                args_delta: "{\"path\": \"/etc".to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        // The loop answers the malformed call with an error result and asks again.
        provider.push_text("recovered");
        let session = session_running(provider.clone());
        let mut subscription = session.subscribe();

        let (code, out, err) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "recovered\n");
        assert!(
            err.contains("unparseable arguments for read_file"),
            "the loop reports what the model got wrong: {err}"
        );
    }

    #[tokio::test]
    async fn a_finished_shell_tool_prints_its_output_block() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("run_shell".to_string()),
                args_delta: r#"{"command":"echo hi"}"#.to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("done");
        let result = serde_json::json!({
            "exit_code": 0, "stdout": "one\n", "stderr": "bad\n",
            "truncated": false, "timed_out": false, "cancelled": false, "running": false
        })
        .to_string();
        let tool = Arc::new(MockTool::new("run_shell", result));
        let session = session_with_tools(provider, vec![tool as Arc<dyn Tool>]);
        let mut subscription = session.subscribe();

        let (code, out, err) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "done\n");
        assert!(err.contains("run_shell echo hi"), "{err}");
        assert!(err.contains("one"), "{err}");
        assert!(err.contains("! bad"), "{err}");
        assert!(err.contains("exit 0 ("), "{err}");
    }

    #[tokio::test]
    async fn a_finished_edit_prints_a_diff_block() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("edit_file".to_string()),
                args_delta: r#"{"path":"src/a.rs","old_string":"old","new_string":"new"}"#
                    .to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("done");
        let result = serde_json::json!({"path": "src/a.rs", "replacements": 1}).to_string();
        let tool = Arc::new(MockTool::new("edit_file", result));
        let session = session_with_tools(provider, vec![tool as Arc<dyn Tool>]);
        let mut subscription = session.subscribe();

        let (code, _, err) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(code, EXIT_OK);
        assert!(err.contains("edit_file src/a.rs"), "{err}");
        assert!(err.contains("-old"), "{err}");
        assert!(err.contains("+new"), "{err}");
    }

    #[tokio::test]
    async fn the_ansi_theme_styles_the_tool_block() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("run_shell".to_string()),
                args_delta: r#"{"command":"echo hi"}"#.to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("done");
        let result = serde_json::json!({
            "exit_code": 0, "stdout": "one\n", "stderr": "",
            "truncated": false, "timed_out": false, "cancelled": false, "running": false
        })
        .to_string();
        let tool = Arc::new(MockTool::new("run_shell", result));
        let session = session_with_tools(provider, vec![tool as Arc<dyn Tool>]);
        let mut subscription = session.subscribe();
        let since = session.snapshot().await.unwrap().seq;
        session.send("go").await.unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = render_turn(
            &mut subscription,
            "go",
            since,
            false,
            Theme::from_name(ThemeName::Ansi),
            &mut out,
            &mut err,
        )
        .await;
        assert_eq!(code, EXIT_OK);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("\x1b[36mrun_shell echo hi\x1b[0m"), "{err}");
        assert!(err.contains("\x1b[2m\x1b[36mone\x1b[0m"), "{err}");
        assert!(err.contains("\x1b[32mexit 0"), "{err}");
    }

    #[tokio::test]
    async fn a_read_is_collapsed_to_the_call_and_outcome() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
                args_delta: r#"{"path":"src/a.rs"}"#.to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("done");
        let result = serde_json::json!({
            "path": "src/a.rs", "start_line": 1, "end_line": 1,
            "total_lines": 1, "truncated": false, "content": "secret\n"
        })
        .to_string();
        let tool = Arc::new(MockTool::new("read_file", result));
        let session = session_with_tools(provider, vec![tool as Arc<dyn Tool>]);
        let mut subscription = session.subscribe();

        let (code, _, err) = run_turn_rendered(&session, &mut subscription, false, None).await;
        assert_eq!(code, EXIT_OK);
        assert!(err.contains("read_file src/a.rs"), "{err}");
        assert!(
            !err.contains("  │"),
            "a read must not print its content: {err}"
        );
        assert!(err.contains("ok ("), "{err}");
    }

    #[tokio::test]
    async fn a_paused_turn_prints_the_pause_lines_and_exits_zero() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("blocking".to_string()),
                args_delta: "{}".to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("unused");
        let tool = Arc::new(BlockingTool::new());
        let session = session_with_tools(provider, vec![tool.clone() as Arc<dyn Tool>]);
        let mut subscription = session.subscribe();

        let since = session.snapshot().await.unwrap().seq;
        session.send("go").await.unwrap();

        let background = tool.clone();
        let handle = session.clone();
        tokio::spawn(async move {
            background.started.acquire().await.unwrap().forget();
            handle.pause().await.unwrap();
            background.release.add_permits(1);
        });

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = render_turn(
            &mut subscription,
            "go",
            since,
            false,
            Theme::off(),
            &mut out,
            &mut err,
        )
        .await;

        assert_eq!(code, EXIT_OK);
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains("smith: pausing... (esc again to terminate)"),
            "{err}"
        );
        assert!(
            err.contains("smith: paused after the current step"),
            "{err}"
        );
        assert_eq!(String::from_utf8(out).unwrap(), "");
    }
}
