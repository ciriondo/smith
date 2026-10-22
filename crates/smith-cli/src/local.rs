//! Tool execution in the current environment for an inherit-mode child.
//!
//! The process is already inside a smith sandbox, so the tools reuse the worker's file and
//! shell implementations directly. No supervisor and no container are created. The worker
//! library stays free of network, terminal and HTTP dependencies.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use smith_core::tools::{spec, TOOL_NAMES};
use smith_core::working_dir::WorkingDir;
use smith_core::{SmithError, Tool, ToolContext, ToolError, ToolSpec};
use smith_worker::protocol::{read_inventory, INVENTORY_PATH};
use smith_worker::tools::{execute, ToolContext as WorkerContext};
use smith_worker::{CancelFlag, TaskRegistry, Workspace};

/// Shared state for every local tool in one session.
struct Handle {
    workspace: Workspace,
    env: BTreeMap<String, String>,
    tasks: Arc<TaskRegistry>,
    inventory: Option<String>,
    working: WorkingDir,
}

/// The five tools that execute in the current environment.
pub fn local_tools(workspace: &Path) -> Result<Vec<Arc<dyn Tool>>, SmithError> {
    let workspace = Workspace::new(workspace).map_err(SmithError::Config)?;
    let env = std::env::vars().collect();
    let handle = Arc::new(Handle {
        working: WorkingDir::new(workspace.root().to_path_buf()),
        workspace,
        env,
        tasks: Arc::new(TaskRegistry::new()),
        inventory: inventory_sentence(),
    });

    Ok(TOOL_NAMES
        .iter()
        .map(|name| {
            Arc::new(LocalTool {
                name: name.to_string(),
                handle: handle.clone(),
            }) as Arc<dyn Tool>
        })
        .collect())
}

fn inventory_sentence() -> Option<String> {
    let inventory = read_inventory(Path::new(INVENTORY_PATH)).ok()?;
    smith_core::tools::inventory_sentence(&inventory)
}

struct LocalTool {
    name: String,
    handle: Arc<Handle>,
}

impl Tool for LocalTool {
    fn spec(&self) -> ToolSpec {
        spec(&self.name).expect("the name comes from TOOL_NAMES")
    }

    fn prepare<'a>(&'a self) -> BoxFuture<'a, Result<Option<String>, ToolError>> {
        Box::pin(async move { Ok(self.handle.inventory.clone()) })
    }

    fn cwd(&self) -> Option<std::path::PathBuf> {
        Some(self.handle.working.current())
    }

    fn call<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let handle = self.handle.clone();
            let name = self.name.clone();
            let flag = Arc::new(CancelFlag::new());
            let bridge = {
                let flag = flag.clone();
                let cancel = ctx.cancel.clone();
                tokio::spawn(async move {
                    cancel.cancelled().await;
                    flag.cancel();
                })
            };

            let mut args = args;
            handle.working.rewrite(&name, &mut args);
            let result_handle = handle.clone();
            let result_name = name.clone();
            let result = tokio::task::spawn_blocking(move || {
                let worker = WorkerContext {
                    workspace: &handle.workspace,
                    env: &handle.env,
                    cancel: &flag,
                    tasks: &handle.tasks,
                };
                execute(&name, args, &worker)
            })
            .await;
            bridge.abort();

            match result {
                Ok(Ok(content)) => Ok(result_handle.working.observe(&result_name, content)),
                Ok(Err(message)) => Err(ToolError::Message(message)),
                Err(e) => Err(ToolError::Message(format!(
                    "the local tool task failed: {e}"
                ))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    async fn call(
        tools: &[Arc<dyn Tool>],
        workspace: &Path,
        name: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        let tool = tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .expect("the tool exists");
        let ctx = ToolContext {
            workspace: PathBuf::from(workspace),
            cancel: CancellationToken::new(),
        };
        let content = tool.call(args, &ctx).await?;
        Ok(serde_json::from_str(&content).expect("the tool returns JSON"))
    }

    #[tokio::test]
    async fn local_tools_read_a_file_and_run_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let tools = local_tools(dir.path()).unwrap();

        let read = call(&tools, dir.path(), "read_file", json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(read["content"], "hello\n");

        let shell = call(
            &tools,
            dir.path(),
            "run_shell",
            json!({"command": "printf local"}),
        )
        .await
        .unwrap();
        assert_eq!(shell["stdout"], "local");
        assert_eq!(shell["stderr"], "");
        assert_eq!(shell["exit_code"], 0);
    }

    #[tokio::test]
    async fn a_local_shell_cd_persists_for_later_calls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "hello\n").unwrap();
        let tools = local_tools(dir.path()).unwrap();

        let result = call(
            &tools,
            dir.path(),
            "run_shell",
            json!({"command": "cd sub"}),
        )
        .await
        .unwrap();
        let cwd = result["cwd"].as_str().expect("a reported directory");
        assert!(cwd.ends_with("/sub"), "{cwd}");

        let pwd = call(&tools, dir.path(), "run_shell", json!({"command": "pwd"}))
            .await
            .unwrap();
        assert!(
            pwd["stdout"].as_str().unwrap().trim_end().ends_with("/sub"),
            "{pwd}"
        );

        let read = call(&tools, dir.path(), "read_file", json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(read["content"], "hello\n");
    }

    #[tokio::test]
    async fn separate_tool_sets_keep_separate_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let first = local_tools(dir.path()).unwrap();
        let second = local_tools(dir.path()).unwrap();

        call(
            &first,
            dir.path(),
            "run_shell",
            json!({"command": "cd sub"}),
        )
        .await
        .unwrap();

        let pwd = call(&second, dir.path(), "run_shell", json!({"command": "pwd"}))
            .await
            .unwrap();
        assert_eq!(
            pwd["stdout"].as_str().unwrap().trim_end(),
            dir.path().canonicalize().unwrap().display().to_string()
        );
    }

    #[tokio::test]
    async fn local_tools_start_a_background_run_and_wait() {
        let dir = tempfile::tempdir().unwrap();
        let tools = local_tools(dir.path()).unwrap();

        let start = call(
            &tools,
            dir.path(),
            "run_shell",
            json!({"command": "printf waited", "background": true}),
        )
        .await
        .unwrap();
        let id = start["task_id"].as_str().expect("a task id");

        let wait = call(
            &tools,
            dir.path(),
            "tool_wait",
            json!({"id": id, "timeout_secs": 5}),
        )
        .await
        .unwrap();
        assert_eq!(wait["running"], false);
        assert_eq!(wait["exit_code"], 0);
        assert_eq!(wait["stdout"], "waited");
    }
}
