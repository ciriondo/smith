//! Sessions: the transcript, the provider and the cancel token, owned by one actor task.
//!
//! Frontends do not hold conversation state. They send [`Command`]s and render [`UiEvent`]s,
//! so the REPL, the headless one-shot mode and the web server are renderers over one
//! implementation.
//!
//! Three rules apply here:
//!
//! * **Deltas are never persisted.** The transcript gains the user message and the assistant
//!   message exactly once, when a turn finishes or pauses. A failed or cancelled turn leaves
//!   the transcript untouched, so the model never sees a partial answer. A paused turn keeps
//!   the steps that completed before the boundary.
//! * **`Send` while a turn is running is rejected**, not queued.
//! * **[`SessionHandle::subscribe`] subscribes before it snapshots**, and
//!   [`Subscription::recv`] drops events that were already replayed. The opposite order
//!   loses every event that falls between the two steps.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{run_agent, AgentError, AgentEvent, AgentOutcome, PauseToken, ToolArgError};
use crate::error::SmithError;
use crate::message::{Message, Usage};
use crate::provider::{ChatRequest, Provider, ProviderEvent};
use crate::tools::{Tool, ToolDisplay};

/// Replay buffer cap per session. A reconnecting client can resume from this buffer.
const BACKLOG_CAP: usize = 1000;
/// Live event channel depth. A subscriber that falls further behind than this receives
/// [`SubError::Lagged`] rather than losing output silently.
const EVENT_CHANNEL_CAP: usize = 256;
/// Commands are infrequent, so a small queue is sufficient.
const COMMAND_CAP: usize = 32;

/// An opaque session handle. Frontends pass it between components and print it. They never
/// parse it.
///
/// The format is also the provider's conversation id (`x-opencode-session` for OpenCode Go),
/// so it stays stable across the process for a given session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// A fresh id, unique per process and per creation time.
    pub fn generate() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self(format!("smith-{nanos:x}-{:x}", std::process::id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Lets sessions be looked up by `&str` without allocating a key, as `PathBuf` does.
/// The inner value is the full hash, so this stays consistent with the derived `Hash`.
impl std::borrow::Borrow<str> for SessionId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// A frontend's only way to affect a session.
///
/// Not `Clone`: a snapshot carries a one-shot reply channel.
#[derive(Debug)]
pub enum Command {
    /// Send a user message. Rejected with [`UiEventKind::SendRejected`] if a turn is already
    /// running.
    Send { text: String },
    /// Stop the running turn at its next provider-request boundary. Ignored when idle.
    Pause,
    /// Cancel the running turn immediately, whichever frontend asked for it.
    Cancel,
    /// Ask for the transcript. This is answered during a turn as well, so a reconnecting
    /// frontend can re-fetch what it missed rather than reconnecting without state.
    Snapshot {
        reply: oneshot::Sender<SessionSnapshot>,
    },
}

/// A session's transcript and state at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub busy: bool,
    /// The seq of the last event emitted. A frontend takes this before sending, so it can
    /// distinguish its own turn's events from earlier events. With two frontends on one
    /// session, the difference is between rendering a reply and dropping it.
    pub seq: u64,
    /// Everything the model sees on the next turn: assistant messages that finished and
    /// nothing from a failed or cancelled turn.
    pub transcript: Vec<Message>,
}

/// Something a frontend renders.
#[derive(Debug, Clone, PartialEq)]
pub struct UiEvent {
    /// The sequence number is session-scoped, monotonic and never reused. The SSE
    /// `Last-Event-ID` header carries this value.
    pub seq: u64,
    pub kind: UiEventKind,
}

/// Serialised with an internal tag, so the JSON discriminant matches the SSE `event:` field,
/// for example `{"type":"text_delta","text":"Hi"}`. Variants hold named fields rather than
/// bare values because an internally-tagged enum cannot contain a newtype.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiEventKind {
    TurnStarted {
        prompt: String,
    },
    TextDelta {
        text: String,
    },
    /// Kept distinct from [`UiEventKind::TextDelta`] so a frontend can hide it without
    /// inspecting the text.
    ReasoningDelta {
        text: String,
    },
    Usage {
        usage: Usage,
    },
    TurnFinished {
        message: Message,
        usage: Usage,
        /// Tool calls whose arguments did not parse. The turn still finishes because this
        /// failure is recoverable.
        tool_arg_errors: Vec<ToolArgError>,
    },
    /// The turn stopped at a provider-request boundary. The transcript already holds the
    /// steps that completed before it.
    TurnPaused {
        usage: Usage,
        tool_arg_errors: Vec<ToolArgError>,
    },
    TurnFailed {
        message: String,
    },
    Cancelled,
    /// A pause request was accepted for the running turn. Emitted once per turn so every
    /// frontend can show the pending state.
    PauseRequested,
    /// A `Send` arrived while a turn was running. Two frontends can race the busy check, so
    /// the actor reports the rejection instead of assuming the frontend knows.
    SendRejected {
        prompt: String,
    },
    /// A tool call started. `summary` is one line built from the arguments, so frontends do
    /// not parse tool-specific JSON.
    ToolStarted {
        call_id: String,
        name: String,
        summary: String,
    },
    /// A tool call finished. `summary` describes the outcome, for example `exit 0`.
    ToolFinished {
        call_id: String,
        name: String,
        ok: bool,
        duration_ms: u64,
        truncated: bool,
        summary: String,
        display: ToolDisplay,
    },
}

