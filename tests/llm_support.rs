//! Light tests for the M3-A support surface: prompt builders, response schemas,
//! the client types/trait, error classification, and the jitter PRNG. The heavy
//! acceptance coverage lives in `llm_canonical`, `llm_retry_policy`, and
//! `llm_pricing`.

use std::time::Duration;

use myansub::llm::canonical_json;
use myansub::llm::client::{
    CommitToken, ContentBlock, ErrorClass, GenerationConfig, LlmClient, LlmError, LlmRequest,
    LlmResponse, ServiceTier, ThinkingSetting, Usage,
};
use myansub::llm::prompts::{
    self, PROMPT_VERSION, TranslationBlock, TranslationLine, analysis_input_line,
};
use myansub::llm::retry::Xorshift;
use myansub::llm::schemas::{analysis_response_schema, translation_response_schema};

#[test]
fn prompt_version_is_one() {
    assert_eq!(PROMPT_VERSION, 1);
}

#[test]
fn translation_system_instruction_states_core_rules() {
    let s = prompts::translation_system_instruction();
    assert!(s.contains("Myanmar"));
    assert!(s.contains("Zawgyi"));
    assert!(s.contains('\u{27E6}')); // placeholder bracket ⟦
    assert!(s.contains("lines"));
}

#[test]
fn analysis_system_instruction_states_core_rules() {
    let s = prompts::analysis_system_instruction();
    assert!(s.contains("English"));
    assert!(s.contains("unknown"));
    assert!(s.to_lowercase().contains("scene"));
}

#[test]
fn analysis_input_line_includes_speaker_only_when_known() {
    assert_eq!(analysis_input_line(104, Some("Aoi"), "Hi"), "[104] Aoi: Hi");
    assert_eq!(analysis_input_line(104, None, "Hi"), "[104] Hi");
    assert_eq!(analysis_input_line(104, Some(""), "Hi"), "[104] Hi");
}

#[test]
fn translation_block_renders_all_sections() {
    let speakers = vec![("c1".to_string(), "Aoi".to_string())];
    let before = vec![TranslationLine {
        idx: 101,
        speaker: Some("Aoi".to_string()),
        text: "Earlier".to_string(),
    }];
    let translate = vec![TranslationLine {
        idx: 104,
        speaker: None,
        text: "Where are you going?".to_string(),
    }];
    let after = vec![TranslationLine {
        idx: 188,
        speaker: None,
        text: "Later".to_string(),
    }];
    let block = TranslationBlock {
        scene: "A tense farewell",
        speakers: &speakers,
        context_before: &before,
        translate: &translate,
        context_after: &after,
    };

    let rendered = block.render();
    assert!(rendered.contains("SCENE: A tense farewell"));
    assert!(rendered.contains("SPEAKERS IN SCENE: c1 (Aoi)"));
    assert!(rendered.contains("CONTEXT BEFORE (reference only, do not translate):"));
    assert!(rendered.contains("[101] Aoi: Earlier"));
    assert!(rendered.contains("TRANSLATE:"));
    assert!(rendered.contains("[104] Where are you going?"));
    assert!(rendered.contains("CONTEXT AFTER (reference only, do not translate):"));
    assert!(rendered.contains("[188] Later"));
}

#[test]
fn translation_block_omits_empty_context_sections() {
    let speakers = vec![("c1".to_string(), "Aoi".to_string())];
    let translate = vec![TranslationLine {
        idx: 1,
        speaker: None,
        text: "Hello".to_string(),
    }];
    let block = TranslationBlock {
        scene: "Opening",
        speakers: &speakers,
        context_before: &[],
        translate: &translate,
        context_after: &[],
    };

    let rendered = block.render();
    // No empty context headers are emitted when their slice is empty.
    assert!(!rendered.contains("CONTEXT BEFORE"));
    assert!(!rendered.contains("CONTEXT AFTER"));
    // The TRANSLATE section is still present.
    assert!(rendered.contains("TRANSLATE:\n[1] Hello"));
    // No stray run of three consecutive newlines (an empty section's blank line).
    assert!(!rendered.contains("\n\n\n"));
}

#[test]
fn translation_schema_has_lines_with_id_and_t() {
    let schema = translation_response_schema();
    let item = &schema["properties"]["lines"]["items"];
    assert_eq!(item["type"], "object");
    assert_eq!(item["properties"]["id"]["type"], "integer");
    assert_eq!(item["properties"]["t"]["type"], "string");
    assert_eq!(schema["required"], serde_json::json!(["lines"]));
}

