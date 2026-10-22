//! System prompt composition.
//!
//! The session builds the static parts once: the command appendix, project guidance and the
//! skill listing. The agent adds the sandbox inventory, which comes from the tool seam, and
//! composes the message for each provider request. The section order follows hax:
//!
//! base prompt, append, background tasks, subagents, sandbox, project context, skills.
//!
//! The sandbox section replaces hax's environment section. Its values describe the container
//! the tools run in, not the host. A missing file is skipped rather than failing the session.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Config, PromptSettings};
use crate::skills::{SkillCatalog, SkillEntry};

/// The most bytes taken from one `AGENTS.md`.
pub const AGENTS_MD_MAX_BYTES: usize = 64 * 1024;

/// Frontmatter is metadata only; the skill body is not read for the listing.
const SKILL_FRONTMATTER_MAX_BYTES: usize = 8192;
const SKILL_DESCRIPTION_MAX_BYTES: usize = 1024;

/// The home directory `standard_env` gives tool processes.
pub const SANDBOX_HOME: &str = "/tmp/home";

/// The shell `run_shell` uses.
pub const SANDBOX_SHELL: &str = "/bin/sh";

const TASKS_PROMPT: &str = "\
# Background tasks

A `run_shell` call with `background: true` starts a task and returns its id immediately. The \
command runs without a timeout. Wait on the task whose result you need next with `tool_wait`; \
it returns the output produced since the previous wait plus the status. Stop a task with \
`tool_wait`'s `kill` flag, which also returns the final output. Never pass time with sleep or a \
polling loop; give `tool_wait` a timeout instead. A finished task is not announced, so call \
`tool_wait` to collect its output. Tasks live as long as the sandbox container and do not \
survive it.";

const SUBAGENTS_PROMPT: &str = "\
# Subagents

`smith -p \"<task>\"` (via `run_shell`) runs a fresh smith instance with clean context in this \
workspace and prints its final answer to stdout. Delegate to subagents only when the user asks \
for it. The child runs in this sandbox with the same workspace and tools, inherits this \
session's model, and needs no provider key because the host proxies its calls. For concurrent \
work, launch it with `background: true` and collect the answer with `tool_wait`; a synchronous \
call is simpler when you would otherwise wait immediately. Subagents are slow: a synchronous \
`run_shell` call is capped at 600 seconds, so pass a generous `timeout_secs` or run it in the \
background.";

/// The facts the sandbox section reports.
///
/// `smith-core` cannot depend on `smith-sandbox`, so the caller supplies the image facts and
/// the network summary. `compose` supplies the model id and the command inventory.
#[derive(Debug, Clone, Default)]
pub struct SandboxFacts {
    pub workspace: PathBuf,
    pub home: String,
    pub os: String,
    pub shell: String,
    pub network: String,
}

/// Where the global prompt files live.
#[derive(Debug, Clone, Default)]
pub struct PromptPaths {
    pub config_home: PathBuf,
    pub home: PathBuf,
}

impl PromptPaths {
    pub fn from_env() -> Self {
        Self {
            config_home: crate::config::config_home(),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
        }
    }
}

/// The static parts of the system prompt.
#[derive(Debug, Clone)]
pub struct PromptParts {
    append: Option<String>,
    project_context: Option<String>,
    skills: Option<String>,
    skill_catalog: Arc<SkillCatalog>,
    git_root: Option<PathBuf>,
    settings: PromptSettings,
    facts: SandboxFacts,
}

impl PromptParts {
    /// A prompt with no appended parts. It leaves the base prompt alone and adds no section.
    pub fn none() -> Self {
        Self {
            append: None,
            project_context: None,
            skills: None,
            skill_catalog: Arc::new(SkillCatalog::default()),
            git_root: None,
            settings: PromptSettings {
                agents_md: false,
                skills: false,
                tasks: false,
                subagents: false,
                sandbox: false,
            },
            facts: SandboxFacts::default(),
        }
    }

