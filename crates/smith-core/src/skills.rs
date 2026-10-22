//! The skill catalog and the `read_skill` tool.
//!
//! The catalog is built with the system prompt. The tool resolves a listed name, not a path, so
//! the model cannot use it to read arbitrary files. Project skills live under `.agents` in the
//! workspace, which the sandbox mounts read-only; the global roots are mounted read-only too.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};

use crate::provider::ToolSpec;
use crate::tools::{spec, Tool, ToolContext, ToolError};

/// The most of one skill body the tool returns.
pub const READ_SKILL_MAX_BYTES: usize = 64 * 1024;

/// One skill the listing mentions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: Option<String>,
    pub path: PathBuf,
}

/// The skills visible to one session, sorted and shadowed by the existing root order.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    entries: Vec<SkillEntry>,
}

impl SkillCatalog {
    /// Sort the entries by name. Callers have already removed duplicate names.
    pub fn new(mut entries: Vec<SkillEntry>) -> Self {
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Self { entries }
    }

    pub fn entries(&self) -> &[SkillEntry] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&SkillEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Read the body of one listed skill.
pub struct ReadSkillTool {
    catalog: Arc<SkillCatalog>,
}

impl ReadSkillTool {
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self { catalog }
    }
}

impl Tool for ReadSkillTool {
    fn spec(&self) -> ToolSpec {
        spec("read_skill").expect("read_skill has a spec")
    }

    fn call<'a>(
        &'a self,
        args: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let Some(name) = args.get("name").and_then(Value::as_str) else {
                return Err(ToolError::Message(
                    "read_skill requires a string name".to_string(),
                ));
            };
            let Some(skill) = self.catalog.get(name) else {
                return Err(ToolError::Message(format!(
                    "no skill named '{name}'; the system prompt lists the available skills"
                )));
            };
            let bytes = fs::read(&skill.path)
                .map_err(|e| ToolError::Message(format!("cannot read skill '{name}': {e}")))?;
            let truncated = bytes.len() > READ_SKILL_MAX_BYTES;
            let slice = &bytes[..bytes.len().min(READ_SKILL_MAX_BYTES)];
            let content = String::from_utf8_lossy(slice).to_string();
            Ok(json!({
                "name": skill.name,
                "path": skill.path.display().to_string(),
                "content": content,
                "truncated": truncated,
            })
            .to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tool_with(entry: SkillEntry) -> ReadSkillTool {
        ReadSkillTool::new(Arc::new(SkillCatalog::new(vec![entry])))
    }

    fn context() -> ToolContext {
        ToolContext {
            workspace: PathBuf::from("."),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn a_listed_skill_returns_its_body() {
        let dir = std::env::temp_dir().join(format!("smith-read-skill-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, "---\ndescription: demo\n---\n\nbody\n").unwrap();
        let tool = tool_with(SkillEntry {
            name: "demo".to_string(),
            description: Some("demo".to_string()),
            path: path.clone(),
        });

        let content = tool
            .call(json!({"name": "demo"}), &context())
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["name"], "demo");
        assert!(value["content"].as_str().unwrap().contains("body"));
        assert_eq!(value["truncated"], false);

        let error = tool
            .call(json!({"name": "other"}), &context())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no skill named 'other'"), "{error}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_long_body_is_truncated_at_the_cap() {
        let dir =
            std::env::temp_dir().join(format!("smith-read-skill-long-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, "x".repeat(READ_SKILL_MAX_BYTES + 10)).unwrap();
        let tool = tool_with(SkillEntry {
            name: "long".to_string(),
            description: None,
            path,
        });

        let content = tool
            .call(json!({"name": "long"}), &context())
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(
            value["content"].as_str().unwrap().len(),
            READ_SKILL_MAX_BYTES
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_catalog_sorts_and_finds_entries() {
        let catalog = SkillCatalog::new(vec![
            SkillEntry {
                name: "zeta".to_string(),
                description: None,
                path: Path::new("/z").to_path_buf(),
            },
            SkillEntry {
                name: "alpha".to_string(),
                description: None,
                path: Path::new("/a").to_path_buf(),
            },
        ]);
        assert_eq!(
            catalog
                .entries()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert_eq!(catalog.get("alpha").unwrap().path, Path::new("/a"));
        assert!(catalog.get("missing").is_none());
        assert!(!catalog.is_empty());
        assert!(SkillCatalog::default().is_empty());
    }
}
