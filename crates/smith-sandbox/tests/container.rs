use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::json;
use smith_core::config::{NetworkEntry, SandboxSettings};
use smith_core::{ProviderError, ProviderProxy, ProxyEvent};
use smith_sandbox::{standard_env, SandboxConfig, Supervisor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A sibling binary. A test binary lives in `target/debug/deps`, so its parent directory is
/// `target/debug`, where `cargo build --workspace` puts the binaries.
fn sibling(name: &str) -> PathBuf {
    let variable = if name == "smith" {
        "SMITH_BIN"
    } else {
        "SMITH_WORKER_BIN"
    };
    if let Some(path) = std::env::var_os(variable) {
        return PathBuf::from(path);
    }
    let executable = std::env::current_exe().unwrap();
    let candidate = executable.parent().unwrap().parent().unwrap().join(name);
    assert!(
        candidate.is_file(),
        "build it first with `cargo build -p {name}`; expected {}",
        candidate.display()
    );
    candidate
}

fn worker() -> PathBuf {
    sibling("smith-worker")
}

fn smith() -> PathBuf {
    sibling("smith")
}

/// The provider endpoint for the in-container test. It answers every request with a canned
/// SSE stream, so the child needs no network and no key.
struct FakeProxy {
    bodies: Arc<Mutex<Vec<String>>>,
}

impl ProviderProxy for FakeProxy {
    fn forward(
        &self,
        body: Vec<u8>,
        _cancel: CancellationToken,
        events: mpsc::Sender<ProxyEvent>,
    ) -> BoxFuture<'static, Result<(), ProviderError>> {
        self.bodies
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&body).into_owned());
        let events = events.clone();
        Box::pin(async move {
            events
                .send(ProxyEvent::Head {
                    status: 200,
                    content_type: Some("text/event-stream".to_string()),
                })
                .await
                .map_err(|_| ProviderError::Cancelled)?;
            let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"proxied \
                         answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            events
                .send(ProxyEvent::Data(chunk.as_bytes().to_vec()))
                .await
                .map_err(|_| ProviderError::Cancelled)?;
            events
                .send(ProxyEvent::End)
                .await
                .map_err(|_| ProviderError::Cancelled)?;
            Ok(())
        })
    }
}

/// A container, the handshake and two tools. Ignored because it builds an image and needs
/// podman. Run it with `cargo test -p smith-sandbox -- --ignored`.
#[tokio::test]
#[ignore]
async fn a_container_runs_a_shell_command() {
    std::env::set_var("SMITH_WORKER_BIN", worker());

    let supervisor = Supervisor::new().unwrap();
    let workspace = std::env::temp_dir().join(format!("smith-it-{}", std::process::id()));
    std::fs::create_dir_all(&workspace).unwrap();

    let config = SandboxConfig {
        workspace: workspace.clone(),
        session: "integration".to_string(),
        allow: Vec::new(),
        ro_mounts: Vec::new(),
        env: standard_env(&SandboxSettings::default()),
        idle_timeout: Duration::from_secs(30),
        provider: None,
    };

    let sandbox = supervisor.start(&config).await.unwrap();
    let cancel = CancellationToken::new();
    let result = sandbox
        .call(
            "c1".to_string(),
            "run_shell".to_string(),
            json!({"command": "printf hi"}),
            &cancel,
        )
        .await
        .unwrap();

    assert!(result.contains("\"stdout\":\"hi\""), "{result}");
    assert!(result.contains("\"exit_code\":0"), "{result}");

    let write = sandbox
        .call(
            "c2".to_string(),
            "write_file".to_string(),
            json!({"path": "note.txt", "content": "written\n"}),
            &cancel,
        )
        .await
        .unwrap();
    assert!(write.contains("\"bytes\":8"), "{write}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("note.txt")).unwrap(),
        "written\n"
    );

    sandbox.stop().await;
    let _ = std::fs::remove_dir_all(&workspace);
}

