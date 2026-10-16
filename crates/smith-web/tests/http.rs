//! HTTP behavior, driven without a browser.
//!
//! Most tests run the router directly through `tower::ServiceExt::oneshot`, without a socket
//! or port. One test opens a real socket because that is the only way to prove the bound
//! address is loopback and that a create → send → stream round trip completes over TCP.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{Stream, StreamExt};
use tower::ServiceExt;

use smith_core::mock::MockProvider;
use smith_core::session::{SessionConfig, SessionManager, Subscription, UiEventKind};
use smith_web::{AppState, ServerConfig};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PORT: u16 = 7878;

/// How long a stream has to be silent before a test treats it as done. The events endpoint is
/// a live stream and never ends by itself, so a test cannot wait for the body to complete the
/// way it can for a JSON response.
const QUIET: Duration = Duration::from_millis(750);

fn sessions(provider: Arc<MockProvider>) -> Arc<SessionManager> {
    Arc::new(SessionManager::new(SessionConfig {
        provider,
        model: "mock-1".to_string(),
        system: None,
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
    }))
}

fn state(provider: Arc<MockProvider>) -> AppState {
    AppState::new(sessions(provider), TOKEN, PORT)
}

/// A request with a valid token and the `Host` the server expects.
fn authorised(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

fn json_request(method: &str, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(app: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.oneshot(request).await.expect("the router must answer");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body must be readable");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Read an SSE body until it has been quiet for [`QUIET`], then return the frames that
/// arrived. Silence is the only end-of-response signal an event stream has.
async fn read_until_quiet<S, E>(stream: S, quiet: Duration) -> String
where
    S: Stream<Item = Result<axum::body::Bytes, E>>,
    E: std::fmt::Debug,
{
    futures_util::pin_mut!(stream);
    let mut seen = String::new();
    while let Ok(Some(chunk)) = tokio::time::timeout(quiet, stream.next()).await {
        seen.push_str(&String::from_utf8_lossy(
            &chunk.expect("the stream must not fail"),
        ));
    }
    seen
}

/// Read a frontend's view until an event matches. This is the loop a renderer runs during a
/// turn.
async fn until(
    subscription: &mut Subscription,
    matches: impl Fn(&UiEventKind) -> bool,
) -> smith_core::session::UiEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout_at(deadline, subscription.recv())
            .await
            .expect("the terminal must not wait forever")
            .expect("the session must stay open");
        if matches(&event.kind) {
            return event;
        }
    }
}

/// Poll the transcript until `reply` appears. A send that arrives while a turn is running is
/// rejected by design, so a test that sends twice must wait for the first to finish.
async fn wait_for_reply(app: &axum::Router, id: &str, reply: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (_, body) = send(
            app.clone(),
            authorised("GET", &format!("/api/sessions/{id}")),
        )
        .await;
        if body.contains(&format!("\"text\":\"{reply}\"")) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no reply {reply:?} arrived: {body}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Open an event stream through the router, without a socket.
async fn stream_from(app: axum::Router, request: Request<Body>) -> String {
    let response = app.oneshot(request).await.expect("the router must answer");
    assert_eq!(response.status(), StatusCode::OK, "the stream must open");
    read_until_quiet(response.into_body().into_data_stream(), QUIET).await
}

#[tokio::test]
async fn api_routes_require_a_token() {
    let app = smith_web::router(state(Arc::new(MockProvider::new("mock"))));

    let missing = Request::builder()
        .uri("/api/health")
        .header("host", format!("127.0.0.1:{PORT}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app.clone(), missing).await.0, StatusCode::UNAUTHORIZED);

    let wrong = Request::builder()
        .uri("/api/health")
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", "Bearer nope")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app.clone(), wrong).await.0, StatusCode::UNAUTHORIZED);

    // A prefix of the real token must not be accepted either.
    let prefix = Request::builder()
        .uri("/api/health")
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", &TOKEN[..16])
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app.clone(), prefix).await.0, StatusCode::UNAUTHORIZED);

    let (status, body) = send(app, authorised("GET", "/api/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"status\":\"ok\""), "{body}");
}

/// DNS rebinding: `evil.com` resolves to 127.0.0.1, the browser sends `Host: evil.com`, and
/// only this check stops it.
#[tokio::test]
async fn a_foreign_host_header_is_rejected_even_with_a_valid_token() {
    let app = smith_web::router(state(Arc::new(MockProvider::new("mock"))));

    for host in [
        "evil.com",
        "evil.com:7878",
        "127.0.0.1:9999",
        "100.65.253.201:7878",
    ] {
        let request = Request::builder()
            .uri("/api/health")
            .header("host", host)
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(app.clone(), request).await.0,
            StatusCode::MISDIRECTED_REQUEST,
            "Host {host} must be refused"
        );
    }

    // Every name the server is reachable under is accepted.
    for host in [
        format!("127.0.0.1:{PORT}"),
        format!("localhost:{PORT}"),
        format!("[::1]:{PORT}"),
    ] {
        let request = Request::builder()
            .uri("/api/health")
            .header("host", host)
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(app.clone(), request).await.0, StatusCode::OK);
    }
}

