//! Fallback codec adapter over the `rsubs-lib` 0.4.x crate.
//!
//! Covers SRT, VTT, SSA and ASS only; SBV has no fallback (`docs/DECISIONS.md`
//! ADR-001). This adapter is reached only when the primary (`subtitler`) fails
//! to *parse* otherwise-usable input. Writing is delegated to the primary codec:
//! `subtitler` serializes every accepted format, and once a [`Document`] exists
//! its origin parser is irrelevant. This keeps us to approved dependencies —
//! constructing `rsubs-lib` timestamps would require the unapproved `time`
//! crate — without hand-writing a serializer.

use super::codec::{FormatError, SubtitleCodec};
use super::markup;
use super::subtitler::SubtitlerCodec;
use crate::domain::{Cue, Document, SubtitleFormat};

/// Converts an `rsubs-lib` timestamp (a `time::Time`) to milliseconds.
///
/// `time` is a transitive, not a direct, dependency, so the type is never named;
/// the macro expands to inherent accessor calls on the concrete value.
macro_rules! time_ms {
    ($t:expr) => {
        ($t.hour() as u64) * 3_600_000
            + ($t.minute() as u64) * 60_000
            + ($t.second() as u64) * 1_000
            + ($t.millisecond() as u64)
    };
}

/// Codec backed by the `rsubs-lib` crate (fallback).
#[derive(Debug, Clone, Copy, Default)]
pub struct RsubsCodec;

impl SubtitleCodec for RsubsCodec {
    fn parse(&self, text: &str, format: SubtitleFormat) -> Result<Document, FormatError> {
        let cues = match format {
            SubtitleFormat::Srt => parse_srt(text, format)?,
            SubtitleFormat::Vtt => parse_vtt(text, format)?,
            SubtitleFormat::Ass | SubtitleFormat::Ssa => parse_ssa(text, format)?,
            SubtitleFormat::Sbv => return Err(FormatError::Unsupported { format }),
        };
        Ok(Document {
            format,
            // The fallback does not reconstruct ASS/SSA script-info or styles in
            // the primary codec's shape, so same-format metadata is not carried
            // through this path.
            format_meta: None,
            cues,
        })
    }

    fn write(&self, doc: &Document, format: SubtitleFormat) -> Result<String, FormatError> {
        SubtitlerCodec.write(doc, format)
    }
}

fn parse_srt(text: &str, format: SubtitleFormat) -> Result<Vec<Cue>, FormatError> {
    let srt = rsubs_lib::SRT::parse(text).map_err(|e| parse_err(format, e))?;
    Ok(srt
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            cue(
                i,
                time_ms!(line.start),
                time_ms!(line.end),
                &line.text,
                None,
                None,
            )
        })
        .collect())
}

fn parse_vtt(text: &str, format: SubtitleFormat) -> Result<Vec<Cue>, FormatError> {
    let vtt = rsubs_lib::VTT::parse(text).map_err(|e| parse_err(format, e))?;
    Ok(vtt
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            cue(
                i,
                time_ms!(line.start),
                time_ms!(line.end),
                &line.text,
                None,
                None,
            )
        })
        .collect())
}

fn parse_ssa(text: &str, format: SubtitleFormat) -> Result<Vec<Cue>, FormatError> {
    let ssa = rsubs_lib::SSA::parse(text).map_err(|e| parse_err(format, e))?;
    Ok(ssa
        .events
        .iter()
        .enumerate()
        .map(|(i, event)| {
            cue(
                i,
                time_ms!(event.start),
                time_ms!(event.end),
                &event.text,
                non_empty(&event.style),
                non_empty(&event.name),
            )
        })
        .collect())
}

/// Builds a [`Cue`], deriving masked fields from verbatim text.
fn cue(
    index: usize,
    start_ms: u64,
    end_ms: u64,
    text: &str,
    style: Option<String>,
    speaker: Option<String>,
) -> Cue {
    let masked = markup::mask(text);
    Cue {
        idx: (index as u32) + 1,
        start_ms,
        end_ms,
        text_raw: text.to_string(),
        text_masked: masked.text_masked,
        lead_tags: masked.lead_tags,
        inline_tags: masked.inline_tags,
        style,
        speaker,
        translatable: masked.translatable,
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn parse_err(format: SubtitleFormat, e: impl std::fmt::Display) -> FormatError {
    FormatError::Parse {
        format,
        message: e.to_string(),
    }
}
