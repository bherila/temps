use async_trait::async_trait;
use bytes::Bytes;
use std::pin::Pin;
use std::time::Duration;
use tokio_stream::Stream;

use crate::error::AiGatewayError;
use crate::providers::{AiProvider, ProviderCapability, ProviderInfo};
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, EmbeddingRequest, EmbeddingResponse, ModelInfo,
};

/// A provider adapter for any OpenAI API-compatible service.
/// OpenAI, xAI, and any compatible endpoint reuse this
/// implementation — only the `ProviderInfo` and model list differ.
pub struct OpenAiCompatProvider {
    info: ProviderInfo,
    models: Vec<ModelInfo>,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(info: ProviderInfo, models: Vec<ModelInfo>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("Failed to build HTTP client");

        Self {
            info,
            models,
            client,
        }
    }

    fn resolve_base_url(&self, base_url: Option<&str>) -> String {
        base_url
            .unwrap_or(self.info.default_base_url)
            .trim_end_matches('/')
            .to_string()
    }

    pub fn openai() -> Self {
        Self::new(
            ProviderInfo {
                id: "openai",
                display_name: "OpenAI",
                default_base_url: "https://api.openai.com/v1",
                capabilities: &[
                    ProviderCapability::ChatCompletion,
                    ProviderCapability::ChatCompletionStreaming,
                    ProviderCapability::Embeddings,
                    ProviderCapability::ToolUse,
                    ProviderCapability::Vision,
                    ProviderCapability::JsonMode,
                ],
            },
            vec![
                // Frontier models
                model("gpt-5.4", "openai"),
                model("gpt-5.4-pro", "openai"),
                model("gpt-5-mini", "openai"),
                model("gpt-5-nano", "openai"),
                model("gpt-5", "openai"),
                model("gpt-4.1", "openai"),
                model("gpt-4.1-mini", "openai"),
                model("gpt-4.1-nano", "openai"),
                // Reasoning models
                model("o3", "openai"),
                model("o3-pro", "openai"),
                model("o4-mini", "openai"),
                model("o3-mini", "openai"),
                // Previous generation
                model("gpt-4o", "openai"),
                model("gpt-4o-mini", "openai"),
                // Embeddings
                model("text-embedding-3-small", "openai"),
                model("text-embedding-3-large", "openai"),
            ],
        )
    }

    pub fn xai() -> Self {
        Self::new(
            ProviderInfo {
                id: "xai",
                display_name: "xAI",
                default_base_url: "https://api.x.ai/v1",
                capabilities: &[
                    ProviderCapability::ChatCompletion,
                    ProviderCapability::ChatCompletionStreaming,
                    ProviderCapability::ToolUse,
                ],
            },
            vec![
                model("grok-4-1-fast-reasoning", "xai"),
                model("grok-4-1-fast-non-reasoning", "xai"),
                model("grok-code-fast-1", "xai"),
                model("grok-4-fast-reasoning", "xai"),
                model("grok-4-fast-non-reasoning", "xai"),
                model("grok-4-0709", "xai"),
                model("grok-3", "xai"),
                model("grok-3-mini", "xai"),
            ],
        )
    }
}

fn model(id: &str, owned_by: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        object: "model".to_string(),
        owned_by: owned_by.to_string(),
    }
}

#[async_trait]
impl AiProvider for OpenAiCompatProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn supports_model(&self, model: &str) -> bool {
        let model_lower = model.to_lowercase();
        self.models
            .iter()
            .any(|m| model_lower.starts_with(m.id.to_lowercase().split('-').next().unwrap_or("")))
            || crate::providers::route_model_to_provider(model)
                .map(|p| p == self.info.id)
                .unwrap_or(false)
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    async fn chat_completion(
        &self,
        api_key: &str,
        base_url: Option<&str>,
        request: &ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, AiGatewayError> {
        let url = format!("{}/chat/completions", self.resolve_base_url(base_url));

        // For OpenAI-compatible providers, pass the request as-is (minus stream flag)
        let mut req = request.clone();
        req.stream = false;

        // Sanitize request for OpenAI model compatibility
        if self.info.id == "openai" {
            sanitize_openai_request(&mut req);
        }

        let response = self
            .client
            .post(&url)
            .bearer_auth(api_key)
            .json(&req)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read response body".to_string());
            return Err(AiGatewayError::UpstreamError {
                model: request.model.clone(),
                status: status.as_u16(),
                message: body,
            });
        }

        let completion: ChatCompletionResponse =
            response
                .json()
                .await
                .map_err(|e| AiGatewayError::TranslationError {
                    provider: self.info.id.to_string(),
                    reason: format!("Failed to parse response: {}", e),
                })?;

        Ok(completion)
    }

