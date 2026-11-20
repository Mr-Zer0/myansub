//! The output stage and the `export` command (`docs/REQUIREMENTS.md` FR-7,
//! `docs/CLI.md` §3/§6, `docs/ARCHITECTURE.md` §2/§3).
//!
//! The output stage runs only once the translation review is approved
//! (`docs/ARCHITECTURE.md` §2). It builds the target [`Document`] from the stored
//! cues and translations: for every translatable cue it restores the real inline
//! tags into the masked translation ([`markup::unmask`]) and re-prepends the lead
//! override block; non-translatable cues keep their source text. Timings, style,
//! and speaker are preserved. Same-format output carries the source
//! [`FormatMeta`] so ASS/SSA styles and headers survive (ADR-020), and ASS/SSA
//! output applies the configured Myanmar font to every style (ADR-021).
//!
//! The written file goes through the crash-safe [`atomic_write`]
//! (`docs/ARCHITECTURE.md` §6 rule 4) as UTF-8, with an optional BOM.
//!
//! Lossy conversions are reported, not hidden: cross-format output and a
//! fallback-parsed ASS/SSA source with no `format_meta` both surface an
//! [`OutputWarning`] (ADR-024). The warnings are returned to the caller (and
//! logged at `warn`) rather than only logged, so callers and tests can act on them
//! (M7-AC4). An export is refused when a translatable cue lacks a translation,
//! unless `--allow-incomplete`, in which case the source text is used and the
//! affected cue ids are reported (FR-7).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::domain::{Cue, Document, FormatMeta, SubtitleFormat};
use crate::formats::{codec, markup};
use crate::store::repo_project::{get_output_prefs, get_project, set_output_prefs, set_stage};
use crate::store::repo_translations::{cue_id_by_idx, list_translations};
use crate::store::{Stage, StageStatus, Store, StoreError, atomic_write, load_cues};

use super::error::PipelineError;
use super::input::check_source_unchanged;
use super::orchestrator::{can_run, load_state};

/// The UTF-8 byte-order mark prepended when `output.utf8_bom` is set.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

// ---------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------

/// A non-fatal issue surfaced by the output stage (`docs/REQUIREMENTS.md` FR-7,
/// `docs/CLI.md` §3). Returned to the caller so it can be printed and asserted on,
/// and also logged at `warn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputWarning {
    /// The output format differs from the source format, so styling and headers
    /// present in the source are dropped (`docs/CLI.md` §3, M7-AC4).
    LostStylingCrossFormat {
        from: SubtitleFormat,
        to: SubtitleFormat,
    },
    /// A same-format ASS/SSA export whose source was recovered via the fallback
    /// parser carries no `format_meta`, so styles are written with defaults
    /// (`docs/DECISIONS.md` ADR-024 carry-over).
    LostStylingNoFormatMeta { format: SubtitleFormat },
    /// `--allow-incomplete` was used; these cues had no translation and their
    /// source text was written instead (`docs/REQUIREMENTS.md` FR-7).
    IncompleteCuesUsedSource { cue_ids: Vec<u32> },
}

impl fmt::Display for OutputWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputWarning::LostStylingCrossFormat { from, to } => write!(
                f,
                "cross-format output ({from} → {to}) drops styling and headers from the source"
            ),
            OutputWarning::LostStylingNoFormatMeta { format } => write!(
                f,
                "the {format} source has no stored style metadata (recovered via the fallback \
                 parser); output styling may be lost"
            ),
            OutputWarning::IncompleteCuesUsedSource { cue_ids } => write!(
                f,
                "{} cue(s) had no translation; their source text was used: {cue_ids:?}",
                cue_ids.len()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Output format / path resolution (docs/CLI.md §3)
// ---------------------------------------------------------------------------

/// A resolved output target: the format to write and the path to write it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOutput {
    pub format: SubtitleFormat,
    pub path: PathBuf,
}

