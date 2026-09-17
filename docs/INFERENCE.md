# Type inference

Annotations are optional in PyRs. This document explains how the compiler
decides what type an unannotated parameter, variable or return value has, why
the algorithm is shaped the way it is, and what happens when there is no
evidence at all.

## The problem

Native code needs types. Adding two integers is one machine instruction;
concatenating two strings is a call into the runtime that allocates. A compiler
that does not know which one it is facing has three options: refuse the
program, compile a dynamic value that decides at run time, or work the type
out.

PyRs does the third wherever it can, falls back to the second when it cannot,
and refuses only for features outside the supported subset. An annotation is
therefore a performance and documentation tool, not an admission ticket.

## Evidence, not guesswork

The central design decision is that inference reads **evidence from the
program**, not heuristics about what a name looks like. There are four sources,
and they are applied in that order of authority.

### 1. Call sites

The strongest evidence. If the program contains

```python
def scale(values, factor):
    return [v * factor for v in values]

print(scale([1, 2], 3))
```

then `values` is a `list[int]` and `factor` is an `int`, because that is what
the program passes. This is a fact about the program, not an inference about
the world, and nothing can contradict it.

Every kind of call is a witness, not only direct function calls:

* a method call, including one dispatched virtually through a base class;
* a constructor call, which types `__init__`;
* a dunder behind an operator, so `a + b` types `__add__`;
* a callable passed as `key=` to `sorted`, `min` or `max`;
* a function value passed as an argument and called elsewhere.

### 2. The body

What the body does with a value constrains it. `x.upper()` means `str`;
`self.x: float = x` means `float` by way of an annotated field; iterating a
parameter and indexing the result constrains the element type.

### 3. Library signatures

A table of parameter and return types for builtins and standard library
functions, generated from typeshed by `scripts/gen_typeshed_seeds.py` and
checked into `semantic/src/typeshed_seeds.rs`. It is generated rather than
handwritten because the handwritten version had been wrong: `abs` was seeded as
`int` although it accepts `SupportsAbs[_T]`, and `math.floor` as `float`
although it has a `_SupportsFloor[_T]` overload.

The generator narrows each typeshed annotation to something PyRs can represent
and abstains when it cannot. A protocol, an unconstrained type variable or a
generic poisons the parameter and leaves it unseeded. `bytes`, `PathLike` and
`complex` are dropped from a union, so `str | bytes` seeds `str`. A constrained
type variable becomes the union of its constraints, so `AnyStr` seeds `str`.
The result is 27 functions, with `abs`, `math.floor` and `math.ceil`
deliberately seeding nothing. `make hygiene` verifies that the table is the
generator's output and carries the typeshed commit it came from.

### 4. Nothing

A parameter that none of the above reaches is dynamic. That is not an error.
See [DYNAMISM.md](DYNAMISM.md) for what a dynamic value can do, which is
everything CPython allows, at a cost.

## The algorithm: rounds of real lowering

Inference is not a separate pass over the AST. It is the real lowering, run
more than once.

A round lowers the whole program. While it does so, every call records the
type it passes to each unannotated parameter, and every unannotated function
records what it returns and what it yields. The next round starts with those
recordings as seeds. The process repeats until the seeds stop changing, with a
cap on the number of rounds.

Running the actual lowering rather than an approximation of it is what keeps
the answer honest: the types recorded are the types the compiler would really
produce, including every promotion and coercion it would really apply.

A fully annotated program settles in one round, because the first round records
nothing that was not already known.

### Optimistic then strict

Rounds settle **optimistically**. While a round is running, a dynamic argument
is ignored rather than treated as a contradiction, because it may be a
parameter that simply has not been typed yet. Treating it as evidence would let
one unresolved parameter poison every parameter downstream of it, and the
result would depend on visit order.

Once the optimistic rounds settle, one **strict** round runs. Any seed that a
dynamic argument contradicts is widened. The invariant this preserves is worth
stating precisely: **a seed is never a guess the program can falsify.** If the
program can pass something incompatible, the parameter ends up dynamic.

### Merging two observations

When two call sites disagree, the rules are:

| Observations | Result |
|---|---|
| The same type twice | that type |
| Two exact types, for example `int` and `float` | dynamic |
| A class and one of its base classes | the base class |
| A type and `None` | an optional of that type |

Integers and floats deliberately do not merge into a float. The conversion
would change what the program prints, and silently changing output to make
inference tidier is the kind of trade this compiler does not make.

### Overrides pool their evidence

A method and the method it overrides share a signature in practice, because a
virtual call site does not know which one it reaches. They therefore pool their
observations: evidence collected at any call site that could reach either one
constrains both.

### Reachability

A dynamic argument passed from a function that nothing reachable calls is
ignored. Roots are module bodies, dunder methods and functions whose value
escapes. Without this rule, a dead branch left over in a file could widen the
types of a hot function that the live program calls perfectly consistently.

### Escaping functions

When a function's value escapes, by being stored in a container or passed
somewhere the compiler cannot follow, its call sites are no longer a complete
census. Such a function keeps whatever its body determined and does not take
call site seeds that might be incomplete.

## Inspecting the result

