# PyRs documentation

PyRs is an ahead of time compiler that turns a subset of Python into native
executables through LLVM. This directory is the complete documentation.

## Start here

| Document | What it covers |
|---|---|
| [GUIDE.md](GUIDE.md) | The language: types, statements, expressions, classes, modules, the standard library subset, runtime errors, and every documented difference from CPython |
| [CLI.md](CLI.md) | Every command and option |
| [TOOLING.md](TOOLING.md) | Projects, the `[tool.pyrs]` manifest, build caching, interpreter resolution, releases |

## How it works

| Document | What it covers |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | The phases, the crates, the IR contract, symbol and layout conventions, multi-module compilation |
| [INFERENCE.md](INFERENCE.md) | How types are recovered without annotations, and the learned predictor behind `--infer` |
| [DYNAMISM.md](DYNAMISM.md) | Values whose type is unknown until run time: representation, kernels, profiles, guarded specialization, polymorphic caches |
| [PERFORMANCE.md](PERFORMANCE.md) | Benchmarks, methodology, and the specific changes that produced the numbers |
| [GC.md](GC.md) | The garbage collector: design, roots, invariants, diagnostics, limits |
| [PRIMITIVES.md](PRIMITIVES.md) | What belongs in LLVM IR, in the C runtime, and in the PyRs standard library |
| [INTEROPERABILITY.md](INTEROPERABILITY.md) | Compatibility mode and the CPython extension bridge |

## Reference and project material

| Document | What it covers |
|---|---|
| [SPECIFICATIONS.md](SPECIFICATIONS.md) | The supported subset stated precisely, phase contracts, versioning |
| [EXTENDING.md](EXTENDING.md) | Contributing: where each kind of change lives, testing discipline, worked examples |
| [ROADMAP.md](ROADMAP.md) | Open gaps and what 1.0 requires |
| [../CHANGELOG.md](../CHANGELOG.md) | What changed in every release |

## Conventions used in these documents

* Code that appears in an example has been run. Output shown next to a program
  is that program's real output.
* Performance figures name the command that produces them. Nothing in these
  documents is an estimate.
* A difference from CPython is documented where it exists, with the reason, in
  the [language reference](GUIDE.md#differences-from-cpython).
