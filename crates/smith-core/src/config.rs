//! Config is assembled from tiers and overlays.
//!
//! Scalars come from the highest tier that sets them: the system file outranks the user file,
//! which outranks the project file. Sandbox lists intersect across the tiers that set them,
//! so no tier can widen another. `SMITH_MODEL` and `SMITH_PROVIDER` are applied last.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::SmithError;

/// Where the system tier lives.
pub const SYSTEM_CONFIG: &str = "/etc/smith/config.toml";
/// The smallest accepted idle timeout.
const MIN_IDLE: Duration = Duration::from_secs(10);

/// Port `smith serve` binds when no port is configured. 7878 avoids 3000 and 8080, which
/// commonly collide with other services.
pub const DEFAULT_SERVE_PORT: u16 = 7878;

/// Wire protocol implemented by an adapter. Adding a provider requires a variant here and a
/// registry arm in `smith-providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible `/chat/completions` with SSE streaming.
    ///
    /// Renamed explicitly: `rename_all = "snake_case"` would produce `open_ai_compat`.
    #[serde(rename = "openai_compat")]
    OpenAiCompat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Adapter to use.
    pub kind: ProviderKind,
    /// Base URL including the version segment, for example
    /// `https://opencode.ai/inference/go/openai/v1`. The adapter appends `/chat/completions`.
    pub base_url: String,
    /// Name of the environment variable holding the key, never the key itself, so a
    /// committed config file cannot contain a credential.
    pub api_key_env: String,
    /// Default model when the user names only a provider.
    pub default_model: String,
    /// Sent as `User-Agent`. OpenCode Go requires clients to identify themselves instead of
    /// using a generic HTTP library name.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Optional header carrying the conversation id. OpenCode Go requires
    /// `x-opencode-session` for routing and prompt caching.
    #[serde(default)]
    pub session_header: Option<String>,
    /// Model ids to advertise. OpenCode Go has no `/models` endpoint (requests return 404),
    /// so the list is configuration rather than discovery.
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Provider used when a model is addressed without a `provider/` prefix.
    pub default_provider: String,
    /// `provider/model`, or a bare model id resolved against `default_provider`.
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub serve: ServeConfig,
    /// Merged by [`load_tiers`]; not part of the file schema this type deserialises.
    #[serde(skip)]
    pub sandbox: SandboxSettings,
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// `[serve]` holds the address `smith serve` binds when the flags do not supply one.
///
/// This type belongs here rather than in `smith-web` because it is configuration, not
/// transport. The frontend reads it but does not define it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeConfig {
    /// Only a loopback address is accepted. A routable address is refused at startup.
    pub bind: IpAddr,
    pub port: u16,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_SERVE_PORT,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();

        // Verified against the live endpoint: https://opencode.ai/inference/go/openai/v1
        // streams OpenAI `chat.completion.chunk` frames. This is the Go subscription
        // product, not `opencode.ai/zen` (the pay-as-you-go Console).
        providers.insert(
            "opencode-go".to_string(),
            ProviderConfig {
                kind: ProviderKind::OpenAiCompat,
                base_url: "https://opencode.ai/inference/go/openai/v1".to_string(),
                api_key_env: "OPENCODE_API_KEY".to_string(),
                default_model: "glm-5.3-flash".to_string(),
                user_agent: Some("smith/0.1".to_string()),
                session_header: Some("x-opencode-session".to_string()),
                models: [
                    "glm-5.3-flash",
                    "glm-5.3",
                    "deepseek-v4.1-flash",
                    "kimi-k2.7-code",
                    "minimax-m3",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            },
        );

        Self {
            default_provider: "opencode-go".to_string(),
            model: "opencode-go/glm-5.3-flash".to_string(),
            system_prompt: None,
            max_tokens: None,
            temperature: None,
            serve: ServeConfig::default(),
            sandbox: SandboxSettings::default(),
            providers,
        }
    }
}

