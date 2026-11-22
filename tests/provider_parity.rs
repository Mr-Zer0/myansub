//! Provider parity (M13-AC18): equivalent fixtures through the Gemini and OpenRouter
//! transports produce equal parsed `LlmResponse` + `Usage`, except the provider-only
//! fields (`cost_usd`, `generation_id`, `served_provider`, `served_tier`,
//! `applied_tier`). Offline: both transports go through `wiremock`.

use std::time::Duration;

use myansub::llm::client::{
    ContentBlock, GenerationConfig, LlmClient, LlmRequest, ServiceTier, ThinkingSetting, Usage,
};
use myansub::llm::gemini::Gemini;
use myansub::llm::openrouter::{OpenRouter, OpenRouterConfig};
use myansub::llm::schemas::translation_response_schema;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEXT: &str = "{\"lines\":[{\"id\":1,\"t\":\"မင်္ဂလာပါ\"}]}";

fn request(model: &str) -> LlmRequest {
    LlmRequest {
        model: model.to_string(),
        service_tier: ServiceTier::Floor,
        system_instruction: "you are a translator".to_string(),
        contents: vec![
            ContentBlock::user("analysis sheet"),
            ContentBlock::user("TRANSLATE:\n[1] hi"),
        ],
        generation_config: GenerationConfig {
            max_output_tokens: 320,
            response_mime_type: "application/json".to_string(),
            response_schema: Some(translation_response_schema()),
            thinking: ThinkingSetting::Off,
        },
    }
}

/// Clears the provider-only fields so two usages can be compared for parity.
fn normalize(mut usage: Usage) -> Usage {
    usage.cost_usd = None;
    usage.generation_id = None;
    usage.served_provider = None;
    usage.served_tier = None;
    usage.applied_tier = None;
    usage
}

#[tokio::test]
async fn gemini_and_openrouter_parse_to_equal_response_and_usage() {
    // Gemini transport + its success body (same token counts as OpenRouter below).
    let gemini_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-flash-latest:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [ { "content": { "parts": [ { "text": TEXT } ] } } ],
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 40,
                "candidatesTokenCount": 20
            }
        })))
        .mount(&gemini_server)
        .await;
    let gemini = Gemini::new(gemini_server.uri(), "k", Duration::from_secs(30)).expect("gemini");

    // OpenRouter transport + an equivalent success body (plus its provider-only extras).
    let or_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "gen-abc",
            "provider": "Anthropic",
            "choices": [ { "message": { "content": TEXT }, "finish_reason": "stop" } ],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cost": 0.0021,
                "prompt_tokens_details": { "cached_tokens": 40 }
            }
        })))
        .mount(&or_server)
        .await;
    let openrouter = OpenRouter::new(
        or_server.uri(),
        "k",
        Duration::from_secs(30),
        OpenRouterConfig::default(),
    )
    .expect("openrouter");

    let g = gemini
        .generate(request("gemini-flash-latest"))
        .await
        .expect("gemini ok");
    let o = openrouter
        .generate(request("openrouter:anthropic/claude-sonnet-5.5"))
        .await
        .expect("openrouter ok");

    // The parsed text is byte-identical.
    assert_eq!(g.text, o.text);
    assert_eq!(g.text, TEXT);

    // Usage parity, ignoring the provider-only fields.
    assert_eq!(normalize(g.usage), normalize(o.usage));

    // The provider-only fields are populated for OpenRouter and absent for Gemini.
    let o2 = openrouter
        .generate(request("openrouter:anthropic/claude-sonnet-5.5"))
        .await
        .expect("openrouter ok");
    assert_eq!(o2.usage.cost_usd, Some(0.0021));
    assert_eq!(o2.usage.generation_id.as_deref(), Some("gen-abc"));
    let g2 = gemini
        .generate(request("gemini-flash-latest"))
        .await
        .expect("gemini ok");
    assert_eq!(g2.usage.cost_usd, None);
    assert_eq!(g2.usage.generation_id, None);
}
