//! Semantic analysis: name resolution, type checking, and lowering the AST
//! into the typed IR.
//!
//! Typing rules (a statically-typed subset of Python, mypy-flavored):
//! - `int`, `float`, `bool`, `str`, `list[T]` values; `bool` is assignable
//!   to `int`, and `int`/`bool` are assignable to `float` (implicit
//!   promotion casts are inserted).
//! - a local's storage type is the join of all RHS types (and annotation);
//!   bare multi-assign like `x = 1; x = "a"` yields `int | str`.
//! - `/` is true division and always produces `float`; `//` and `%` follow
//!   Python's floored semantics; `**` on ints yields int (a negative
//!   exponent traps at runtime), on floats yields float.
//! - str supports `+` (concat), `*` int (repeat), comparisons, indexing,
//!   `len()`, and `str(...)` conversions.
//! - lists are homogeneous; they support indexing (read/write), `len()`,
//!   `.append(...)`, and iteration; assignment aliases (like Python).
//! - conditions accept any value with truthiness (numerics `!= 0`,
//!   str/list `len != 0`); `and`/`or`/`not` produce `bool`.
//! - `for` iterates `range(...)`, lists, and strings; it desugars to a
//!   `while` whose `continue` target runs the increment.
//!
//! The program entry is the top-level script statements; if there are none
//! but a zero-parameter `main` is defined, `main()` is called instead.
//!
//! The implementation is split across domain modules; [`prelude`] re-exports
//! crate-visible helpers so call sites stay unqualified after the split.

mod analyze;
mod builtins;
mod class_env;
mod classes;
mod closures;
mod ctx;
mod error;
mod flow;
mod imports;
mod infer;
mod lower_expr;
mod lower_fn;
mod lower_stmt;
mod module;
mod ops;
mod types;

mod prelude;

#[cfg(test)]
mod tests;

pub use analyze::{analyze, analyze_library, analyze_program};
pub use error::{ENTRY_NAME, FUTURE_MODULE, TYPING_MODULES, is_typing_module};
pub use module::ModuleInput;

/// Health-check used by smoke tests.
pub fn ping() -> String {
    String::from("pong")
}