/// The default output path `<source-stem>.my.<ext>` next to the source
/// (`docs/CLI.md` §3). The stem drops the source extension, so `movie.ja.srt`
/// with an `srt` output becomes `movie.ja.my.srt`.
pub fn default_output_path(source_path: &Path, format: SubtitleFormat) -> PathBuf {
    let stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let name = format!("{stem}.my.{}", format.extension());
    match source_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// Resolves the output format implied by this command's flags alone
/// (`docs/CLI.md` §3): the explicit `--output-format`, or the format derived from a
/// known `--output` extension when `--output-format` is absent. A `--output`
/// extension that is not a known subtitle format is ignored. When both are given
/// and disagree, this is a usage error (exit 2).
pub fn resolve_output_format_from_flags(
    flag_format: Option<SubtitleFormat>,
    flag_path: Option<&Path>,
) -> Result<Option<SubtitleFormat>, PipelineError> {
    let ext_format = flag_path
        .and_then(Path::extension)
        .and_then(|e| e.to_str())
        .and_then(|e| e.parse::<SubtitleFormat>().ok());
    match (flag_format, ext_format) {
        (Some(flag), Some(ext)) if flag != ext => {
            Err(PipelineError::OutputFormatExtensionMismatch {
                format: flag,
                extension: ext,
            })
        }
        (Some(flag), _) => Ok(Some(flag)),
        (None, ext) => Ok(ext),
    }
}

/// Resolves the output target by precedence: a flag on this command wins over a
/// value stored in the project, which wins over the default (`docs/CLI.md` §3).
/// Format defaults to the source format (ADR-020); the path defaults to
/// [`default_output_path`].
pub fn resolve_output_target(
    flag_format: Option<SubtitleFormat>,
    flag_path: Option<&Path>,
    stored_format: Option<SubtitleFormat>,
    stored_path: Option<&Path>,
    source_path: &Path,
    source_format: SubtitleFormat,
) -> Result<ResolvedOutput, PipelineError> {
    let command_format = resolve_output_format_from_flags(flag_format, flag_path)?;
    let format = command_format.or(stored_format).unwrap_or(source_format);
    let path = flag_path
        .map(Path::to_path_buf)
        .or_else(|| stored_path.map(Path::to_path_buf))
        .unwrap_or_else(|| default_output_path(source_path, format));
    Ok(ResolvedOutput { format, path })
}

// ---------------------------------------------------------------------------
// Document building (pure)
// ---------------------------------------------------------------------------

/// The built output document together with the warnings produced while building it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltOutput {
    pub document: Document,
    pub warnings: Vec<OutputWarning>,
}

