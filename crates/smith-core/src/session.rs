//! Sessions: the transcript, the provider and the cancel token, owned by one actor task.
//!
//! Frontends do not hold conversation state. They send [`Command`]s and render [`UiEvent`]s,
//! so the REPL, the headless one-shot mode and the web server are renderers over one
//! implementation.
//!
//! Three rules apply here:
//!
//! * **Deltas are never persisted.** The transcript gains the user message and the assistant
//!   message exactly once, when a turn finishes. A failed or cancelled turn leaves the
//!   transcript untouched, so the model never sees a partial answer.
//! * **`Send` while a turn is running is rejected**, not queued.
//! * **[`SessionHandle::subscribe`] subscribes before it snapshots**, and
//!   [`Subscription::recv`] drops events that were already replayed. The opposite order
//!   loses every event that falls between the two steps.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{run_turn, ToolArgError};
use crate::error::{ProviderError, SmithError};
use crate::message::{Message, Usage};
use crate::provider::{ChatRequest, Provider, ProviderEvent, ToolSpec};

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
    /// Cancel the running turn, whichever frontend asked for it.
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
    TurnFailed {
        message: String,
    },
    Cancelled,
    /// A `Send` arrived while a turn was running. Two frontends can race the busy check, so
    /// the actor reports the rejection instead of assuming the frontend knows.
    SendRejected {
        prompt: String,
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
            UiEventKind::TurnFailed { .. } => "turn_failed",
            UiEventKind::Cancelled => "cancelled",
            UiEventKind::SendRejected { .. } => "send_rejected",
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
#[derive(Clone)]
pub struct SessionConfig {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
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

        let actor = Actor {
            id: id.clone(),
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            system: self.config.system.clone(),
            tools: self.config.tools.clone(),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            transcript: Vec::new(),
            seq: 0,
            events: events.clone(),
            backlog: backlog.clone(),
            busy: busy.clone(),
            cancel: CancellationToken::new(),
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
    tools: Vec<ToolSpec>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    transcript: Vec<Message>,
    seq: u64,
    events: broadcast::Sender<UiEvent>,
    backlog: Arc<Mutex<VecDeque<UiEvent>>>,
    busy: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl Actor {
    async fn run(mut self, mut cmds: mpsc::Receiver<Command>) {
        while let Some(cmd) = cmds.recv().await {
            match cmd {
                Command::Send { text } => self.turn(text, &mut cmds).await,
                // A `Cancel` with nothing in flight is a no-op, not an error: Ctrl-C at the
                // prompt, or a UI button pressed a moment too late, must be harmless.
                Command::Cancel => {}
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

        self.emit(UiEventKind::TurnStarted {
            prompt: prompt.clone(),
        });

        let mut messages = self.transcript.clone();
        messages.push(Message::user(prompt.clone()));
        let mut request = ChatRequest::new(&self.model, messages, self.id.as_str());
        request.system = self.system.clone();
        request.tools = self.tools.clone();
        request.max_tokens = self.max_tokens;
        request.temperature = self.temperature;

        // The turn runs in its own task so this loop remains responsive to `Command`s.
        // Cancellation therefore takes effect during the turn rather than when it finishes.
        // The event channel is unbounded so the producer never blocks on a slow renderer.
        // The broadcast channel is bounded, and a lagging client receives `SubError::Lagged`
        // instead of stalling the session.
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<ProviderEvent>();
        let (done_tx, mut done_rx) = oneshot::channel();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let mut forward = |event: &ProviderEvent| {
                let _ = ev_tx.send(event.clone());
            };
            let result = run_turn(provider.as_ref(), request, cancel, &mut forward).await;
            let _ = done_tx.send(result);
        });

        let mut commands_open = true;
        let mut events_open = true;
        let result = loop {
            tokio::select! {
                cmd = cmds.recv(), if commands_open => match cmd {
                    Some(Command::Cancel) => self.cancel.cancel(),
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
            Ok(Ok(turn)) => {
                self.transcript.push(Message::user(prompt));
                self.transcript.push(turn.message.clone());
                self.emit(UiEventKind::TurnFinished {
                    message: turn.message,
                    usage: turn.usage,
                    tool_arg_errors: turn.tool_arg_errors,
                });
            }
            Ok(Err(ProviderError::Cancelled)) => self.emit(UiEventKind::Cancelled),
            Ok(Err(e)) => self.emit(UiEventKind::TurnFailed {
                message: e.to_string(),
            }),
            // The turn task itself died, which means it panicked.
            Err(_) => self.emit(UiEventKind::TurnFailed {
                message: "the turn ended unexpectedly".to_string(),
            }),
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
    fn forward(&mut self, event: &ProviderEvent) {
        match event {
            ProviderEvent::TextDelta(text) => {
                self.emit(UiEventKind::TextDelta { text: text.clone() })
            }
            ProviderEvent::ReasoningDelta(text) => {
                self.emit(UiEventKind::ReasoningDelta { text: text.clone() })
            }
            ProviderEvent::Usage(usage) => self.emit(UiEventKind::Usage { usage: *usage }),
            ProviderEvent::ToolCallDelta { .. } | ProviderEvent::Done { .. } => {}
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
    use crate::mock::MockProvider;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn manager(provider: Arc<MockProvider>) -> (Arc<MockProvider>, SessionManager) {
        let mgr = SessionManager::new(SessionConfig {
            provider: provider.clone(),
            model: "mock-1".to_string(),
            system: Some("be brief".to_string()),
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
        });
        (provider, mgr)
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
