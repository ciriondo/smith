//! The event stream.
//!
//! The server uses Server-Sent Events rather than WebSocket: the protocol needs one-way
//! streaming and a few POST requests, and SSE provides a monotonic event id and built-in
//! resumption.
//!
//! Resumption is the part with the most detail. [`Subscription`] returns the backlog first
//! and the live stream after it, dropping events already returned, so a client sees no gap
//! and no duplicate regardless of where it attaches. The remaining failure is a client that
//! falls far enough behind for the broadcast channel to drop events. The server reports that
//! as a `resync` instead of skipping events silently, because a client cannot distinguish a
//! dropped delta from a short answer.

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde::Deserialize;
use serde_json::json;

use smith_core::session::{SubError, UiEvent};

use crate::api;
use crate::AppState;

/// The sequence number a client starts after. `?since=41` and `Last-Event-ID: 41` are
/// equivalent.
#[derive(Debug, Deserialize)]
pub struct Since {
    since: Option<u64>,
}

pub async fn events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<Since>,
    headers: HeaderMap,
) -> Response {
    let session = match api::session_or_404(&state, &id) {
        Ok(session) => session,
        Err(response) => return response,
    };

    let since = query
        .since
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
        })
        .unwrap_or(0);

    let mut subscription = session.subscribe();
    subscription.skip_before(since);

    let stream = stream::unfold(subscription, |mut subscription| async move {
        match subscription.recv().await {
            Ok(event) => Some((Ok::<Event, Infallible>(frame(&event)), subscription)),
            Err(SubError::Lagged(skipped)) => Some((Ok(resync(skipped)), subscription)),
            // The session is gone, so end the stream.
            Err(SubError::Closed) => None,
        }
    });

    // A comment every 15 seconds keeps idle streams open through intermediaries. This
    // covers a turn that takes time to produce its first token.
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn frame(event: &UiEvent) -> Event {
    Event::default()
        .id(event.seq.to_string())
        .event(event.kind.name())
        .data(data(event))
}

/// Tell a lagged client to re-fetch the transcript and resume from there. The event has no
/// `id:` field, so a reconnect does not treat it as progress.
fn resync(skipped: u64) -> Event {
    Event::default()
        .event("resync")
        .data(json!({ "skipped": skipped }).to_string())
}

fn data(event: &UiEvent) -> String {
    match serde_json::to_string(&event.kind) {
        Ok(json) => json,
        // Unreachable for these types. Reporting the serialisation error keeps the stream
        // open.
        Err(e) => {
            json!({ "type": event.kind.name(), "serialisation_error": e.to_string() }).to_string()
        }
    }
}