/// Builds the target [`Document`] from the source cues and their translations
/// (`docs/REQUIREMENTS.md` FR-7).
///
/// `translations` maps a cue `idx` to its stored masked target text. For each
/// translatable cue the masked text is unmasked against the cue's `inline_tags`
/// and the cue's `lead_tags` are re-prepended; a non-translatable cue keeps its
/// source `text_raw`. Timings, style, and speaker are carried through unchanged.
///
/// Same-format output carries the source `format_meta`; cross-format output drops
/// it and warns. For ASS/SSA output, a non-empty `ass_font_override` is applied to
/// every style (ADR-021). If any translatable cue lacks a translation and
/// `allow_incomplete` is false, this returns [`PipelineError::IncompleteTranslation`];
/// otherwise the source text is used and an [`OutputWarning::IncompleteCuesUsedSource`]
/// lists the affected cues.
#[allow(clippy::too_many_arguments)]
pub fn build_output_document(
    source_format: SubtitleFormat,
    source_format_meta: Option<&FormatMeta>,
    cues: &[Cue],
    translations: &BTreeMap<u32, String>,
    output_format: SubtitleFormat,
    ass_font_override: &str,
    allow_incomplete: bool,
    cue_final_stop: CueFinalStop,
) -> Result<BuiltOutput, PipelineError> {
    let mut out_cues = Vec::with_capacity(cues.len());
    let mut missing = Vec::new();
    let mut used_source = Vec::new();

    for cue in cues {
        let text = if cue.translatable {
            match translations.get(&cue.idx) {
                Some(masked) => apply_cue_final_stop(&restore_text(masked, cue), cue_final_stop),
                None => {
                    if allow_incomplete {
                        used_source.push(cue.idx);
                    } else {
                        missing.push(cue.idx);
                    }
                    cue.text_raw.clone()
                }
            }
        } else {
            cue.text_raw.clone()
        };
        let mut out = cue.clone();
        out.text_raw = text;
        out_cues.push(out);
    }

    if !missing.is_empty() {
        return Err(PipelineError::IncompleteTranslation { cue_ids: missing });
    }

    let mut warnings = Vec::new();
    let same_format = output_format == source_format;
    if !same_format {
        warnings.push(OutputWarning::LostStylingCrossFormat {
            from: source_format,
            to: output_format,
        });
    }

    // Same-format output is the only lossless path (ADR-020): carry the source
    // metadata only when the formats match; drop it (and warn) otherwise.
    let mut format_meta = if same_format {
        source_format_meta.cloned()
    } else {
        None
    };

    // ADR-024 carry-over: a same-format ASS/SSA export with no stored metadata
    // (fallback-parsed source) loses styling even though the format matches.
    if same_format && is_ass_like(output_format) && format_meta.is_none() {
        warnings.push(OutputWarning::LostStylingNoFormatMeta {
            format: output_format,
        });
    }

    // Apply the Myanmar font override to every style (ADR-021); an empty override
    // keeps the original fonts.
    if is_ass_like(output_format)
        && !ass_font_override.is_empty()
        && let Some(meta) = format_meta.as_mut()
        && let Some(styles) = meta.styles.as_deref()
    {
        meta.styles = Some(apply_font_override(styles, ass_font_override)?);
    }

    if !used_source.is_empty() {
        warnings.push(OutputWarning::IncompleteCuesUsedSource {
            cue_ids: used_source,
        });
    }

    Ok(BuiltOutput {
        document: Document {
            format: output_format,
            format_meta,
            cues: out_cues,
        },
        warnings,
    })
}

/// Restores a cue's real tags into its masked translation: unmask the inline
/// placeholders, then re-prepend the lead override block (`docs/DATA_MODEL.md` §1).
fn restore_text(masked: &str, cue: &Cue) -> String {
    let body = markup::unmask(masked, &cue.inline_tags);
    match &cue.lead_tags {
        Some(lead) => format!("{lead}{body}"),
        None => body,
    }
}

/// The Burmese sentence terminator ။ (U+104B) — the cue-final full stop handled at
/// export (`[burmese].cue_final_stop`, ADR-049(e)). The clause separator ၊ is
/// U+104A and is never touched here.
const BURMESE_SENTENCE_STOP: char = '\u{104B}';

/// How a trailing ။ at the end of a cue is handled at export
/// (`[burmese].cue_final_stop`, `docs/CONFIG.md` §6, ADR-049(e)). Applied
/// deterministically here, never by the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CueFinalStop {
    /// Remove one trailing ။ (and trailing whitespace) at the end of the cue.
    #[default]
    Strip,
    /// Leave the cue text unchanged.
    Keep,
}

/// Applies the cue-final ။ policy to one cue's restored body (ADR-049(e)).
///
/// `Keep` returns the text unchanged. `Strip` removes exactly one trailing ။
/// (U+104B) and any whitespace around it at the END of the cue text — for a
/// multi-line cue that is the final line's trailing stop only; a ။ in the middle
/// is never touched. A cue with no trailing ။ is returned unchanged (its trailing
/// whitespace is preserved), so the transform is a no-op when there is no stop to
/// strip.
fn apply_cue_final_stop(text: &str, policy: CueFinalStop) -> String {
    match policy {
        CueFinalStop::Keep => text.to_string(),
        CueFinalStop::Strip => {
            let no_trailing_ws = text.trim_end_matches(char::is_whitespace);
            match no_trailing_ws.strip_suffix(BURMESE_SENTENCE_STOP) {
                Some(body) => body.trim_end_matches(char::is_whitespace).to_string(),
                None => text.to_string(),
            }
        }
    }
}

