//! The `DryRun` transport (`docs/LLM.md` §9): estimates token usage without
//! sending anything over the network.
//!
//! It implements [`LlmClient`] so the real pipeline runs unchanged under
//! `--dry-run` — real batches, real prompts, real local-cache lookups — but each
//! would-be call returns a synthetic [`LlmResponse`] whose [`Usage`] is estimated
//! from the request text. Writing the `llm_calls dry_run = 1` rows is the
//! metered layer's / pipeline's job (M3-C / M5); this transport only estimates.
//!
//! Tokens are estimated as `ceil(utf8_bytes / bytes_per_token)` *per script*
//! (Latin, Myanmar, CJK) because Burmese is far denser per byte than Latin
//! (`docs/LLM.md` §9, `[estimate]` config). Output tokens are estimated from the
//! variable block (the last content block, which carries the lines to translate)
//! scaled by `output_ratio` and capped at the request's `maxOutputTokens`
//! (ADR-029).

use crate::llm::client::{CommitToken, LlmClient, LlmError, LlmRequest, LlmResponse, Usage};

/// Per-script bytes-per-token ratios from the `[estimate]` config
/// (`docs/CONFIG.md`). Kept as a plain struct so this module does not depend on
/// the `config` type (mirrors `retry::RetryConfig`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimator {
    pub bytes_per_token_latin: f64,
    pub bytes_per_token_myanmar: f64,
    pub bytes_per_token_cjk: f64,
}

impl Estimator {
    /// Estimates the token count of `text` as the sum over scripts of
    /// `ceil(script_utf8_bytes / bytes_per_token_script)`.
    pub fn estimate_tokens(&self, text: &str) -> u64 {
        let mut latin_bytes = 0u64;
        let mut myanmar_bytes = 0u64;
        let mut cjk_bytes = 0u64;
        for ch in text.chars() {
            let len = ch.len_utf8() as u64;
            match classify(ch) {
                Script::Myanmar => myanmar_bytes += len,
                Script::Cjk => cjk_bytes += len,
                Script::Latin => latin_bytes += len,
            }
        }
        ceil_div(latin_bytes, self.bytes_per_token_latin)
            + ceil_div(myanmar_bytes, self.bytes_per_token_myanmar)
            + ceil_div(cjk_bytes, self.bytes_per_token_cjk)
    }
}

/// The `--dry-run` transport. Does not perform any I/O.
#[derive(Debug, Clone, Copy)]
pub struct DryRun {
    estimator: Estimator,
    output_ratio: f64,
}

impl DryRun {
    /// Builds a dry-run transport from the per-script estimator and the
    /// translation `output_ratio` (`docs/CONFIG.md`).
    pub fn new(estimator: Estimator, output_ratio: f64) -> Self {
        Self {
            estimator,
            output_ratio,
        }
    }

    /// Estimates [`Usage`] for a request without sending it.
    fn estimate_usage(&self, req: &LlmRequest) -> Usage {
        let mut prompt_tokens = self.estimator.estimate_tokens(&req.system_instruction);
        for block in &req.contents {
            prompt_tokens += self.estimator.estimate_tokens(&block.text);
        }

        // Output is scaled from the variable block (the lines actually
        // translated), not the whole cached prefix; the last block is that
        // variable block in the fixed prompt layout (`docs/LLM.md` §2).
        let source_tokens = req
            .contents
            .last()
            .map(|b| self.estimator.estimate_tokens(&b.text))
            .unwrap_or(0);
        let scaled = (source_tokens as f64 * self.output_ratio).ceil() as u64;
        let output_tokens = scaled.min(u64::from(req.generation_config.max_output_tokens));

        Usage {
            prompt_tokens,
            cached_tokens: 0,
            output_tokens,
            thinking_tokens: 0,
            cache_hit_local: false,
            applied_tier: None,
        }
    }
}

impl LlmClient for DryRun {
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        Ok(LlmResponse {
            text: String::new(),
            usage: self.estimate_usage(&req),
            commit: CommitToken::noop(),
        })
    }
}

enum Script {
    Latin,
    Myanmar,
    Cjk,
}