impl UiEventKind {
    /// Wire name, used as the SSE `event:` field and as the JSON tag.
    pub fn name(&self) -> &'static str {
        match self {
            UiEventKind::TurnStarted { .. } => "turn_started",
            UiEventKind::TextDelta { .. } => "text_delta",
            UiEventKind::ReasoningDelta { .. } => "reasoning_delta",
            UiEventKind::Usage { .. } => "usage",
            UiEventKind::TurnFinished { .. } => "turn_finished",
            UiEventKind::TurnPaused { .. } => "turn_paused",
            UiEventKind::TurnFailed { .. } => "turn_failed",
            UiEventKind::Cancelled => "cancelled",
            UiEventKind::PauseRequested => "pause_requested",
            UiEventKind::SendRejected { .. } => "send_rejected",
            UiEventKind::ToolStarted { .. } => "tool_started",
            UiEventKind::ToolFinished { .. } => "tool_finished",
        }
    }
}

/// Why a subscription stopped delivering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubError {
    /// The subscriber fell more than [`EVENT_CHANNEL_CAP`] events behind. The count is how
    /// many events were dropped. A client that receives this must re-fetch and reconnect
    /// instead of showing a transcript with a gap.
    Lagged(u64),
    /// The session actor is gone.
    Closed,
}

impl fmt::Display for SubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubError::Lagged(n) => write!(f, "fell {n} events behind"),
            SubError::Closed => f.write_str("session closed"),
        }
    }
}

/// A live view of one session's events.
///
/// Created by [`SessionHandle::subscribe`], which subscribes and snapshots the backlog in one
/// call, so no caller can choose the wrong order. `recv` returns the snapshot first and the
/// live stream after it, dropping events already returned, so a consumer sees no gaps or
/// duplicates.
pub struct Subscription {
    rx: broadcast::Receiver<UiEvent>,
    /// The backlog as it was when this subscription was created.
    snapshot: Vec<UiEvent>,
    /// Index into `snapshot` of the next entry `recv` returns.
    next: usize,
    /// Highest seq already returned, so a live copy of a replayed event is dropped.
    last_seq: u64,
}

impl Subscription {
    /// Events emitted before this subscription existed, oldest first.
    pub fn backlog(&self) -> &[UiEvent] {
        &self.snapshot
    }

    /// Backlog entries a resuming client has not seen, for `Last-Event-ID`.
    pub fn backlog_since(&self, since: u64) -> impl Iterator<Item = &UiEvent> {
        self.snapshot.iter().filter(move |e| e.seq > since)
    }

    /// Treat everything up to and including `seq` as already seen. This implements resumption
    /// for `Last-Event-ID`.
    pub fn skip_before(&mut self, seq: u64) {
        while self.next < self.snapshot.len() && self.snapshot[self.next].seq <= seq {
            self.next += 1;
        }
        self.last_seq = self.last_seq.max(seq);
    }

    /// Return the next event: the backlog first, then the live stream.
    pub async fn recv(&mut self) -> Result<UiEvent, SubError> {
        if let Some(event) = self.snapshot.get(self.next) {
            self.next += 1;
            self.last_seq = event.seq;
            return Ok(event.clone());
        }

        loop {
            match self.rx.recv().await {
                // Already returned from the snapshot. Delivering it twice would duplicate
                // output. This part of the replay rule makes the race impossible.
                Ok(event) if event.seq <= self.last_seq => continue,
                Ok(event) => {
                    self.last_seq = event.seq;
                    return Ok(event);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => return Err(SubError::Lagged(n)),
                Err(broadcast::error::RecvError::Closed) => return Err(SubError::Closed),
            }
        }
    }
}

/// Configuration that does not change per turn.
type ToolFactory = Arc<dyn Fn(&SessionId) -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// How a session obtains its tools. The sandbox needs the session id in the container label,
/// so it supplies a factory rather than one fixed list.
#[derive(Clone)]
pub enum ToolSet {
    Fixed(Vec<Arc<dyn Tool>>),
    Factory(ToolFactory),
}

impl ToolSet {
    fn build(&self, id: &SessionId) -> Vec<Arc<dyn Tool>> {
        match self {
            ToolSet::Fixed(tools) => tools.clone(),
            ToolSet::Factory(factory) => factory(id),
        }
    }
}

impl From<Vec<Arc<dyn Tool>>> for ToolSet {
    fn from(tools: Vec<Arc<dyn Tool>>) -> Self {
        ToolSet::Fixed(tools)
    }
}

#[derive(Clone)]
pub struct SessionConfig {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub system: Option<String>,
    pub tools: ToolSet,
    /// Canonical workspace root. Every tool call receives it.
    pub workspace: PathBuf,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// A frontend's handle on a session. Cloning it shares the actor.
#[derive(Clone)]
pub struct SessionHandle {
    id: SessionId,
    tx: mpsc::Sender<Command>,
    events: broadcast::Sender<UiEvent>,
    backlog: Arc<Mutex<VecDeque<UiEvent>>>,
    busy: Arc<AtomicBool>,
}

impl SessionHandle {
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Whether a turn is running. The HTTP layer uses it as a pre-check for `409`. The actor
    /// rejects a racing `Send` regardless.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub async fn send(&self, text: impl Into<String>) -> Result<(), SmithError> {
        self.tx
            .send(Command::Send { text: text.into() })
            .await
            .map_err(|_| self.closed())
    }

    pub async fn cancel(&self) -> Result<(), SmithError> {
        self.tx
            .send(Command::Cancel)
            .await
            .map_err(|_| self.closed())
    }

    /// Ask the running turn to stop at its next provider-request boundary.
    pub async fn pause(&self) -> Result<(), SmithError> {
        self.tx
            .send(Command::Pause)
            .await
            .map_err(|_| self.closed())
    }