/// Whether `format` is an ASS-family format carrying a style table.
fn is_ass_like(format: SubtitleFormat) -> bool {
    matches!(format, SubtitleFormat::Ass | SubtitleFormat::Ssa)
}

/// Rewrites the `fontname` of every style in the serialized style table to
/// `font`, leaving all other style fields untouched (ADR-021). The style table is
/// stored as a JSON array (`docs/DECISIONS.md` ADR-023); it is edited as generic
/// JSON so this stays independent of the codec's exact style struct.
fn apply_font_override(styles_json: &str, font: &str) -> Result<String, PipelineError> {
    let mut styles: Vec<serde_json::Value> = serde_json::from_str(styles_json)
        .map_err(|e| unexpected(format!("parse style table for font override: {e}")))?;
    for style in &mut styles {
        if let Some(obj) = style.as_object_mut() {
            obj.insert(
                "fontname".to_string(),
                serde_json::Value::String(font.to_string()),
            );
        }
    }
    serde_json::to_string(&styles).map_err(|e| unexpected(format!("re-serialize style table: {e}")))
}

fn unexpected(message: String) -> PipelineError {
    PipelineError::Store(StoreError::Unexpected(message))
}

// ---------------------------------------------------------------------------
// The output stage (store-driven)
// ---------------------------------------------------------------------------

/// The result of a successful export (`docs/CLI.md` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputReport {
    /// The path the file was written to.
    pub path: PathBuf,
    /// The format written.
    pub format: SubtitleFormat,
    /// The number of cues written.
    pub cue_count: usize,
    /// Non-fatal warnings surfaced while building the output.
    pub warnings: Vec<OutputWarning>,
}

