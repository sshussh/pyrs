//! Shared foundation types: source spans and diagnostics.
//!
//! Every compilation phase reports errors as [`Diagnostic`]s carrying a
//! [`Span`] into the original source text, so the driver can render
//! consistent, source-annotated error messages.

use std::fmt;
use std::ops::Range;

pub fn ping() -> String {
    String::from("pong")
}

/// A half-open byte range `[start, end)` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

impl From<Range<usize>> for Span {
    fn from(r: Range<usize>) -> Self {
        Span {
            start: r.start,
            end: r.end,
        }
    }
}

/// Which phase produced a diagnostic. Only used for labeling output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Lex,
    Parse,
    Semantic,
    Codegen,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::Lex => write!(f, "lex"),
            Phase::Parse => write!(f, "parse"),
            Phase::Semantic => write!(f, "semantic"),
            Phase::Codegen => write!(f, "codegen"),
        }
    }
}

/// A compiler error tied to a location in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub phase: Phase,
    pub message: String,
    pub span: Span,
}

impl Diagnostic {
    pub fn new(phase: Phase, message: impl Into<String>, span: Span) -> Self {
        Self {
            phase,
            message: message.into(),
            span,
        }
    }

    /// Render the diagnostic with a source snippet and caret underline:
    ///
    /// ```text
    /// error[semantic]: type mismatch: expected int, found float
    ///  --> fib.py:3:12
    ///   |
    /// 3 |     return 1.5
    ///   |            ^^^
    /// ```
    pub fn render(&self, file_name: &str, source: &str) -> String {
        let (line, col) = line_col(source, self.span.start);
        let line_text = source.lines().nth(line - 1).unwrap_or("");
        let line_num = line.to_string();
        let gutter = " ".repeat(line_num.len());

        // Caret width within this line, at least 1, clamped to the line end.
        let span_len = self.span.end.saturating_sub(self.span.start).max(1);
        let width = span_len.min(line_text.chars().count().saturating_sub(col - 1).max(1));

        let mut out = String::new();
        out.push_str(&format!("error[{}]: {}\n", self.phase, self.message));
        out.push_str(&format!("{gutter}--> {file_name}:{line}:{col}\n"));
        out.push_str(&format!("{gutter} |\n"));
        out.push_str(&format!("{line_num} | {line_text}\n"));
        out.push_str(&format!(
            "{gutter} | {}{}",
            " ".repeat(col - 1),
            "^".repeat(width)
        ));
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}]: {}", self.phase, self.message)
    }
}

/// 1-based (line, column) of a byte offset. Columns count characters.
pub fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = source[line_start..offset].chars().count() + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_basics() {
        let src = "abc\ndef\nghi";
        assert_eq!(line_col(src, 0), (1, 1));
        assert_eq!(line_col(src, 2), (1, 3));
        assert_eq!(line_col(src, 4), (2, 1));
        assert_eq!(line_col(src, 9), (3, 2));
    }

    #[test]
    fn span_join() {
        let a = Span::new(2, 5);
        let b = Span::new(8, 12);
        assert_eq!(a.to(b), Span::new(2, 12));
        assert_eq!(b.to(a), Span::new(2, 12));
    }

    #[test]
    fn render_points_at_source() {
        let src = "x = 1\ny = oops\n";
        let d = Diagnostic::new(Phase::Semantic, "name 'oops' is not defined", Span::new(10, 14));
        let rendered = d.render("test.py", src);
        assert!(rendered.contains("test.py:2:5"));
        assert!(rendered.contains("y = oops"));
        assert!(rendered.contains("^^^^"));
    }
}
