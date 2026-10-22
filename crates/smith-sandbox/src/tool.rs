//! The five [`smith_core::Tool`] values that forward calls to the sandbox.
//!
//! One [`Handle`] is shared by the five tools of a session. It starts the container on the
//! first call, refreshes the idle clock on every call, and removes the container after
//! `idle_timeout` without activity. A container with an open background run is not removed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::Value;
use smith_core::config::SandboxSettings;
use smith_core::tools::{spec, TOOL_NAMES};
use smith_core::working_dir::WorkingDir;
use smith_core::{Tool, ToolContext, ToolError, ToolSpec};
use tokio::sync::Mutex as AsyncMutex;

use crate::supervisor::{Sandbox, SandboxConfig, Supervisor};

/// Shared state for the tools of one session.
pub struct Handle {
    supervisor: Arc<Supervisor>,
    config: SandboxConfig,
    state: AsyncMutex<Option<Arc<Sandbox>>>,
    busy: AtomicBool,
    last_used: Mutex<Instant>,
    next_call: AtomicU64,
    /// Task ids whose background run has not reported completion through `tool_wait`.
    open_tasks: Mutex<BTreeSet<String>>,
    /// The session working directory shared by every tool.
    working: WorkingDir,
}

/// Build the five tools. The container starts lazily, on the first call.
pub fn sandbox_tools(supervisor: Arc<Supervisor>, config: SandboxConfig) -> Vec<Arc<dyn Tool>> {
    let working = WorkingDir::new(config.workspace.clone());
    let handle = Arc::new(Handle {
        supervisor,
        config,
        state: AsyncMutex::new(None),
        busy: AtomicBool::new(false),
        last_used: Mutex::new(Instant::now()),
        next_call: AtomicU64::new(1),
        open_tasks: Mutex::new(BTreeSet::new()),
        working,
    });
    spawn_idle_monitor(&handle);

    TOOL_NAMES
        .iter()
        .map(|name| {
            Arc::new(SandboxTool {
                name: name.to_string(),
                handle: handle.clone(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

impl Handle {
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    async fn ensure(&self) -> Result<Arc<Sandbox>, ToolError> {
        let mut state = self.state.lock().await;
        if let Some(sandbox) = state.as_ref() {
            return Ok(sandbox.clone());
        }

        match self.supervisor.start(&self.config).await {
            Ok(sandbox) => {
                // A new container has no tasks from the previous one.
                self.open_tasks.lock().unwrap().clear();
                *state = Some(sandbox.clone());
                Ok(sandbox)
            }
            Err(e) => {
                // The turn fails, and the next turn tries again. There is no host fallback.
                eprintln!("smith: sandbox: {e}");
                Err(ToolError::SandboxGone)
            }
        }
    }

    async fn prepare_impl(&self) -> Result<Option<String>, ToolError> {
        let sandbox = self.ensure().await?;
        self.touch();
        Ok(smith_core::tools::inventory_sentence(sandbox.inventory()))
    }

    async fn call_impl(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let sandbox = self.ensure().await?;
        self.touch();
        let mut args = args;
        self.working.rewrite(name, &mut args);
        let call_id = format!("c{}", self.next_call.fetch_add(1, Ordering::SeqCst));
        let result = sandbox
            .call(call_id, name.to_string(), args, &ctx.cancel)
            .await;
        let result = result.map(|content| self.working.observe(name, content));
        if let Ok(content) = &result {
            update_open_tasks(&self.open_tasks, name, content);
        }
        self.touch();
        result
    }
}

/// Keep the set of background runs that have not reported completion. The idle monitor must
/// not stop a container while a run is still writing to its spool.
fn update_open_tasks(open: &Mutex<BTreeSet<String>>, name: &str, content: &str) {
    let Ok(value) = serde_json::from_str::<Value>(content) else {
        return;
    };
    let Some(id) = value
        .get("id")
        .or_else(|| value.get("task_id"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let running = value.get("running").and_then(Value::as_bool);
    let mut open = open.lock().unwrap();
    match name {
        "run_shell" if running == Some(true) && value.get("task_id").is_some() => {
            open.insert(id.to_string());
        }
        "tool_wait" if running == Some(false) => {
            open.remove(id);
        }
        _ => {}
    }
}

fn spawn_idle_monitor(handle: &Arc<Handle>) {
    let weak = Arc::downgrade(handle);
    let idle_timeout = handle.config.idle_timeout;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let Some(handle) = weak.upgrade() else {
                break;
            };
            if handle.busy.load(Ordering::SeqCst) {
                continue;
            }
            if !handle.open_tasks.lock().unwrap().is_empty() {
                continue;
            }
            if handle.last_used.lock().unwrap().elapsed() < idle_timeout {
                continue;
            }
            let mut state = handle.state.lock().await;
            if let Some(sandbox) = state.take() {
                sandbox.stop().await;
            }
        }
    });
}

/// One tool value, bound to a name and the shared handle.
pub struct SandboxTool {
    name: String,
    handle: Arc<Handle>,
}

impl Tool for SandboxTool {
    fn spec(&self) -> ToolSpec {
        spec(&self.name).expect("the name comes from TOOL_NAMES")
    }

    fn prepare<'a>(&'a self) -> BoxFuture<'a, Result<Option<String>, ToolError>> {
        Box::pin(async move { self.handle.prepare_impl().await })
    }

    fn call<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String, ToolError>> {
        Box::pin(async move { self.handle.call_impl(&self.name, args, ctx).await })
    }

    fn set_busy(&self, busy: bool) {
        self.handle.busy.store(busy, Ordering::SeqCst);
        if busy {
            self.handle.touch();
        }
    }

    fn cwd(&self) -> Option<std::path::PathBuf> {
        Some(self.handle.working.current())
    }
}

/// The environment every tool process receives.
///
/// Inherited and configured names come from the settings, which already rejected the deny
/// list. The rest is computed: a writable home on the tmpfs, the cache directories, the proxy
/// that the worker bridges, and the host toolchain paths that exist inside the container
/// because the default mounts keep their host paths.
pub fn standard_env(settings: &SandboxSettings) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    env.insert("HOME".to_string(), "/tmp/home".to_string());
    env.insert("TERM".to_string(), "dumb".to_string());
    env.insert("NO_COLOR".to_string(), "1".to_string());
    env.insert("LC_ALL".to_string(), "C.UTF-8".to_string());
    env.insert("LANG".to_string(), "C.UTF-8".to_string());
    env.insert(
        "CARGO_HOME".to_string(),
        "/var/cache/smith/cargo".to_string(),
    );
    env.insert(
        "SSL_CERT_FILE".to_string(),
        "/etc/ssl/certs/ca-certificates.crt".to_string(),
    );

    let home = std::env::var("HOME").unwrap_or_default();
    env.insert(
        "PATH".to_string(),
        format!("{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"),
    );
    if settings
        .ro_mounts
        .iter()
        .any(|path| path.ends_with(".rustup"))
    {
        env.insert("RUSTUP_HOME".to_string(), format!("{home}/.rustup"));
    }

    // Inherited and configured values win over the computed defaults above.
    for name in &settings.env_inherit {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.clone(), value);
        }
    }
    for (name, value) in &settings.env_set {
        env.insert(name.clone(), value.clone());
    }

    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        env.insert(name.to_string(), "http://127.0.0.1:3128".to_string());
    }
    // Loopback is the worker's own listeners: the egress proxy and the provider proxy. A
    // child's provider client must reach the provider listener directly, not through the
    // egress proxy, which would refuse a loopback target.
    for name in ["NO_PROXY", "no_proxy"] {
        env.insert(name.to_string(), "127.0.0.1,localhost".to_string());
    }

    // The marker tells a nested smith that it is already inside the sandbox. It is set after
    // the inherited and configured values, so configuration cannot clear it and cause a
    // nested container.
    env.insert("SMITH_IN_SANDBOX".to_string(), "1".to_string());

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_computed_not_inherited() {
        let settings = SandboxSettings::default();
        let env = standard_env(&settings);
        assert_eq!(env["HOME"], "/tmp/home");
        assert_eq!(env["TERM"], "dumb");
        assert_eq!(env["CARGO_HOME"], "/var/cache/smith/cargo");
        assert_eq!(env["HTTPS_PROXY"], "http://127.0.0.1:3128");
        assert_eq!(env["NO_PROXY"], "127.0.0.1,localhost");
        assert!(env["PATH"].starts_with('/'));
        assert_eq!(env["SMITH_IN_SANDBOX"], "1");
        assert!(!env.contains_key("EDITOR"));
    }

    #[test]
    fn open_tasks_follow_background_and_wait_results() {
        let open = Mutex::new(BTreeSet::new());
        update_open_tasks(
            &open,
            "run_shell",
            &serde_json::json!({"task_id": "t1", "running": true}).to_string(),
        );
        assert!(open.lock().unwrap().contains("t1"));

        update_open_tasks(
            &open,
            "tool_wait",
            &serde_json::json!({"id": "t1", "running": true}).to_string(),
        );
        assert!(open.lock().unwrap().contains("t1"));

        update_open_tasks(
            &open,
            "tool_wait",
            &serde_json::json!({"id": "t1", "running": false}).to_string(),
        );
        assert!(open.lock().unwrap().is_empty());

        update_open_tasks(
            &open,
            "run_shell",
            &serde_json::json!({"exit_code": 0, "running": false}).to_string(),
        );
        assert!(open.lock().unwrap().is_empty());
    }

    #[test]
    fn configured_values_win_over_computed_defaults() {
        let mut settings = SandboxSettings::default();
        settings
            .env_set
            .insert("TERM".to_string(), "xterm".to_string());
        let env = standard_env(&settings);
        assert_eq!(env["TERM"], "xterm");
    }

    #[test]
    fn the_sandbox_marker_cannot_be_overridden() {
        let mut settings = SandboxSettings::default();
        settings
            .env_set
            .insert("SMITH_IN_SANDBOX".to_string(), "0".to_string());
        let env = standard_env(&settings);
        assert_eq!(env["SMITH_IN_SANDBOX"], "1");
    }
}
