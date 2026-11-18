//! Subtitle format handling: byte decoding, tag masking, and codecs.
//!
//! [`codec`] defines the [`codec::SubtitleCodec`] trait, content-based format
//! detection, and the primary→fallback selection chain (`docs/ARCHITECTURE.md`
//! §3). [`subtitler`] is the primary codec adapter and [`rsubs`] the fallback.
//! All codecs operate on already-decoded UTF-8 text produced by [`encoding`].

pub mod codec;
pub mod encoding;
pub mod markup;
pub mod rsubs;
pub mod subtitler;
