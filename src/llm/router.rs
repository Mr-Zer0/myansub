//! The provider-routing transport (`docs/LLM.md` §10.1, ADR-051c).
//!
//! [`Router`] implements [`LlmClient`] by reading the provider off a request's model
//! reference ([`LlmRequest::provider`]) and delegating to the matching transport. The
//! pipeline wraps the `Router` once in `Metered<Cached<Retrying<Router>>>`, so BOTH
//! providers sit under the SAME shared layers (`docs/ARCHITECTURE.md` §5) and share
//! one cache/metering/retry path.
//!
//! A provider whose transport is absent (its key was not set, ADR-052a) is `None`; a
//! request for it fails with a fatal, actionable error — so a missing key fails ONLY
//! when a stage actually dispatches to that provider.

use crate::domain::Provider;
use crate::llm::client::{LlmClient, LlmError, LlmRequest, LlmResponse};
use crate::llm::gemini::Gemini;
use crate::llm::openrouter::OpenRouter;

/// Routes each request to the provider named in its model reference (ADR-051c).
#[derive(Debug, Clone)]
pub struct Router {
    gemini: Option<Gemini>,
    openrouter: Option<OpenRouter>,
    default_provider: Provider,
}

impl Router {
    /// Builds a router over the available transports. A `None` transport means its
    /// key was absent; a request routed to it returns a fatal, actionable error.
    /// `default_provider` resolves an unprefixed reference (though resolved config
    /// refs are already canonical, `docs/LLM.md` §10.1).
    pub fn new(
        gemini: Option<Gemini>,
        openrouter: Option<OpenRouter>,
        default_provider: Provider,
    ) -> Self {
        Self {
            gemini,
            openrouter,
            default_provider,
        }
    }
}

/// The fatal, actionable error for a request routed to a provider with no configured
/// transport (`docs/LLM.md` §10.1, ADR-052a).
fn missing_key_error(provider: Provider) -> LlmError {
    let env = match provider {
        Provider::Gemini => "GEMINI_API_KEY",
        Provider::OpenRouter => "OPENROUTER_API_KEY",
    };
    LlmError::Fatal {
        status: 0,
        message: format!("no {provider} transport configured: set {env}"),
    }
}

impl LlmClient for Router {
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        match req.provider(self.default_provider) {
            Provider::Gemini => match &self.gemini {
                Some(transport) => transport.generate(req).await,
                None => Err(missing_key_error(Provider::Gemini)),
            },
            Provider::OpenRouter => match &self.openrouter {
                Some(transport) => transport.generate(req).await,
                None => Err(missing_key_error(Provider::OpenRouter)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::{ContentBlock, GenerationConfig, ServiceTier, ThinkingSetting};

    fn request(model: &str) -> LlmRequest {
        LlmRequest {
            model: model.to_string(),
            service_tier: ServiceTier::Floor,
            system_instruction: "sys".to_string(),
            contents: vec![ContentBlock::user("sheet"), ContentBlock::user("lines")],
            generation_config: GenerationConfig {
                max_output_tokens: 64,
                response_mime_type: "application/json".to_string(),
                response_schema: None,
                thinking: ThinkingSetting::Off,
            },
        }
    }

    // AC: M13-AC3/AC6 — with neither transport configured, a request routes by its
    // provider prefix and fails with the provider's own actionable message.
    #[tokio::test]
    async fn routes_by_provider_and_names_the_absent_key() {
        let router = Router::new(None, None, Provider::Gemini);

        let gemini_err = router
            .generate(request("gemini:gemini-flash-latest"))
            .await
            .expect_err("no gemini transport");
        assert!(
            gemini_err.to_string().contains("GEMINI_API_KEY"),
            "{gemini_err}"
        );

        let or_err = router
            .generate(request("openrouter:anthropic/claude-sonnet-5.5"))
            .await
            .expect_err("no openrouter transport");
        assert!(
            or_err.to_string().contains("OPENROUTER_API_KEY"),
            "{or_err}"
        );
    }
}
