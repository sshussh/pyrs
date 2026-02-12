# Python Compiler in Rust: Detailed Plan (Beginner-Friendly)

This plan assumes you are new to compilers and language design. It explains each concept as we go and breaks the project into phases that build on each other. You will start with a strict, typed subset of Python 3.14 and gradually grow toward CPython compatibility.

Key choices already made:
- Python version: 3.14
- Parser strategy: LALR (parser generator)
- Backend: LLVM IR (native codegen)
- Type annotations: mandatory (strict) in Phase A

Table of contents
1) What a compiler is (in plain words)
2) High-level architecture of this project
3) Concepts you will meet (glossary)
4) Phase-by-phase roadmap
5) Detailed steps for each phase
6) Milestones and verification
7) Suggested folder structure
8) Optional reading and stretch goals

-------------------------------------------------------------------------------
1) What a compiler is (in plain words)
-------------------------------------------------------------------------------
A compiler is a program that takes source code (like Python) and converts it
into another form that computers can run (like native machine code).

To do this, a compiler usually performs the following steps:
1) Parse the text into a structured tree (AST).
2) Understand names and types (semantic analysis).
3) Convert the AST into a lower-level representation (IR).
4) Translate the IR into machine code (using LLVM in our case).

Think of it as a series of translators, each turning the program into a more
precise and computer-friendly form.

-------------------------------------------------------------------------------
2) High-level architecture of this project
-------------------------------------------------------------------------------
Input Python 3.14 source
  -> Lexer + Parser (LALR) produces AST
  -> Semantic analysis (scopes, symbols, type checking)
  -> Typed IR (lower-level, explicit types and control flow)
  -> LLVM IR codegen
  -> Native executable
  -> Small runtime library (for printing, memory, etc.)

Because we require type annotations, the compiler can avoid many of Python's
dynamic behaviors at first. Later phases will add more dynamic features.

-------------------------------------------------------------------------------
3) Concepts you will meet (glossary)
-------------------------------------------------------------------------------
Lexer:
  Splits raw text into tokens (keywords, identifiers, numbers, etc.).

Parser:
  Consumes tokens and builds a structured representation of the program.
  We will use LALR, which is a formal parsing technique suitable for many
  programming languages. LALR is supported by parser generators and is
  deterministic and efficient.

AST (Abstract Syntax Tree):
  A tree representation of the program structure (functions, statements,
  expressions). It is "abstract" because it omits low-level details like
  parentheses and commas.

Semantic analysis:
  Checks meaning beyond syntax. Examples: Is a variable defined before use?
  Do types match? Are function calls correct?

Symbol table:
  A data structure that tracks names (variables, functions) and their info
  (type, scope, location).

Type checking:
  Ensures operations are done on compatible types (e.g. adding two ints).
  In our strict mode, missing type annotations are errors.

IR (Intermediate Representation):
  A lower-level, compiler-friendly form of the program. Easier to optimize and
  translate to machine code. Our IR will be typed and explicit.

LLVM IR:
  A well-known IR used by LLVM to generate machine code. We will emit LLVM IR
  from our typed IR.

Runtime:
  Supporting code your compiled program needs (printing, memory helpers,
  runtime errors). Python-like languages need more runtime support than C.

-------------------------------------------------------------------------------
4) Phase-by-phase roadmap
-------------------------------------------------------------------------------
Phase A: Strict typed core (bootstrap)
  Goal: Compile a small, typed Python subset to native code.

Phase B: Hybrid Python
  Goal: Introduce a boxed "object" type and limited dynamic features.

Phase C: CPython-like semantics
  Goal: Add major Python behaviors (classes, descriptors, exceptions, etc.).

Phase D: Compatibility drive
  Goal: Reach broad CPython compatibility and run many real Python programs.

-------------------------------------------------------------------------------
5) Detailed steps for each phase
-------------------------------------------------------------------------------

PHASE A: STRICT TYPED CORE
--------------------------
What you build:
- A compiler that accepts Python 3.14 syntax but only for a strict subset
- All variables and function signatures must be annotated
- Generates native binaries using LLVM

Step A1: Set up a Rust workspace
  Concept: A Rust workspace holds multiple crates (compiler, runtime, tests).
  Create a workspace with these crates:
  - compiler: the compiler logic
  - runtime: a small runtime library (print, alloc)
  - tests: integration tests

Step A2: Choose and integrate an LALR parser generator
  Concept: LALR parsers are often generated from grammar files.
  The generator will give you a parser that outputs your AST.
  Key tasks:
  - Define tokens (keywords, identifiers, literals)
  - Define grammar rules for expressions and statements
  - Implement AST node types in Rust
  - Write unit tests: tokenization and parsing

Step A3: Define the AST
  Concept: AST nodes are Rust structs or enums that represent code structure.
  You need nodes for:
  - Module (list of statements)
  - Function definitions
  - Variable declarations
  - Expressions (literals, binary ops, calls, names)
  - Control flow (if, while, for)

Step A4: Implement a basic error system
  Concept: Good errors are essential and help debugging.
  Tasks:
  - Add file/line/column tracking to tokens and AST nodes
  - Create an error type with human-readable messages
  - Surface parse and type errors cleanly

Step A5: Implement name resolution and symbol tables
  Concept: Names must be resolved to declarations. Use scopes.
  Tasks:
  - Add scope stack (global, function, local)
  - When you enter a function, push a new scope
  - Store each name with its type and definition location
  - Error if a name is used before definition

