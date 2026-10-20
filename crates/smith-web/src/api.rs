//! The JSON API.
//!
//! Every handler resolves a session and forwards a `Command` or reads a snapshot. Agent logic
//! belongs in `smith-core`. A second implementation here would duplicate it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use smith_core::session::SessionHandle;

use crate::AppState;

pub async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "sessions": state.sessions().list().len(),
    }))
}

pub async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let sessions: Vec<Value> = state
        .sessions()
        .list()
        .into_iter()
        .map(|session| {
            json!({
                "id": session.id().to_string(),
                "busy": session.is_busy(),
            })
        })
        .collect();
    Json(json!({ "sessions": sessions }))
}

pub async fn create_session(State(state): State<AppState>) -> Response {
    let session = state.sessions().create();
    (
        StatusCode::CREATED,
        Json(json!({ "id": session.id().to_string() })),
    )
        .into_response()
}

pub async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(session) = state.sessions().get(&id) else {
        return unknown_session(&id);
    };

    match session.snapshot().await {
        Ok(snapshot) => Json(json!({
            "id": snapshot.id.to_string(),
            "busy": snapshot.busy,
            // The transcript contains messages, never deltas.
            "transcript": snapshot.transcript,
        }))
        .into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
pub struct SendMessage {
    text: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SendMessage>,
) -> Response {
    let Some(session) = state.sessions().get(&id) else {
        return unknown_session(&id);
    };

    // This check gives the caller a 409 instead of a `send_rejected` event. The session
    // rejects a racing send because two frontends can pass this check at the same time.
    if session.is_busy() {
        return conflict();
    }
    if body.text.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "text must not be empty");
    }

    match session.send(body.text).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))).into_response(),
        Err(e) => server_error(e),
    }
}

pub async fn pause_turn(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(session) = state.sessions().get(&id) else {
        return unknown_session(&id);
    };

    // Pausing when nothing runs is explicitly not an error; the button is disabled, but a
    // racing request or a second frontend may still send it.
    match session.pause().await {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({ "status": if session.is_busy() { "pausing" } else { "idle" } })),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

pub async fn cancel_turn(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(session) = state.sessions().get(&id) else {
        return unknown_session(&id);
    };

    // Cancelling when nothing runs is explicitly not an error.
    match session.cancel().await {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({ "status": if session.is_busy() { "cancelling" } else { "idle" } })),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

fn unknown_session(id: &str) -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        format!("no session '{id}' in this process"),
    )
}

fn conflict() -> Response {
    json_error(
        StatusCode::CONFLICT,
        "a turn is already running in this session",
    )
}

fn server_error(error: smith_core::SmithError) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub(crate) fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Used by the SSE handler, which needs the session handle in the same form.
///
/// The error type is `Response`, which triggers `clippy::result_large_err`. Boxing it would
/// not help: this runs once per request, not in a loop, and boxing would move the unwrapping
/// to every call site.
#[allow(clippy::result_large_err)]
pub(crate) fn session_or_404(state: &AppState, id: &str) -> Result<SessionHandle, Response> {
    state.sessions().get(id).ok_or_else(|| unknown_session(id))
}
