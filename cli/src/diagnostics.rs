//! How a compiler error reaches whoever asked for it.
//!
//! Two audiences want the same failure in different shapes. A person wants
//! the annotated source snippet; an editor wants a location it can put a
//! squiggle on, and cannot get one by parsing prose. So a diagnostic keeps
//! its structure — phase, message, file, byte span — until the moment it is
//! printed, and the format decides what happens then.
//!
//! The JSON shape follows `cargo --message-format=json`: one object per
//! line, carrying both the machine fields and the `rendered` human text, so
//! a tool can show exactly what the terminal would have shown without
//! reimplementing the renderer.

use common::{Diagnostic, Span};

/// How diagnostics are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Format {
    /// Annotated source snippets.
    #[default]
    Human,
    /// One JSON object per line.
    Json,
}

/// A failure with everything needed to render it either way.
///
/// `located` is absent for failures with no source position at all — a file
/// that could not be read, a link step that failed. Those still reach JSON
/// consumers, just without a span to point at.
pub struct Failure {
    pub message: String,
    pub located: Option<Located>,
}

/// A diagnostic together with the source it points into.
pub struct Located {
    pub phase: String,
    pub message: String,
    pub file: String,
    pub span: Span,
    pub source: String,
}

impl Failure {
    pub fn plain(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            located: None,
        }
    }

    pub fn located(diagnostic: &Diagnostic, file: &str, source: &str) -> Self {
        let located = Located {
            phase: diagnostic.phase.to_string(),
            message: diagnostic.message.clone(),
            file: file.to_string(),
            span: diagnostic.span,
            source: source.to_string(),
        };
        Failure {
            message: located.human(),
            located: Some(located),
        }
    }

    /// The text to print for `format`.
    pub fn render(&self, format: Format) -> String {
        match (format, &self.located) {
            (Format::Human, _) => self.message.clone(),
            (Format::Json, Some(located)) => located.json(),
            // No span, but a consumer asked for JSON: give it JSON. Falling
            // back to prose here would mean a tool's parser breaks on
            // exactly the errors it did not anticipate.
            (Format::Json, None) => json_object(&[
                ("level", Field::Str("error")),
                ("message", Field::Str(&self.message)),
                ("rendered", Field::Str(&self.message)),
            ]),
        }
    }
}

impl Located {
    /// The annotated snippet, or the bare message when the span is
    /// synthesized and points at nothing useful.
    pub fn human(&self) -> String {
        if self.span == Span::default() {
            return format!("error[{}]: {}", self.phase, self.message);
        }
        // The renderer reads the phase back out only to print it, and a
        // `Phase` cannot be reconstructed from its name without a parser for
        // something already stringly typed here.
        let diagnostic = Diagnostic::new(common::Phase::Semantic, self.message.clone(), self.span);
        diagnostic.render(&self.file, &self.source).replacen(
            "error[semantic]",
            &format!("error[{}]", self.phase),
            1,
        )
    }

    pub fn json(&self) -> String {
        let (line, column) = common::line_col(&self.source, self.span.start);
        let (end_line, end_column) = common::line_col(&self.source, self.span.end);
        json_object(&[
            ("level", Field::Str("error")),
            ("phase", Field::Str(&self.phase)),
            ("message", Field::Str(&self.message)),
            ("file", Field::Str(&self.file)),
            ("line", Field::Num(line as u64)),
            ("column", Field::Num(column as u64)),
            ("end_line", Field::Num(end_line as u64)),
            ("end_column", Field::Num(end_column as u64)),
            ("byte_start", Field::Num(self.span.start as u64)),
            ("byte_end", Field::Num(self.span.end as u64)),
            ("rendered", Field::Str(&self.human())),
        ])
    }
}

enum Field<'a> {
    Str(&'a str),
    Num(u64),
}

/// Serialize a flat object. Hand-written because the alternative is a
/// dependency for one object shape, and PyRs's posture is that a dependency
/// has to earn its place.
fn json_object(fields: &[(&str, Field)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        escape_into(&mut out, key);
        out.push_str("\":");
        match value {
            Field::Str(text) => {
                out.push('"');
                escape_into(&mut out, text);
                out.push('"');
            }
            Field::Num(n) => out.push_str(&n.to_string()),
        }
    }
    out.push('}');
    out
}

/// JSON string escaping. Every control character has to be escaped, not just
/// the familiar ones: a raw byte below 0x20 makes the whole line
/// unparseable, and diagnostics quote user source, which can contain
/// anything.
fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(source: &str, span: Span, message: &str) -> Located {
        Located {
            phase: "semantic".into(),
            message: message.into(),
            file: "prog.py".into(),
            span,
            source: source.into(),
        }
    }

    #[test]
    fn json_carries_a_location_an_editor_can_use() {
        let source = "x = 1\ny = 2.0\n";
        let json = sample(source, Span::new(10, 13), "type mismatch").json();
        assert!(json.contains(r#""level":"error""#), "{json}");
        assert!(json.contains(r#""file":"prog.py""#), "{json}");
        assert!(json.contains(r#""line":2"#), "{json}");
        assert!(json.contains(r#""column":5"#), "{json}");
        assert!(json.contains(r#""byte_start":10"#), "{json}");
        assert!(json.contains(r#""byte_end":13"#), "{json}");
    }

    #[test]
    fn json_is_one_line_even_when_the_rendering_is_not() {
        let source = "x = 1\ny = 2.0\n";
        let json = sample(source, Span::new(10, 13), "type mismatch").json();
        assert!(!json.contains('\n'), "a newline breaks line-delimited JSON");
        // The pretty rendering survives, escaped.
        assert!(json.contains(r"\n"), "{json}");
        assert!(json.contains("--> prog.py:2:5"), "{json}");
    }

    #[test]
    fn source_text_cannot_break_the_encoding() {
        // Diagnostics quote user source, which can contain anything.
        let hostile = "\"}\\\n\t\u{1}";
        let json = sample(hostile, Span::default(), hostile).json();
        assert!(json.contains(r#"\"}\\"#), "{json}");
        assert!(json.contains("\\u0001"), "control byte not escaped: {json}");
        assert!(
            json.chars().all(|c| (c as u32) >= 0x20),
            "raw control character in output: {json:?}"
        );
    }

    #[test]
    fn a_failure_with_no_position_is_still_json() {
        // A tool's parser must not break on exactly the errors it did not
        // anticipate.
        let out = Failure::plain("linking failed").render(Format::Json);
        assert!(out.starts_with('{') && out.ends_with('}'), "{out}");
        assert!(out.contains(r#""message":"linking failed""#), "{out}");
    }

    #[test]
    fn the_human_format_is_unchanged_by_any_of_this() {
        let source = "x = 1\ny = 2.0\n";
        let failure = Failure::located(
            &Diagnostic::new(common::Phase::Semantic, "type mismatch", Span::new(10, 13)),
            "prog.py",
            source,
        );
        let text = failure.render(Format::Human);
        assert!(text.starts_with("error[semantic]: type mismatch"), "{text}");
        assert!(text.contains("--> prog.py:2:5"), "{text}");
        assert!(text.contains('^'), "{text}");
    }

    #[test]
    fn the_phase_survives_into_both_formats() {
        let failure = Failure::located(
            &Diagnostic::new(common::Phase::Load, "circular import", Span::new(0, 1)),
            "prog.py",
            "import a\n",
        );
        assert!(failure.render(Format::Human).contains("error[load]"));
        assert!(failure.render(Format::Json).contains(r#""phase":"load""#));
    }
}
