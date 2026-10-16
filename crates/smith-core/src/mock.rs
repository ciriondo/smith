//! A scripted provider for tests and for exercising frontends without a network.
//!
//! The session layer and the agent loop need deterministic turns, including turns that
//! stall until cancelled and turns that fail. No provider network is involved, so every
//! test that uses it is offline.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::message::StopReason;
use crate::provider::{ChatRequest, ModelInfo, Provider, ProviderEvent, ProviderStream};

/// One scripted turn.
enum MockTurn {
    /// Yield these events, then end the stream.
    Events(Vec<ProviderEvent>),
    /// Yield the prefix, then block until cancelled, as a provider that is still generating
    /// does.
    Stall { prefix: Vec<ProviderEvent> },
    /// Fail with this error.
    Fail(ProviderError),
}

/// A [`Provider`] that replays a script instead of performing network I/O.
pub struct MockProvider {
    id: String,
    turns: Mutex<VecDeque<MockTurn>>,
    requests: Mutex<Vec<ChatRequest>>,
    /// Reply used when the script runs dry, so a test cannot hang by forgetting a turn.
    fallback: String,
}

impl MockProvider {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            fallback: "ok".to_string(),
        }
    }

    /// Queue one turn with this text as a single delta, followed by a normal stop.
    pub fn push_text(&self, text: &str) {
        self.push_events(vec![
            ProviderEvent::TextDelta(text.to_string()),
            ProviderEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ]);
    }

    /// Queue one turn whose text arrives split, as a real stream delivers it.
    pub fn push_chunks<I, S>(&self, chunks: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut events: Vec<ProviderEvent> = chunks
            .into_iter()
            .map(|c| ProviderEvent::TextDelta(c.into()))
            .collect();
        events.push(ProviderEvent::Done {
            stop_reason: StopReason::EndTurn,
        });
        self.push_events(events);
    }

    /// Queue a turn that emits `prefix` and then blocks until it is cancelled.
    pub fn push_stalling_text(&self, prefix: &str) {
        self.turns.lock().unwrap().push_back(MockTurn::Stall {
            prefix: vec![ProviderEvent::TextDelta(prefix.to_string())],
        });
    }

    pub fn push_error(&self, error: ProviderError) {
        self.turns.lock().unwrap().push_back(MockTurn::Fail(error));
    }

    pub fn push_events(&self, events: Vec<ProviderEvent>) {
        self.turns
            .lock()
            .unwrap()
            .push_back(MockTurn::Events(events));
    }

    /// Every request passed to [`Provider::stream`], in order. Tests use it to assert the
    /// transcript visible to the model.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.id.clone(),
            label: self.id.clone(),
            context_window: None,
        }]
    }

    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        self.requests.lock().unwrap().push(req);

        let turn = self.turns.lock().unwrap().pop_front().unwrap_or_else(|| {
            MockTurn::Events(vec![
                ProviderEvent::TextDelta(self.fallback.clone()),
                ProviderEvent::Done {
                    stop_reason: StopReason::EndTurn,
                },
            ])
        });

        match turn {
            MockTurn::Events(events) => {
                Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
            }
            MockTurn::Fail(error) => Box::pin(futures_util::stream::iter(vec![Err(error)])),
            MockTurn::Stall { prefix } => Box::pin(futures_util::stream::unfold(
                (prefix.into_iter(), false),
                move |(mut prefix, cancelled)| {
                    let cancel = cancel.clone();
                    async move {
                        if !cancelled {
                            if let Some(event) = prefix.next() {
                                return Some((Ok(event), (prefix, false)));
                            }
                            cancel.cancelled().await;
                            return Some((Err(ProviderError::Cancelled), (prefix, true)));
                        }
                        None
                    }
                },
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn scripted_turns_are_replayed_in_order_and_requests_are_recorded() {
        let provider = MockProvider::new("mock");
        provider.push_text("first");
        provider.push_text("second");

        for expected in ["first", "second"] {
            let stream = provider.stream(
                ChatRequest::new("mock-1", Vec::new(), "session"),
                CancellationToken::new(),
            );
            let events: Vec<_> = stream.collect().await;
            let text: String = events
                .into_iter()
                .filter_map(|e| match e.unwrap() {
                    ProviderEvent::TextDelta(t) => Some(t),
                    _ => None,
                })
                .collect();
            assert_eq!(text, expected);
        }

        assert_eq!(provider.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_stalling_turn_ends_only_when_cancelled() {
        let provider = MockProvider::new("mock");
        provider.push_stalling_text("partial");
        let cancel = CancellationToken::new();

        let stream = provider.stream(
            ChatRequest::new("mock-1", Vec::new(), "session"),
            cancel.clone(),
        );

        // The prefix is available before cancellation.
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), {
            let mut stream = stream;
            async move {
                let first = stream.next().await;
                cancel.cancel();
                let second = stream.next().await;
                (first, second)
            }
        })
        .await
        .expect("the mock must not hang before it is cancelled");

        assert!(matches!(first.0, Some(Ok(ProviderEvent::TextDelta(_)))));
        assert!(matches!(first.1, Some(Err(ProviderError::Cancelled))));
    }
}
