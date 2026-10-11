//! G9 red team (pass 2): the U1 recovery-key rule that `clipboard.rs`
//! `looks_sensitive` applies to every clipboard text (after `trim`). The rule
//! is `recovery::is_recovery_key`: the text parses as a checksummed RK after
//! the `RecoveryKey::parse` normalization. These cases attack its edges:
//! embedding, case, separators, Unicode whitespace and look-alikes, and
//! well-formed 56-character Base32 with a bad checksum, which must still be
//! recorded as ordinary text.

use crate::sync::recovery::{is_recovery_key, RecoveryError, RecoveryKey};

const BASE32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn display() -> String {
    RecoveryKey::generate().unwrap().to_display().to_string()
}

/// What the clipboard poller asks: `looks_sensitive` trims, then delegates.
fn captured_as_rk(text: &str) -> bool {
    is_recovery_key(text.trim())
}

#[test]
fn u1_case_extra_separators_and_unicode_whitespace_are_still_an_rk() {
    for _ in 0..32 {
        let rk = display();
        let groups: Vec<&str> = rk.split('-').collect();
        let variants = [
            rk.to_lowercase(),
            rk.replace('-', "--"),
            rk.replace('-', " - "),
            rk.replace('-', "\r\n"),
            rk.replace('-', "\t"),
            groups.join("\u{00A0}"), // no-break space
            groups.join("\u{2003}"), // em space
            groups.join("\u{3000}"), // ideographic space
            groups.join("\u{2028}"), // line separator
            format!("\n\n  {rk}  \n"),
            // Digits typed for look-alike letters, as `parse` accepts them.
            rk.replace('O', "0").replace('I', "1").replace('B', "8"),
            rk.chars().enumerate().map(|(i, c)| if i % 2 == 0 { c.to_ascii_lowercase() } else { c }).collect(),
        ];
        for v in &variants {
            assert!(RecoveryKey::parse(v).is_ok(), "parse {v:?}");
            assert!(captured_as_rk(v), "a recovery key form would be recorded: {v:?}");
        }
    }
}

#[test]
fn u1_rk_embedded_in_surrounding_text_is_outside_the_parse_rule() {
    // U1 defines the rule as "the text parses as a valid RK"; a labelled or
    // quoted RK does not parse, so it is recorded. Pinned so a change to the
    // scope is a deliberate decision, not a silent drift.
    let rk = display();
    for text in [
        format!("Recovery key: {rk}"),
        format!("{rk} (keep offline)"),
        format!("\"{rk}\""),
        format!("{rk}\n{rk}"),
        format!("This key opens the vault.\n{rk}\nStore it offline; this app does not keep it."),
    ] {
        assert!(RecoveryKey::parse(&text).is_err());
        assert!(!captured_as_rk(&text), "{text:?}");
    }
}

#[test]
fn u1_unicode_look_alikes_are_neither_captured_nor_accepted_as_an_rk() {
    // A look-alike form is not an RK for `parse` either, so a join with it
    // fails the same way: the rule and the parser never disagree.
    let rk = display();
    let fullwidth: String =
        rk.chars().map(|c| if c.is_ascii_alphanumeric() { char::from_u32(c as u32 + 0xFEE0).unwrap() } else { c }).collect();
    let cyrillic = rk.replace('A', "\u{0410}").replace('E', "\u{0415}").replace('O', "\u{041E}").replace('C', "\u{0421}");
    let variants = [
        fullwidth,
        rk.replace('-', "\u{2010}"), // hyphen
        rk.replace('-', "\u{2013}"), // en dash
        rk.replace('-', "\u{2212}"), // minus sign
        rk.replace('-', "\u{FF0D}"), // fullwidth hyphen-minus
        rk.replace('-', "\u{200B}"), // zero-width space (not whitespace)
        format!("\u{FEFF}{rk}"),     // BOM
        rk.replace('-', "_"),
        rk.replace('-', "."),
    ];
    for v in &variants {
        let changed = v != &rk && !v.is_ascii();
        let parses = RecoveryKey::parse(v).is_ok();
        assert_eq!(captured_as_rk(v), parses, "rule and parser disagree on {v:?}");
        if changed || v.contains('_') || v.contains('.') {
            assert!(!parses, "look-alike accepted as an RK: {v:?}");
        }
    }
    // Cyrillic only changes the text when the RK holds one of those letters.
    if cyrillic != rk {
        assert!(!captured_as_rk(&cyrillic));
        assert!(RecoveryKey::parse(&cyrillic).is_err());
    }
}

#[test]
fn u1_well_formed_56_char_base32_with_a_bad_checksum_is_recorded() {
    // Every single-character substitution of a real RK is well-formed Base32
    // whose checksum fails; it stays ordinary clipboard text.
    for _ in 0..8 {
        let rk = display();
        let compact: Vec<u8> = rk.bytes().filter(|b| *b != b'-').collect();
        assert_eq!(compact.len(), 56);
        for pos in 0..compact.len() {
            let mut typo = compact.clone();
            typo[pos] = if typo[pos] == b'A' { b'Q' } else { b'A' };
            let typo = String::from_utf8(typo).unwrap();
            match RecoveryKey::parse(&typo) {
                Err(RecoveryError::Checksum) => {}
                other => panic!("position {pos}: {:?}", other.map(|_| "parsed")),
            }
            assert!(!captured_as_rk(&typo), "typo at {pos} treated as an RK");
        }
    }
    // Deterministic well-formed strings that are not RKs.
    let cycle: String = BASE32.iter().cycle().take(56).map(|b| *b as char).collect();
    let rev: String = BASE32.iter().rev().cycle().take(56).map(|b| *b as char).collect();
    let mut grouped = String::new();
    for (i, c) in cycle.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            grouped.push('-');
        }
        grouped.push(c);
    }
    for text in [cycle.as_str(), rev.as_str(), grouped.as_str()] {
        assert_eq!(RecoveryKey::parse(text).err(), Some(RecoveryError::Checksum), "{text}");
        assert!(!captured_as_rk(text), "{text}");
    }
    for text in ["A".repeat(56), "7".repeat(56)] {
        assert!(RecoveryKey::parse(&text).is_err(), "{text}");
        assert!(!captured_as_rk(&text), "{text}");
    }
    // One character short or long is a format error, not an RK.
    let rk = display().replace('-', "");
    assert!(!captured_as_rk(&rk[..55]));
    assert!(!captured_as_rk(&format!("{rk}A")));
}