    async fn chat_completion_stream(
        &self,
        api_key: &str,
        base_url: Option<&str>,
        request: &ChatCompletionRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, AiGatewayError>> + Send>>, AiGatewayError>
    {
        let url = format!("{}/chat/completions", self.resolve_base_url(base_url));

        let mut req = request.clone();
        req.stream = true;

        if self.info.id == "openai" {
            sanitize_openai_request(&mut req);
        }

        // Inject stream_options.include_usage so the final chunk includes token counts
        let extra = req.extra.get_or_insert_with(Default::default);
        extra
            .entry("stream_options")
            .or_insert_with(|| serde_json::json!({"include_usage": true}));

        let response = self
            .client
            .post(&url)
            .bearer_auth(api_key)
            .json(&req)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read response body".to_string());
            return Err(AiGatewayError::UpstreamError {
                model: request.model.clone(),
                status: status.as_u16(),
                message: body,
            });
        }

        // Stream bytes directly from upstream — already in OpenAI SSE format
        let stream = response.bytes_stream();
        let model = request.model.clone();

        let mapped = tokio_stream::StreamExt::map(stream, move |result| {
            result.map_err(|e| AiGatewayError::StreamError {
                model: model.clone(),
                reason: e.to_string(),
            })
        });

        Ok(Box::pin(mapped))
    }

    async fn embeddings(
        &self,
        api_key: &str,
        base_url: Option<&str>,
        request: &EmbeddingRequest,
    ) -> Result<EmbeddingResponse, AiGatewayError> {
        if !self
            .info
            .capabilities
            .contains(&ProviderCapability::Embeddings)
        {
            return Err(AiGatewayError::ModelNotFound {
                model: request.model.clone(),
            });
        }

        let url = format!("{}/embeddings", self.resolve_base_url(base_url));

        let response = self
            .client
            .post(&url)
            .bearer_auth(api_key)
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read response body".to_string());
            return Err(AiGatewayError::UpstreamError {
                model: request.model.clone(),
                status: status.as_u16(),
                message: body,
            });
        }

        let embedding: EmbeddingResponse =
            response
                .json()
                .await
                .map_err(|e| AiGatewayError::TranslationError {
                    provider: self.info.id.to_string(),
                    reason: format!("Failed to parse embedding response: {}", e),
                })?;

        Ok(embedding)
    }
}

/// Returns true if the model is an OpenAI o-series reasoning model.
fn is_o_series_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

