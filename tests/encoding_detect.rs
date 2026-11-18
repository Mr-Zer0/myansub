//! Encoding detection tests (M1-AC4).

use std::path::Path;

use myansub::formats::encoding::{self, EncodingError};

fn read_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/encodings")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// AC: M1-AC4
#[test]
fn shift_jis_fixture_decodes() {
    let bytes = read_fixture("shift_jis.txt");
    let got = encoding::decode(&bytes, None, Some("ja")).expect("decode");
    assert_eq!(got.text, "こんにちは世界");
    assert!(!got.had_replacement);
    assert!(got.encoding.to_ascii_lowercase().contains("shift"));
}

// AC: M1-AC4
#[test]
fn gbk_fixture_decodes() {
    let bytes = read_fixture("gbk.txt");
    let got = encoding::decode(&bytes, None, Some("zh")).expect("decode");
    assert_eq!(got.text, "你好世界");
    assert!(!got.had_replacement);
}

// AC: M1-AC4
#[test]
fn cp1252_fixture_decodes() {
    let bytes = read_fixture("cp1252.txt");
    let got = encoding::decode(&bytes, None, None).expect("decode");
    assert_eq!(got.text, "Café — “quote”");
    assert!(!got.had_replacement);
}

// AC: M1-AC4
#[test]
fn utf16le_fixture_decodes_via_bom() {
    let bytes = read_fixture("utf16le.txt");
    let got = encoding::decode(&bytes, None, None).expect("decode");
    assert_eq!(got.text, "Hi 世界");
    assert!(!got.had_replacement);
    assert_eq!(got.encoding, "UTF-16LE");
}

// AC: M1-AC4
#[test]
fn utf16be_fixture_decodes_via_bom() {
    let bytes = read_fixture("utf16be.txt");
    let got = encoding::decode(&bytes, None, None).expect("decode");
    assert_eq!(got.text, "Hi 世界");
    assert!(!got.had_replacement);
    assert_eq!(got.encoding, "UTF-16BE");
}

// AC: M1-AC4
#[test]
fn utf8_bom_fixture_decodes_and_strips_bom() {
    let bytes = read_fixture("utf8bom.txt");
    let got = encoding::decode(&bytes, None, None).expect("decode");
    assert_eq!(got.text, "Héllo 世界");
    assert!(!got.had_replacement);
    assert_eq!(got.encoding, "UTF-8");
}

// AC: M1-AC4
#[test]
fn plain_utf8_without_bom_decodes_strictly() {
    let got = encoding::decode("Hello 世界".as_bytes(), None, None).expect("decode");
    assert_eq!(got.text, "Hello 世界");
    assert_eq!(got.encoding, "UTF-8");
    assert!(!got.had_replacement);
}

// AC: M1-AC4
#[test]
fn encoding_override_forces_named_encoding() {
    let bytes = read_fixture("cp1252.txt");
    let got = encoding::decode(&bytes, Some("windows-1252"), None).expect("decode");
    assert_eq!(got.text, "Café — “quote”");
    assert_eq!(got.encoding, "windows-1252");
}

// AC: M1-AC4
#[test]
fn unknown_override_label_is_an_error() {
    let err = encoding::decode(b"abc", Some("not-a-real-encoding"), None).unwrap_err();
    assert_eq!(
        err,
        EncodingError::UnknownLabel {
            label: "not-a-real-encoding".to_string()
        }
    );
}

// AC: M1-AC4
#[test]
fn mis_decoding_surfaces_replacement_characters() {
    // cp1252 bytes forced through UTF-8 contain invalid sequences -> U+FFFD.
    let bytes = read_fixture("cp1252.txt");
    let got = encoding::decode(&bytes, Some("utf-8"), None).expect("decode");
    assert!(got.had_replacement);
    assert!(got.text.contains('\u{FFFD}'));
}
