//! Exposure control.
//!
//! Binding to loopback keeps other machines out. It does not stop a request that a browser on
//! this machine was tricked into making. The two checks below handle that case:
//!
//! * [`require_local_host`]: DNS rebinding. A page on `evil.com` can re-resolve its hostname
//!   to `127.0.0.1`, and the browser then sends the request to this server as a same-origin
//!   request, so CORS does not apply. Checking the `Host` header against the addresses the
//!   listener accepts catches it.
//! * [`require_token`]: a bearer token on every `/api` route and an `Origin` check on
//!   mutating requests. Authentication uses a header only. A cookie would allow CSRF, and a
//!   token in a query string would reach logs.

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

/// Reject a request whose `Host` is not one the listener accepts.
pub async fn require_local_host(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());

    match host {
        Some(host) if state.accepts_host(host) => next.run(request).await,
        Some(host) => (
            StatusCode::MISDIRECTED_REQUEST,
            format!("unexpected Host header: {host}\n"),
        )
            .into_response(),
        None => (StatusCode::MISDIRECTED_REQUEST, "missing Host header\n").into_response(),
    }
}

/// Require `Authorization: Bearer <token>`, and reject cross-origin mutations.
pub async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !state.token_matches(request.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token\n",
        )
            .into_response();
    }

    // Defence in depth: the Host check above blocks the rebinding case, and there is no CORS
    // layer, so this only catches a browser that was directed to send a mutation from
    // another origin.
    if is_mutating(request.method()) {
        if let Some(origin) = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
        {
            if !state.accepts_origin(origin) {
                return (
                    StatusCode::FORBIDDEN,
                    format!("cross-origin request rejected: {origin}\n"),
                )
                    .into_response();
            }
        }
    }

    next.run(request).await
}

fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}