`pyrs check --show-types` prints the signature every function compiled with.
This is the direct way to see what inference decided, and it is worth reaching
for before assuming a program is slow for some other reason.

```console
$ pyrs check --show-types -i prog.py
scale(values: list[int], factor: int) -> list[int]
describe(label: Any, verbose: Any) -> str
```

## Learned prediction

There is one case the evidence based engine cannot help with: **a function
nothing calls.** A library module exports functions for other people's code,
and every kernel in a CPython extension is called from outside the file by
definition. There is no call site to read, and for an extension a dynamic
parameter is not even representable, because the signature is a contract with
the C caller.

Until this point the answer was to require an annotation. `--infer ml` and
`--infer dl` add a second answer: a model trained offline on real annotated
Python, which reads the parameter's name, the enclosing function's name, its
default and the sequence of operations the body performs on it, and predicts
one of 25 types.

| Engine | Model | Weights |
|---|---|---|
| `ml` | Multinomial logistic regression over hashed features, solved with L-BFGS | 0.4 MB |
| `dl` | Hashed embeddings, a two layer transformer encoder over the usage sequence, an MLP head | 1.4 MB |

The weights are compiled into the binary with `include_bytes!` and evaluated by
`semantic/src/predict.rs` as a deterministic forward pass on the CPU. There is
no machine learning framework in the compiler, nothing is read from disk at
build time, and nothing touches the network. This is the arrangement LLVM uses
to ship its MLGO policies, and it is the only one that keeps a build
reproducible.

### A prediction is never trusted as a proof

The safety argument matters more than the accuracy, because a compiler that
guesses types is a compiler that produces wrong programs. The guess is fenced
in on five sides:

1. **It only fills a vacuum.** A parameter that any call site types keeps that
   type. The predictor is consulted only where the static engine has nothing at
   all, which in a whole program means only code that nothing calls.
2. **It abstains below 0.5 probability**, and the parameter stays dynamic.
3. **A prediction that breaks its function's lowering is withdrawn**, and the
   rounds run again without it.
4. **A prediction for a function whose value escapes is withdrawn**, because no
   argument would ever be able to contradict it.
5. **A program that fails to compile with predictions is compiled again without
   them**, so turning the flag on cannot turn a working program into an error.

`--infer ml` and `--infer dl` also bypass the build cache, which is keyed by
source, because the same source compiles to a different program under them.

What the flag can change is how fast a function nobody calls is, and whether an
extension builds at all. It cannot change what a program does.

### What it is worth

Measured on the 293 programs in this repository that compile and match CPython,
with every parameter and return annotation stripped. `exact` means the engine
chose the type the annotation compiles to.

With call sites present, which is the ordinary case:

| Engine | Compiles | Matches CPython | Parameters exact |
|---|---:|---:|---:|
| `static` | 99.3% | 99.3% | 89.0% |
| `ml` | 99.3% | 99.3% | 90.2% |
| `dl` | 99.3% | 99.3% | 90.0% |

The engines are indistinguishable, as intended. The predictors recover eight
parameters the static engine left dynamic, all of them in code nothing calls,
and the counts of wrong and failed types do not move at all.

With every module level statement removed, so that no function has a caller,
which is the library and extension case:

| Engine | Compiles | Parameters exact | Dynamic | Wrong |
|---|---:|---:|---:|---:|
| `static` | 92.5% | 41.3% | 30.8% | 7.7% |
| `ml` | 95.9% | 72.4% | 8.1% | 3.2% |
| `dl` | 95.9% | 70.4% | 8.9% | 4.4% |

The model is both more informative and more accurate here: 41.3% against 72.4%
exact, with fewer wrong types rather than more, because the static engine's
fallback of guessing from the body (`x + x` must be an `int`) is a worse guess
than the model's.

On parameters from five projects held out of training entirely, the linear
model names the annotation the author wrote 63.7% of the time. Read that for
what it is. It is enough to type a kernel nobody annotated. It is not a
signature, and an annotation still beats it: of 20 unannotated numerical
kernels built as CPython extensions, the annotated file gets 16 working and
`--infer ml` gets 4.

Every mispredicted signature in that experiment refused its call with a
`TypeError` naming the parameter. None returned a wrong value, because the
extension boundary checks the argument against the signature it was built with.

### Training and reproducibility

The offline harness lives in `scripts/infer_eval/` and is driven by make
targets. `make infer-train` builds a uv virtual environment from a locked
requirements file, clones the training projects, rebuilds the evaluation
corpus, trains both models on the GPU when one is present, and rewrites
`semantic/models/`.

The training data is deliberately not pinned to fixed commits: each project is
fetched at the head of its default branch, because seeing newer code is the
reason to retrain. Reproducibility comes from the record rather than the pin.
Every resolved commit, the compiler revision, a hash of every script in the
pipeline, and the device, seed and library versions of the run are written into
`semantic/models/MANIFEST.json`. `make infer-replay` fetches those exact
commits back.

`make hygiene` fails if the shipped weights do not match their manifest hashes,
if the feature extractor has changed since they were trained, or if the label
table in `predict.rs` and the manifest disagree. A model's output is an index
into that table, so a silent reordering would change every prediction.

[EXTENDING.md](EXTENDING.md) documents the harness for contributors.
