//! The host half of the sandbox provider proxy.
//!
//! A sandboxed child sends its request to the worker's loopback listener. The worker forwards
//! the body over the channel, and this module sends it to the parent's provider with the
//! host's credential. The child holds no key. Only the request body travels from the child;
//! the model, the authorization header, the user agent and the session header are the host's.

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use smith_core::config::ProviderConfig;
use smith_core::{ProviderError, ProviderProxy, ProxyEvent};

/// The largest body chunk sent to the worker in one frame.
const CHUNK: usize = 16 * 1024;

/// An OpenAI-compatible proxy for one parent session.
///
/// It is built per session because the session id and the model are the parent's, and a
/// session outlives any number of child processes.
#[derive(Clone)]
pub struct OpenAiCompatProxy {
    provider: String,
    endpoint: String,
    model: String,
    headers: HeaderMap,
    client: reqwest::Client,
}

impl OpenAiCompatProxy {
    pub fn new(
        id: &str,
        cfg: &ProviderConfig,
        session: &str,
        model: &str,
    ) -> Result<Self, ProviderError> {
        let api_key = std::env::var(&cfg.api_key_env)
            .map_err(|_| ProviderError::MissingApiKey(cfg.api_key_env.clone()))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|_| {
                ProviderError::Transport {
                    provider: id.to_string(),
                    message: format!("{} is not a valid header value", cfg.api_key_env),
                }
            })?,
        );
        if let Some(agent) = &cfg.user_agent {
            headers.insert(
                USER_AGENT,
                HeaderValue::from_str(agent).map_err(|_| ProviderError::Transport {
                    provider: id.to_string(),
                    message: format!("the configured user agent {agent:?} is not a header value"),
                })?,
            );
        }
        if let Some(name) = &cfg.session_header {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Transport {
                    provider: id.to_string(),
                    message: format!("the configured session header {name:?} is not a header name"),
                }
            })?;
            headers.insert(
                name,
                HeaderValue::from_str(session).map_err(|_| ProviderError::Transport {
                    provider: id.to_string(),
                    message: "the session id is not a header value".to_string(),
                })?,
            );
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::Transport {
                provider: id.to_string(),
                message: e.to_string(),
            })?;

        Ok(Self {
            provider: id.to_string(),
            endpoint: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            model: model.to_string(),
            headers,
            client,
        })
    }

    /// The headers sent upstream. Tests read this instead of a socket.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

impl ProviderProxy for OpenAiCompatProxy {
    fn forward(
        &self,
        body: Vec<u8>,
        cancel: CancellationToken,
        events: mpsc::Sender<ProxyEvent>,
    ) -> BoxFuture<'static, Result<(), ProviderError>> {
        let proxy = self.clone();
        Box::pin(async move { proxy.send(body, cancel, events).await })
    }
}

impl OpenAiCompatProxy {
    async fn send(
        &self,
        body: Vec<u8>,
        cancel: CancellationToken,
        events: mpsc::Sender<ProxyEvent>,
    ) -> Result<(), ProviderError> {
        let body = replace_model(&body, &self.model, &self.provider)?;

        let request = self
            .client
            .post(&self.endpoint)
            .headers(self.headers.clone())
            .json(&body);

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = request.send() => result.map_err(|e| ProviderError::Transport {
                provider: self.provider.clone(),
                message: e.to_string(),
            })?,
        };

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        send_event(
            &events,
            ProxyEvent::Head {
                status,
                content_type,
            },
        )
        .await?;

        if !response.status().is_success() {
            // The child's adapter classifies the status; the body carries the provider's
            // explanation.
            let text = response.text().await.unwrap_or_default();
            send_event(&events, ProxyEvent::Data(text.into_bytes())).await?;
            send_event(&events, ProxyEvent::End).await?;
            return Ok(());
        }