#[test]
fn analysis_schema_has_fields_and_array_caps() {
    let schema = analysis_response_schema();
    let props = &schema["properties"];
    assert_eq!(props["characters"]["maxItems"], 40);
    assert_eq!(props["address_matrix"]["maxItems"], 80);
    assert_eq!(props["glossary"]["maxItems"], 120);
    assert_eq!(props["scenes"]["maxItems"], 120);
    assert_eq!(props["premise"]["maxLength"], 500);
    // Gender/age enums are modeled.
    let gender = &props["characters"]["items"]["properties"]["gender"]["enum"];
    assert_eq!(gender, &serde_json::json!(["m", "f", "unknown"]));
}

#[test]
fn error_class_maps_each_variant() {
    assert_eq!(
        LlmError::RateLimited {
            retry_after: Some(Duration::from_secs(2))
        }
        .class(),
        ErrorClass::RateLimited {
            retry_after: Some(Duration::from_secs(2))
        }
    );
    assert_eq!(
        LlmError::Server { status: 500 }.class(),
        ErrorClass::Retryable
    );
    assert_eq!(LlmError::Timeout.class(), ErrorClass::Retryable);
    assert_eq!(
        LlmError::Connection("x".into()).class(),
        ErrorClass::Retryable
    );
    assert_eq!(
        LlmError::Transport("x".into()).class(),
        ErrorClass::Retryable
    );
    assert_eq!(LlmError::Shed.class(), ErrorClass::Shed);
    assert_eq!(
        LlmError::Fatal {
            status: 400,
            message: "bad".into()
        }
        .class(),
        ErrorClass::Fatal
    );
    assert_eq!(LlmError::Decode("x".into()).class(), ErrorClass::Fatal);
}

#[test]
fn usage_serde_round_trips() {
    let usage = Usage {
        prompt_tokens: 10,
        cached_tokens: 4,
        output_tokens: 6,
        thinking_tokens: 2,
        cache_hit_local: true,
    };
    let json = serde_json::to_string(&usage).unwrap();
    let back: Usage = serde_json::from_str(&json).unwrap();
    assert_eq!(usage, back);
}

#[test]
fn service_tier_label() {
    assert_eq!(ServiceTier::Flex.label(), "flex");
    assert_eq!(ServiceTier::Standard.label(), "standard");
}

#[test]
fn commit_token_carries_optional_key() {
    assert_eq!(CommitToken::noop().key(), None);
    assert_eq!(CommitToken::for_key("abc").key(), Some("abc"));
}

#[test]
fn request_is_canonicalizable_for_the_cache_key() {
    // The cache key (LLM.md §4) is the canonical JSON of the request fields;
    // confirm a request serializes and canonicalizes without loss.
    let req = LlmRequest {
        model: "gemini-flash-latest".to_string(),
        service_tier: ServiceTier::Flex,
        system_instruction: "sys".to_string(),
        contents: vec![ContentBlock::user("sheet")],
        generation_config: GenerationConfig {
            max_output_tokens: 320,
            response_mime_type: "application/json".to_string(),
            response_schema: Some(translation_response_schema()),
            thinking: ThinkingSetting::Off,
        },
    };
    let value = serde_json::to_value(&req).unwrap();
    let canonical = canonical_json(&value);
    assert!(canonical.contains("gemini-flash-latest"));
    assert!(canonical.contains("application/json"));
}

#[test]
fn xorshift_is_deterministic_and_in_unit_range() {
    let mut a = Xorshift::new(42);
    let mut b = Xorshift::new(42);
    for _ in 0..1000 {
        let x = a.next_f64();
        assert_eq!(x, b.next_f64());
        assert!((0.0..1.0).contains(&x), "value out of [0,1): {x}");
    }
    // A different seed yields a different stream.
    let mut c = Xorshift::new(43);
    assert_ne!(Xorshift::new(42).next_u64(), c.next_u64());
}

/// A trivial in-test transport proving the trait is implementable with static
/// dispatch and that its future is `Send` (the layer stack relies on this).
struct Echo;

impl LlmClient for Echo {
    // An `async fn` here satisfies the trait's `-> impl Future + Send` return and
    // proves the stack can be implemented without `async-trait` (ADR-008).
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        Ok(LlmResponse {
            text: req.system_instruction,
            usage: Usage::default(),
            commit: CommitToken::noop(),
        })
    }
}

fn assert_send<T: Send>(_: &T) {}

#[tokio::test]
async fn trait_future_is_send_and_usable() {
    let echo = Echo;
    let req = LlmRequest {
        model: "m".to_string(),
        service_tier: ServiceTier::Standard,
        system_instruction: "hi".to_string(),
        contents: vec![],
        generation_config: GenerationConfig {
            max_output_tokens: 1,
            response_mime_type: "application/json".to_string(),
            response_schema: None,
            thinking: ThinkingSetting::Off,
        },
    };
    let fut = echo.generate(req);
    assert_send(&fut); // the layer stack spawns these on the multi-thread runtime
    let resp = fut.await.expect("echo succeeds");
    assert_eq!(resp.text, "hi");
}