    /// Read the configured files and build the static parts.
    ///
    /// A missing or unreadable file is skipped, so a session starts with a partial prompt
    /// rather than failing. The git root is resolved once and reused by the project context and
    /// the skill walk.
    pub fn build(
        config: &Config,
        workspace: &Path,
        paths: &PromptPaths,
        facts: SandboxFacts,
    ) -> Self {
        let git_root = find_git_root(workspace);
        let project_context = if config.prompt.agents_md {
            build_project_context(workspace, git_root.as_deref(), paths)
        } else {
            None
        };
        let (skills, skill_catalog) = if config.prompt.skills {
            build_skills(workspace, git_root.as_deref(), paths)
        } else {
            (None, SkillCatalog::default())
        };
        Self {
            append: config.system_prompt_append.clone(),
            project_context,
            skills,
            skill_catalog: Arc::new(skill_catalog),
            git_root,
            settings: config.prompt.clone(),
            facts,
        }
    }

    /// The skills listed by this prompt. The `read_skill` tool uses the same catalog.
    pub fn skill_catalog(&self) -> Arc<SkillCatalog> {
        self.skill_catalog.clone()
    }

    /// Assemble the system message.
    ///
    /// `base` is the configured or `--system` prompt. `fragments` are the tool-provided lines
    /// placed inside the sandbox section. `cwd` is the session working directory and overrides
    /// the workspace in that line. Returns `None` when no part is present.
    pub fn compose(
        &self,
        base: Option<&str>,
        model: &str,
        fragments: &[String],
        cwd: Option<&Path>,
    ) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(base) = base.filter(|text| !text.is_empty()) {
            parts.push(base.to_string());
        }
        if let Some(append) = self.append.as_deref().filter(|text| !text.is_empty()) {
            parts.push(append.to_string());
        }
        if self.settings.tasks {
            parts.push(TASKS_PROMPT.to_string());
        }
        if self.settings.subagents {
            parts.push(SUBAGENTS_PROMPT.to_string());
        }
        if self.settings.sandbox {
            if let Some(section) = self.sandbox_section(model, fragments, cwd) {
                parts.push(section);
            }
        }
        if let Some(project_context) = &self.project_context {
            parts.push(project_context.clone());
        }
        if let Some(skills) = &self.skills {
            parts.push(skills.clone());
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    fn sandbox_section(
        &self,
        model: &str,
        fragments: &[String],
        cwd: Option<&Path>,
    ) -> Option<String> {
        if self.facts.workspace.as_os_str().is_empty() {
            return None;
        }

        let working = cwd
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(&self.facts.workspace);
        let mut lines = vec![format!("- Working directory: {}", display_path(working))];
        if !self.facts.home.is_empty() {
            lines.push(format!("- Home directory: {}", self.facts.home));
        }
        if !self.facts.os.is_empty() {
            lines.push(format!("- Operating system: {}", self.facts.os));
        }
        if !self.facts.shell.is_empty() {
            lines.push(format!("- Command shell: {}", self.facts.shell));
        }
        if !model.is_empty() {
            lines.push(format!("- Model: {model}"));
        }
        match &self.git_root {
            Some(root) => lines.push(format!("- Git repository root: {}", display_path(root))),
            None => lines.push("- Git repository root: no".to_string()),
        }
        let network = if self.facts.network.is_empty() {
            "disabled"
        } else {
            self.facts.network.as_str()
        };
        lines.push(format!("- Network: {network}"));

        let mut section = format!("# Sandbox\n\n{}\n", lines.join("\n"));
        for fragment in fragments.iter().filter(|fragment| !fragment.is_empty()) {
            section.push('\n');
            section.push_str(fragment.trim_end());
            section.push('\n');
        }
        Some(section.trim_end().to_string())
    }
}

/// The global skill roots, which the caller mounts read-only when the section is enabled.
pub fn global_skill_roots(paths: &PromptPaths) -> Vec<PathBuf> {
    vec![
        paths.config_home.join("smith").join("skills"),
        paths.home.join(".agents").join("skills"),
    ]
}

/// The nearest ancestor of `workspace` that holds a `.git` entry. A worktree root has a
/// `.git` file rather than a directory.
pub fn find_git_root(workspace: &Path) -> Option<PathBuf> {
    workspace
        .ancestors()
        .take(64)
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn build_project_context(
    workspace: &Path,
    git_root: Option<&Path>,
    paths: &PromptPaths,
) -> Option<String> {
    let mut files = vec![paths.config_home.join("smith").join("AGENTS.md")];
    for dir in project_dirs(workspace, git_root) {
        files.push(dir.join("AGENTS.md"));
    }

    let mut section = String::from(
        "# Project Context\n\nProject guidance below overrides the assistant defaults above.\n",
    );
    let mut found = false;
    for file in files {
        let Ok(bytes) = fs::read(&file) else {
            continue;
        };
        found = true;
        let truncated = bytes.len() > AGENTS_MD_MAX_BYTES;
        let slice = &bytes[..bytes.len().min(AGENTS_MD_MAX_BYTES)];
        let text = String::from_utf8_lossy(slice).replace('\0', "");
        section.push_str(&format!("\n## {}\n\n", display_path(&file)));
        section.push_str(text.trim_end_matches('\n'));
        section.push('\n');
        if truncated {
            section.push_str("[truncated]\n");
        }
    }

    if found {
        Some(section.trim_end().to_string())
    } else {
        None
    }
}

fn build_skills(
    workspace: &Path,
    git_root: Option<&Path>,
    paths: &PromptPaths,
) -> (Option<String>, SkillCatalog) {
    let mut roots = Vec::new();
    for dir in project_dirs(workspace, git_root) {
        roots.push(dir.join(".agents").join("skills"));
    }
    roots.extend(global_skill_roots(paths));

    let mut skills: Vec<SkillEntry> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || skills.iter().any(|skill| skill.name == name) {
                continue;
            }
            let path = entry.path().join("SKILL.md");
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            skills.push(SkillEntry {
                name,
                description: skill_description(&bytes),
                path,
            });
        }
    }

