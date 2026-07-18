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

/// a class of characters to forbid wholesale, for cases where listing
/// every codepoint by hand is impractical (emoji, arrow glyphs).
/// combine with the explicit `chars` set in [`ForbiddenChars`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CharClass {
    /// pictographic emoji. matches `Emoji=YES | Emoji_Component=YES` but
    /// excludes ASCII (the `0-9`, `#`, `*` keycap bases carry `Emoji=YES`
    /// yet are ordinary code), so real source is never rejected.
    Emoji,
    /// arrow glyphs from the Unicode arrow blocks (`→ ← ↑ ↓ ⇒ ⬆` etc).
    /// the ASCII `->` is unaffected.
    Arrows,
}

impl CharClass {
    fn matches(self, ch: char) -> bool {
        match self {
            Self::Emoji => is_emoji(ch),
            Self::Arrows => is_arrow(ch),
        }
    }

    /// short human name used in the violation message.
    fn label(self) -> &'static str {
        match self {
            Self::Emoji => "emoji",
            Self::Arrows => "arrow",
        }
    }
}

fn is_emoji(ch: char) -> bool {
    use unicode_properties::UnicodeEmoji;
    !ch.is_ascii() && ch.is_emoji_char_or_emoji_component()
}

fn is_arrow(ch: char) -> bool {
    matches!(ch,
        '\u{2190}'..='\u{21FF}'   // Arrows
        | '\u{2794}'..='\u{27BF}' // Dingbats (heavy/curved arrows)
        | '\u{27F0}'..='\u{27FF}' // Supplemental Arrows-A
        | '\u{2900}'..='\u{297F}' // Supplemental Arrows-B
        | '\u{2B05}'..='\u{2B07}' // black left/up/down arrows
        | '\u{2B95}'              // black rightwards arrow
    )
}

/// validator for a configured set of forbidden characters.
///
/// cheap to clone (internally `Arc`-shared). pass into tool
/// constructors so every call re-uses the same shared set.
#[derive(Debug, Clone)]
pub struct ForbiddenChars {
    chars: Arc<HashSet<char>>,
    classes: Arc<Vec<CharClass>>,
}

impl ForbiddenChars {
    /// build from an iterator of forbidden characters.
    pub fn new(chars: impl IntoIterator<Item = char>) -> Self {
        Self::with_classes(chars, std::iter::empty())
    }

    /// build from explicit characters plus whole character classes.
    pub fn with_classes(
        chars: impl IntoIterator<Item = char>,
        classes: impl IntoIterator<Item = CharClass>,
    ) -> Self {
        Self {
            chars: Arc::new(chars.into_iter().collect()),
            classes: Arc::new(classes.into_iter().collect()),
        }
    }

    /// validator that always passes.
    pub fn empty() -> Self {
        Self::new(std::iter::empty())
    }

