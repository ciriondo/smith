//! smith's localhost web server: the third frontend over the same session layer.
//!
//! Session state, providers and the agent loop are in `smith-core`. This crate converts
//! [`smith_core::session::UiEvent`]s into SSE frames, converts [`Command`]s into HTTP
//! requests and contains no agent logic.
//!
//! This crate also controls exposure in two layers: the bind address and the request checks
//! in [`auth`]. The first layer is not sufficient on its own. [`auth`] documents why.
//!
//! [`Command`]: smith_core::session::Command

mod api;
mod assets;
mod auth;
mod sse;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::Router;

use smith_core::session::SessionManager;
use smith_core::SmithError;

/// State shared by the handlers.
#[derive(Clone)]
pub struct AppState {
    sessions: Arc<SessionManager>,
    token: Arc<str>,
    /// Exact `Host` header values the server accepts.
    hosts: Arc<[String]>,
    /// Exact `Origin` values the server accepts on mutating requests.
    origins: Arc<[String]>,
}

impl AppState {
    pub fn new(sessions: Arc<SessionManager>, token: impl Into<String>, port: u16) -> Self {
        let hosts: Vec<String> = ["127.0.0.1", "localhost", "[::1]"]
            .iter()
            .map(|host| format!("{host}:{port}"))
            .collect();
        let origins: Vec<String> = ["127.0.0.1", "localhost", "[::1]"]
            .iter()
            .map(|host| format!("http://{host}:{port}"))
            .collect();

        Self {
            sessions,
            token: token.into().into(),
            hosts: hosts.into(),
            origins: origins.into(),
        }
    }

    pub fn sessions(&self) -> &SessionManager {
        &self.sessions
    }

    /// Hostnames are case-insensitive, so the comparison is too.
    pub fn accepts_host(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        self.hosts.contains(&host)
    }

    pub fn accepts_origin(&self, origin: &str) -> bool {
        let origin = origin.trim().to_ascii_lowercase();
        self.origins.contains(&origin)
    }

    /// The token is a bearer credential, so the comparison must not reveal the position of
    /// the first differing byte.
    pub fn token_matches(&self, headers: &HeaderMap) -> bool {
        let Some(presented) = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };

        let expected = self.token.as_bytes();
        let presented = presented.as_bytes();
        if expected.len() != presented.len() {
            return false;
        }
        expected
            .iter()
            .zip(presented)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    /// The URL to open, with the token in the fragment: a fragment is never sent to a server
    /// and never appears in a `Referer`.
    pub fn token_url(&self, address: SocketAddr) -> String {
        format!("http://{address}/#token={}", self.token)
    }
}

/// Generate a 32-byte token from the OS CSPRNG and hex-encode it. The token is per process
/// and never persisted.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("the OS random source is required for the token");
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Refuse any address that is not loopback.
///
/// This is a hard error because the development machine has global-scope interfaces and a
/// default route. Binding `0.0.0.0` would expose the service to the network, and a warning
/// would not prevent it.
pub fn ensure_loopback(ip: IpAddr) -> Result<(), SmithError> {
    if ip.is_loopback() {
        return Ok(());
    }
    Err(SmithError::Config(format!(
        "refusing to bind {ip}: smith serve is reachable from localhost only. \
         It can run the agent's tools and has no real authentication, so binding a routable \
         address would expose this machine. Use 127.0.0.1 or ::1"
    )))
}

/// Build the router. This is separate from [`serve`] so tests can exercise it without a
/// socket.
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(api::health))
        .route(
            "/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        .route("/sessions/{id}", get(api::get_session))
        .route("/sessions/{id}/messages", post(api::send_message))
        .route("/sessions/{id}/cancel", post(api::cancel_turn))
        .route("/sessions/{id}/events", get(sse::events))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ));

    Router::new()
        .route("/", get(assets::index))
        .route("/app.js", get(assets::javascript))
        .route("/style.css", get(assets::stylesheet))
        .nest("/api", api)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_local_host,
        ))
        .with_state(state)
}

/// What `smith serve` needs to start.
pub struct ServerConfig {
    pub sessions: Arc<SessionManager>,
    pub bind: IpAddr,
    pub port: u16,
    pub token: String,
}

/// Bind the listener and build the router without serving.
///
/// This is separate from [`serve`] so the caller learns the real address (port 0 asks the
/// operating system to choose it), and so the startup banner is printed once before any
/// request is accepted.
pub async fn bind(config: ServerConfig) -> Result<Bound, SmithError> {
    ensure_loopback(config.bind)?;

    let listener = tokio::net::TcpListener::bind(SocketAddr::new(config.bind, config.port)).await?;
    let address = listener.local_addr()?;
    let state = AppState::new(config.sessions, config.token, address.port());

    eprintln!("smith: listening on http://{address} (localhost only)");
    eprintln!("smith: open this URL: {}", state.token_url(address));
    eprintln!("smith: the token is this process's only credential and is never written to disk");

    Ok(Bound {
        listener,
        address,
        router: router(state),
    })
}

/// A bound listener that has not started accepting yet.
pub struct Bound {
    pub listener: tokio::net::TcpListener,
    pub address: SocketAddr,
    pub router: Router,
}

impl Bound {
    /// The base URL for the server, without the token fragment. The token is added
    /// separately.
    pub fn url(&self) -> String {
        format!("http://{}/", self.address)
    }
}

/// Serve until the listener fails.
pub async fn serve(bound: Bound) -> Result<(), SmithError> {
    axum::serve(bound.listener, bound.router).await?;
    Ok(())
}