        let mut stream = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                chunk = stream.next() => chunk,
            };
            match chunk {
                None => break,
                Some(Err(e)) => {
                    return Err(ProviderError::Transport {
                        provider: self.provider.clone(),
                        message: e.to_string(),
                    })
                }
                Some(Ok(bytes)) => {
                    for part in bytes.chunks(CHUNK) {
                        send_event(&events, ProxyEvent::Data(part.to_vec())).await?;
                    }
                }
            }
        }
        send_event(&events, ProxyEvent::End).await?;
        Ok(())
    }
}

/// Parse the child's body and replace its model with the parent's.
///
/// The child cannot select a model, so a compromised or misconfigured child cannot spend the
/// parent's quota on a different one.
fn replace_model(body: &[u8], model: &str, provider: &str) -> Result<Value, ProviderError> {
    let mut value: Value = serde_json::from_slice(body).map_err(|e| ProviderError::BadRequest {
        provider: provider.to_string(),
        message: format!("the sandboxed child sent an unparseable request body: {e}"),
    })?;
    let Some(object) = value.as_object_mut() else {
        return Err(ProviderError::BadRequest {
            provider: provider.to_string(),
            message: "the sandboxed child sent a request body that is not a JSON object"
                .to_string(),
        });
    };
    object.insert("model".to_string(), json!(model));
    Ok(value)
}

async fn send_event(
    events: &mpsc::Sender<ProxyEvent>,
    event: ProxyEvent,
) -> Result<(), ProviderError> {
    events
        .send(event)
        .await
        .map_err(|_| ProviderError::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        smith_core::config::Config::default()
            .provider("opencode-go")
            .unwrap()
            .clone()
    }

    #[test]
    fn the_parent_model_replaces_the_childs() {
        let value = replace_model(
            br#"{"model":"child-model","messages":[],"stream":true}"#,
            "parent-model",
            "opencode-go",
        )
        .unwrap();
        assert_eq!(value["model"], "parent-model");
        assert_eq!(value["messages"], json!([]));
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn a_malformed_or_non_object_body_is_a_request_error() {
        let error = replace_model(b"not json", "m", "opencode-go").unwrap_err();
        assert!(error.to_string().contains("unparseable"), "{error}");

        let error = replace_model(b"[]", "m", "opencode-go").unwrap_err();
        assert!(error.to_string().contains("not a JSON object"), "{error}");
    }

    /// The key comes from the host environment, never from the child request.
    #[test]
    fn the_headers_carry_the_host_key_user_agent_and_session() {
        // A name outside the SMITH_* namespace, so this test does not race a config test.
        let var = "PROXY_TEST_API_KEY";
        std::env::set_var(var, "sk-test");
        let mut cfg = config();
        cfg.api_key_env = var.to_string();
        cfg.user_agent = Some("smith/0.1".to_string());
        cfg.session_header = Some("x-opencode-session".to_string());

        let proxy = OpenAiCompatProxy::new("opencode-go", &cfg, "session-1", "model-1").unwrap();
        let headers = proxy.headers();
        assert_eq!(headers[AUTHORIZATION], "Bearer sk-test");
        assert_eq!(headers[USER_AGENT], "smith/0.1");
        assert_eq!(headers["x-opencode-session"], "session-1");
        assert!(!headers.contains_key(CONTENT_TYPE));
        assert!(proxy.endpoint.ends_with("chat/completions"));
        assert_eq!(proxy.model, "model-1");

        std::env::remove_var(var);
    }

    #[test]
    fn a_missing_key_is_reported_with_the_variable_name() {
        let mut cfg = config();
        cfg.api_key_env = "PROXY_TEST_DEFINITELY_UNSET".to_string();
        match OpenAiCompatProxy::new("opencode-go", &cfg, "s", "m") {
            Err(ProviderError::MissingApiKey(var)) => {
                assert_eq!(var, "PROXY_TEST_DEFINITELY_UNSET")
            }
            Err(e) => panic!("wrong error variant: {e}"),
            Ok(_) => panic!("expected a missing-key error"),
        }
    }
}