/// Egress through the channel: `cargo fetch` over `CONNECT`, then a refusal for a host that
/// is not on the list. Ignored for the same reason as the test above.
#[tokio::test]
#[ignore]
async fn egress_is_proxied_and_allowlisted() {
    std::env::set_var("SMITH_WORKER_BIN", worker());

    let supervisor = Supervisor::new().unwrap();
    let workspace = std::env::temp_dir().join(format!("smith-egress-{}", std::process::id()));
    std::fs::create_dir_all(&workspace).unwrap();

    let settings = SandboxSettings::default();
    let config = SandboxConfig {
        workspace: workspace.clone(),
        session: "egress".to_string(),
        allow: [
            "index.crates.io:443",
            "static.crates.io:443",
            "github.com:443",
        ]
        .iter()
        .map(|entry| entry.parse::<NetworkEntry>().unwrap())
        .collect(),
        ro_mounts: settings.ro_mounts.clone(),
        env: standard_env(&settings),
        idle_timeout: Duration::from_secs(60),
        provider: None,
    };

    let sandbox = supervisor.start(&config).await.unwrap();
    let cancel = CancellationToken::new();

    let fetch = r#"mkdir -p /tmp/crate/src && cd /tmp/crate && printf '[package]\nname="probe"\nversion="0.1.0"\nedition="2021"\n\n[dependencies]\nserde = "1"\n' > Cargo.toml && printf 'fn main(){}\n' > src/main.rs && cargo fetch 2>&1 | tail -3"#;
    let fetched = sandbox
        .call(
            "c1".to_string(),
            "run_shell".to_string(),
            json!({ "command": fetch }),
            &cancel,
        )
        .await
        .unwrap();
    assert!(fetched.contains("\"exit_code\":0"), "{fetched}");

    let denied = sandbox
        .call(
            "c2".to_string(),
            "run_shell".to_string(),
            json!({"command": "git ls-remote https://example.com/nope 2>&1 | tail -2"}),
            &cancel,
        )
        .await
        .unwrap();
    assert!(denied.contains("403"), "{denied}");

    sandbox.stop().await;
    let _ = std::fs::remove_dir_all(&workspace);
}

/// The child provider proxy: `smith -p` runs inside the container, its provider request
/// reaches this test through the channel, and the reply comes back the same way. Ignored for
/// the same reason as the tests above.
#[tokio::test]
#[ignore]
async fn a_sandboxed_child_reaches_the_host_provider_proxy() {
    std::env::set_var("SMITH_WORKER_BIN", worker());
    std::env::set_var("SMITH_BIN", smith());

    let supervisor = Supervisor::new().unwrap();
    let workspace = std::env::temp_dir().join(format!("smith-child-{}", std::process::id()));
    std::fs::create_dir_all(&workspace).unwrap();

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let settings = SandboxSettings::default();
    let mut env = standard_env(&settings);
    env.insert("SMITH_PROVIDER".to_string(), "sandbox".to_string());
    env.insert("SMITH_MODEL".to_string(), "test-model".to_string());

    let config = SandboxConfig {
        workspace: workspace.clone(),
        session: "child".to_string(),
        allow: Vec::new(),
        ro_mounts: Vec::new(),
        env,
        idle_timeout: Duration::from_secs(60),
        provider: Some(Arc::new(FakeProxy {
            bodies: bodies.clone(),
        })),
    };

    let sandbox = supervisor.start(&config).await.unwrap();
    let cancel = CancellationToken::new();

    let answer = sandbox
        .call(
            "c1".to_string(),
            "run_shell".to_string(),
            json!({"command": "smith -p 'say hi' --sandbox=auto"}),
            &cancel,
        )
        .await
        .unwrap();
    assert!(answer.contains("\"stdout\":\"proxied answer"), "{answer}");
    assert!(answer.contains("\"exit_code\":0"), "{answer}");

    // The child environment carries the proxy token, not the host provider key.
    let present = sandbox
        .call(
            "c2".to_string(),
            "run_shell".to_string(),
            json!({"command": "test -n \"$SMITH_SANDBOX_TOKEN\" && test -z \"$OPENCODE_API_KEY\""}),
            &cancel,
        )
        .await
        .unwrap();
    assert!(present.contains("\"exit_code\":0"), "{present}");

    // The provider request carried the child's chat completion body.
    {
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains("\"stream\":true"), "{}", bodies[0]);
        assert!(bodies[0].contains("\"messages\""), "{}", bodies[0]);
    }

    sandbox.stop().await;
    let _ = std::fs::remove_dir_all(&workspace);
}