    if skills.is_empty() {
        return (None, SkillCatalog::default());
    }
    let catalog = SkillCatalog::new(skills);

    let mut section =
        String::from("# Skills\n\nUse `read_skill` with the listed name to read a skill:\n");
    for skill in catalog.entries() {
        section.push('\n');
        match &skill.description {
            Some(description) => section.push_str(&format!(
                "- {}: {} ({})\n",
                clean_line(&skill.name),
                clean_line(description),
                display_path(&skill.path)
            )),
            None => section.push_str(&format!(
                "- {} ({})\n",
                clean_line(&skill.name),
                display_path(&skill.path)
            )),
        }
    }
    (Some(section.trim_end().to_string()), catalog)
}

/// The directories whose `AGENTS.md` files are injected, root first.
fn project_dirs(workspace: &Path, git_root: Option<&Path>) -> Vec<PathBuf> {
    let Some(root) = git_root.filter(|root| workspace.starts_with(root)) else {
        return vec![workspace.to_path_buf()];
    };
    if root == workspace {
        return vec![root.to_path_buf()];
    }

    let mut dirs = vec![root.to_path_buf()];
    let mut current = root.to_path_buf();
    if let Ok(relative) = workspace.strip_prefix(root) {
        for component in relative.components() {
            current.push(component);
            dirs.push(current.clone());
        }
    }
    dirs
}

