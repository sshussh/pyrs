# PyRs Compiler Specification & Architecture

This document outlines the architectural decisions, technology stack, and build strategies for the PyRs hybrid compiler project.

## 1. Core Technology Stack

- **Frontend / Driver:** Rust (Edition 2024)
- **Backend / LLVM Shim:** C++23 / C++26
- **Build System:** Cargo Workspaces + CMake
- **Native Toolchain:** LLVM (linked statically via `llvm-config`), Clang/GCC
- **Key Rust Libraries:** * `logos` (Lexer)
  - `clap` (CLI)
  - `cmake` (FFI build)
  - `chumsky` (Parser - Planned)
  - `serde` + `rmp-serde` (IR Serialization - Planned)

## 2. Workspace Architecture & Data Flow

The project is divided into specialized crates to enforce a strict, unidirectional data flow and isolate the C++ FFI.

| Phase / Crate  | Role & Responsibility                                                                                                         | Input           | Output            | Dependencies             |
| :------------- | :---------------------------------------------------------------------------------------------------------------------------- | :-------------- | :---------------- | :----------------------- |
| **`cli`**      | **Orchestrator.** Parses CLI arguments (`clap`), handles File I/O, pipes data through phases, and manages final LLVM linking. | Source File     | Native Executable | All crates               |
| **`common`**   | **Foundation.** Houses shared types (Spans, Diagnostics, File IDs) to prevent circular dependencies.                          | -               | Shared Types      | None                     |
| **`lexer`**    | **Scanner.** Wraps `logos` with a custom state machine to handle Python's semantic whitespace (`INDENT`/`DEDENT`).            | Source Text     | `Vec<Token>`      | `common`                 |
| **`parser`**   | **Syntax Analysis.** Consumes tokens to build the Abstract Syntax Tree (AST).                                                 | `Vec<Token>`    | AST               | `lexer`, `common`        |
| **`semantic`** | **Analysis & Lowering.** Handles name resolution, type checking, and lowers the AST into the Intermediate Representation.     | AST             | IR                | `parser`, `ir`, `common` |
| **`ir`**       | **The Contract.** Pure data structures representing lowered instructions. Defines the boundary between Rust and C++.          | -               | -                 | `common`                 |
| **`codegen`**  | **LLVM Bridge.** Exposes `extern "C"` bindings. Responsible for building the C++ LLVM shim via CMake.                         | IR (Serialized) | Object Code       | `ir`, `cmake`            |

## 3. Build and Linking Strategy

To successfully bind Rust to statically-linked LLVM on Linux (specifically Arch Linux), the build pipeline enforces a strict dependency resolution order via `build.rs`:

1. **CMake Invocation (`codegen/build.rs`):** Compiles the C++ shim (`lib.cc`) into a static archive (`libcodegen_shim.a`).
2. **Metadata Propagation:** The `links = "codegen_shim"` directive in `codegen/Cargo.toml` guarantees linker flags propagate up to the final binary crate.
3. **Shim Linking (`cli/build.rs`):** Cargo links the native shim archive first.
4. **LLVM Component Resolution (`cli/build.rs`):** `llvm-config --link-static --libs` dynamically fetches the exact static LLVM component libraries (e.g., `core`, `executionengine`, `analysis`) and links them.
5. **System Dependency Resolution (`cli/build.rs`):** `llvm-config --system-libs` fetches required system libraries (e.g., `z`, `zstd`, `m`, `rt`, `dl`, `ncurses`, `xml2`) to satisfy LLVM's internal requirements.
6. **C++ Runtime (`cli/build.rs`):** Statically links the C++ standard library (`stdc++`).

## 4. Language Implementation Rules

- **Semantic Whitespace:** Managed entirely in the `lexer` via a visual-width calculation (`calc_indent`) and an `indent_stack`. It buffers `Indent` and `Dedent` tokens into a `VecDeque` and correctly unrolls open blocks upon hitting `EOF`.
- **C++ FFI Boundary:** C++ logic is strictly isolated in `codegen/shim`. Rust communicates with it by passing pointers to serialized IR byte arrays (e.g., MessagePack) to ensure memory safety across the boundary.
- **Symbol Visibility:** Visibility of internal C++ symbols is explicitly hidden (`CXX_VISIBILITY_PRESET hidden`) to prevent namespace collisions with Rust's native toolchain.