/// The config file's schema: every field optional, so a file states only what it changes and
/// every other field keeps the built-in default.
///
/// This type is deliberately not [`Config`]. Deserialising into `Config` makes a partial file
/// replace the defaults and fail with `missing field 'default_provider'`. An overlay merged
/// field by field implements the layered precedence.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    default_provider: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    serve: Option<FileServe>,
    project: Option<FileProject>,
    sandbox: Option<FileSandbox>,
    policy: Option<FilePolicy>,
    providers: Option<BTreeMap<String, FileProvider>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProject {
    path: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSandbox {
    idle_timeout: Option<String>,
    network: Option<FileNetwork>,
    mounts: Option<FileMounts>,
    env: Option<FileEnv>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileNetwork {
    allow: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMounts {
    ro: Option<Vec<PathBuf>>,
    rw: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEnv {
    inherit: Option<Vec<String>>,
    set: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePolicy {
    managed: Option<bool>,
}

/// `[serve]` in a file: the same two settings, either of them optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileServe {
    bind: Option<IpAddr>,
    port: Option<u16>,
}

/// One provider entry in a file. An entry for a configured provider overrides only the fields
/// it mentions. An entry for a new provider has nothing to inherit from and must be complete.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProvider {
    kind: Option<ProviderKind>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    default_model: Option<String>,
    user_agent: Option<String>,
    session_header: Option<String>,
    models: Option<Vec<String>>,
}

/// A model reference split into its provider and model halves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl Config {
    /// Resolve a `provider/model` string, falling back to `default_provider` for bare ids.
    pub fn resolve_model(&self, spec: &str) -> Result<ModelRef, SmithError> {
        let (provider, model) = match spec.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => (self.default_provider.clone(), spec.to_string()),
        };
        if !self.providers.contains_key(&provider) {
            return Err(SmithError::Config(format!(
                "unknown provider '{provider}'; configured providers: {}",
                self.providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(ModelRef { provider, model })
    }

    pub fn provider(&self, id: &str) -> Result<&ProviderConfig, SmithError> {
        self.providers
            .get(id)
            .ok_or_else(|| SmithError::Config(format!("unknown provider '{id}'")))
    }

    /// Load one file over the defaults, skipping the machine's tiers. `--config` and tests
    /// use this.
    pub fn load(path: Option<&Path>) -> Result<Self, SmithError> {
        match path {
            Some(path) => load_tiers(&[(Tier::User, path.to_path_buf())]),
            None => load_tiers(&[]),
        }
    }

    /// Load the system, user and project tiers for a workspace, highest priority last.
    pub fn load_for_workspace(workspace: &Path) -> Result<Self, SmithError> {
        let canonical = workspace.canonicalize().map_err(|e| {
            SmithError::Config(format!(
                "cannot resolve the workspace {}: {e}",
                workspace.display()
            ))
        })?;

        let mut tiers = Vec::new();
        let project_path = project_config_path(&canonical)?;
        if project_path.exists() {
            let text = std::fs::read_to_string(&project_path)?;
            let file: FileConfig = toml::from_str(&text)
                .map_err(|e| SmithError::Config(format!("{}: {e}", project_path.display())))?;
            match &file.project {
                Some(project) if project.path == canonical => {
                    tiers.push((Tier::Project, project_path));
                }
                Some(project) => eprintln!(
                    "smith: ignoring {}: it is for {}, and the workspace is {}",
                    project_path.display(),
                    project.path.display(),
                    canonical.display()
                ),
                None => eprintln!(
                    "smith: ignoring {}: it has no [project] path",
                    project_path.display()
                ),
            }
        }

        for (tier, path) in [
            (Tier::User, user_config_path()),
            (Tier::System, PathBuf::from(SYSTEM_CONFIG)),
        ] {
            if path.exists() {
                tiers.push((tier, path));
            }
        }

        load_tiers(&tiers)
    }

    /// Apply a config file's overlay over the defaults, changing the fields it names.
    fn overlay(&mut self, file: FileConfig, path: &Path) -> Result<(), SmithError> {
        if let Some(default_provider) = file.default_provider {
            self.default_provider = default_provider;
        }
        if let Some(model) = file.model {
            self.model = model;
        }
        if let Some(system_prompt) = file.system_prompt {
            self.system_prompt = Some(system_prompt);
        }
        if let Some(max_tokens) = file.max_tokens {
            self.max_tokens = Some(max_tokens);
        }
        if let Some(temperature) = file.temperature {
            self.temperature = Some(temperature);
        }
        if let Some(serve) = file.serve {
            if let Some(bind) = serve.bind {
                self.serve.bind = bind;
            }
            if let Some(port) = serve.port {
                self.serve.port = port;
            }
        }

        for (id, overlay) in file.providers.unwrap_or_default() {
            let entry = self
                .providers
                .entry(id.clone())
                .or_insert_with(ProviderConfig::empty);
            overlay.merge_into(entry, &id, path)?;
        }

        Ok(())
    }
}

impl ProviderConfig {
    /// Create a provider with no fields set. This is only a base for a config-file entry and
    /// is valid only if that entry fills in the required fields.
    fn empty() -> Self {
        Self {
            kind: ProviderKind::OpenAiCompat,
            base_url: String::new(),
            api_key_env: String::new(),
            default_model: String::new(),
            user_agent: None,
            session_header: None,
            models: Vec::new(),
        }
    }
}

impl FileProvider {
    /// Merge this entry into `entry`, overriding the fields it sets.
    fn merge_into(
        self,
        entry: &mut ProviderConfig,
        id: &str,
        path: &Path,
    ) -> Result<(), SmithError> {
        if let Some(kind) = self.kind {
            entry.kind = kind;
        }
        if let Some(base_url) = self.base_url {
            entry.base_url = base_url;
        }
        if let Some(api_key_env) = self.api_key_env {
            entry.api_key_env = api_key_env;
        }
        if let Some(default_model) = self.default_model {
            entry.default_model = default_model;
        }
        if let Some(user_agent) = self.user_agent {
            entry.user_agent = Some(user_agent);
        }
        if let Some(session_header) = self.session_header {
            entry.session_header = Some(session_header);
        }
        if let Some(models) = self.models {
            entry.models = models;
        }

        // Checked after merging: overriding one field of a configured provider passes. A new
        // provider that omits a required field fails here rather than later at request time
        // with an empty URL.
        for (field, value) in [
            ("base_url", &entry.base_url),
            ("api_key_env", &entry.api_key_env),
            ("default_model", &entry.default_model),
        ] {
            if value.is_empty() {
                return Err(SmithError::Config(format!(
                    "{}: provider '{id}' is new, so it has to set {field}",
                    path.display()
                )));
            }
        }

        Ok(())
    }
}

/// Which file a setting came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    System,
    User,
    Project,
}

/// One allowlist entry: a host name, optionally restricted to one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEntry {
    pub host: String,
    pub port: Option<u16>,
}

impl NetworkEntry {
    pub fn matches(&self, host: &str, port: u16) -> bool {
        self.host.eq_ignore_ascii_case(host) && self.port.is_none_or(|wanted| wanted == port)
    }
}

impl FromStr for NetworkEntry {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let text = raw.trim();
        if text.is_empty() {
            return Err("an empty allowlist entry is not valid".to_string());
        }
        if text.matches(':').count() > 1 {
            return Err(format!("'{text}': IPv6 entries are not supported"));
        }
        let (host, port) = match text.rsplit_once(':') {
            Some((host, port)) => {
                let port: u16 = port
                    .parse()
                    .map_err(|_| format!("'{text}': '{port}' is not a port"))?;
                if port == 0 {
                    return Err(format!("'{text}': port 0 is not valid"));
                }
                (host, Some(port))
            }
            None => (text, None),
        };
        if host.is_empty() {
            return Err(format!("'{text}': the host is empty"));
        }
        if host.parse::<IpAddr>().is_ok() {
            return Err(format!(
                "'{text}': IP entries are not supported; use a host name"
            ));
        }
        if host.chars().any(char::is_whitespace) {
            return Err(format!("'{text}': the host contains whitespace"));
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }
}

impl std::fmt::Display for NetworkEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}:{port}", self.host),
            None => f.write_str(&self.host),
        }
    }
}