    /// A pause request from a thread that is not running a Tokio runtime.
    ///
    /// The terminal watcher uses this. Returns false when the command queue is full or the
    /// actor is gone, so the caller can keep the next key as a pause rather than escalate.
    pub fn pause_now(&self) -> bool {
        self.tx.try_send(Command::Pause).is_ok()
    }

    /// A cancel request from a thread that is not running a Tokio runtime.
    pub fn cancel_now(&self) -> bool {
        self.tx.try_send(Command::Cancel).is_ok()
    }

    /// The transcript at this moment. This is answered mid-turn too.
    pub async fn snapshot(&self) -> Result<SessionSnapshot, SmithError> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Command::Snapshot { reply })
            .await
            .map_err(|_| self.closed())?;
        answer.await.map_err(|_| self.closed())
    }

    /// Subscribe first and snapshot second, in one call, so no caller can choose the wrong
    /// order.
    pub fn subscribe(&self) -> Subscription {
        let rx = self.events.subscribe();
        let snapshot: Vec<UiEvent> = self.backlog.lock().unwrap().iter().cloned().collect();
        Subscription {
            rx,
            snapshot,
            next: 0,
            last_seq: 0,
        }
    }

    fn closed(&self) -> SmithError {
        SmithError::SessionClosed(self.id.to_string())
    }
}

/// A registry of sessions, one per process or per server, built from resolved config.
pub struct SessionManager {
    config: SessionConfig,
    sessions: Mutex<HashMap<SessionId, SessionHandle>>,
}

impl SessionManager {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn a session actor. Requires a Tokio runtime, since it spawns.
    pub fn create(&self) -> SessionHandle {
        let id = SessionId::generate();
        let (tx, rx) = mpsc::channel(COMMAND_CAP);
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let backlog = Arc::new(Mutex::new(VecDeque::new()));
        let busy = Arc::new(AtomicBool::new(false));

        let tools = self.config.tools.build(&id);
        let actor = Actor {
            id: id.clone(),
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            system: self.config.system.clone(),
            tools,
            workspace: self.config.workspace.clone(),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            transcript: Vec::new(),
            seq: 0,
            events: events.clone(),
            backlog: backlog.clone(),
            busy: busy.clone(),
            cancel: CancellationToken::new(),
            pause: PauseToken::default(),
        };
        tokio::spawn(actor.run(rx));

        let handle = SessionHandle {
            id: id.clone(),
            tx,
            events,
            backlog,
            busy,
        };
        self.sessions.lock().unwrap().insert(id, handle.clone());
        handle
    }

