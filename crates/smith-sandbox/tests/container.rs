use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use smith_core::config::{NetworkEntry, SandboxSettings};
use smith_sandbox::{standard_env, SandboxConfig, Supervisor};
use tokio_util::sync::CancellationToken;

/// The worker is a sibling binary. A test binary lives in `target/debug/deps`, so its parent
/// directory is `target/debug`, where `cargo build --workspace` puts the worker.
fn worker() -> PathBuf {
    if let Some(path) = std::env::var_os("SMITH_WORKER_BIN") {
        return PathBuf::from(path);
    }
    let executable = std::env::current_exe().unwrap();
    let candidate = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("smith-worker");
    assert!(
        candidate.is_file(),
        "build the worker first with `cargo build -p smith-worker`; expected {}",
        candidate.display()
    );
    candidate
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