Step A6: Implement strict type checking
  Concept: Every variable must have a known type.
  Rules for Phase A:
  - All function parameters and return types are required
  - All local variables must be annotated
  - Literal types are known (e.g., 1 is int)
  - Binary ops must match types (int + int is ok, int + str is error)
  Tasks:
  - Parse type annotations into a type AST (e.g., list[int])
  - Define a Type enum in Rust
  - Implement type checking for each AST node

Step A7: Lower the AST to a typed IR
  Concept: IR is simpler than AST, with explicit control flow.
  Tasks:
  - Create IR types and values
  - Convert expressions to IR instructions
  - Convert if/while/for into explicit basic blocks and jumps

Step A8: Emit LLVM IR
  Concept: LLVM IR is text or in-memory structures that LLVM turns into machine
  code. You will map your IR types to LLVM types.
  Example mapping:
  - Python int (Phase A) -> i64
  - float -> double
  - bool -> i1
  Tasks:
  - Implement a codegen layer that walks your IR
  - Emit LLVM instructions for arithmetic, branches, calls
  - Link against runtime library

Step A9: Build a minimal runtime library
  Concept: Even a tiny language needs helper functions.
  Tasks:
  - Provide print_int, print_float, print_str
  - Add basic string/array helpers as needed
  - Keep it small and C-like for now

Step A10: End-to-end tests
  Concept: Test the full pipeline.
  Tasks:
  - Create simple programs and expected outputs
  - Add CI script to compile and run

PHASE B: HYBRID PYTHON
----------------------
Goal: Add limited dynamic features without losing LLVM performance.

Step B1: Introduce a boxed Object type
  Concept: A boxed object is a heap-allocated value that carries type info.
  Tasks:
  - Define a runtime object layout: type_id + pointer to data
  - Add runtime helpers to create boxed int/float/str
  - Add dynamic dispatch for operations on object

Step B2: Allow untyped locals (optional)
  Concept: Gradual typing. Untyped locals become object type.
  Tasks:
  - Add a type "object"
  - Change type checker to allow missing local annotations
  - Insert runtime checks where needed

Step B3: Add core container types
  Concept: list and dict become runtime-managed objects.
  Tasks:
  - Implement list and dict in runtime
  - Support indexing and iteration

Step B4: Improve errors
  Concept: Dynamic errors must be understandable.
  Tasks:
  - Add runtime exceptions with messages
  - Map them to source locations when possible

PHASE C: CPYTHON-LIKE SEMANTICS
-------------------------------
Goal: Add key Python behaviors that users expect.

Step C1: Classes and attribute access
  Concept: Python objects have attributes stored in dictionaries.
  Tasks:
  - Implement class objects and instances
  - Implement attribute get/set
  - Add method lookup

Step C2: Descriptor protocol basics
  Concept: Properties and methods rely on descriptors.
  Tasks:
  - Implement __get__ and __set__ behavior
  - Ensure method binding (self)

Step C3: Exceptions
  Concept: Python uses exceptions for control flow and errors.
  Tasks:
  - Runtime exception objects
  - Try/except and raise in IR and codegen

Step C4: Iterators and generators
  Concept: for loops and generators rely on iterators.
  Tasks:
  - Implement __iter__ and __next__
  - Add generator state machine lowering

Step C5: Async/await (advanced)
  Concept: Async uses coroutines and event loops.
  Tasks:
  - Basic coroutine objects
  - await lowering to state machines

PHASE D: COMPATIBILITY DRIVE
----------------------------
Goal: Make it run real Python programs and pass tests.

Step D1: CPython test suite sampling
  Concept: Test against real Python behavior.
  Tasks:
  - Build a harness to run selected tests
  - Track gaps as issues

Step D2: Expand standard library coverage
  Concept: Many Python programs need stdlib.
  Tasks:
  - Implement or wrap key stdlib modules
  - Document unsupported modules

Step D3: Performance tuning
  Concept: LLVM can optimize with passes.
  Tasks:
  - Add optimization levels
  - Profile hotspots

-------------------------------------------------------------------------------
6) Milestones and verification
-------------------------------------------------------------------------------
Milestone 1: Hello World
  - Parse and typecheck a function with print
  - Emit LLVM IR
  - Run executable

Milestone 2: Expressions and functions
  - Typed arithmetic
  - Function calls with typed signatures

Milestone 3: Control flow
  - if/else, while, for

Milestone 4: Data structures
  - list and dict support (typed or boxed)

Milestone 5: Classes
  - Define a class, instantiate, call methods

Milestone 6: Exceptions and iterators
  - try/except and for loops over custom iterators

-------------------------------------------------------------------------------
7) Suggested folder structure
-------------------------------------------------------------------------------
project-root/
  compiler/
    src/
      lexer/
      parser/
      ast/
      sema/
      ir/
      codegen/
  runtime/
    src/
  tests/
    fixtures/
    integration/
  docs/
    design.md
    roadmap.md
  PLAN.md

-------------------------------------------------------------------------------
8) Optional reading and stretch goals
-------------------------------------------------------------------------------
Optional reading:
- "Crafting Interpreters" (for intuition about parsing and ASTs)
- LLVM docs on IR and optimization passes

Stretch goals:
- A small REPL for quick testing (even though you are compiling)
- Source-level debugger integration
- JIT compilation for faster feedback

-------------------------------------------------------------------------------
End of plan
-------------------------------------------------------------------------------
