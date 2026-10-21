//! Provider adapters and the registry that maps config onto them.
//!
//! Adding a provider requires a new module implementing [`smith_core::Provider`], one arm in
//! [`build`] and a `kind` in the user's config.

pub mod openai_compat;
pub mod proxy;
pub mod sse;

use std::sync::Arc;

use smith_core::config::{ProviderConfig, ProviderKind};
use smith_core::{Provider, ProviderError, ProviderProxy};

/// Construct a provider from its config entry.
pub fn build(id: &str, cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> {
    match cfg.kind {
        ProviderKind::OpenAiCompat => Ok(Arc::new(openai_compat::OpenAiCompat::new(id, cfg)?)),
    }
}

/// Construct the host proxy for a sandboxed child.
///
/// The proxy is bound to the parent's provider, model and session. Only an
/// OpenAI-compatible parent is supported: another wire protocol would need a pass-through of
/// its native body and headers.
pub fn build_proxy(
    id: &str,
    cfg: &ProviderConfig,
    session: &str,
    model: &str,
) -> Result<Arc<dyn ProviderProxy>, ProviderError> {
    match cfg.kind {
        ProviderKind::OpenAiCompat => Ok(Arc::new(proxy::OpenAiCompatProxy::new(
            id, cfg, session, model,
        )?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go_config() -> ProviderConfig {
        smith_core::config::Config::default()
            .provider("opencode-go")
            .unwrap()
            .clone()
    }

    /// This checks that `build` produces a provider for the configured kind.
    #[test]
    fn openai_compat_kind_builds() {
        let cfg = go_config();
        assert_eq!(cfg.kind, ProviderKind::OpenAiCompat);
        match build("opencode-go", &cfg) {
            Ok(provider) => assert_eq!(provider.id(), "opencode-go"),
            // Only acceptable when the key is absent from the environment.
            Err(ProviderError::MissingApiKey(_)) => {
                assert!(std::env::var(&cfg.api_key_env).is_err())
            }
            Err(e) => panic!("unexpected build failure: {e}"),
        }
    }

    #[test]
    fn missing_api_key_is_reported_with_the_variable_name() {
        let mut cfg = go_config();
        cfg.api_key_env = "SMITH_DEFINITELY_UNSET_VAR".to_string();
        match build("opencode-go", &cfg) {
            Err(ProviderError::MissingApiKey(var)) => {
                assert_eq!(var, "SMITH_DEFINITELY_UNSET_VAR")
            }
            Err(e) => panic!("wrong error variant: {e}"),
            Ok(_) => panic!("expected a missing-key error"),
        }
    }
}