#[tokio::test]
async fn a_foreign_origin_is_rejected_on_mutations_but_not_on_reads() {
    let app = smith_web::router(state(Arc::new(MockProvider::new("mock"))));

    let mutation = Request::builder()
        .method("POST")
        .uri("/api/sessions")
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("origin", "http://evil.com")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app.clone(), mutation).await.0, StatusCode::FORBIDDEN);

    let read = Request::builder()
        .uri("/api/health")
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("origin", "http://evil.com")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(app.clone(), read).await.0, StatusCode::OK);

    let our_mutation = Request::builder()
        .method("POST")
        .uri("/api/sessions")
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("origin", format!("http://127.0.0.1:{PORT}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(app, our_mutation).await.0,
        StatusCode::CREATED,
        "our own origin must be accepted"
    );
}

#[tokio::test]
async fn sessions_are_created_listed_and_read_back() {
    let provider = Arc::new(MockProvider::new("mock"));
    provider.push_text("hello");
    let app = smith_web::router(state(provider));

    let (status, body) = send(app.clone(), authorised("POST", "/api/sessions")).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a session id")
        .to_string();
    assert!(id.starts_with("smith-"), "{id}");

    let (status, body) = send(app.clone(), authorised("GET", "/api/sessions")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&id), "{body}");

    // A session the server does not know is a 404, not an empty transcript.
    assert_eq!(
        send(app.clone(), authorised("GET", "/api/sessions/nope"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // Send, then read the transcript back: the user message and the reply are both there.
    let (status, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/api/sessions/{id}/messages"),
            r#"{"text":"hi"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let (status, body) = send(app, authorised("GET", &format!("/api/sessions/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"text\":\"hi\""), "{body}");
    assert!(body.contains("\"text\":\"hello\""), "{body}");
    assert!(body.contains("\"busy\":false"), "{body}");
}

#[tokio::test]
async fn sending_while_a_turn_is_running_is_a_conflict() {
    let provider = Arc::new(MockProvider::new("mock"));
    provider.push_stalling_text("working");
    let app = smith_web::router(state(provider));

    let (_, body) = send(app.clone(), authorised("POST", "/api/sessions")).await;
    let id = body
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();

    let messages = format!("/api/sessions/{id}/messages");
    assert_eq!(
        send(
            app.clone(),
            json_request("POST", &messages, r#"{"text":"first"}"#)
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );

    // Wait until the turn is running.
    let mut busy = false;
    for _ in 0..100 {
        let (_, body) = send(
            app.clone(),
            authorised("GET", &format!("/api/sessions/{id}")),
        )
        .await;
        if body.contains("\"busy\":true") {
            busy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(busy, "the turn never started");

    let (status, body) = send(
        app.clone(),
        json_request("POST", &messages, r#"{"text":"second"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("already running"), "{body}");

    // Cancelling is accepted, and is not an error when nothing is running.
    assert_eq!(
        send(
            app.clone(),
            authorised("POST", &format!("/api/sessions/{id}/cancel"))
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        send(app, authorised("POST", "/api/sessions/nope/cancel"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn empty_messages_are_rejected() {
    let provider = Arc::new(MockProvider::new("mock"));
    let app = smith_web::router(state(provider));
    let (_, body) = send(app.clone(), authorised("POST", "/api/sessions")).await;
    let id = body
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();

    let (status, _) = send(
        app,
        json_request(
            "POST",
            &format!("/api/sessions/{id}/messages"),
            r#"{"text":"   "}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_ui_is_served_without_a_token_but_still_needs_a_valid_host() {
    let app = smith_web::router(state(Arc::new(MockProvider::new("mock"))));

    // The page itself cannot require the token: the token arrives in the URL fragment that
    // loads it.
    let (status, body) = send(app.clone(), authorised("GET", "/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>smith</title>"), "{body}");

    let (status, js) = send(app.clone(), authorised("GET", "/app.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(js.contains("Authorization"));

    let (status, css) = send(app.clone(), authorised("GET", "/style.css")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        css.contains("--accent"),
        "the stylesheet must be the real one"
    );

    let foreign = Request::builder()
        .uri("/")
        .header("host", "evil.com")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(app, foreign).await.0,
        StatusCode::MISDIRECTED_REQUEST,
        "static assets are behind the host check too"
    );
}

#[test]
fn only_loopback_addresses_are_accepted() {
    for allowed in ["127.0.0.1", "127.1.2.3", "::1"] {
        let ip: std::net::IpAddr = allowed.parse().unwrap();
        assert!(smith_web::ensure_loopback(ip).is_ok(), "{allowed}");
    }
    for refused in ["0.0.0.0", "100.65.253.201", "192.168.1.10", "::"] {
        let ip: std::net::IpAddr = refused.parse().unwrap();
        let error = smith_web::ensure_loopback(ip).unwrap_err().to_string();
        assert!(error.contains("localhost only"), "{error}");
        assert!(
            error.contains("no real authentication"),
            "the message must say why: {error}"
        );
    }
}

#[test]
fn tokens_are_random_and_long_enough() {
    let first = smith_web::new_token();
    let second = smith_web::new_token();
    assert_eq!(first.len(), 64, "32 bytes of hex");
    assert_ne!(first, second);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
}

/// The only test that uses a real socket. It proves the bound address is loopback and
/// exercises the full create → send → stream path over TCP.
#[tokio::test]
async fn a_real_socket_round_trip_streams_a_turn() {
    let provider = Arc::new(MockProvider::new("mock"));
    provider.push_chunks(["Hel", "lo"]);
    let token = smith_web::new_token();

    let bound = smith_web::bind(ServerConfig {
        sessions: sessions(provider),
        // Port 0 asks the operating system to choose. The returned address is checked below.
        bind: "127.0.0.1".parse().unwrap(),
        port: 0,
        token: token.clone(),
    })
    .await
    .expect("binding loopback must work");

    assert!(
        bound.address.ip().is_loopback(),
        "must never bind anything else, got {}",
        bound.address
    );

    let address = bound.address;
    tokio::spawn(smith_web::serve(bound));

    let client = reqwest::Client::new();
    let base = format!("http://{address}");

    let created = client
        .post(format!("{base}/api/sessions"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("the server must be reachable");
    assert_eq!(created.status(), StatusCode::CREATED);
    let id: String = created.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Subscribe before sending, as a frontend does.
    let events = client
        .get(format!("{base}/api/sessions/{id}/events"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("the events endpoint must be reachable");
    assert_eq!(events.status(), StatusCode::OK);

    let accepted = client
        .post(format!("{base}/api/sessions/{id}/messages"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "text": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);

    let seen = read_until_quiet(events.bytes_stream(), QUIET).await;

    assert!(seen.contains("event: text_delta"), "no deltas: {seen}");
    assert!(seen.contains("\"text\":\"Hel\""), "delta payloads: {seen}");
    assert!(
        seen.contains("event: turn_finished"),
        "no completion: {seen}"
    );
    assert!(seen.contains("id: "), "every frame carries a seq: {seen}");
    assert!(
        seen.contains("\"text\":\"Hello\""),
        "the finished message is the assembled one: {seen}"
    );
}

/// A resuming client asks from a seq and receives only later events, with no gap or
/// duplicate. This is the replay race over HTTP.
#[tokio::test]
async fn resuming_from_a_seq_replays_only_what_came_after_it() {
    let provider = Arc::new(MockProvider::new("mock"));
    provider.push_text("first");
    provider.push_text("second");
    let sessions = sessions(provider);
    let app = smith_web::router(AppState::new(sessions.clone(), TOKEN, PORT));

    let (_, body) = send(app.clone(), authorised("POST", "/api/sessions")).await;
    let id = body
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();

    let messages = format!("/api/sessions/{id}/messages");
    for (prompt, reply) in [("one", "first"), ("two", "second")] {
        let (status, _) = send(
            app.clone(),
            json_request("POST", &messages, &format!(r#"{{"text":"{prompt}"}}"#)),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        wait_for_reply(&app, &id, reply).await;
    }

    let id_of = |frame: &str| -> u64 {
        frame
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };

    let full = stream_from(
        app.clone(),
        authorised("GET", &format!("/api/sessions/{id}/events")),
    )
    .await;
    let frames: Vec<&str> = full
        .split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .collect();
    assert!(frames.len() > 4, "both turns must be in the log: {full}");

    let resume_from = id_of(frames[2]);
    let resumed = stream_from(
        app,
        authorised(
            "GET",
            &format!("/api/sessions/{id}/events?since={resume_from}"),
        ),
    )
    .await;

    let resumed_frames: Vec<&str> = resumed
        .split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .collect();
    assert!(!resumed_frames.is_empty());
    for frame in &resumed_frames {
        assert!(
            id_of(frame) > resume_from,
            "resumed stream re-sent an old frame: {frame}"
        );
    }
    assert_eq!(
        resumed_frames.len(),
        frames.len() - 3,
        "resuming must not skip or duplicate anything"
    );
}

/// One session shared by two frontends.
///
/// The terminal's view is a [`Subscription`], which is what `smith-cli` renders from, and the
/// browser's view is the HTTP event stream. Both must see the same turn, either must be able
/// to cancel it, and a turn the terminal starts must reach the browser. This test covers the
/// protocol without driving a terminal.
#[tokio::test]
async fn a_terminal_and_a_browser_render_one_session() {
    let provider = Arc::new(MockProvider::new("mock"));
    provider.push_stalling_text("half an ans");
    provider.push_text("done");
    let sessions = sessions(provider);
    let app = smith_web::router(AppState::new(sessions.clone(), TOKEN, PORT));

    let (_, body) = send(app.clone(), authorised("POST", "/api/sessions")).await;
    let id = body
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a session id")
        .to_string();

    // The same session object, reached two ways: `get` is what the server's own handlers use,
    // and it is what `--repl` holds when it attaches a terminal to the server's manager.
    let terminal = sessions.get(&id).expect("one session, two frontends");
    let mut terminal_events = terminal.subscribe();
    let messages = format!("/api/sessions/{id}/messages");

    // The browser sends, and the terminal renders the turn it did not start.
    assert_eq!(
        send(
            app.clone(),
            json_request("POST", &messages, r#"{"text":"hi"}"#)
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    until(
        &mut terminal_events,
        |kind| matches!(kind, UiEventKind::TurnStarted { prompt } if prompt == "hi"),
    )
    .await;
    let streamed = until(&mut terminal_events, |kind| {
        matches!(kind, UiEventKind::TextDelta { .. })
    })
    .await;
    assert_eq!(
        streamed.kind,
        UiEventKind::TextDelta {
            text: "half an ans".to_string()
        }
    );

    // The browser cancels, and the terminal renders the cancellation of the turn it started.
    assert_eq!(
        send(
            app.clone(),
            authorised("POST", &format!("/api/sessions/{id}/cancel"))
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    let cancelled = until(&mut terminal_events, |kind| {
        matches!(kind, UiEventKind::Cancelled)
    })
    .await;

    // The other direction: the terminal sends, the browser sees it. The browser resumes from
    // the cancellation, so what it renders is exactly the run it has not seen yet.
    terminal.send("again").await.unwrap();
    until(&mut terminal_events, |kind| {
        matches!(kind, UiEventKind::TurnFinished { .. })
    })
    .await;

    let browser = stream_from(
        app.clone(),
        authorised(
            "GET",
            &format!("/api/sessions/{id}/events?since={}", cancelled.seq),
        ),
    )
    .await;
    assert_eq!(
        browser.matches("event: turn_started").count(),
        1,
        "one turn, from the terminal: {browser}"
    );
    assert!(browser.contains("\"text\":\"done\""), "{browser}");
    assert!(browser.contains("event: turn_finished"), "{browser}");
    assert!(
        !browser.contains("half an ans"),
        "the cancelled turn is behind the resume point: {browser}"
    );

    // Both frontends see one transcript. It contains the finished turn and nothing from the
    // browser's cancelled turn, not even its prompt, because the transcript either gains a
    // complete turn or remains unchanged.
    let (_, transcript) = send(app, authorised("GET", &format!("/api/sessions/{id}"))).await;
    assert!(transcript.contains("\"text\":\"again\""), "{transcript}");
    assert!(transcript.contains("\"text\":\"done\""), "{transcript}");
    assert!(
        !transcript.contains("\"text\":\"hi\""),
        "the cancelled turn must not be half-recorded: {transcript}"
    );
}

/// A stream that falls further behind than the channel capacity receives a `resync` event.
/// Without it the client would render a gap as a complete answer.
#[tokio::test]
async fn a_stream_that_lags_is_told_to_resync() {
    let provider = Arc::new(MockProvider::new("mock"));
    let reply: String = (0..400).map(|i| format!("{i},")).collect();
    provider.push_chunks((0..400).map(|i| format!("{i},")));
    let app = smith_web::router(state(provider));

    let (_, body) = send(app.clone(), authorised("POST", "/api/sessions")).await;
    let id = body
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();

    // Open the stream and never read it. The subscription exists and events flow into it, as
    // with a browser tab that is throttled or hidden.
    let response = app
        .clone()
        .oneshot(authorised("GET", &format!("/api/sessions/{id}/events")))
        .await
        .expect("the router must answer");
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        send(
            app.clone(),
            json_request(
                "POST",
                &format!("/api/sessions/{id}/messages"),
                r#"{"text":"flood"}"#
            )
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    // Waiting on the transcript rather than on the stream: the stream is deliberately unread,
    // and reading it would be what stops it from falling behind.
    wait_for_reply(&app, &id, &reply).await;

    let seen = read_until_quiet(response.into_body().into_data_stream(), QUIET).await;
    let frames: Vec<&str> = seen
        .split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .collect();

    assert!(
        frames[0].contains("event: resync"),
        "the first thing a lagged client hears must be the resync: {seen}"
    );
    assert!(
        !frames[0].contains("id:"),
        "a resync is not progress and must not be mistaken for it: {}",
        frames[0]
    );
    assert!(frames[0].contains("skipped"), "{}", frames[0]);
    assert!(
        seen.contains("event: turn_finished"),
        "the stream carries on after resyncing: {seen}"
    );
}
