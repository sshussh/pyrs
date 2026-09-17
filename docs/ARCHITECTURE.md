# Architecture

How PyRs is built: the phases a program passes through, what each one is
allowed to know, and the contracts between them.

For the language itself see the [language reference](GUIDE.md). For the type
inference engine see [INFERENCE.md](INFERENCE.md), and for the machinery that
runs values whose type is not known until run time see
[DYNAMISM.md](DYNAMISM.md).

## The shape of the compiler

PyRs is an ahead of time compiler. It reads Python source and writes a native
executable. Nothing of the compiler survives into the program it produces:
there is no interpreter, no bytecode, and no dependency on CPython at run time.

```
source text
    |  lexer        logos tokens, INDENT and DEDENT synthesis, line joining
    v
Vec<(Token, Span)>
    |  parser       recursive descent, syntax only
    v
ast::Module
    |  semantic     name resolution, type inference, checking, lowering
    v
ir::Module          fully typed, fully desugared
    |  codegen      LLVM IR as text
    v
LLVM IR string
    |  C++ shim     parse, verify, optimize at O0 to O3, emit an object
    v
object file + runtime.c + gc.c  --cc -lm-->  native executable
```

Data flows in one direction. Each phase reads only the output of the phase
before it, and no phase can ask a later phase a question. The practical value
of that rule shows up in the IR contract below.

## Workspace layout

A Cargo workspace of seven crates whose dependencies form a directed acyclic
graph.

| Crate | Responsibility | Input | Output |
|---|---|---|---|
| `common` | `Span`, `Phase`, `Diagnostic`, shared by every phase | | types |
| `lexer` | Tokenize; synthesize `INDENT` and `DEDENT`; implicit line joining | source text | token vector |
| `parser` | Untyped abstract syntax tree, hand written recursive descent | source or tokens | `ast::Module` |
| `semantic` | Resolve names and imports, infer and check types, lower | one or more ASTs | `ir::Module` |
| `ir` | The typed contract handed to the backend. Data only, no analysis | | data types |
| `codegen` | Emit LLVM IR text, drive the C++ shim, embed the C runtime | `ir::Module` | object file |
| `cli` | Driver: module loading, build cache, project manifest, linking | `.py` paths | executable |

`ir` depends on neither `parser` nor `semantic`, which is what keeps the
backend honest: it cannot reach back into syntax to recover something the
front end failed to decide.

## The IR contract

`ir::Module` is a fully typed tree. Every expression carries a `Ty`, every
implicit conversion is an explicit cast node, and every piece of syntactic
sugar is gone. A `for` loop has become a `while` with an explicit step, a
comprehension has become a loop that appends, a `with` statement has become
the acquire, body and release sequence, and a chained comparison has become
temporaries with explicit ands.

The rule this buys is short: **if codegen has to work something out, semantic
is missing a check.** Users should not routinely see an `error[codegen]`
diagnostic, and when they do it is a compiler bug rather than a program error.
That rule is what makes the backend mechanical and the front end the only
place where language knowledge lives.

Compiler generated temporaries are named with a leading `.`, which is not a
legal Python identifier, so they can never collide with user names.

## Phase detail

### Lexer

`logos` provides the scanner. On top of it sits a visual width indent stack
that synthesizes `INDENT` and `DEDENT` tokens, a pending token queue, and a
parenthesis depth counter for implicit line joining. Blank and comment only
lines do not affect indentation, and any open blocks are closed with `DEDENT`
tokens at end of file. Bad input produces a `Diagnostic` with phase `Lex`, not
a panic.

### Parser

Hand written recursive descent with a precedence ladder from `or` down to
primary expressions. It was written by hand rather than generated so that
unsupported constructs can be recognized and reported by name: a `not
supported yet` diagnostic that names the feature is more useful than a generic
syntax error at the same position.

The AST carries no types and no resolved names.

### Semantic analysis and lowering

The largest phase, and the only one that understands Python. It resolves
locals, `global` and `nonlocal` declarations, module globals and imports;
infers and checks types; applies the numeric promotion ladder `bool` to `int`
to `float` and inserts explicit casts; desugars; verifies that every path of a
value returning function returns; and emits one flat `ir::Module` for the
whole program.