/// Read the `description` scalar from the skill's YAML frontmatter.
///
/// Only a single-line scalar is handled. Block scalars and folded values are outside the
/// metadata the listing needs.
fn skill_description(bytes: &[u8]) -> Option<String> {
    let slice = &bytes[..bytes.len().min(SKILL_FRONTMATTER_MAX_BYTES)];
    let text = String::from_utf8_lossy(slice);
    let mut lines = text.lines();
    if lines.next()?.trim_end_matches('\r') != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("description:") {
            let value = unquote(value.trim());
            if value.is_empty() {
                return None;
            }
            return Some(cap_description(value));
        }
    }
    None
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// A path shown on one prompt line. A model-created directory can contain a newline, which
/// would otherwise start a new line in a section.
fn display_path(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// One line of model-authored metadata, with control characters replaced by spaces.
fn clean_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn cap_description(value: &str) -> String {
    if value.len() <= SKILL_DESCRIPTION_MAX_BYTES {
        return value.to_string();
    }
    let mut cut = SKILL_DESCRIPTION_MAX_BYTES;
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    value[..cut].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("smith-prompt-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn facts(workspace: &Path) -> SandboxFacts {
        SandboxFacts {
            workspace: workspace.to_path_buf(),
            home: SANDBOX_HOME.to_string(),
            os: "Debian 13.7".to_string(),
            shell: SANDBOX_SHELL.to_string(),
            network: "disabled".to_string(),
        }
    }

    fn paths(config_home: &Path, home: &Path) -> PromptPaths {
        PromptPaths {
            config_home: config_home.to_path_buf(),
            home: home.to_path_buf(),
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn composition_follows_the_hax_order() {
        let dir = TestDir::new();
        let parts = PromptParts {
            append: Some("append text".to_string()),
            project_context: Some("# Project Context\n\nproject".to_string()),
            skills: Some("# Skills\n\n- a (x)".to_string()),
            skill_catalog: Arc::new(SkillCatalog::default()),
            git_root: Some(dir.path().join("root")),
            settings: PromptSettings::default(),
            facts: facts(dir.path()),
        };

        let text = parts
            .compose(
                Some("base text"),
                "model-1",
                &["Available commands: rg".into()],
                None,
            )
            .unwrap();

        let order = [
            "base text",
            "append text",
            "# Background tasks",
            "# Subagents",
            "# Sandbox",
            "# Project Context",
            "# Skills",
        ];
        let mut previous = 0;
        for needle in order {
            let at = text.find(needle).unwrap_or_else(|| panic!("{needle}"));
            assert!(at >= previous, "{needle} is out of order:\n{text}");
            previous = at;
        }
    }

    #[test]
    fn sandbox_section_reports_the_environment() {
        let parts = PromptParts {
            append: None,
            project_context: None,
            skills: None,
            skill_catalog: Arc::new(SkillCatalog::default()),
            git_root: None,
            settings: PromptSettings {
                agents_md: false,
                skills: false,
                tasks: false,
                subagents: false,
                sandbox: true,
            },
            facts: SandboxFacts {
                workspace: PathBuf::from("/work"),
                home: SANDBOX_HOME.to_string(),
                os: "Debian 13.7".to_string(),
                shell: SANDBOX_SHELL.to_string(),
                network: "allowlist: index.crates.io:443".to_string(),
            },
        };

        let text = parts
            .compose(
                None,
                "deepseek-v4.1-flash",
                &["Available commands: bash, smith".into()],
                None,
            )
            .unwrap();

        assert!(text.starts_with("# Sandbox\n\n"), "{text}");
        assert!(text.contains("- Working directory: /work"), "{text}");
        assert!(text.contains("- Home directory: /tmp/home"), "{text}");
        assert!(text.contains("- Operating system: Debian 13.7"), "{text}");
        assert!(text.contains("- Command shell: /bin/sh"), "{text}");
        assert!(text.contains("- Model: deepseek-v4.1-flash"), "{text}");
        assert!(text.contains("- Git repository root: no"), "{text}");
        assert!(
            text.contains("- Network: allowlist: index.crates.io:443"),
            "{text}"
        );
        assert!(text.ends_with("Available commands: bash, smith"), "{text}");
    }

    #[test]
    fn sandbox_reports_the_current_directory() {
        let parts = PromptParts {
            append: None,
            project_context: None,
            skills: None,
            skill_catalog: Arc::new(SkillCatalog::default()),
            git_root: None,
            settings: PromptSettings {
                agents_md: false,
                skills: false,
                tasks: false,
                subagents: false,
                sandbox: true,
            },
            facts: SandboxFacts {
                workspace: PathBuf::from("/work"),
                home: SANDBOX_HOME.to_string(),
                os: "Debian 13.7".to_string(),
                shell: SANDBOX_SHELL.to_string(),
                network: "disabled".to_string(),
            },
        };

        let text = parts
            .compose(None, "model", &[], Some(Path::new("/work/sub")))
            .unwrap();
        assert!(text.contains("- Working directory: /work/sub"), "{text}");

        let injected = parts
            .compose(
                None,
                "model",
                &[],
                Some(Path::new("/work/sub\n- Injected: yes")),
            )
            .unwrap();
        assert!(
            injected.contains("- Working directory: /work/sub?- Injected: yes"),
            "{injected}"
        );
        assert_eq!(
            injected
                .lines()
                .filter(|line| line.contains("Injected: yes"))
                .count(),
            1,
            "{injected}"
        );
    }

    #[test]
    fn project_context_runs_from_global_to_workspace() {
        let dir = TestDir::new();
        let root = dir.path().join("project");
        let workspace = root.join("a").join("b");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        let config_home = dir.path().join("config");
        let paths = paths(&config_home, dir.path());

        write(&config_home.join("smith/AGENTS.md"), "global rules\n");
        write(&root.join("AGENTS.md"), "root rules\n");
        write(&workspace.join("AGENTS.md"), "workspace rules\n");

        let section = build_project_context(&workspace, Some(&root), &paths).unwrap();
        let global = section.find("global rules").unwrap();
        let root_at = section.find("root rules").unwrap();
        let workspace_at = section.find("workspace rules").unwrap();
        assert!(global < root_at && root_at < workspace_at, "{section}");
        assert!(
            section.contains("Project guidance below overrides the assistant defaults above."),
            "{section}"
        );
    }

    #[test]
    fn project_context_outside_a_repository_reads_the_workspace_file() {
        let dir = TestDir::new();
        let workspace = dir.path().join("scratch");
        fs::create_dir_all(&workspace).unwrap();
        write(&workspace.join("AGENTS.md"), "local rules\n");
        let paths = paths(&dir.path().join("config"), dir.path());

        let section = build_project_context(&workspace, None, &paths).unwrap();
        assert!(section.contains("local rules"), "{section}");
        assert_eq!(section.matches("## ").count(), 1, "{section}");
    }

    #[test]
    fn project_context_marks_a_truncated_file() {
        let dir = TestDir::new();
        let workspace = dir.path().join("big");
        fs::create_dir_all(&workspace).unwrap();
        let body = "x".repeat(AGENTS_MD_MAX_BYTES + 10);
        write(&workspace.join("AGENTS.md"), &body);
        let paths = paths(&dir.path().join("config"), dir.path());

        let section = build_project_context(&workspace, None, &paths).unwrap();
        assert!(section.ends_with("[truncated]"), "{section}");
        assert!(
            section.len() < AGENTS_MD_MAX_BYTES + 2000,
            "cap is not applied"
        );
    }

    #[test]
    fn skills_are_sorted_and_the_nearest_root_wins() {
        let dir = TestDir::new();
        let workspace = dir.path().join("workspace");
        let config_home = dir.path().join("config");
        let home = dir.path().join("home");
        let paths = paths(&config_home, &home);

        write(
            &workspace.join(".agents/skills/beta/SKILL.md"),
            "---\ndescription: \"second skill\"\n---\nbody\n",
        );
        write(
            &workspace.join(".agents/skills/alpha/SKILL.md"),
            "---\nname: alpha\n---\nbody\n",
        );
        write(
            &config_home.join("smith/skills/alpha/SKILL.md"),
            "---\ndescription: shadowed\n---\nbody\n",
        );
        write(
            &home.join(".agents/skills/gamma/SKILL.md"),
            "---\ndescription: third skill\n---\nbody\n",
        );

        let (section, catalog) = build_skills(&workspace, None, &paths);
        let section = section.unwrap();
        assert_eq!(
            catalog
                .entries()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
        assert!(section.contains("`read_skill`"), "{section}");
        let alpha = section.find("- alpha (").unwrap();
        let beta = section.find("- beta: second skill (").unwrap();
        let gamma = section.find("- gamma: third skill (").unwrap();
        assert!(alpha < beta && beta < gamma, "{section}");
        assert!(
            section.contains(&format!(
                "- alpha ({})",
                workspace.join(".agents/skills/alpha/SKILL.md").display()
            )),
            "{section}"
        );
        assert!(!section.contains("shadowed"), "{section}");
    }

    #[test]
    fn the_skill_toggle_omits_only_that_section() {
        let dir = TestDir::new();
        let workspace = dir.path().join("workspace");
        write(
            &workspace.join(".agents/skills/alpha/SKILL.md"),
            "---\ndescription: first\n---\n",
        );
        let mut config = Config::default();
        config.prompt.skills = false;
        let parts = PromptParts::build(
            &config,
            &workspace,
            &paths(&dir.path().join("config"), dir.path()),
            facts(&workspace),
        );

        let text = parts.compose(Some("base"), "model", &[], None).unwrap();
        assert!(!text.contains("# Skills"), "{text}");
        assert!(text.contains("# Sandbox"), "{text}");
        assert!(text.contains("# Project Context") || !workspace.join("AGENTS.md").exists());
    }

    #[test]
    fn git_root_is_the_nearest_ancestor_with_a_git_entry() {
        let dir = TestDir::new();
        let root = dir.path().join("repo");
        let workspace = root.join("crates").join("one");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(root.join(".git"), "gitdir: elsewhere\n").unwrap();

        assert_eq!(find_git_root(&workspace).as_deref(), Some(root.as_path()));
    }

    #[test]
    fn an_empty_prompt_adds_nothing() {
        let parts = PromptParts::none();
        assert_eq!(parts.compose(None, "model", &[], None), None);
        assert_eq!(
            parts
                .compose(Some("only the base"), "model", &[], None)
                .as_deref(),
            Some("only the base")
        );
    }
}