    pub fn get(&self, id: &str) -> Option<SessionHandle> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Known sessions, ordered by id so listings are stable.
    pub fn list(&self) -> Vec<SessionHandle> {
        let mut out: Vec<SessionHandle> = self.sessions.lock().unwrap().values().cloned().collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

/// The session's state, owned exclusively by its task. The transcript needs no lock because
/// only this task accesses it.
struct Actor {
    id: SessionId,
    provider: Arc<dyn Provider>,
    model: String,
    system: Option<String>,
    tools: Vec<Arc<dyn Tool>>,
    workspace: PathBuf,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    transcript: Vec<Message>,
    seq: u64,
    events: broadcast::Sender<UiEvent>,
    backlog: Arc<Mutex<VecDeque<UiEvent>>>,
    busy: Arc<AtomicBool>,
    cancel: CancellationToken,
    pause: PauseToken,
}

impl Actor {
    async fn run(mut self, mut cmds: mpsc::Receiver<Command>) {
        while let Some(cmd) = cmds.recv().await {
            match cmd {
                Command::Send { text } => self.turn(text, &mut cmds).await,
                // A `Cancel` or `Pause` with nothing in flight is a no-op, not an error:
                // Ctrl-C at the prompt, or a UI button pressed a moment too late, must be
                // harmless.
                Command::Cancel | Command::Pause => {}
                Command::Snapshot { reply } => {
                    let _ = reply.send(self.snapshot());
                }
            }
        }
    }

    async fn turn(&mut self, prompt: String, cmds: &mut mpsc::Receiver<Command>) {
        self.busy.store(true, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        self.cancel = cancel.clone();
        self.pause.clear();

        for tool in &self.tools {
            tool.set_busy(true);
        }

        self.emit(UiEventKind::TurnStarted {
            prompt: prompt.clone(),
        });

        let mut messages = self.transcript.clone();
        messages.push(Message::user(prompt.clone()));
        let mut request = ChatRequest::new(&self.model, messages, self.id.as_str());
        request.system = self.system.clone();
        request.max_tokens = self.max_tokens;
        request.temperature = self.temperature;

        // The turn runs in its own task so this loop remains responsive to `Command`s.
        // Cancellation therefore takes effect during the turn rather than when it finishes.
        // The event channel is unbounded so the producer never blocks on a slow renderer.
        // The broadcast channel is bounded, and a lagging client receives `SubError::Lagged`
        // instead of stalling the session.
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (done_tx, mut done_rx) = oneshot::channel();
        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let workspace = self.workspace.clone();
        let pause = self.pause.clone();
        tokio::spawn(async move {
            let mut forward = |event: AgentEvent| {
                let _ = ev_tx.send(event);
            };
            let result = run_agent(
                provider.as_ref(),
                &tools,
                &workspace,
                request,
                pause,
                cancel,
                &mut forward,
            )
            .await;
            let _ = done_tx.send(result);
        });

        let mut commands_open = true;
        let mut events_open = true;
        let result = loop {
            tokio::select! {
                cmd = cmds.recv(), if commands_open => match cmd {
                    Some(Command::Cancel) => self.cancel.cancel(),
                    Some(Command::Pause) => {
                        if !self.pause.is_requested() {
                            self.pause.request();
                            self.emit(UiEventKind::PauseRequested);
                        }
                    }
                    Some(Command::Send { text }) => {
                        self.emit(UiEventKind::SendRejected { prompt: text });
                    }
                    Some(Command::Snapshot { reply }) => {
                        let _ = reply.send(self.snapshot());
                    }
                    None => {
                        // Every handle is gone, so nobody is watching this turn.
                        commands_open = false;
                        self.cancel.cancel();
                    }
                },
                event = ev_rx.recv(), if events_open => match event {
                    Some(event) => self.forward(&event),
                    None => events_open = false,
                },
                result = &mut done_rx => break result,
            }
        };

        // The producer queues every event before it signals completion, but the two channels
        // are independent, so anything still queued is taken here to keep
        // `TurnFinished` last.
        while let Ok(event) = ev_rx.try_recv() {
            self.forward(&event);
        }

        match result {
            // The transcript is extended only here. A turn that produced no message does not
            // appear, so the next request cannot contain a partial answer.
            Ok(Ok(AgentOutcome {
                added,
                final_message,
                usage,
                tool_arg_errors,
                paused,
            })) => {
                // A paused turn keeps the steps that completed before the boundary. An
                // empty pause (no request ran) records nothing, including the prompt.
                if paused {
                    if !added.is_empty() {
                        self.transcript.push(Message::user(prompt));
                        self.transcript.extend(added);
                    }
                    self.emit(UiEventKind::TurnPaused {
                        usage,
                        tool_arg_errors,
                    });
                } else {
                    let final_message =
                        final_message.expect("a completed turn has an assistant message");
                    self.transcript.push(Message::user(prompt));
                    self.transcript.extend(added);
                    self.emit(UiEventKind::TurnFinished {
                        message: final_message,
                        usage,
                        tool_arg_errors,
                    });
                }
            }
            Ok(Err(AgentError::Cancelled)) => self.emit(UiEventKind::Cancelled),
            Ok(Err(AgentError::Failed(message))) => self.emit(UiEventKind::TurnFailed { message }),
            // The turn task itself died, which means it panicked.
            Err(_) => self.emit(UiEventKind::TurnFailed {
                message: "the turn ended unexpectedly".to_string(),
            }),
        }

        for tool in &self.tools {
            tool.set_busy(false);
        }
        self.busy.store(false, Ordering::SeqCst);
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id.clone(),
            busy: self.busy.load(Ordering::SeqCst),
            seq: self.seq,
            transcript: self.transcript.clone(),
        }
    }

    /// Forward the events a frontend can render. Tool-call fragments and `finish_reason` are
    /// internal, and the assembled result arrives with `TurnFinished`.
    fn forward(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Provider(ProviderEvent::TextDelta(text)) => {
                self.emit(UiEventKind::TextDelta { text: text.clone() })
            }
            AgentEvent::Provider(ProviderEvent::ReasoningDelta(text)) => {
                self.emit(UiEventKind::ReasoningDelta { text: text.clone() })
            }
            AgentEvent::Provider(ProviderEvent::Usage(usage)) => {
                self.emit(UiEventKind::Usage { usage: *usage })
            }
            AgentEvent::Provider(_) => {}
            AgentEvent::ToolStarted {
                call_id,
                name,
                summary,
            } => self.emit(UiEventKind::ToolStarted {
                call_id: call_id.clone(),
                name: name.clone(),
                summary: summary.clone(),
            }),
            AgentEvent::ToolFinished {
                call_id,
                name,
                ok,
                duration_ms,
                truncated,
                summary,
                display,
            } => self.emit(UiEventKind::ToolFinished {
                call_id: call_id.clone(),
                name: name.clone(),
                ok: *ok,
                duration_ms: *duration_ms,
                truncated: *truncated,
                summary: summary.clone(),
                display: display.clone(),
            }),
        }
    }

    fn emit(&mut self, kind: UiEventKind) {
        self.seq += 1;
        let event = UiEvent {
            seq: self.seq,
            kind,
        };

        // The backlog is updated first, then the broadcast is sent, both while holding the
        // backlog lock. A subscriber that attaches during this sequence receives the event
        // from its snapshot or from the live channel, and the seq check drops a duplicate.
        let mut backlog = self.backlog.lock().unwrap();
        if backlog.len() == BACKLOG_CAP {
            backlog.pop_front();
        }
        backlog.push_back(event.clone());
        let _ = self.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use crate::message::{ContentBlock, StopReason};
    use crate::mock::{MockProvider, MockTool};
    use crate::provider::{ProviderEvent, ToolSpec};
    use crate::tools::{Tool, ToolContext, ToolError};
    use futures_util::future::BoxFuture;
    use serde_json::Value;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn manager(provider: Arc<MockProvider>) -> (Arc<MockProvider>, SessionManager) {
        let mgr = SessionManager::new(SessionConfig {
            provider: provider.clone(),
            model: "mock-1".to_string(),
            system: Some("be brief".to_string()),
            tools: Vec::new().into(),
            workspace: std::env::temp_dir(),
            max_tokens: None,
            temperature: None,
        });
        (provider, mgr)
    }

    fn manager_with_tools(
        provider: Arc<MockProvider>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> SessionManager {
        SessionManager::new(SessionConfig {
            provider,
            model: "mock-1".to_string(),
            system: Some("be brief".to_string()),
            tools: tools.into(),
            workspace: std::env::temp_dir(),
            max_tokens: None,
            temperature: None,
        })
    }

    #[tokio::test]
    async fn a_finished_tool_event_carries_its_display() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".to_string()),
                name: Some("run_shell".to_string()),
                args_delta: "{\"command\":\"echo one\"}".to_string(),
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
        let mock = Arc::new(MockTool::new("run_shell", result));
        let manager = manager_with_tools(provider.clone(), vec![mock as Arc<dyn Tool>]);
        let session = manager.create();
        let mut subscription = session.subscribe();

        session.send("run it").await.unwrap();
        let (seen, terminal) = drain_until_terminal(&mut subscription).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnFinished { .. }));

        let display = seen
            .iter()
            .find_map(|event| match &event.kind {
                UiEventKind::ToolFinished { display, .. } => Some(display.clone()),
                _ => None,
            })
            .expect("a tool_finished event");
        match display {
            ToolDisplay::HeadTail { head, .. } => assert_eq!(head, vec!["one".to_string()]),
            other => panic!("unexpected display: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_turn_runs_tool_calls_and_calls_the_provider_again() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".to_string()),
                name: Some("read_file".to_string()),
                args_delta: "{\"path\":\"a.txt\"}".to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("all done");

        let mock = Arc::new(MockTool::new(
            "read_file",
            "{\"content\":\"hi\",\"truncated\":false}",
        ));
        let manager = manager_with_tools(provider.clone(), vec![mock.clone() as Arc<dyn Tool>]);
        let session = manager.create();
        let mut subscription = session.subscribe();

        session.send("read it").await.unwrap();
        let (seen, terminal) = drain_until_terminal(&mut subscription).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnFinished { .. }));