/// Sanitize a chat completion request for OpenAI API compatibility.
///
/// - All OpenAI models: rewrite `max_tokens` → `max_completion_tokens`
/// - O-series reasoning models (o1, o3, o4-mini, etc.): strip unsupported
///   parameters (`temperature`, `top_p`, `frequency_penalty`, `presence_penalty`)
fn sanitize_openai_request(req: &mut ChatCompletionRequest) {
    // Rewrite max_tokens → max_completion_tokens for all OpenAI models
    if let Some(value) = req.max_tokens.take() {
        let extra = req.extra.get_or_insert_with(Default::default);
        extra
            .entry("max_completion_tokens")
            .or_insert_with(|| serde_json::json!(value));
    }

    // O-series models reject sampling parameters
    if is_o_series_model(&req.model) {
        req.temperature = None;
        req.top_p = None;
        req.frequency_penalty = None;
        req.presence_penalty = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_provider_info() {
        let provider = OpenAiCompatProvider::openai();
        assert_eq!(provider.info().id, "openai");
        assert_eq!(
            provider.info().default_base_url,
            "https://api.openai.com/v1"
        );
        assert!(provider
            .info()
            .capabilities
            .contains(&ProviderCapability::ChatCompletion));
        assert!(provider
            .info()
            .capabilities
            .contains(&ProviderCapability::Embeddings));
    }

    #[test]
    fn test_xai_provider_info() {
        let provider = OpenAiCompatProvider::xai();
        assert_eq!(provider.info().id, "xai");
        assert!(!provider
            .info()
            .capabilities
            .contains(&ProviderCapability::Embeddings));
    }

    #[test]
    fn test_openai_supports_model() {
        let provider = OpenAiCompatProvider::openai();
        assert!(provider.supports_model("gpt-4o"));
        assert!(provider.supports_model("gpt-4o-mini"));
        assert!(!provider.supports_model("claude-sonnet-4-6"));
    }

    #[test]
    fn test_xai_supports_model() {
        let provider = OpenAiCompatProvider::xai();
        assert!(provider.supports_model("grok-3"));
        assert!(!provider.supports_model("gpt-4o"));
    }

    #[test]
    fn test_resolve_base_url() {
        let provider = OpenAiCompatProvider::openai();
        assert_eq!(provider.resolve_base_url(None), "https://api.openai.com/v1");
        assert_eq!(
            provider.resolve_base_url(Some("https://custom.endpoint.com/v1/")),
            "https://custom.endpoint.com/v1"
        );
    }

    #[test]
    fn test_available_models() {
        let provider = OpenAiCompatProvider::openai();
        let models = provider.available_models();
        assert!(!models.is_empty());
        assert!(models.iter().any(|m| m.id == "gpt-4o"));
        assert!(models.iter().all(|m| m.owned_by == "openai"));
    }

    fn test_request(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            n: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            user: None,
            extra: None,
        }
    }

    #[test]
    fn test_sanitize_rewrites_max_tokens() {
        let mut req = test_request("gpt-5-nano");
        req.max_tokens = Some(500);

        sanitize_openai_request(&mut req);

        assert!(req.max_tokens.is_none());
        let extra = req.extra.as_ref().unwrap();
        assert_eq!(
            extra.get("max_completion_tokens").unwrap(),
            &serde_json::json!(500)
        );

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("max_completion_tokens"));
        assert!(!json.contains("\"max_tokens\""));
    }

    #[test]
    fn test_sanitize_noop_when_no_max_tokens() {
        let mut req = test_request("gpt-5-nano");

        sanitize_openai_request(&mut req);

        assert!(req.max_tokens.is_none());
        assert!(req.extra.is_none());
    }

    #[test]
    fn test_sanitize_preserves_existing_max_completion_tokens() {
        let mut extra = serde_json::Map::new();
        extra.insert("max_completion_tokens".to_string(), serde_json::json!(1000));

        let mut req = test_request("gpt-5-nano");
        req.max_tokens = Some(500);
        req.extra = Some(extra);

        sanitize_openai_request(&mut req);

        assert!(req.max_tokens.is_none());
        assert_eq!(
            req.extra
                .as_ref()
                .unwrap()
                .get("max_completion_tokens")
                .unwrap(),
            &serde_json::json!(1000)
        );
    }

    #[test]
    fn test_sanitize_strips_sampling_params_for_o_series() {
        let mut req = test_request("o3");
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        req.frequency_penalty = Some(0.5);
        req.presence_penalty = Some(0.3);
        req.max_tokens = Some(500);

        sanitize_openai_request(&mut req);

        assert!(req.temperature.is_none());
        assert!(req.top_p.is_none());
        assert!(req.frequency_penalty.is_none());
        assert!(req.presence_penalty.is_none());
        assert!(req.max_tokens.is_none());
        assert_eq!(
            req.extra
                .as_ref()
                .unwrap()
                .get("max_completion_tokens")
                .unwrap(),
            &serde_json::json!(500)
        );
    }

    #[test]
    fn test_sanitize_strips_sampling_params_for_o4_mini() {
        let mut req = test_request("o4-mini");
        req.temperature = Some(0.5);
        req.top_p = Some(0.8);

        sanitize_openai_request(&mut req);

        assert!(req.temperature.is_none());
        assert!(req.top_p.is_none());
    }

    #[test]
    fn test_sanitize_keeps_sampling_params_for_gpt() {
        let mut req = test_request("gpt-5-nano");
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        req.frequency_penalty = Some(0.5);
        req.presence_penalty = Some(0.3);

        sanitize_openai_request(&mut req);

        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.top_p, Some(0.9));
        assert_eq!(req.frequency_penalty, Some(0.5));
        assert_eq!(req.presence_penalty, Some(0.3));
    }

    #[test]
    fn test_is_o_series_model() {
        assert!(is_o_series_model("o3"));
        assert!(is_o_series_model("o3-pro"));
        assert!(is_o_series_model("o3-mini"));
        assert!(is_o_series_model("o4-mini"));
        assert!(is_o_series_model("o1-preview"));
        assert!(!is_o_series_model("gpt-5-nano"));
        assert!(!is_o_series_model("gpt-4o"));
        assert!(!is_o_series_model("grok-3"));
    }
}
