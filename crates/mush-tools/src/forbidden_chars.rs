//! forbidden-characters validator
//!
//! a small helper used by the `write` and `edit` tools to enforce a
//! user-configured "alphabet of forbidden characters" on file content.
//! the validator scans content character-by-character and reports the
//! first offending occurrence with its line and column.
//!
//! configured via the `forbidden_chars` config key. each entry must be
//! exactly one Unicode scalar (one char). example:
//!
//! ```toml
//! forbidden_chars = ["—", "–", ";"]
//! ```
//!
//! tools that produce text (`write`, `edit`) reject the call with a
//! pointed error unless the model passes `allow_forbidden_chars: true`.

use std::collections::HashSet;
use std::sync::Arc;

use thiserror::Error;

/// validator for a configured set of forbidden characters.
///
/// cheap to clone (internally `Arc`-shared). pass into tool
/// constructors so every call re-uses the same shared set.
#[derive(Debug, Clone)]
pub struct ForbiddenChars {
    chars: Arc<HashSet<char>>,
}

impl ForbiddenChars {
    /// build from an iterator of forbidden characters.
    pub fn new(chars: impl IntoIterator<Item = char>) -> Self {
        Self {
            chars: Arc::new(chars.into_iter().collect()),
        }
    }

    /// validator that always passes.
    pub fn empty() -> Self {
        Self::new(std::iter::empty())
    }

    /// whether the validator has anything to enforce. tools can short-
    /// circuit the scan when this is true.
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// scan `content` for the first forbidden character.
    ///
    /// reports 1-indexed line and column counted in Unicode scalars
    /// (chars), not bytes, so the position lines up with what an
    /// end-user / model sees in an editor.
    pub fn check(&self, content: &str) -> Result<(), ForbiddenCharError> {
        if self.chars.is_empty() {
            return Ok(());
        }
        let mut line: usize = 1;
        let mut col: usize = 1;
        for ch in content.chars() {
            if self.chars.contains(&ch) {
                return Err(ForbiddenCharError {
                    ch,
                    line,
                    column: col,
                });
            }
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        Ok(())
    }
}

impl Default for ForbiddenChars {
    fn default() -> Self {
        Self::empty()
    }
}

/// a forbidden character was found at a specific location.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "forbidden character {} (U+{:04X}) at line {line}, column {column}. \
     remove or replace it, or set `allow_forbidden_chars: true` to bypass",
    fmt_char(*.ch), *.ch as u32
)]
pub struct ForbiddenCharError {
    pub ch: char,
    pub line: usize,
    pub column: usize,
}

/// render a char in a way that's unambiguous in error messages: visible
/// chars print as `'X'`, control / whitespace chars print by name so
/// the model isn't asked to find a character it can't see
fn fmt_char(ch: char) -> String {
    match ch {
        '\n' => "'\\n' (newline)".into(),
        '\r' => "'\\r' (carriage return)".into(),
        '\t' => "'\\t' (tab)".into(),
        ' ' => "' ' (space)".into(),
        c if c.is_control() => format!("U+{:04X} (control)", c as u32),
        c => format!("'{c}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_validator_accepts_anything() {
        let fc = ForbiddenChars::empty();
        assert!(fc.is_empty());
        assert!(fc.check("anything goes — even em dashes;").is_ok());
    }

    #[test]
    fn rejects_first_occurrence_with_location() {
        let fc = ForbiddenChars::new(['—', ';']);
        let err = fc
            .check("hello world\nthis line — has em dash")
            .unwrap_err();
        assert_eq!(err.ch, '—');
        assert_eq!(err.line, 2);
        assert_eq!(err.column, 11);
    }

    #[test]
    fn reports_first_match_when_multiple_forbidden() {
        // `;` comes before `—` in input order; we report the first
        // occurrence, not the lexicographically smallest forbidden char
        let fc = ForbiddenChars::new(['—', ';']);
        let err = fc.check("a; b—c").unwrap_err();
        assert_eq!(err.ch, ';');
    }

    #[test]
    fn column_counts_chars_not_bytes() {
        // multi-byte chars before the offender shouldn't inflate the
        // reported column. "café—" puts é at col 4, em dash at col 5
        let fc = ForbiddenChars::new(['—']);
        let err = fc.check("café—").unwrap_err();
        assert_eq!(err.column, 5);
    }

    #[test]
    fn line_count_increments_on_newline() {
        let fc = ForbiddenChars::new([';']);
        let err = fc.check("\n\n;").unwrap_err();
        assert_eq!(err.line, 3);
        assert_eq!(err.column, 1);
    }

    #[test]
    fn error_message_names_the_char_and_codepoint() {
        let fc = ForbiddenChars::new(['—']);
        let msg = fc.check("foo—bar").unwrap_err().to_string();
        assert!(
            msg.contains("'—'"),
            "expected literal char in message: {msg}"
        );
        assert!(
            msg.contains("U+2014"),
            "expected codepoint in message: {msg}"
        );
        assert!(
            msg.contains("allow_forbidden_chars"),
            "expected bypass hint in message: {msg}"
        );
    }

    #[test]
    fn error_message_names_control_chars_by_descriptor() {
        // \t shouldn't render literally in the message; we want a name
        let fc = ForbiddenChars::new(['\t']);
        let msg = fc.check("a\tb").unwrap_err().to_string();
        assert!(msg.contains("tab"), "expected `tab` descriptor: {msg}");
    }

    #[test]
    fn clone_shares_char_set() {
        let fc = ForbiddenChars::new(['—']);
        let cloned = fc.clone();
        assert!(cloned.check("foo—bar").is_err());
    }
}