fn classify(ch: char) -> Script {
    let c = ch as u32;
    // Myanmar + Myanmar Extended-A/B (`docs/LLM.md` §8 ranges).
    if (0x1000..=0x109F).contains(&c)
        || (0xAA60..=0xAA7F).contains(&c)
        || (0xA9E0..=0xA9FF).contains(&c)
    {
        return Script::Myanmar;
    }
    // CJK ideographs, kana, and Hangul — denser than Latin per byte.
    if (0x3040..=0x30FF).contains(&c)      // Hiragana + Katakana
        || (0x3400..=0x4DBF).contains(&c)  // CJK Ext A
        || (0x4E00..=0x9FFF).contains(&c)  // CJK Unified
        || (0xAC00..=0xD7AF).contains(&c)  // Hangul syllables
        || (0xF900..=0xFAFF).contains(&c)
    // CJK compatibility
    {
        return Script::Cjk;
    }
    Script::Latin
}

/// `ceil(bytes / per_token)` with guards against a non-positive ratio.
fn ceil_div(bytes: u64, per_token: f64) -> u64 {
    if bytes == 0 {
        return 0;
    }
    if !per_token.is_finite() || per_token <= 0.0 {
        // A misconfigured ratio should not divide by zero; treat as 1 byte/token.
        return bytes;
    }
    (bytes as f64 / per_token).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::{ContentBlock, GenerationConfig, ServiceTier, ThinkingSetting};

    fn estimator() -> Estimator {
        Estimator {
            bytes_per_token_latin: 4.0,
            bytes_per_token_myanmar: 3.0,
            bytes_per_token_cjk: 3.0,
        }
    }

    #[test]
    fn latin_tokens_use_four_bytes_each() {
        // "hello" = 5 ascii bytes → ceil(5/4) = 2.
        assert_eq!(estimator().estimate_tokens("hello"), 2);
    }

    #[test]
    fn myanmar_is_denser_than_latin_per_byte() {
        // Each Myanmar code point is 3 utf-8 bytes; ratio 3.0 → ~1 token each.
        let text = "\u{1000}\u{1001}\u{1002}"; // 9 bytes → ceil(9/3) = 3
        assert_eq!(estimator().estimate_tokens(text), 3);
    }

    #[test]
    fn scripts_are_summed_independently() {
        // "ab" (2 latin bytes) + one Myanmar char (3 bytes).
        let text = "ab\u{1000}";
        // ceil(2/4)=1 + ceil(3/3)=1 = 2.
        assert_eq!(estimator().estimate_tokens(text), 2);
    }

    #[test]
    fn output_tokens_scale_from_variable_block_and_cap() {
        let dry = DryRun::new(estimator(), 4.0);
        let req = LlmRequest {
            model: "m".to_string(),
            service_tier: ServiceTier::Flex,
            system_instruction: "system".to_string(),
            contents: vec![
                ContentBlock::user("analysis sheet goes here"),
                ContentBlock::user("line"), // 4 bytes → 1 token → *4 = 4 output
            ],
            generation_config: GenerationConfig {
                max_output_tokens: 10,
                response_mime_type: "application/json".to_string(),
                response_schema: None,
                thinking: ThinkingSetting::Off,
            },
        };
        let usage = dry.estimate_usage(&req);
        assert_eq!(usage.output_tokens, 4);
        assert!(usage.prompt_tokens > usage.output_tokens);
        assert_eq!(usage.cached_tokens, 0);
    }

    #[test]
    fn output_tokens_capped_at_max_output_tokens() {
        let dry = DryRun::new(estimator(), 100.0);
        let req = LlmRequest {
            model: "m".to_string(),
            service_tier: ServiceTier::Flex,
            system_instruction: String::new(),
            contents: vec![ContentBlock::user("many words here to translate")],
            generation_config: GenerationConfig {
                max_output_tokens: 5,
                response_mime_type: "application/json".to_string(),
                response_schema: None,
                thinking: ThinkingSetting::Off,
            },
        };
        assert_eq!(dry.estimate_usage(&req).output_tokens, 5);
    }
}
