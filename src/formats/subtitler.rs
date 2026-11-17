//! Primary codec adapter over the `subtitler` 2.x crate.
//!
//! Handles SRT, VTT, ASS, SSA and SBV for both parsing and writing
//! (`docs/DECISIONS.md` ADR-001). ASS/SSA script-info and style tables are
//! captured into [`FormatMeta`] so same-format output can reproduce them
//! (`docs/REQUIREMENTS.md` FR-7).

use std::collections::HashMap;

use subtitler::SubtitleFormat as _;
use subtitler::model::{AssData, AssStyle, Subtitle, SubtitleFile};

use super::codec::{FormatError, SubtitleCodec};
use super::markup;
use crate::domain::{Cue, Document, FormatMeta, SubtitleFormat};

/// Codec backed by the `subtitler` crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubtitlerCodec;

impl SubtitleCodec for SubtitlerCodec {
    fn parse(&self, text: &str, format: SubtitleFormat) -> Result<Document, FormatError> {
        let file = subtitler::parse_bytes_as(text.as_bytes(), to_subtitler_format(format))
            .map_err(|e| FormatError::Parse {
                format,
                message: e.to_string(),
            })?;

        let cues = file
            .subtitles()
            .iter()
            .enumerate()
            .map(|(i, sub)| cue_from_subtitle(i, sub))
            .collect();

        let format_meta = ass_format_meta(&file, format)?;

        Ok(Document {
            format,
            format_meta,
            cues,
        })
    }

    fn write(&self, doc: &Document, format: SubtitleFormat) -> Result<String, FormatError> {
        let subtitles: Vec<Subtitle> = doc.cues.iter().map(subtitle_from_cue).collect();
        let file = build_file(doc, subtitles, format)?;
        Ok(file.to_string_with_format(&to_subtitler_format(format)))
    }
}

/// Builds a [`Cue`] from a `subtitler` [`Subtitle`], deriving the masked fields
/// from the verbatim text (`docs/DATA_MODEL.md` §1).
fn cue_from_subtitle(index: usize, sub: &Subtitle) -> Cue {
    let masked = markup::mask(&sub.text);
    Cue {
        // idx is 1-based and stable; derived from position, not the source file's
        // own numbering, which may be absent or non-contiguous.
        idx: (index as u32) + 1,
        start_ms: sub.start,
        end_ms: sub.end,
        text_raw: sub.text.clone(),
        text_masked: masked.text_masked,
        lead_tags: masked.lead_tags,
        inline_tags: masked.inline_tags,
        style: sub.style.clone(),
        speaker: sub.actor.clone(),
        translatable: masked.translatable,
    }
}

/// Builds a `subtitler` [`Subtitle`] from a [`Cue`], writing the verbatim
/// `text_raw` back out.
fn subtitle_from_cue(cue: &Cue) -> Subtitle {
    let mut sub = Subtitle::new(cue.start_ms, cue.end_ms, &cue.text_raw);
    sub.index = Some(cue.idx as usize);
    sub.style = cue.style.clone();
    sub.actor = cue.speaker.clone();
    sub
}

/// Captures ASS/SSA script-info and styles into [`FormatMeta`] as JSON blobs.
///
/// For non-ASS formats this is `None`. Embedded `[Fonts]` are not retained
/// (the domain [`FormatMeta`] has no slot for them); fixtures carry no fonts.
fn ass_format_meta(
    file: &SubtitleFile,
    format: SubtitleFormat,
) -> Result<Option<FormatMeta>, FormatError> {
    let data = match file {
        SubtitleFile::Ass(data) | SubtitleFile::Ssa(data) => data,
        _ => return Ok(None),
    };
    let script_info = serde_json::to_string(&data.info).map_err(|e| FormatError::Parse {
        format,
        message: format!("serializing script info: {e}"),
    })?;
    let styles = serde_json::to_string(&data.styles).map_err(|e| FormatError::Parse {
        format,
        message: format!("serializing styles: {e}"),
    })?;
    Ok(Some(FormatMeta {
        script_info: Some(script_info),
        styles: Some(styles),
    }))
}

/// Wraps parsed subtitles into the `subtitler` [`SubtitleFile`] variant for
/// `format`, restoring ASS/SSA script-info and styles from [`FormatMeta`].
fn build_file(
    doc: &Document,
    subtitles: Vec<Subtitle>,
    format: SubtitleFormat,
) -> Result<SubtitleFile, FormatError> {
    let file = match format {
        SubtitleFormat::Srt => SubtitleFile::Srt(subtitles),
        SubtitleFormat::Vtt => SubtitleFile::Vtt {
            header: None,
            subtitles,
        },
        SubtitleFormat::Sbv => SubtitleFile::Sbv(subtitles),
        SubtitleFormat::Ass | SubtitleFormat::Ssa => {
            let data = ass_data_from_meta(doc, subtitles, format)?;
            if format == SubtitleFormat::Ass {
                SubtitleFile::Ass(data)
            } else {
                SubtitleFile::Ssa(data)
            }
        }
    };
    Ok(file)
}

/// Reconstructs [`AssData`] from a document's [`FormatMeta`] JSON blobs.
fn ass_data_from_meta(
    doc: &Document,
    subtitles: Vec<Subtitle>,
    format: SubtitleFormat,
) -> Result<AssData, FormatError> {
    let (info, styles) = match &doc.format_meta {
        Some(meta) => {
            let info = decode_meta::<HashMap<String, String>>(meta.script_info.as_deref(), format)?
                .unwrap_or_default();
            let styles =
                decode_meta::<Vec<AssStyle>>(meta.styles.as_deref(), format)?.unwrap_or_default();
            (info, styles)
        }
        None => (HashMap::new(), Vec::new()),
    };
    Ok(AssData {
        info,
        styles,
        fonts: Vec::new(),
        subtitles,
    })
}

/// Deserializes an optional JSON blob from [`FormatMeta`].
fn decode_meta<T: serde::de::DeserializeOwned>(
    raw: Option<&str>,
    format: SubtitleFormat,
) -> Result<Option<T>, FormatError> {
    raw.map(|s| serde_json::from_str::<T>(s))
        .transpose()
        .map_err(|e| FormatError::Write {
            format,
            message: format!("restoring format metadata: {e}"),
        })
}

/// Maps this crate's [`SubtitleFormat`] to the `subtitler` format tag.
fn to_subtitler_format(format: SubtitleFormat) -> subtitler::Format {
    match format {
        SubtitleFormat::Srt => subtitler::Format::Srt,
        SubtitleFormat::Vtt => subtitler::Format::Vtt,
        SubtitleFormat::Ass => subtitler::Format::Ass,
        SubtitleFormat::Ssa => subtitler::Format::Ssa,
        SubtitleFormat::Sbv => subtitler::Format::Sbv,
    }
}
