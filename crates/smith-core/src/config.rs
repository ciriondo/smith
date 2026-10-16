//! Config is loaded in four layers: built-in defaults, `smith.toml`, `SMITH_*` environment
//! variables and CLI flags, in increasing precedence.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::SmithError;

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
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    default_provider: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    serve: Option<FileServe>,
    providers: Option<BTreeMap<String, FileProvider>>,
}

/// `[serve]` in a file: the same two settings, either of them optional.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileServe {
    bind: Option<IpAddr>,
    port: Option<u16>,
}

/// One provider entry in a file. An entry for a configured provider overrides only the fields
/// it mentions. An entry for a new provider has nothing to inherit from and must be complete.
#[derive(Debug, Deserialize)]
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

    /// Load config, layering defaults, an optional `smith.toml`, then `SMITH_*` environment
    /// variables.
    ///
    /// Each layer overrides only the fields it states. Unknown fields in `smith.toml` are
    /// rejected, so a typo is an error rather than a setting that does nothing.
    pub fn load(path: Option<&Path>) -> Result<Self, SmithError> {
        let mut config = Config::default();

        if let Some(path) = path {
            if path.exists() {
                let text = std::fs::read_to_string(path)?;
                let file: FileConfig = toml::from_str(&text)
                    .map_err(|e| SmithError::Config(format!("{}: {e}", path.display())))?;
                config.overlay(file, path)?;
            }
        }

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