Type inference is substantial enough to have its own document:
[INFERENCE.md](INFERENCE.md).

### Codegen

`emit_llvm_ir` walks the IR and writes textual LLVM IR. Two styles coexist,
and the choice between them is a documented policy rather than a preference
(see [PRIMITIVES.md](PRIMITIVES.md)):

* **Inline IR and LLVM intrinsics** for operations that are hot or simple:
  list indexing with its bounds check, the length field load, `abs` through
  `llvm.abs.i64` or `llvm.fabs.f64`, scalar arithmetic.
* **Calls into `runtime.c`** for bulk string and list work, I/O, printing, and
  anything that is layout or operating system facing.

The distinction matters for speed in a way that is easy to underestimate. The
runtime is a separate object file, so a call into it is opaque to LLVM: it
blocks loop invariant hoisting for everything around it, not only for itself.
Section 3 of [PERFORMANCE.md](PERFORMANCE.md) has the measurement that made
this concrete.

### The C++ shim

`codegen/shim/src/lib.cc` exports a single function,
`pyrs_compile_ir(ir_bytes, len, out_path, opt_level, err_buf, ...)`. It parses
the textual IR with LLVM's IRReader, verifies it, runs the standard new pass
manager pipeline at the requested optimization level, and emits a position
independent object for the host triple.

The shim contains no language knowledge. Adding a PyRs feature never requires
touching it, except when a new LLVM release changes a C++ API. Its internal
symbols are compiled with hidden visibility so they cannot collide with the
Rust toolchain's own LLVM.

Textual IR is the only payload crossing the Rust to C++ boundary. There is no
binary serialization format to keep in sync.

### The C runtime

`codegen/runtime/runtime.c` and `codegen/runtime/gc.c` are embedded in the
`codegen` crate and written to a temporary directory at link time. They provide
Python faithful printing (shortest round trip float representation, `True` and
`False`, list representation), string and list heap objects, file I/O, the
`input` and `sys.argv` wiring, arithmetic helpers such as floored division and
integer power, the dynamic dispatch kernels described in
[DYNAMISM.md](DYNAMISM.md), and trap messages that match CPython.

Memory is managed by a nonmoving mark and sweep collector, documented in
[GC.md](GC.md).

### Linking

```text
cc program.o runtime.c gc.c -O2 -lm -o <output>
```

The `pyrs` binary links LLVM and the C++ shim. User programs link neither:
they get the object file the shim emitted plus the two C files. This is why a
compiled program has no dependency on a PyRs installation.

## Symbol and layout conventions

Changing anything in this table requires a coordinated edit of `emit.rs`,
`runtime.c` and the tracing logic in `gc.c`.

| Kind | Convention |
|---|---|
| User function | `@pyrs_<name>`, module qualified before mangling |
| Module global | `@g.<name>` |
| Compiler temporary | local name beginning with `.` |
| Runtime API | C functions named `pyrs_*` |
| String layout | `{ i64 len, bytes... }`, length prefixed, trailing NUL for C interop |
| List layout | `{ i64 len, i64 cap, i64* data }` with 8 byte value slots |
| Print and contains tags | scalars 0 to 3; nested list element tag is `4 + 8 * inner` |
| Program entry | synthetic module `__main__` for the root script |

List slots hold 8 bytes whatever the element type: integers and booleans as
integers, floats bit cast, pointers through `ptrtoint` and `inttoptr`. The
encoding is owned by two helpers, `slot_from_value` and `value_from_slot`, so
it is defined in exactly one place.

### Integers

`int` is arbitrary precision, so it cannot simply be an `i64`. It is a tagged
word:

```text
T(v) = (v << 1) | 1        for v in [-2^62, 2^62 - 1]   (bit 0 set)
       pointer to PyrsInt  otherwise                    (bit 0 clear)
```