/// The sandbox settings after merging every tier.
#[derive(Debug, Clone)]
pub struct SandboxSettings {
    pub idle_timeout: Duration,
    pub allow: Vec<NetworkEntry>,
    pub ro_mounts: Vec<PathBuf>,
    pub rw_mounts: Vec<PathBuf>,
    pub env_inherit: Vec<String>,
    pub env_set: BTreeMap<String, String>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(5 * 60),
            allow: Vec::new(),
            ro_mounts: default_ro_mounts(),
            rw_mounts: Vec::new(),
            env_inherit: Vec::new(),
            env_set: BTreeMap::new(),
        }
    }
}

/// The host toolchain directories, mounted read-only when they exist. A file that sets `ro`
/// replaces this list.
fn default_ro_mounts() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    [home.join(".rustup"), home.join(".cargo/bin")]
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

/// The variable deny list. Each name is refused with the reason it is refused.
pub fn denied_env_name(name: &str) -> Option<&'static str> {
    let reason = match name {
        "CONTAINER_HOST" | "DOCKER_HOST" => "a runtime socket is a full escape",
        "SSH_AUTH_SOCK" | "SSH_AGENT_PID" => "it signs commits and mail as the user",
        "GPG_AGENT_INFO" | "KRB5CCNAME" => "it carries a credential into the sandbox",
        "DBUS_SESSION_BUS_ADDRESS" | "XDG_RUNTIME_DIR" => "it reaches the desktop session bus",
        "DISPLAY" | "WAYLAND_DISPLAY" => "it reaches the display server",
        "GIT_SSH_COMMAND" | "GIT_ASKPASS" | "BASH_ENV" | "PROMPT_COMMAND" | "EDITOR" | "PAGER" => {
            "it injects execution into later tool calls"
        }
        "HOME" => "it is computed for the sandbox, not inherited",
        _ if name.starts_with("PODMAN_") => "a runtime socket is a full escape",
        _ if name.starts_with("AWS_") => "it forwards cloud credentials",
        _ if name.ends_with("_TOKEN") => "it forwards a credential",
        _ => return None,
    };
    Some(reason)
}

