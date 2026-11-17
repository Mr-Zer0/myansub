//! Prompt builders and the [`PROMPT_VERSION`] constant (`docs/LLM.md` §2).
//!
//! These are pure string/structure builders used by the analysis and translation
//! pipelines (M5/M6). The content above the cached-prefix line must be
//! byte-identical across all calls of a stage, so everything here is
//! deterministic. Any change to a prompt or the response schema must bump
//! [`PROMPT_VERSION`], which is part of every cache key (`CLAUDE.md`,
//! `docs/LLM.md` §2/§4).

/// The prompt/schema version. Bump on any change to a system instruction, the
/// variable-block layout, or a response schema.
///
/// v2: the translation content-retry reminders became tier-specific
/// ([`translation_missing_ids_reminder`], [`translation_per_line_reminder`]).
pub const PROMPT_VERSION: u32 = 2;

/// The static system instruction for the analysis stage (`docs/LLM.md` §2,
/// "Analysis prompt"). English prose; the output is the analysis document
/// (`docs/DATA_MODEL.md` §3).
pub fn analysis_system_instruction() -> String {
    [
        "You analyze a subtitle transcript to prepare it for translation into Burmese (Myanmar).",
        "Produce a single analysis document as JSON matching the provided schema.",
        "Write all prose fields in English. Write self_ref, addressee_ref, particle, and glossary targets in Burmese (Unicode, never Zawgyi).",
        "Mark a character's gender as \"unknown\" when the text is insufficient. Do not guess.",
        "List concrete ambiguities that would change the Burmese output.",
        "Scenes must cover the cue range contiguously.",
        "Return only JSON matching the schema.",
    ]
    .join("\n")
}

/// The static system instruction for the translation stage (`docs/LLM.md` §2,
/// "System instruction for translation").
pub fn translation_system_instruction() -> String {
    [
        "You translate subtitle lines into natural, spoken Myanmar (Burmese) in Unicode. Never use Zawgyi.",
        "Choose pronouns, kinship terms, register, and sentence-final particles from the address matrix. Infer the speaker from the scene speakers and the dialogue when not given.",
        "Keep glossary terms marked keep: true unchanged. Use glossary targets consistently.",
        "Preserve every \u{27E6}n\u{27E7} placeholder exactly once.",
        "Translate each line only from its own source. Never move text between lines, even when a sentence spans several cues.",
        "Keep lines short enough to read; subtitles are not prose.",
        "Return only JSON matching the schema: {\"lines\":[{\"id\":\u{2026},\"t\":\"\u{2026}\"}]}.",
    ]
    .join("\n")
}

/// The strict reminder appended to a unit's request on a content retry
/// (`docs/LLM.md` §6). This is prompt text: any edit must bump [`PROMPT_VERSION`].
pub fn strict_retry_reminder() -> String {
    "Your previous response was not valid JSON matching the required analysis schema. Respond with \
     ONLY a single JSON object matching the analysis document schema — no prose, no code fences."
        .to_string()
}

/// The tier-1 reminder appended to a translation batch's request when the previous
/// response was structurally invalid JSON (`docs/LLM.md` §6.1). This is prompt text:
/// any edit must bump [`PROMPT_VERSION`].
pub fn translation_retry_reminder() -> String {
    "Your previous response was not valid JSON matching the required schema. Respond with ONLY a \
     single JSON object of the form {\"lines\":[{\"id\":<integer>,\"t\":\"<translation>\"}]} — one \
     entry per line to translate, no prose, no code fences."
        .to_string()
}

/// The tier-2 reminder appended when the previous response was valid JSON but omitted
/// some requested ids (`docs/LLM.md` §6.2). The response *was* parseable, so this
/// targets coverage rather than JSON shape. This is prompt text: any edit must bump
/// [`PROMPT_VERSION`].
pub fn translation_missing_ids_reminder() -> String {
    "Your previous response was missing some of the requested lines. Return every requested id \
     exactly once, and output only the ids listed below — no others, no prose, no code fences."
        .to_string()
}

/// The tier-3 reminder appended when the previous response covered the ids but a line
/// failed a per-line quality check (`placeholder_mismatch`, `untranslated`,
/// `zawgyi_suspect`; `docs/LLM.md` §6.3). This is prompt text: any edit must bump
/// [`PROMPT_VERSION`].
pub fn translation_per_line_reminder() -> String {
    "Re-translate the lines below. Preserve every \u{27E6}n\u{27E7} placeholder exactly once, and \
     output natural spoken Myanmar (Burmese) in Unicode — never Zawgyi, never the untranslated \
     source. Output only the ids listed below, no prose, no code fences."
        .to_string()
}

/// The instruction heading the map-reduce reduce call's variable block
/// (`docs/LLM.md` §2). This is prompt text: any edit must bump [`PROMPT_VERSION`].
pub fn analysis_reduce_instruction() -> String {
    "Merge the following partial analysis documents, listed in chunk order, into one complete \
     analysis document. Reconcile characters, address matrix, glossary, scenes (contiguous over the \
     whole cue range), style rules, and ambiguities. Return ONLY the merged JSON document."
        .to_string()
}

/// Formats one transcript line for the analysis input as `[idx] Speaker: text`,
/// or `[idx] text` when the speaker is unknown (`docs/LLM.md` §2).
pub fn analysis_input_line(idx: u32, speaker: Option<&str>, text: &str) -> String {
    match speaker {
        Some(name) if !name.is_empty() => format!("[{idx}] {name}: {text}"),
        _ => format!("[{idx}] {text}"),
    }
}

/// A single line in a translation variable block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationLine {
    pub idx: u32,
    /// The ASS Name/Actor speaker, when known (`docs/LLM.md` §2).
    pub speaker: Option<String>,
    pub text: String,
}

impl TranslationLine {
    fn render(&self) -> String {
        analysis_input_line(self.idx, self.speaker.as_deref(), &self.text)
    }
}

/// The inputs for one translation batch's variable block (`docs/LLM.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationBlock<'a> {
    pub scene: &'a str,
    /// Scene speakers as `(id, name)` pairs, e.g. `("c1", "Aoi")`.
    pub speakers: &'a [(String, String)],
    pub context_before: &'a [TranslationLine],
    pub translate: &'a [TranslationLine],
    pub context_after: &'a [TranslationLine],
}

impl TranslationBlock<'_> {
    /// Renders the variable block as laid out in `docs/LLM.md` §2. A context
    /// section is omitted entirely when its line slice is empty, so no empty
    /// header or stray blank line is emitted.
    pub fn render(&self) -> String {
        let speakers = self
            .speakers
            .iter()
            .map(|(id, name)| format!("{id} ({name})"))
            .collect::<Vec<_>>()
            .join(", ");

        let render_lines = |lines: &[TranslationLine]| {
            lines
                .iter()
                .map(TranslationLine::render)
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Each present section is a block of non-empty text; sections are joined
        // with a blank line between them, so no empty section leaves a gap.
        let mut sections: Vec<String> = Vec::new();
        sections.push(format!(
            "SCENE: {}\nSPEAKERS IN SCENE: {speakers}",
            self.scene
        ));
        if !self.context_before.is_empty() {
            sections.push(format!(
                "CONTEXT BEFORE (reference only, do not translate):\n{}",
                render_lines(self.context_before)
            ));
        }
        sections.push(format!("TRANSLATE:\n{}", render_lines(self.translate)));
        if !self.context_after.is_empty() {
            sections.push(format!(
                "CONTEXT AFTER (reference only, do not translate):\n{}",
                render_lines(self.context_after)
            ));
        }
        sections.join("\n\n")
    }
}