/// Runs the output stage: gate, build, and atomic write (`docs/REQUIREMENTS.md`
/// FR-7, `docs/ARCHITECTURE.md` §2/§6).
///
/// The output gate (translation review approved) is enforced first and maps to
/// exit 3 (`docs/ARCHITECTURE.md` §2). The source hash is then re-checked because
/// cues are read (`docs/ARCHITECTURE.md` §6 rule 5). The built document is
/// serialized by the codec and written atomically (rule 4) as UTF-8, with an
/// optional BOM. Finally the `output` stage is marked completed. Warnings are
/// returned to the caller and logged at `warn`.
#[allow(clippy::too_many_arguments)]
pub async fn run_output(
    store: &Store,
    source_path: &Path,
    resolved: &ResolvedOutput,
    ass_font_override: &str,
    utf8_bom: bool,
    allow_incomplete: bool,
    cue_final_stop: CueFinalStop,
) -> Result<OutputReport, PipelineError> {
    // Gate: output may run only when the translation review is approved.
    let state = load_state(store).await?;
    can_run(Stage::Output, &state)?;

    // Cues are read below, so the source must be unchanged (§6 rule 5).
    check_source_unchanged(store, source_path).await?;

    let (source_format, format_meta, cues, translations) = store
        .call(|conn| {
            let project = get_project(conn)?
                .ok_or_else(|| StoreError::Unexpected("missing project row".to_string()))?;
            let source_format = project
                .source_format
                .parse::<SubtitleFormat>()
                .map_err(|e| StoreError::Unexpected(format!("stored source_format: {e}")))?;
            let format_meta = match project.format_meta.as_deref() {
                Some(json) => Some(
                    serde_json::from_str::<FormatMeta>(json)
                        .map_err(|e| StoreError::Unexpected(format!("parse format_meta: {e}")))?,
                ),
                None => None,
            };
            let cues = load_cues(conn)?;
            let id_by_idx = cue_id_by_idx(conn)?;
            let mut text_by_id: BTreeMap<i64, String> = BTreeMap::new();
            for row in list_translations(conn)? {
                text_by_id.insert(row.cue_id, row.text);
            }
            let mut translations: BTreeMap<u32, String> = BTreeMap::new();
            for (idx, id) in id_by_idx {
                if let Some(text) = text_by_id.get(&id) {
                    translations.insert(idx, text.clone());
                }
            }
            Ok((source_format, format_meta, cues, translations))
        })
        .await?;

    let built = build_output_document(
        source_format,
        format_meta.as_ref(),
        &cues,
        &translations,
        resolved.format,
        ass_font_override,
        allow_incomplete,
        cue_final_stop,
    )?;

    let serialized = codec::write_document(&built.document, resolved.format)?;
    let mut bytes = Vec::with_capacity(serialized.len() + UTF8_BOM.len());
    if utf8_bom {
        bytes.extend_from_slice(&UTF8_BOM);
    }
    bytes.extend_from_slice(serialized.as_bytes());

    write_atomic(&resolved.path, bytes).await?;

    // Record the output stage as completed. The file write is already durable, so
    // a crash before this leaves a valid file and a re-run is a harmless no-op.
    store
        .call(|conn| set_stage(conn, Stage::Output, StageStatus::Completed))
        .await?;

    for warning in &built.warnings {
        tracing::warn!("{warning}");
    }

    Ok(OutputReport {
        path: resolved.path.clone(),
        format: resolved.format,
        cue_count: built.document.cues.len(),
        warnings: built.warnings,
    })
}

/// Writes `bytes` to `path` atomically on the blocking pool (`docs/ARCHITECTURE.md`
/// §6 rule 4, §7).
async fn write_atomic(path: &Path, bytes: Vec<u8>) -> Result<(), PipelineError> {
    let target = path.to_path_buf();
    let write_target = target.clone();
    match tokio::task::spawn_blocking(move || atomic_write(&write_target, &bytes)).await {
        Ok(result) => Ok(result?),
        Err(join) => Err(PipelineError::Io {
            path: target,
            source: std::io::Error::other(join),
        }),
    }
}

/// Resolves and stores the output preferences given at project creation
/// (`docs/CLI.md` §3). The format implied by the flags (explicit or from a known
/// `--output` extension) and the given `--output` path are persisted on the project
/// row; a disagreement between `--output-format` and the extension is a usage error
/// (exit 2). Does nothing when neither flag was given.
pub async fn store_creation_prefs(
    store: &Store,
    flag_format: Option<SubtitleFormat>,
    flag_path: Option<&Path>,
) -> Result<(), PipelineError> {
    if flag_format.is_none() && flag_path.is_none() {
        return Ok(());
    }
    let format = resolve_output_format_from_flags(flag_format, flag_path)?;
    let format_label = format.map(|f| f.label().to_string());
    let path_string = flag_path.map(|p| p.to_string_lossy().into_owned());
    store
        .call(move |conn| set_output_prefs(conn, format_label.as_deref(), path_string.as_deref()))
        .await?;
    Ok(())
}

/// Reads the stored output preferences and parses the format label
/// (`docs/CLI.md` §3). An unparseable stored format label is treated as "no stored
/// format" rather than an error.
pub async fn stored_output_prefs(
    store: &Store,
) -> Result<(Option<SubtitleFormat>, Option<PathBuf>), PipelineError> {
    let (format, path) = store.call(|conn| get_output_prefs(conn)).await?;
    let format = format.and_then(|s| s.parse::<SubtitleFormat>().ok());
    let path = path.map(PathBuf::from);
    Ok((format, path))
}