/// Load explicit tiers, lowest priority first. Each tier names the file it came from.
pub fn load_tiers(tiers: &[(Tier, PathBuf)]) -> Result<Config, SmithError> {
    let mut loaded = Vec::new();
    for (tier, path) in tiers {
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(path)?;
        let file: FileConfig = toml::from_str(&text)
            .map_err(|e| SmithError::Config(format!("{}: {e}", path.display())))?;
        loaded.push((*tier, path.clone(), file));
    }

    let sandbox = merge_sandbox(&loaded)?;
    let mut config = Config::default();
    for (_, path, file) in &loaded {
        config.overlay(file.clone(), path)?;
    }
    config.sandbox = sandbox;

    if let Ok(model) = std::env::var("SMITH_MODEL") {
        if !model.is_empty() {
            config.model = model;
        }
    }
    if let Ok(provider) = std::env::var("SMITH_PROVIDER") {
        if !provider.is_empty() {
            config.default_provider = provider;
        }
    }

    Ok(config)
}

fn merge_sandbox(loaded: &[(Tier, PathBuf, FileConfig)]) -> Result<SandboxSettings, SmithError> {
    // `[policy]` may come from the system or user file, and it changes how a project file is
    // read, so it is decided before the project tier is examined.
    let mut managed = false;
    for (tier, path, file) in loaded {
        if let Some(policy) = &file.policy {
            if *tier == Tier::Project {
                return Err(config_error(
                    path,
                    "[policy] belongs in the system or user config",
                ));
            }
            if policy.managed == Some(true) {
                managed = true;
            }
        }
    }

    let defaults = SandboxSettings::default();
    let mut idle_timeout = defaults.idle_timeout;
    let mut allow_lists: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut inherit_lists: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut env_set = BTreeMap::new();
    let mut ro = Vec::new();
    let mut rw = Vec::new();
    let mut ro_set = false;
    let mut rw_set = false;

    for (tier, path, file) in loaded {
        let Some(sandbox) = &file.sandbox else {
            continue;
        };
        if *tier == Tier::Project {
            if sandbox.mounts.is_some() {
                return Err(config_error(
                    path,
                    "[sandbox.mounts] belongs in the system or user config",
                ));
            }
            let attempts_list = sandbox
                .network
                .as_ref()
                .is_some_and(|network| network.allow.is_some())
                || sandbox
                    .env
                    .as_ref()
                    .is_some_and(|env| env.inherit.is_some());
            if managed && attempts_list {
                return Err(config_error(
                    path,
                    "[policy] managed forbids sandbox lists in a project config",
                ));
            }
        }

        if let Some(text) = &sandbox.idle_timeout {
            idle_timeout = parse_idle_timeout(text).map_err(|e| config_error(path, &e))?;
        }
        if let Some(network) = &sandbox.network {
            if let Some(allow) = &network.allow {
                allow_lists.push((path.clone(), allow.clone()));
            }
        }
        if let Some(mounts) = &sandbox.mounts {
            if let Some(list) = &mounts.ro {
                ro_set = true;
                ro.extend(list.iter().cloned());
            }
            if let Some(list) = &mounts.rw {
                rw_set = true;
                rw.extend(list.iter().cloned());
            }
        }
        if let Some(env) = &sandbox.env {
            if let Some(inherit) = &env.inherit {
                inherit_lists.push((path.clone(), inherit.clone()));
            }
            if let Some(map) = &env.set {
                for (name, value) in map {
                    env_set.insert(name.clone(), value.clone());
                }
            }
        }
    }

    let mut allow = Vec::new();
    for (path, list) in &allow_lists {
        let parsed: Vec<NetworkEntry> = list
            .iter()
            .map(|entry| NetworkEntry::from_str(entry).map_err(|e| config_error(path, &e)))
            .collect::<Result<_, _>>()?;
        if allow.is_empty() && allow_lists.len() > 1 {
            // The first list seeds the intersection; an empty first list means "deny all".
        }
        if allow.is_empty() {
            allow = parsed;
        } else {
            allow.retain(|entry| parsed.contains(entry));
        }
    }
    if allow_lists.len() > 1 && allow_lists.iter().any(|(_, list)| list.is_empty()) {
        allow.clear();
    }

    let inherit = intersect_names(&inherit_lists);
    for name in inherit.iter().chain(env_set.keys()) {
        if let Some(reason) = denied_env_name(name) {
            return Err(SmithError::Config(format!(
                "refusing environment variable {name}: {reason}"
            )));
        }
    }
    for name in &inherit {
        if std::env::var_os(name).is_none() {
            return Err(SmithError::Config(format!(
                "inherited environment variable {name} is not set"
            )));
        }
    }

    Ok(SandboxSettings {
        idle_timeout,
        allow,
        ro_mounts: if ro_set { ro } else { default_ro_mounts() },
        rw_mounts: if rw_set { rw } else { Vec::new() },
        env_inherit: inherit,
        env_set,
    })
}

