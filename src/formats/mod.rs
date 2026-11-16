//! Subtitle format handling: byte decoding and tag masking.
//!
//! Codec adapters (`subtitler`, `rsubs-lib`) arrive in a later task; this module
//! currently exposes only encoding detection and markup masking, both of which
//! operate on in-memory bytes / strings and never touch the filesystem.

pub mod encoding;
pub mod markup;
