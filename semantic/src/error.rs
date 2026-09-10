//! Diagnostic helpers and the semantic `Result` alias.

use common::{Diagnostic, Phase, Span};

pub(crate) type SResult<T> = Result<T, Diagnostic>;

/// Name of the synthesized entry function holding top-level statements.
pub const ENTRY_NAME: &str = "__main__";

/// `__future__` is a compiler directive, not a loadable module.
pub const FUTURE_MODULE: &str = "__future__";

/// Modules that exist only to name types. They have no runtime content here,
/// so importing one binds annotation names and loads nothing — which is what
/// lets an ordinary typed Python file, `from typing import Optional` and all,
/// compile at all.
pub const TYPING_MODULES: [&str; 2] = ["typing", "collections.abc"];

/// Is this an annotation-only module?
pub fn is_typing_module(name: &str) -> bool {
    TYPING_MODULES.contains(&name)
}

/// Feature names CPython's `__future__` accepts. Every one of these is either
/// mandatory in Python 3 or, for `annotations`, already how PyRs behaves: names
/// in annotations are resolved after the whole module is parsed, so the import
/// is a no-op rather than a behaviour switch.
pub(crate) const FUTURE_FEATURES: [&str; 10] = [
    "nested_scopes",
    "generators",
    "division",
    "absolute_import",
    "with_statement",
    "print_function",
    "unicode_literals",
    "barry_as_FLUFL",
    "generator_stop",
    "annotations",
];

/// Validate `from __future__ import ...` the way CPython does, then treat it as
/// a no-op. Unknown features must be rejected rather than silently ignored,
/// since a program relying on one would otherwise compile with wrong semantics.
pub(crate) fn check_future_import(
    names: &[(String, Option<String>, Span)],
    star: bool,
    span: Span,
) -> SResult<()> {
    if star {
        return Err(err("'from __future__ import *' is not allowed", span));
    }
    for (name, _, name_span) in names {
        if !FUTURE_FEATURES.contains(&name.as_str()) {
            return Err(err(
                format!("future feature {name} is not defined"),
                *name_span,
            ));
        }
        if name == "barry_as_FLUFL" {
            return Err(err(
                "future feature barry_as_FLUFL is not supported",
                *name_span,
            ));
        }
    }
    Ok(())
}

pub(crate) fn err(message: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic::new(Phase::Semantic, message, span)
}