fn intersect_names(lists: &[(PathBuf, Vec<String>)]) -> Vec<String> {
    let mut result: Option<Vec<String>> = None;
    for (_, list) in lists {
        result = Some(match result {
            None => list.clone(),
            Some(current) => current
                .into_iter()
                .filter(|name| list.contains(name))
                .collect(),
        });
    }
    result.unwrap_or_default()
}

fn config_error(path: &Path, message: &str) -> SmithError {
    SmithError::Config(format!("{}: {message}", path.display()))
}

fn parse_idle_timeout(text: &str) -> Result<Duration, String> {
    let trimmed = text.trim();
    if trimmed.len() < 2 {
        return Err(format!("'{trimmed}' is not a duration; use 30s, 5m or 1h"));
    }
    let (number, unit) = trimmed.split_at(trimmed.len() - 1);
    let value: u64 = number
        .parse()
        .map_err(|_| format!("'{trimmed}' is not a duration; use 30s, 5m or 1h"))?;
    let seconds = match unit {
        "s" => value,
        "m" => value.saturating_mul(60),
        "h" => value.saturating_mul(3600),
        _ => return Err(format!("'{trimmed}': use s, m or h as the unit")),
    };
    let duration = Duration::from_secs(seconds);
    if duration < MIN_IDLE {
        return Err(format!("'{trimmed}' is below the 10-second minimum"));
    }
    Ok(duration)
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
pub fn config_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
}