Two properties of that encoding carry the arithmetic. `T` is strictly
increasing and never wraps, because its image is exactly `[-2^63 + 1,
2^63 - 1]`. And `T(v)` always has bit 0 set where a collector pointer does not,
so `a & b` has bit 0 set precisely when both operands are small.

The overflow check exploits the first property. Rather than untagging, adding,
and checking the result against the small range, the emitted code computes
`2s + 1` directly in one `i64` operation and takes the signed overflow flag as
the range test. Writing `s` for the true mathematical result:

```text
2s + 1 <=  2^63 - 1   <=>   s <= 2^62 - 1 = SMALL_MAX
2s + 1 >= -2^63       <=>   s >= -2^62 - 1/2   <=>   s >= -2^62 = SMALL_MIN
```

The half integer on the lower bound is what makes the test exact rather than
conservative: `s = SMALL_MIN - 1` gives `2s + 1 = -2^63 - 1`, which does
overflow. So "no overflow" means exactly "the result is a small int", with no
slack at either end, in one instruction.

### Strings

A string is UTF-8 with a cached code point count:

```c
typedef struct {
    long long cplen;   /* code points, which is what len() returns */
    long long len;     /* UTF-8 bytes */
    char data[];
} PyrsStr;
```

UTF-8 was kept rather than a PEP 393 style kind tagged buffer because the
alternative converts at every boundary: `print`'s raw `fwrite`, file I/O, the
CPython bridge, literals, and every concatenation between kinds. The cost is
that indexing a non-ASCII string is amortized rather than exactly O(1), which
is a [documented difference](GUIDE.md#differences-from-cpython).

Two details do most of the work. `cplen` comes first, because codegen's
`emit_len` loads the first `i64` of every sized object, so putting the Python
answer there meant `len()` needed no codegen change at all. And the byte count
keeps the name `len`, because nearly every existing use of `->len` means bytes:
`memcmp`, `memcpy`, `fwrite`, the substring searches, the bridge's
`PyUnicode_DecodeUTF8`. The ASCII fast path then needs no flag, since every
code point being one byte is exactly `cplen == len`.

## Multi-module compilation

`cli/src/modules.rs` resolves the import graph before analysis begins. Absolute
imports search four roots in order, first hit wins:

1. the entry script's directory, which shadows everything else;
2. `PYRS_STDLIB` when set to a directory, for development and testing;
3. the workspace `stdlib/` directory when present, for development;
4. the standard library sources embedded in the `pyrs` binary itself.

Once a top level package is found under one origin, its submodules resolve
only under that same origin. An incomplete user `os/` directory therefore
cannot silently pick up `os.path` from the embedded standard library.

The loader parses the root and its dependencies, rewrites relative imports to
absolute names using the importer's `__package__`, detects cycles, and returns
the modules in topological order with dependencies first. The index of a module
in that vector is its diagnostic file identifier.

`semantic::analyze_program` then collects signatures and global surfaces across
all modules, validates every import binding, lowers each module into a
per-module namespace (the root keeps bare IR names, others are prefixed with
their dotted module name), and merges the result into a single `ir::Module`.
Module bodies execute at their import site, exactly as in Python, and parent
package initializers run before their children.

## Diagnostics

Every user facing failure from the lexer, parser or semantic phase is a
`common::Diagnostic` carrying a phase, a message, a span and a file index.
Rendering produces `file:line:column`, the message, and a caret underline
beneath the narrowest useful span.

Runtime failures print to standard error and exit with status 1, reusing
CPython's message text wherever the runtime claims parity.

## Design principles

1. Crates are unidirectional. The IR is the only contract between the front
   and back ends.
2. Python semantics win on every supported construct. Parity with CPython is
   verified by differential testing, not asserted.
3. Textual LLVM IR is the only payload crossing into C++.
4. The shim stays thin. Language knowledge lives in Rust and `runtime.c`.
5. Diagnostics, never panics, for user program errors.
6. Measure parity first and performance second. Never trade silent wrongness
   for speed.
7. Primitives before standard library. A finite native kit, then modules
   written in PyRs itself.
8. Document every deviation from CPython until it is removed.