        let started = seen
            .iter()
            .position(|event| matches!(event.kind, UiEventKind::ToolStarted { .. }))
            .expect("a tool_started event");
        let finished = seen
            .iter()
            .position(|event| matches!(event.kind, UiEventKind::ToolFinished { .. }))
            .expect("a tool_finished event");
        assert!(started < finished);

        assert_eq!(mock.calls(), vec![serde_json::json!({"path": "a.txt"})]);

        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "the provider is called once per step");
        let has_result = requests[1].messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        assert!(has_result, "the second request carries the tool result");

        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.transcript.len(), 4, "user, call, result, answer");
    }

    #[tokio::test]
    async fn a_failing_tool_does_not_fail_the_turn() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            ProviderEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".to_string()),
                name: Some("read_file".to_string()),
                args_delta: "{}".to_string(),
            },
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("recovered");

        let mock = Arc::new(MockTool::failing("read_file", "no such file"));
        let manager = manager_with_tools(provider.clone(), vec![mock.clone() as Arc<dyn Tool>]);
        let session = manager.create();
        let mut subscription = session.subscribe();

        session.send("go").await.unwrap();
        let (_, terminal) = drain_until_terminal(&mut subscription).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnFinished { .. }));
        assert_eq!(provider.requests().len(), 2);
    }

    /// Read events until the turn ends, returning everything read and the terminal event.
    async fn drain_until_terminal(sub: &mut Subscription) -> (Vec<UiEvent>, UiEvent) {
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(TIMEOUT, sub.recv())
                .await
                .expect("timed out waiting for a terminal event")
                .expect("subscription failed");
            let terminal = matches!(
                event.kind,
                UiEventKind::TurnFinished { .. }
                    | UiEventKind::TurnPaused { .. }
                    | UiEventKind::TurnFailed { .. }
                    | UiEventKind::Cancelled
            );
            seen.push(event.clone());
            if terminal {
                return (seen, event);
            }
        }
    }

    /// Return the terminal event, discarding earlier events.
    async fn until_terminal(sub: &mut Subscription) -> UiEvent {
        drain_until_terminal(sub).await.1
    }

    fn text_of(event: &UiEvent) -> Option<&str> {
        match &event.kind {
            UiEventKind::TextDelta { text } => Some(text),
            _ => None,
        }
    }

    #[tokio::test]
    async fn a_turn_streams_deltas_then_finishes_with_the_assembled_message() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("Hello, world");
        let (_provider, mgr) = manager(provider);

        let session = mgr.create();
        let mut sub = session.subscribe();
        session.send("hi").await.unwrap();

        let mut text = String::new();
        let terminal = loop {
            let event = tokio::time::timeout(TIMEOUT, sub.recv())
                .await
                .unwrap()
                .unwrap();
            if let Some(delta) = text_of(&event) {
                text.push_str(delta);
            }
            if matches!(
                event.kind,
                UiEventKind::TurnFinished { .. }
                    | UiEventKind::TurnPaused { .. }
                    | UiEventKind::TurnFailed { .. }
                    | UiEventKind::Cancelled
            ) {
                break event;
            }
        };

        match terminal.kind {
            UiEventKind::TurnFinished { message, .. } => {
                assert_eq!(message.text(), "Hello, world");
                assert_eq!(text, message.text(), "deltas must add up to the message");
            }
            other => panic!("expected a finished turn, got {other:?}"),
        }
        assert!(!session.is_busy(), "busy must clear when the turn ends");
    }

    /// The transcript gains exactly one assistant message per turn, and the provider sees the
    /// assembled message rather than the deltas that produced it.
    #[tokio::test]
    async fn deltas_never_reach_the_transcript() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("one");
        provider.push_text("two");
        let (provider, mgr) = manager(provider);

        let session = mgr.create();
        let mut sub = session.subscribe();

        session.send("first").await.unwrap();
        until_terminal(&mut sub).await;
        session.send("second").await.unwrap();
        until_terminal(&mut sub).await;

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);

        // Second request: user "first", assistant "one", user "second".
        let messages = &requests[1].messages;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].text(), "first");
        assert_eq!(messages[1].role, crate::message::Role::Assistant);
        assert_eq!(messages[1].text(), "one");
        assert_eq!(messages[2].text(), "second");
        assert_eq!(requests[1].system.as_deref(), Some("be brief"));
        assert_eq!(requests[1].session_id, session.id().as_str());
    }

    #[tokio::test]
    async fn cancel_drops_the_in_flight_turn_and_leaves_the_transcript_alone() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_stalling_text("half an ans");
        provider.push_text("recovered");
        let (provider, mgr) = manager(provider);

        let session = mgr.create();
        let mut sub = session.subscribe();

        session.send("doomed").await.unwrap();
        // Wait until the turn is in flight before cancelling.
        loop {
            if let Some(text) = text_of(&sub.recv().await.unwrap()) {
                assert_eq!(text, "half an ans");
                break;
            }
        }
        session.cancel().await.unwrap();
        let terminal = until_terminal(&mut sub).await;
        assert_eq!(terminal.kind, UiEventKind::Cancelled);

        // The cancelled turn is not recorded. The next request contains only the follow-up
        // prompt.
        session.send("again").await.unwrap();
        until_terminal(&mut sub).await;

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].messages.len(), 1);
        assert_eq!(requests[1].messages[0].text(), "again");
        assert!(!session.is_busy());
    }

    #[tokio::test]
    async fn send_while_busy_is_rejected_and_leaves_the_session_usable() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_stalling_text("working");
        provider.push_text("done");
        let (provider, mgr) = manager(provider);

        let session = mgr.create();
        let mut sub = session.subscribe();

        session.send("first").await.unwrap();
        loop {
            if text_of(&sub.recv().await.unwrap()).is_some() {
                break;
            }
        }
        session.send("second").await.unwrap();

        // The rejection is reported, and no second turn starts.
        let rejected = loop {
            let event = tokio::time::timeout(TIMEOUT, sub.recv())
                .await
                .unwrap()
                .unwrap();
            if let UiEventKind::SendRejected { prompt } = &event.kind {
                break prompt.clone();
            }
        };
        assert_eq!(rejected, "second");

        session.cancel().await.unwrap();
        assert_eq!(until_terminal(&mut sub).await.kind, UiEventKind::Cancelled);

        session.send("third").await.unwrap();
        let terminal = until_terminal(&mut sub).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnFinished { .. }));

        // Only the accepted turns reached the provider.
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].messages[0].text(), "third");
    }

    #[tokio::test]
    async fn failed_turns_are_reported_and_do_not_enter_the_transcript() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_error(ProviderError::Transport {
            provider: "mock".to_string(),
            message: "connection reset".to_string(),
        });
        provider.push_text("fine");
        let (provider, mgr) = manager(provider);

        let session = mgr.create();
        let mut sub = session.subscribe();

        session.send("doomed").await.unwrap();
        match until_terminal(&mut sub).await.kind {
            UiEventKind::TurnFailed { message } => assert!(message.contains("connection reset")),
            other => panic!("expected a failed turn, got {other:?}"),
        }

        session.send("ok").await.unwrap();
        assert!(matches!(
            until_terminal(&mut sub).await.kind,
            UiEventKind::TurnFinished { .. }
        ));

        let requests = provider.requests();
        assert_eq!(requests[1].messages.len(), 1);
        assert_eq!(requests[1].messages[0].text(), "ok");
    }

    #[tokio::test]
    async fn sequence_numbers_are_monotonic_and_shared_by_all_subscribers() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("hi");
        let (_provider, mgr) = manager(provider);

        let session = mgr.create();
        let mut first = session.subscribe();
        let mut second = session.subscribe();
        session.send("hello").await.unwrap();

        let a = until_terminal(&mut first).await;
        let b = until_terminal(&mut second).await;
        assert!(a.seq > 1, "seq starts at 1 and counts every event");
        assert_eq!(a.seq, b.seq, "seq is session-scoped, not per-subscriber");
        assert_eq!(a.kind, b.kind);
    }

    /// A subscriber created while events are emitted sees every event exactly once, in order.
    /// The backlog and live channel together cover `1..=seq` with no gap and no repeat.
    #[tokio::test]
    async fn subscribing_while_events_are_emitted_loses_nothing_and_duplicates_nothing() {
        for _ in 0..50 {
            let provider = Arc::new(MockProvider::new("mock"));
            // Enough deltas that the subscription starts in the middle of the turn.
            provider.push_chunks((0..40).map(|i| format!("{i},")));
            let (_provider, mgr) = manager(provider);
            let session = mgr.create();

            session.send("go").await.unwrap();
            let mut sub = session.subscribe();
            // Ensure the subscription starts mid-turn rather than before it, or the property
            // holds vacuously.
            while sub.backlog().is_empty() {
                tokio::task::yield_now().await;
                sub = session.subscribe();
            }

            let (events, terminal) = drain_until_terminal(&mut sub).await;
            let seen: Vec<u64> = events.iter().map(|e| e.seq).collect();

            assert_eq!(
                seen,
                (1..=terminal.seq).collect::<Vec<_>>(),
                "gap or duplicate in the backlog and live stream together"
            );
        }
    }

    /// An event already replayed from the backlog must not be delivered again by the live
    /// channel.
    #[tokio::test]
    async fn live_events_already_replayed_are_dropped() {
        let (events, rx) = broadcast::channel(EVENT_CHANNEL_CAP);
        let replayed = UiEvent {
            seq: 7,
            kind: UiEventKind::TextDelta {
                text: "already seen".to_string(),
            },
        };

        let _ = events.send(replayed.clone());
        let _ = events.send(UiEvent {
            seq: 8,
            kind: UiEventKind::TextDelta {
                text: "new".to_string(),
            },
        });

        let mut sub = Subscription {
            rx,
            snapshot: vec![replayed],
            next: 1,
            last_seq: 7,
        };

        let next = sub.recv().await.unwrap();
        assert_eq!(next.seq, 8);
        assert_eq!(text_of(&next), Some("new"));
    }

    #[tokio::test]
    async fn a_slow_subscriber_is_told_it_lagged_rather_than_losing_output_silently() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_chunks((0..EVENT_CHANNEL_CAP + 50).map(|i| format!("{i},")));
        let (_provider, mgr) = manager(provider);

        let session = mgr.create();
        // This one never reads while the turn runs, so it must fall behind the channel.
        let mut idle = session.subscribe();

        session.send("flood").await.unwrap();

        // Wait on the session rather than on a second subscriber. A reader that keeps up can
        // still miss events during the flood, so its own lag is not reliable. The backlog is
        // append-only and readable without an await, which makes this deterministic.
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            let finished = matches!(
                session.subscribe().backlog().last().map(|e| &e.kind),
                Some(UiEventKind::TurnFinished { .. })
            );
            if finished {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the flooded turn never finished"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        match idle.recv().await {
            Err(SubError::Lagged(n)) => assert!(n > 0),
            other => panic!("expected a lag report, got {other:?}"),
        }

        // The session is unaffected. The backlog retains the most recent events, and the
        // transcript is complete. Lag affects only the client.
        let after = session.subscribe();
        assert!(after.backlog().len() > EVENT_CHANNEL_CAP);
        assert!(matches!(
            until_terminal(&mut session.subscribe()).await.kind,
            UiEventKind::TurnFinished { .. }
        ));
    }

    /// A tool whose call blocks until the test releases it. Pausing while a tool runs is the
    /// case the pause boundary has to handle.
    struct BlockingTool {
        name: String,
        calls: Mutex<Vec<Value>>,
        started: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }

    impl BlockingTool {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                calls: Mutex::new(Vec::new()),
                started: Arc::new(Semaphore::new(0)),
                release: Arc::new(Semaphore::new(0)),
            }
        }

        fn calls(&self) -> Vec<Value> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Tool for BlockingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: "a tool that blocks until released".to_string(),
                schema: serde_json::json!({"type": "object"}),
            }
        }

        fn call<'a>(
            &'a self,
            args: Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<String, ToolError>> {
            self.calls.lock().unwrap().push(args);
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

    fn tool_call_event(id: &str, name: &str, args: &str) -> ProviderEvent {
        ProviderEvent::ToolCallDelta {
            index: 0,
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            args_delta: args.to_string(),
        }
    }

    #[tokio::test]
    async fn a_pause_after_a_tool_batch_records_the_steps_and_stops() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            tool_call_event("call_1", "blocking", "{}"),
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("after steering");
        let tool = Arc::new(BlockingTool::new("blocking"));
        let manager = manager_with_tools(provider.clone(), vec![tool.clone() as Arc<dyn Tool>]);
        let session = manager.create();
        let mut sub = session.subscribe();

        session.send("go").await.unwrap();
        tool.started.acquire().await.unwrap().forget();
        session.pause().await.unwrap();
        tool.release.add_permits(1);

        let (seen, terminal) = drain_until_terminal(&mut sub).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnPaused { .. }));
        assert!(
            seen.iter()
                .any(|event| matches!(event.kind, UiEventKind::PauseRequested)),
            "the frontend must learn that a pause was accepted"
        );
        assert_eq!(
            provider.requests().len(),
            1,
            "the next provider request is not sent after a pause"
        );

        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(
            snapshot.transcript.len(),
            3,
            "user, assistant call and tool result"
        );
        assert_eq!(tool.calls().len(), 1);

        session.send("steer").await.unwrap();
        let terminal = until_terminal(&mut sub).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnFinished { .. }));

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].messages.len(),
            4,
            "the paused steps plus the steering message"
        );
        assert!(
            requests[1].messages.iter().any(|message| message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))),
            "the steering request carries the recorded tool result"
        );
        assert_eq!(requests[1].messages.last().unwrap().text(), "steer");
    }

    #[tokio::test]
    async fn repeated_pause_commands_emit_one_pause_requested() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            tool_call_event("call_1", "blocking", "{}"),
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("done");
        let tool = Arc::new(BlockingTool::new("blocking"));
        let manager = manager_with_tools(provider, vec![tool.clone() as Arc<dyn Tool>]);
        let session = manager.create();
        let mut sub = session.subscribe();

        session.send("go").await.unwrap();
        tool.started.acquire().await.unwrap().forget();
        session.pause().await.unwrap();
        session.pause().await.unwrap();
        tool.release.add_permits(1);

        let (seen, terminal) = drain_until_terminal(&mut sub).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnPaused { .. }));
        let count = seen
            .iter()
            .filter(|event| matches!(event.kind, UiEventKind::PauseRequested))
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn a_cancel_while_a_pause_is_pending_discards_the_turn() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_events(vec![
            tool_call_event("call_1", "blocking", "{}"),
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        let tool = Arc::new(BlockingTool::new("blocking"));
        let manager = manager_with_tools(provider.clone(), vec![tool.clone() as Arc<dyn Tool>]);
        let session = manager.create();
        let mut sub = session.subscribe();

        session.send("go").await.unwrap();
        tool.started.acquire().await.unwrap().forget();
        session.pause().await.unwrap();
        session.cancel().await.unwrap();
        tool.release.add_permits(1);

        let terminal = until_terminal(&mut sub).await;
        assert!(matches!(terminal.kind, UiEventKind::Cancelled));
        let snapshot = session.snapshot().await.unwrap();
        assert!(
            snapshot.transcript.is_empty(),
            "a cancelled turn is not recorded, even after a pause request"
        );
    }

    #[tokio::test]
    async fn repeated_tool_prompt_fragments_are_appended_once() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("ok");
        let first =
            Arc::new(MockTool::new("first", "{}").with_prepare("The sandbox provides: bash."));
        let second =
            Arc::new(MockTool::new("second", "{}").with_prepare("The sandbox provides: bash."));
        let manager = manager_with_tools(provider.clone(), vec![first, second]);
        let session = manager.create();
        let mut sub = session.subscribe();

        session.send("go").await.unwrap();
        let terminal = until_terminal(&mut sub).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnFinished { .. }));

        let requests = provider.requests();
        let system = requests[0].system.as_deref().expect("a system message");
        assert_eq!(
            system.matches("The sandbox provides: bash.").count(),
            1,
            "the shared inventory fragment appears once: {system}"
        );
        assert!(
            system.starts_with("be brief\n\nThe sandbox provides"),
            "the fragment is its own section: {system}"
        );
    }

    #[tokio::test]
    async fn pause_when_idle_does_not_disturb_the_next_turn() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("ok");
        let session = manager(provider).1.create();

        session.pause().await.unwrap();
        session.send("go").await.unwrap();
        let mut sub = session.subscribe();
        let terminal = until_terminal(&mut sub).await;
        assert!(matches!(terminal.kind, UiEventKind::TurnFinished { .. }));
        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.transcript.len(), 2);
    }

    #[tokio::test]
    async fn the_manager_finds_and_lists_sessions() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("hi");
        let (_provider, mgr) = manager(provider);

        let session = mgr.create();
        let id = session.id().clone();
        assert!(mgr.get(id.as_str()).is_some());
        assert_eq!(mgr.list().len(), 1);
        assert!(mgr.get("nope").is_none());
    }

    #[tokio::test]
    async fn a_snapshot_reports_the_transcript_and_whether_a_turn_is_running() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_stalling_text("working");
        provider.push_text("done");
        let (_provider, mgr) = manager(provider);

        let session = mgr.create();
        let empty = session.snapshot().await.unwrap();
        assert_eq!(&empty.id, session.id());
        assert!(!empty.busy);
        assert!(empty.transcript.is_empty());
        assert_eq!(empty.seq, 0, "nothing has been emitted yet");

        let mut sub = session.subscribe();
        session.send("first").await.unwrap();
        loop {
            if text_of(&sub.recv().await.unwrap()).is_some() {
                break;
            }
        }

        // Answered mid-turn. A reconnecting frontend must be able to ask while a turn is
        // running, and must not see the unfinished turn as a message.
        let during = session.snapshot().await.unwrap();
        assert!(during.busy);
        assert!(during.transcript.is_empty());
        // A frontend takes this value before sending to identify the turn it starts.
        assert!(during.seq > 0, "the running turn has emitted by now");
        assert!(empty.seq < during.seq, "seq only ever moves forward");

        session.cancel().await.unwrap();
        assert_eq!(until_terminal(&mut sub).await.kind, UiEventKind::Cancelled);

        session.send("second").await.unwrap();
        until_terminal(&mut sub).await;

        let after = session.snapshot().await.unwrap();
        assert!(!after.busy);
        assert_eq!(after.transcript.len(), 2);
        assert_eq!(after.transcript[0].text(), "second");
        assert_eq!(after.transcript[1].text(), "done");
    }

    /// The SSE `event:` name and the JSON `type` tag encode one discriminant. This test fails
    /// if a variant is added to one and not the other.
    #[test]
    fn event_names_match_the_serialised_tag() {
        let kinds = vec![
            UiEventKind::TurnStarted {
                prompt: "hi".to_string(),
            },
            UiEventKind::TextDelta {
                text: "hi".to_string(),
            },
            UiEventKind::ReasoningDelta {
                text: "hmm".to_string(),
            },
            UiEventKind::Usage {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                },
            },
            UiEventKind::TurnFinished {
                message: Message::assistant("done"),
                usage: Usage::default(),
                tool_arg_errors: vec![ToolArgError {
                    tool: "read_file".to_string(),
                    raw: "{".to_string(),
                    message: "eof".to_string(),
                }],
            },
            UiEventKind::TurnFailed {
                message: "nope".to_string(),
            },
            UiEventKind::Cancelled,
            UiEventKind::SendRejected {
                prompt: "hi".to_string(),
            },
        ];

        for kind in kinds {
            let json = serde_json::to_value(&kind).expect("every event must serialise");
            assert_eq!(
                json["type"],
                kind.name(),
                "{} serialised as {json}",
                kind.name()
            );
        }
    }

    #[tokio::test]
    async fn sessions_are_listed_independently() {
        let provider = Arc::new(MockProvider::new("mock"));
        provider.push_text("hi");
        let (_provider, mgr) = manager(provider);

        let a = mgr.create();
        let b = mgr.create();
        assert_ne!(a.id(), b.id());
        assert_eq!(mgr.list().len(), 2);

        // One session's turn must not move the other's seq.
        let mut sub_a = a.subscribe();
        a.send("to a").await.unwrap();
        let event = until_terminal(&mut sub_a).await;
        assert_eq!(event.seq, 3, "TurnStarted, TextDelta, TurnFinished");
        assert!(!b.is_busy());
    }
}