/// The user tier: `$XDG_CONFIG_HOME/smith/config.toml`.
pub fn user_config_path() -> PathBuf {
    config_home().join("smith/config.toml")
}

/// The project tier for a workspace, keyed by the first 16 hex digits of the sha256 of the
/// canonical root.
pub fn project_config_path(workspace: &Path) -> Result<PathBuf, SmithError> {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_bytes());
    let digest = hasher.finalize();
    let hash: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(config_home()
        .join("smith/projects")
        .join(format!("{hash}.toml")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn bare_model_resolves_to_default_provider() {
        let cfg = Config::default();
        let r = cfg.resolve_model("glm-5.3-flash").unwrap();
        assert_eq!(r.provider, "opencode-go");
        assert_eq!(r.model, "glm-5.3-flash");
    }

    #[test]
    fn qualified_model_keeps_provider() {
        let cfg = Config::default();
        let r = cfg.resolve_model("opencode-go/glm-5.3").unwrap();
        assert_eq!(r.provider, "opencode-go");
        assert_eq!(r.model, "glm-5.3");
    }

    #[test]
    fn unknown_provider_is_an_error_not_a_silent_fallback() {
        let cfg = Config::default();
        let err = cfg.resolve_model("nope/whatever").unwrap_err();
        assert!(err.to_string().contains("unknown provider"), "{err}");
    }

    #[test]
    fn provider_kind_config_value_is_openai_compat() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            kind: ProviderKind,
        }

        // Guards the spelling used in config files. `rename_all = "snake_case"` alone would
        // yield `open_ai_compat`.
        let parsed: Wrapper = toml::from_str("kind = \"openai_compat\"").unwrap();
        assert_eq!(parsed.kind, ProviderKind::OpenAiCompat);

        assert!(toml::from_str::<Wrapper>("kind = \"open_ai_compat\"").is_err());
    }

    #[test]
    fn default_config_points_at_the_go_subscription_endpoint() {
        let cfg = Config::default();
        let p = cfg.provider("opencode-go").unwrap();
        assert_eq!(
            p.base_url, "https://opencode.ai/inference/go/openai/v1",
            "Go is not the zen/Console endpoint"
        );
        assert_eq!(p.api_key_env, "OPENCODE_API_KEY");
    }

    #[test]
    fn env_overrides_model() {
        // This test sets `SMITH_MODEL` and calls `load()`. The value is restored so the test
        // is order-independent. The guard keeps a parallel test from loading a config while
        // this override is in place.
        let _guard = lock_env();
        let prev = std::env::var("SMITH_MODEL").ok();
        std::env::set_var("SMITH_MODEL", "opencode-go/kimi-k2.7-code");
        let cfg = Config::load(None).unwrap();
        assert_eq!(cfg.model, "opencode-go/kimi-k2.7-code");
        match prev {
            Some(v) => std::env::set_var("SMITH_MODEL", v),
            None => std::env::remove_var("SMITH_MODEL"),
        }
    }

    /// `Config::load` reads the process environment, and `env_overrides_model` sets
    /// `SMITH_MODEL`. Every test that loads a config takes this, so none of them can observe
    /// another's environment change.
    static ENV: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        // A poisoned lock only means some other test panicked. The lock is still usable.
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write a config file, load it, and remove it again.
    fn load_str(name: &str, body: &str) -> Result<Config, String> {
        let _guard = lock_env();
        let path = std::env::temp_dir().join(format!("smith-{name}-{}.toml", std::process::id()));
        std::fs::write(&path, body).unwrap();
        let loaded = Config::load(Some(&path)).map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&path);
        loaded
    }

    fn with_config<T>(name: &str, body: &str, use_it: impl FnOnce(&Config) -> T) -> T {
        use_it(&load_str(name, body).expect("the config must load"))
    }

    fn write_tier(name: &str, body: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("smith-tier-{name}-{}.toml", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn load_named(name: &str, tiers: &[(Tier, &str)]) -> Result<Config, String> {
        let _guard = lock_env();
        let mut paths = Vec::new();
        for (index, (tier, body)) in tiers.iter().enumerate() {
            paths.push((*tier, write_tier(&format!("{name}-{index}"), body)));
        }
        let result = load_tiers(&paths).map_err(|e| e.to_string());
        for (_, path) in &paths {
            let _ = std::fs::remove_file(path);
        }
        result
    }

    #[test]
    fn scalars_take_the_highest_tier() {
        let cfg = load_named(
            "scalars",
            &[
                (Tier::Project, "model = \"project-model\"\n"),
                (Tier::User, "model = \"user-model\"\n"),
                (Tier::System, "model = \"system-model\"\n"),
            ],
        )
        .unwrap();
        assert_eq!(cfg.model, "system-model");

        let cfg = load_named(
            "scalars-two",
            &[
                (Tier::Project, "model = \"project-model\"\n"),
                (Tier::User, "model = \"user-model\"\n"),
            ],
        )
        .unwrap();
        assert_eq!(cfg.model, "user-model");
    }

    #[test]
    fn allow_lists_intersect_across_tiers() {
        let cfg = load_named(
            "allow",
            &[
                (
                    Tier::Project,
                    "[sandbox.network]\nallow = [\"b:443\", \"c:443\"]\n",
                ),
                (
                    Tier::System,
                    "[sandbox.network]\nallow = [\"a:443\", \"b:443\"]\n",
                ),
            ],
        )
        .unwrap();
        let names: Vec<String> = cfg.sandbox.allow.iter().map(|e| e.to_string()).collect();
        assert_eq!(names, vec!["b:443".to_string()]);
    }

    #[test]
    fn an_empty_list_in_one_tier_denies_everything() {
        let cfg = load_named(
            "allow-empty",
            &[
                (Tier::Project, "[sandbox.network]\nallow = [\"a:443\"]\n"),
                (Tier::System, "[sandbox.network]\nallow = []\n"),
            ],
        )
        .unwrap();
        assert!(cfg.sandbox.allow.is_empty());
    }

    #[test]
    fn managed_rejects_a_project_list() {
        let error = load_named(
            "managed",
            &[
                (Tier::Project, "[sandbox.network]\nallow = [\"a:443\"]\n"),
                (Tier::System, "[policy]\nmanaged = true\n"),
            ],
        )
        .unwrap_err();
        assert!(error.contains("managed"), "{error}");
    }

    #[test]
    fn a_project_mount_table_is_an_error() {
        let error = load_named(
            "project-mount",
            &[(Tier::Project, "[sandbox.mounts]\nro = [\"/tmp\"]\n")],
        )
        .unwrap_err();
        assert!(error.contains("[sandbox.mounts]"), "{error}");
    }

    #[test]
    fn an_ip_allowlist_entry_is_rejected() {
        let error = load_named(
            "ip",
            &[(Tier::User, "[sandbox.network]\nallow = [\"1.2.3.4\"]\n")],
        )
        .unwrap_err();
        assert!(error.contains("IP entries"), "{error}");
    }

    #[test]
    fn a_denied_environment_name_is_an_error() {
        let error = load_named(
            "denied-env",
            &[(Tier::User, "[sandbox.env]\nset = { EDITOR = \"vi\" }\n")],
        )
        .unwrap_err();
        assert!(error.contains("EDITOR"), "{error}");
        assert!(error.contains("injects execution"), "{error}");
    }

    #[test]
    fn a_missing_inherited_name_is_an_error() {
        let error = load_named(
            "missing-env",
            &[(
                Tier::User,
                "[sandbox.env]\ninherit = [\"SMITH_TEST_MISSING_VARIABLE\"]\n",
            )],
        )
        .unwrap_err();
        assert!(error.contains("SMITH_TEST_MISSING_VARIABLE"), "{error}");
    }

    #[test]
    fn idle_timeout_parses_and_enforces_the_minimum() {
        let cfg = load_named(
            "idle",
            &[(Tier::User, "[sandbox]\nidle_timeout = \"5m\"\n")],
        )
        .unwrap();
        assert_eq!(cfg.sandbox.idle_timeout, Duration::from_secs(300));

        let error = load_named(
            "idle-short",
            &[(Tier::User, "[sandbox]\nidle_timeout = \"5s\"\n")],
        )
        .unwrap_err();
        assert!(error.contains("10-second minimum"), "{error}");
    }

    #[test]
    fn allowlist_entries_match_host_and_port() {
        let open: NetworkEntry = "GitHub.com".parse().unwrap();
        assert!(open.matches("github.com", 443));
        assert!(open.matches("github.com", 22));

        let pinned: NetworkEntry = "index.crates.io:443".parse().unwrap();
        assert!(pinned.matches("index.crates.io", 443));
        assert!(!pinned.matches("index.crates.io", 80));
        assert!(!pinned.matches("static.crates.io", 443));
    }

    #[test]
    fn denied_names_are_named_with_a_reason() {
        assert!(denied_env_name("PODMAN_SOCKET").is_some());
        assert!(denied_env_name("AWS_SECRET_ACCESS_KEY").is_some());
        assert!(denied_env_name("SOME_TOKEN").is_some());
        assert!(denied_env_name("HOME").is_some());
        assert!(denied_env_name("RUST_BACKTRACE").is_none());
    }

    /// A partial config file overrides the fields it names and keeps every default it does
    /// not mention.
    #[test]
    fn a_partial_config_file_merges_with_the_defaults() {
        with_config("partial", "temperature = 0.25\n", |cfg| {
            assert_eq!(cfg.temperature, Some(0.25));
            assert_eq!(
                cfg.default_provider, "opencode-go",
                "unmentioned fields keep their default"
            );
            assert_eq!(cfg.model, "opencode-go/glm-5.3-flash");
            assert_eq!(cfg.serve.port, DEFAULT_SERVE_PORT);
            assert!(
                cfg.providers.contains_key("opencode-go"),
                "the default provider table survives a partial file"
            );
        });
    }

    /// An empty file is legal and leaves every default unchanged.
    #[test]
    fn an_empty_config_file_changes_nothing() {
        with_config("empty", "", |cfg| {
            let default = Config::default();
            assert_eq!(cfg.model, default.model);
            assert_eq!(cfg.default_provider, default.default_provider);
            assert_eq!(cfg.providers.len(), default.providers.len());
        });
    }

    #[test]
    fn a_provider_entry_overrides_only_the_fields_it_names() {
        with_config(
            "provider-override",
            "[providers.opencode-go]\ndefault_model = \"glm-5.3\"\nmodels = [\"glm-5.3\"]\n",
            |cfg| {
                let provider = cfg.provider("opencode-go").unwrap();
                assert_eq!(provider.default_model, "glm-5.3");
                assert_eq!(provider.models, vec!["glm-5.3".to_string()]);
                assert_eq!(
                    provider.base_url, "https://opencode.ai/inference/go/openai/v1",
                    "an inherited field must not be blanked out"
                );
                assert_eq!(provider.api_key_env, "OPENCODE_API_KEY");
            },
        );
    }

    #[test]
    fn a_new_provider_must_be_complete() {
        let error = load_str(
            "new-provider",
            "[providers.other]\nbase_url = \"http://x/v1\"\n",
        )
        .unwrap_err();

        assert!(error.contains("provider 'other'"), "{error}");
        assert!(
            error.contains("api_key_env"),
            "it must name the field: {error}"
        );
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_ignored() {
        let error = load_str("typo", "temperatur = 0.5\n").unwrap_err();

        assert!(
            error.contains("temperatur"),
            "the typo must be named: {error}"
        );
    }

    #[test]
    fn the_serve_section_is_optional_and_loopback_by_default() {
        let default = Config::default();
        assert_eq!(default.serve.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(default.serve.port, DEFAULT_SERVE_PORT);

        with_config("serve", "[serve]\nport = 9999\n", |cfg| {
            assert_eq!(cfg.serve.port, 9999);
            assert_eq!(
                cfg.serve.bind,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                "setting the port must not disturb the bind address"
            );
        });
    }
}