    /// whether the validator has anything to enforce. tools can short-
    /// circuit the scan when this is true.
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty() && self.classes.is_empty()
    }

    /// scan `content` for the first forbidden character.
    ///
    /// reports 1-indexed line and column counted in Unicode scalars
    /// (chars), not bytes, so the position lines up with what an
    /// end-user / model sees in an editor.
    pub fn check(&self, content: &str) -> Result<(), ForbiddenCharError> {
        if self.is_empty() {
            return Ok(());
        }
        let mut line: usize = 1;
        let mut col: usize = 1;
        for ch in content.chars() {
            let violation = if self.chars.contains(&ch) {
                Some(None)
            } else {
                self.classes
                    .iter()
                    .copied()
                    .find(|c| c.matches(ch))
                    .map(Some)
            };
            if let Some(class) = violation {
                return Err(ForbiddenCharError {
                    ch,
                    line,
                    column: col,
                    class,
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
    "forbidden character {} (U+{:04X}) at line {line}, column {column}{}. \
     remove or replace it, or set `allow_forbidden_chars: true` to bypass",
    fmt_char(*.ch), *.ch as u32, class_suffix(*.class)
)]
pub struct ForbiddenCharError {
    pub ch: char,
    pub line: usize,
    pub column: usize,
    /// which class matched, or `None` when the char was listed explicitly.
    pub class: Option<CharClass>,
}

/// describe a class match so the model knows it hit a whole category
/// rather than a single configured char.
fn class_suffix(class: Option<CharClass>) -> String {
    match class {
        Some(c) => format!(" (matches the forbidden {} class)", c.label()),
        None => String::new(),
    }
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

/// pulls the user-supplied text fragments out of a tool call's json
/// args. each tool that produces file content provides its own
/// extractor when wrapping under a [`ForbiddenCharsGuard`]:
///
/// - `write` returns `[args.content]`
/// - `edit` returns `[args.newText]`
/// - `apply_patch` parses the patch and returns each added/replaced
///   line of content
///
/// returning an empty `Vec` skips validation (the inner tool will
/// usually then fail with a parse error, surfacing the real issue).
pub type ContentExtractor = std::sync::Arc<dyn Fn(&serde_json::Value) -> Vec<String> + Send + Sync>;

/// json key on tool call args that bypasses the guard for a single
/// call. the guard reads and consumes this key before delegating to
/// the inner tool, so inner deserialisers using `deny_unknown_fields`
/// stay happy without having to know about the policy
pub const ALLOW_FORBIDDEN_CHARS_KEY: &str = "allow_forbidden_chars";

/// wraps any `AgentTool` and enforces a forbidden-characters policy
/// on the user-supplied text fragments extracted from each call's
/// args. inner tools stay completely oblivious to the policy:
///
/// - validation runs before delegation; on a hit, the inner tool is
///   never called and the call returns a [`ForbiddenCharError`]
/// - the schema is augmented with an `allow_forbidden_chars` boolean
///   so the model has a documented per-call escape hatch
/// - the override flag is stripped from args before delegation so
///   inner tools using `deny_unknown_fields` don't choke on it
///
/// the wrapper short-circuits when the configured set is empty: prefer
/// [`ForbiddenCharsGuard::maybe_wrap`] which returns the inner tool
/// untouched in that case so callers don't pay the wrapper tax for
/// the no-config common case.
/// whether the per-call bypass is advertised in the tool schema.
///
/// `write`/`edit`/`apply_patch` advertise it so the model has a
/// documented escape hatch. `bash` keeps it [`Hidden`](Self::Hidden):
/// the field still works and is named in the rejection message, but it
/// is deliberately left out of the advertised schema so the model only
/// reaches for it after a genuine block (e.g. the user asking it to
/// `sed` a forbidden char out of a file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassVisibility {
    Advertised,
    Hidden,
}

pub struct ForbiddenCharsGuard {
    inner: mush_agent::tool::SharedTool,
    chars: ForbiddenChars,
    extractor: ContentExtractor,
    visibility: BypassVisibility,
}

impl ForbiddenCharsGuard {
    pub fn new(
        inner: mush_agent::tool::SharedTool,
        chars: ForbiddenChars,
        extractor: ContentExtractor,
        visibility: BypassVisibility,
    ) -> Self {
        Self {
            inner,
            chars,
            extractor,
            visibility,
        }
    }

    /// wrap `inner` only when there's something to enforce. with an
    /// empty validator, `inner` is returned unchanged so callers don't
    /// need to special-case the no-config path
    pub fn maybe_wrap(
        inner: mush_agent::tool::SharedTool,
        chars: ForbiddenChars,
        extractor: ContentExtractor,
        visibility: BypassVisibility,
    ) -> mush_agent::tool::SharedTool {
        if chars.is_empty() {
            inner
        } else {
            std::sync::Arc::new(Self::new(inner, chars, extractor, visibility))
        }
    }

    /// augment the schema with the bypass knob only when advertised.
    /// hidden guards return the inner schema untouched.
    fn maybe_augment(&self, schema: serde_json::Value) -> serde_json::Value {
        match self.visibility {
            BypassVisibility::Advertised => augment_schema(schema),
            BypassVisibility::Hidden => schema,
        }
    }
}

/// inject the `allow_forbidden_chars` knob into a tool's parameters
/// schema. exposed as a free function so both `parameters_schema` and
/// `parameters_schema_for(api)` can re-use it without duplicating the
/// description text
fn augment_schema(mut schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(ref mut top) = schema else {
        return schema;
    };
    let Some(serde_json::Value::Object(props)) = top.get_mut("properties") else {
        return schema;
    };
    props.insert(
        ALLOW_FORBIDDEN_CHARS_KEY.into(),
        serde_json::json!({
            "type": "boolean",
            "description": "bypass the configured forbidden_chars guard for this call. \
                            defaults to false. only set when the forbidden char is genuinely \
                            required (e.g. quoting a verbatim source) - prefer fixing the \
                            content first"
        }),
    );
    schema
}

#[async_trait::async_trait]
impl mush_agent::tool::AgentTool for ForbiddenCharsGuard {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn description_for(&self, api: mush_ai::types::Api) -> &str {
        self.inner.description_for(api)
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.maybe_augment(self.inner.parameters_schema())
    }
    fn parameters_schema_for(&self, api: mush_ai::types::Api) -> serde_json::Value {
        self.maybe_augment(self.inner.parameters_schema_for(api))
    }
    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        self.inner.output_limit()
    }

    async fn execute(&self, mut args: serde_json::Value) -> mush_agent::tool::ToolResult {
        let bypass = args
            .get(ALLOW_FORBIDDEN_CHARS_KEY)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let serde_json::Value::Object(ref mut map) = args {
            map.remove(ALLOW_FORBIDDEN_CHARS_KEY);
        }
        if !bypass {
            for content in (self.extractor)(&args) {
                if let Err(err) = self.chars.check(&content) {
                    return mush_agent::tool::ToolResult::error(err.to_string());
                }
            }
        }
        self.inner.execute(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_agent::tool::{AgentTool, SharedTool, ToolResult};
    use std::sync::{Arc, Mutex};

    #[test]
    fn empty_validator_accepts_anything() {
        let fc = ForbiddenChars::empty();
        assert!(fc.is_empty());
        assert!(fc.check("anything goes — even em dashes;").is_ok());
    }

    #[test]
    fn emoji_class_blocks_pictographic_but_not_ascii_code() {
        let fc = ForbiddenChars::with_classes([], [CharClass::Emoji]);
        assert!(!fc.is_empty(), "a class alone is not empty");
        assert!(fc.check("ship it 🚀").is_err());
        assert!(fc.check("done ✅").is_err());
        // Emoji=YES technically covers ASCII digits, '#', '*' (keycap bases);
        // ordinary code must still pass
        assert!(fc.check("let x = 12; a[3] = *p; # note").is_ok());
    }

    #[test]
    fn arrows_class_blocks_arrow_glyphs_not_ascii_arrow() {
        let fc = ForbiddenChars::with_classes([], [CharClass::Arrows]);
        assert!(fc.check("a → b").is_err());
        assert!(fc.check("x ⇒ y").is_err());
        assert!(fc.check("closure |a| -> b").is_ok());
    }

    #[test]
    fn explicit_chars_and_classes_combine() {
        let fc = ForbiddenChars::with_classes(['—'], [CharClass::Emoji]);
        assert!(fc.check("em — dash").is_err());
        assert!(fc.check("party 🎉").is_err());
        assert!(fc.check("clean ascii text").is_ok());
    }

    #[test]
    fn class_violation_names_the_class_in_the_error() {
        let fc = ForbiddenChars::with_classes([], [CharClass::Emoji]);
        let msg = fc.check("oops 🔥").unwrap_err().to_string();
        assert!(
            msg.contains("emoji"),
            "expected class name in message: {msg}"
        );
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

    /// minimal `AgentTool` that records what args it saw and returns
    /// "ok". lets the guard tests verify both the validator behaviour
    /// and what gets passed downstream
    struct RecordingTool {
        received: Mutex<Option<serde_json::Value>>,
    }

    impl RecordingTool {
        fn new() -> Self {
            Self {
                received: Mutex::new(None),
            }
        }

        fn last_received(&self) -> Option<serde_json::Value> {
            self.received.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentTool for RecordingTool {
        fn name(&self) -> &str {
            "record"
        }
        fn label(&self) -> &str {
            "Record"
        }
        fn description(&self) -> &str {
            "test tool that just records args"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string"}
                },
                "required": ["content"]
            })
        }
        fn output_limit(&self) -> mush_agent::tool::OutputLimit {
            mush_agent::tool::OutputLimit::Head
        }
        async fn execute(&self, args: serde_json::Value) -> ToolResult {
            *self.received.lock().unwrap() = Some(args);
            ToolResult::text("ok")
        }
    }

    fn content_extractor() -> ContentExtractor {
        Arc::new(|args: &serde_json::Value| {
            args.get("content")
                .and_then(|v| v.as_str())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default()
        })
    }

    fn record_with_guard(chars: ForbiddenChars) -> (Arc<RecordingTool>, ForbiddenCharsGuard) {
        let inner = Arc::new(RecordingTool::new());
        let shared: SharedTool = inner.clone();
        let guard = ForbiddenCharsGuard::new(
            shared,
            chars,
            content_extractor(),
            BypassVisibility::Advertised,
        );
        (inner, guard)
    }

    #[tokio::test]
    async fn guard_blocks_forbidden_content_without_calling_inner() {
        let (inner, guard) = record_with_guard(ForbiddenChars::new(['—']));
        let result = guard
            .execute(serde_json::json!({"content": "no — allowed"}))
            .await;
        assert!(!result.outcome.is_success(), "guard should reject");
        assert!(
            inner.last_received().is_none(),
            "inner tool must not be invoked when validation fails"
        );
    }

    #[tokio::test]
    async fn guard_passes_clean_content_through_to_inner() {
        let (inner, guard) = record_with_guard(ForbiddenChars::new(['—']));
        let result = guard
            .execute(serde_json::json!({"content": "plain hyphen-minus"}))
            .await;
        assert!(result.outcome.is_success());
        let received = inner.last_received().expect("inner should be called");
        assert_eq!(
            received,
            serde_json::json!({"content": "plain hyphen-minus"})
        );
    }

    #[tokio::test]
    async fn guard_strips_allow_forbidden_chars_before_delegating() {
        // bypass flag must not leak through to the inner tool, otherwise
        // inner deserialisers using deny_unknown_fields would reject it
        let (inner, guard) = record_with_guard(ForbiddenChars::new(['—']));
        let result = guard
            .execute(serde_json::json!({
                "content": "with — em dash",
                "allow_forbidden_chars": true,
            }))
            .await;
        assert!(result.outcome.is_success(), "bypass should let it through");
        let received = inner.last_received().expect("inner should be called");
        assert_eq!(
            received,
            serde_json::json!({"content": "with — em dash"}),
            "allow_forbidden_chars must be stripped from args before inner.execute"
        );
    }

    #[tokio::test]
    async fn guard_validates_every_extracted_string() {
        // some extractors return multiple strings (e.g. apply_patch
        // returning every added line). a forbidden char in any of
        // them must block the call
        let inner: SharedTool = Arc::new(RecordingTool::new());
        let multi: ContentExtractor = Arc::new(|args| {
            args.get("lines")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        });
        let guard = ForbiddenCharsGuard::new(
            inner,
            ForbiddenChars::new(['—']),
            multi,
            BypassVisibility::Advertised,
        );
        let result = guard
            .execute(serde_json::json!({"lines": ["clean", "with — em dash", "also clean"]}))
            .await;
        assert!(
            !result.outcome.is_success(),
            "any extracted line containing a forbidden char should block"
        );
    }

    #[test]
    fn guard_schema_injects_allow_forbidden_chars() {
        let inner: SharedTool = Arc::new(RecordingTool::new());
        let guard = ForbiddenCharsGuard::new(
            inner,
            ForbiddenChars::new(['—']),
            content_extractor(),
            BypassVisibility::Advertised,
        );
        let schema = guard.parameters_schema();
        assert_eq!(
            schema["properties"]["allow_forbidden_chars"]["type"],
            "boolean"
        );
        // existing properties should still be present
        assert_eq!(schema["properties"]["content"]["type"], "string");
    }

    #[tokio::test]
    async fn hidden_guard_omits_schema_knob_but_still_enforces_and_bypasses() {
        // bash uses a hidden guard: the bypass field must not appear in the
        // advertised schema, yet the guard still blocks forbidden content and
        // still honours the bypass flag when passed
        let inner: SharedTool = Arc::new(RecordingTool::new());
        let guard = ForbiddenCharsGuard::new(
            inner.clone(),
            ForbiddenChars::new(['—']),
            content_extractor(),
            BypassVisibility::Hidden,
        );
        assert!(
            guard.parameters_schema()["properties"]["allow_forbidden_chars"].is_null(),
            "hidden guard must not advertise the bypass field"
        );
        // still enforces, and the rejection message names the hidden field
        let blocked = guard
            .execute(serde_json::json!({"content": "no — dash"}))
            .await;
        assert!(
            !blocked.outcome.is_success(),
            "hidden guard must still block"
        );
        // still bypassable via the undocumented field
        let allowed = guard
            .execute(serde_json::json!({
                "content": "with — em dash",
                "allow_forbidden_chars": true,
            }))
            .await;
        assert!(
            allowed.outcome.is_success(),
            "hidden bypass must still work"
        );
    }

    #[test]
    fn maybe_wrap_returns_inner_unchanged_when_chars_empty() {
        // tests don't have a way to compare SharedTool identity directly,
        // so verify behaviourally: inner schema lacks the override knob
        // when the validator is empty, and would have it after wrapping
        let inner: SharedTool = Arc::new(RecordingTool::new());
        let wrapped = ForbiddenCharsGuard::maybe_wrap(
            inner,
            ForbiddenChars::empty(),
            content_extractor(),
            BypassVisibility::Advertised,
        );
        assert!(
            wrapped.parameters_schema()["properties"]["allow_forbidden_chars"].is_null(),
            "empty validator should not augment the schema"
        );
    }

    #[test]
    fn maybe_wrap_wraps_when_chars_configured() {
        let inner: SharedTool = Arc::new(RecordingTool::new());
        let wrapped = ForbiddenCharsGuard::maybe_wrap(
            inner,
            ForbiddenChars::new(['—']),
            content_extractor(),
            BypassVisibility::Advertised,
        );
        assert_eq!(
            wrapped.parameters_schema()["properties"]["allow_forbidden_chars"]["type"],
            "boolean"
        );
    }

    #[tokio::test]
    async fn guard_delegates_metadata_methods_to_inner() {
        let inner: SharedTool = Arc::new(RecordingTool::new());
        let guard = ForbiddenCharsGuard::new(
            inner.clone(),
            ForbiddenChars::new(['—']),
            content_extractor(),
            BypassVisibility::Advertised,
        );
        assert_eq!(guard.name(), inner.name());
        assert_eq!(guard.label(), inner.label());
        assert_eq!(guard.description(), inner.description());
    }
}
