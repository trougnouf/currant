// ./core/src/text.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Unicode text folding for accent-insensitive, case-insensitive search.
//!
//! [`fold`] decomposes accented characters (NFKD), strips combining
//! diacritical marks, and lowercases the result. It is registered as a
//! SQLite scalar function so that SQL comparisons can match regardless of
//! accents or case.

use unicode_normalization::UnicodeNormalization;

/// Fold a string for accent-insensitive, case-insensitive comparison.
///
/// NFKD decomposition splits accented characters into a base character plus
/// a combining diacritical mark; the filter removes those marks. The result
/// is lowercased so that case differences are also ignored.
pub fn fold(s: &str) -> String {
    s.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

/// Whether a character is a Unicode combining diacritical mark.
fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'   // Combining Diacritical Marks
        | '\u{1AB0}'..='\u{1AFF}' // Combining Diacritical Marks Extended
        | '\u{1DC0}'..='\u{1DFF}' // Combining Diacritical Marks Supplement
        | '\u{20D0}'..='\u{20FF}' // Combining Diacritical Marks for Symbols
        | '\u{FE20}'..='\u{FE2F}' // Combining Half Marks
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_accents() {
        assert_eq!(fold("café"), "cafe");
        assert_eq!(fold("Sören"), "soren");
        assert_eq!(fold("Über"), "uber");
        assert_eq!(fold("Ñoño"), "nono");
        assert_eq!(fold("Beyoncé"), "beyonce");
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(fold("CAFÉ"), "cafe");
        assert_eq!(fold("über"), "uber");
    }

    #[test]
    fn ascii_unchanged() {
        assert_eq!(fold("Hello World"), "hello world");
        assert_eq!(fold("Miles Davis"), "miles davis");
    }
}
