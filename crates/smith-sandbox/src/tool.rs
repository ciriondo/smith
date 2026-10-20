//! The four [`smith_core::Tool`] values that forward calls to the sandbox.
//!
//! One [`Handle`] is shared by the four tools of a session. It starts the container on the
//! first call, refreshes the idle clock on every call, and removes the container after
//! `idle_timeout` without activity.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::Value;
use smith_core::config::SandboxSettings;
use smith_core::tools::{spec, TOOL_NAMES};
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
}

/// Build the four tools. The container starts lazily, on the first call.
pub fn sandbox_tools(supervisor: Arc<Supervisor>, config: SandboxConfig) -> Vec<Arc<dyn Tool>> {
    let handle = Arc::new(Handle {
        supervisor,
        config,
        state: AsyncMutex::new(None),
        busy: AtomicBool::new(false),
        last_used: Mutex::new(Instant::now()),
        next_call: AtomicU64::new(1),
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
        let inventory = sandbox.inventory();
        if inventory.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!(
                "The sandbox image provides: {}.",
                inventory.join(", ")
            )))
        }
    }

    async fn call_impl(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let sandbox = self.ensure().await?;
        self.touch();
        let call_id = format!("c{}", self.next_call.fetch_add(1, Ordering::SeqCst));
        let result = sandbox
            .call(call_id, name.to_string(), args, &ctx.cancel)
            .await;
        self.touch();
        result
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
        assert!(env["PATH"].starts_with('/'));
        assert!(!env.contains_key("EDITOR"));
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
}
