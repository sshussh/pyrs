# Learned type prediction, behind `--infer`

**Status: implemented, unreleased (ships in 0.153.0).** Follows
[the prediction research](2026-09-10-type-prediction-research.md) and
[the inference engine](2026-09-15-inference-engine.md).

## Why the question was reopened

The research doc concluded that a learned predictor was not justified, and the
inference-engine record measured that conclusion rather than asserting it:
every parameter the static engine still failed on in the PyRs corpus had its
answer written in the source, so those failures needed static rules, not a
model. Both were careful to scope the finding — **the corpus is program code,
where call sites exist**. Both named the untested case:

> Unannotated third-party code at a library boundary is the case a predictor
> would earn its keep, and the measurement that would show it has not been run.

`pyrs build-extension` is that boundary, exactly. Nothing inside an extension
calls its kernels, so *every* parameter is evidence-free: the static engine has
no call site to read, and a kernel's signature is a contract with every CPython
caller. That is the setting this work builds a measurement for, and then a
predictor for.

## What shipped

`--infer static|dl|ml` on `compile`, `run`, `check` and `build-extension`,
defaulting to `static`. Two predictors, trained offline in
`scripts/infer_eval/`, shipped as weights:

| engine | model | size |
|---|---|---:|
| `ml` | multinomial logistic regression over hashed features, solved with L-BFGS | 0.4 MB |
| `dl` | hashed embeddings, 2-layer transformer encoder over usage events, MLP head | 1.4 MB |

The weights are `include_bytes!`d from `semantic/models/` and run by
`semantic/src/predict.rs` as a deterministic CPU forward pass: no framework in
the compiler, nothing read from disk at build time, nothing on the network.
This is MLGO's arrangement, and it is the only one that keeps a build
reproducible.

Evidence, as the models read it, is the parameter's name and subtokens, the
enclosing function's name, its position and arity, its default, and a sequence
of usage events (`iterated`, `meth:upper`, `arg:len:0`, `getitem:int`,
`binop:mul:r:name`, …). The answer is one of 25 labels — the scalars,
containers PyRs can name, `class`, their optionals, and `any`.

## The safety argument

A prediction is a guess, and the compiler treats it as one:

- **It only fills a vacuum.** A parameter any call site types keeps that type;
  the predictor is asked only where the static engine has nothing at all. In a
  whole program that leaves only code nothing calls.
- **Below 0.5 probability it abstains**, and the parameter stays dynamic.
- **A prediction that breaks its function's lowering is withdrawn**, and the
  rounds run again without it. So is one whose function's value escapes, where
  no argument would ever contradict it.
- **A program that fails to compile with predictions is compiled again
  without them**, so the engine cannot turn a working program into an error.
- **`--infer ml|dl` bypasses the build cache**, which is keyed by source; the
  same source compiles to a different program under a predictor.

The claim these add up to is the one `cli/tests/learned_inference.rs` holds:
a program's *behaviour* never depends on the engine — only the speed of code
nothing calls, and whether an extension builds at all.

## Training and measurement

29 real projects (aiohttp … werkzeug, listed in
`scripts/infer_eval/projects.txt`), labelled from their own annotations, with
five projects — black, flask, httpx, rich, networkx — held out of training
entirely.

| | n | `ml` | `dl` |
|---|---:|---:|---:|
| held-out projects, exact label | 3,925 | **60.4%** | **58.2%** |
| PyRs corpus, out of fold (5 folds, grouped by file) | 468 | 58.9% | 67.5% |
| int8-quantized agreement with the original model | — | 99.06% | 99.85% |

The shipped models train on the corpus as well as the external projects, so
their in-sample corpus accuracy (99.4% / 95.7%) says nothing; the out-of-fold
row is the honest one, and `train_cv.py` groups folds by source file so no file
is on both sides.

Read the headline number as what it is: **on code it has never seen, a
predictor names the annotation the author wrote about six times in ten**. That
is enough to type a kernel nobody annotated. It is not a signature.

## Retraining, and what "reproducible" means here

`make infer-train` is the whole pipeline: a uv venv from a locked
`requirements.txt`, the projects fetched, the corpus rebuilt, both models
trained, and `semantic/models/` rewritten. It trains on the GPU by default —
`uv pip install --torch-backend=auto` resolves the CUDA, ROCm or CPU build of
the pinned torch from the machine's driver, and the scripts pick `cuda`, `xpu`,
`mps` or `cpu` in that order; `DEVICE=cpu` forces the fallback and a device
named explicitly but missing is an error.

The training data is deliberately **not** pinned. A retrain takes each
project's newest commit, because seeing newer code is the reason to retrain at
all. Reproducibility comes from the record instead of the pin: every resolved
commit lands in `fetched.json` and then in `MANIFEST.json`, beside the compiler
revision, a hash of every script in the pipeline, and the device, seed and
library versions of the run. `make infer-replay` fetches those commits back, so
any shipped weights can be rebuilt from the artifact that ships with them.

Determinism is centralised in `repro.py`, imported before torch (cuBLAS reads
its workspace setting at CUDA-context creation): one seed across `random`,
numpy and torch, deterministic cuDNN, the math SDPA backend rather than the
fused kernels whose backward pass reduces non-deterministically, `random_state`
on every scikit-learn estimator, and a sorted file walk in `dataset.py`. Two
runs of the same commits on the same device give bit-identical weights, checked
directly. A CPU run differs from a GPU run in the last bits of the float —
3 × 10⁻⁸ on a measured run — which is why the device is part of the record
rather than something to pretend away.

Both models train on the accelerator. The linear one used to be scikit-learn's
`saga`, which was CPU-only and, at 290k rows, stopped at `max_iter` with a
ConvergenceWarning every time — the shipped coefficients were wherever the
optimiser had got to, not the optimum. It is now full-batch L-BFGS in float64
through torch, which reaches a 1e-7 gradient in 1,316 iterations and 22
seconds, and scores better for being finished: **63.7% against 62.6%** on the
held-out projects. float64 because the stopping test is a gradient norm, and in
float32 the gradient bottoms out in rounding noise around 1e-5, so "converged"
would have meant "as close as float32 can see".

Two reproducibility bugs turned up while doing this, both real: the
cross-validation folds were assigned by `hash(str)`, which is salted per
process, so the out-of-fold accuracy moved every run; and `dataset.py` emitted
rows in `os.walk` order, which is the filesystem's business, and row order
reaches the solver.

## Verification

- **Feature parity.** The models are trained on `features.py`'s output and run
  on `predict.rs`'s, so a disagreement is a silent accuracy loss.
  `scripts/infer_eval/parity.py` compares the two through the hidden
  `pyrs check --show-features`, parameter by parameter: **488 parameters over
  the corpus, 100% identical** in features and in events.
- **Forward-pass parity.** Both of the compiler's forward passes were checked
  against independent stdlib-Python implementations of the numpy reference in
  `export_models.py` (itself checked against scikit-learn and PyTorch). Label,
  and probability to 6–7 significant digits, agree on every case tried,
  including the quantized embedding table.
- **Provenance.** `make hygiene` now fails when the weights do not match their
  manifest hashes, when `features.py` has changed since the models were
  trained, when `predict.rs`'s label table and the manifest's disagree (a
  model's output is an index into it), or when the manifest does not say what
  it was trained on. `scripts/test_gates.py` breaks each of those on purpose.

## What the models key on, and one surprise

The features are mostly what a reader would guess — `verbose` used in a
condition is a `bool`, `name` with `.upper()` is a `str`. The enclosing
function's name carries more weight than expected: the same body under
`def f(name)` reads as `any` at 0.99 confidence, because in the training
projects (mypy's test suite among them) a function called `f` is a fixture
whose parameters are annotated `Any`. The model is right about its corpus and
useless on a placeholder. `predict.rs` pins this as a test, since it is the
reason a sampled prediction must use the names real code uses.

## What it does to real programs

`corpus.py` admits every program in `examples/`, `compatibility/cases/`,
`benchmarks/` and the sources embedded in `cli/tests/*.rs` that compiles *with*
its annotations and matches CPython byte for byte: **293 programs, 663
parameters, 770 returns**. `evaluate.py` then strips every parameter and return
annotation and compiles each program with each engine, scoring every parameter
against the type PyRs compiles when the annotations are present.

**With call sites — a whole program, which is the ordinary case:**

| engine | compiles | matches CPython | params exact | dynamic | wrong | lost |
|---|---:|---:|---:|---:|---:|---:|
| `static` | 99.3% | 99.3% | 89.0% | 3.6% | 3.5% | 3.9% |
| `ml` | 99.3% | 99.3% | **90.2%** | 2.4% | 3.5% | 3.9% |
| `dl` | 99.3% | 99.3% | 90.0% | 2.6% | 3.5% | 3.9% |

The engines are indistinguishable here, as designed: every program compiles and
runs the same, and the predictors recover eight parameters the static engine
left dynamic — all of them in code nothing calls. `wrong` and `lost` do not
move at all.

**Without call sites — `--library` deletes every module-level statement, so no
function has a caller. This is the extension and library boundary:**

| engine | compiles | params exact | dynamic | wrong | lost |
|---|---:|---:|---:|---:|---:|
| `static` | 92.5% | 41.3% | 30.8% | 7.7% | 20.2% |
| `ml` | **95.9%** | **72.4%** | 8.1% | **3.2%** | 16.3% |
| `dl` | 95.9% | 70.4% | 8.9% | 4.4% | 16.3% |

This is the measurement the earlier records said had not been run, and it is
the one that justifies the feature: with nothing but names and bodies to read,
a prediction is right about **72% of parameters against 41%**, and it is right
*more often* than the static engine is — the body-inference guesses that
`static` falls back on (`x + x` must be an `int`) produce 51 wrong types where
`ml` produces 21.

**The kernels, end to end** (`extension_bench.py`, 20 numerical kernels built
as CPython extensions one at a time, called from Python, timed against the pure
Python function):

| build | kernels built | correct | geomean speedup |
|---|---:|---:|---:|
| annotated (the reference) | 16 / 20 | 16 | **3.94×** |
| `--infer static` | 0 | — | — |
| `--infer ml` | 7 | 4 | 2.47× |
| `--infer dl` | 7 | 5 | 1.81× |

`static` builds nothing, by design: an unannotated kernel parameter is named,
not guessed. The predictors get a third of the annotated build's kernels
working, and where a kernel does build correctly it is the same code — for
`count_primes`, 11.9× under every engine that types it, matching the annotated
build exactly.

**A misprediction refuses the call; it never returns a wrong number.** Five
kernel builds across both engines had a mispredicted signature (`bmi`,
`trapezoid`, `clamp` — `int` where the caller passes a float), and all five
raised `TypeError: bmi(): 'weight_kg' requires exact int` at the boundary. Not
one produced a wrong value. That is the extension ABI doing its job, and it is
what makes a predicted signature survivable rather than dangerous.

**Compile time.** Median `pyrs check` over the 293 stripped library-mode
programs: `static` 16.1 ms, `ml` 18.0 ms, `dl` 21.6 ms. The linear model is
free; the transformer costs about a third more, and it is the less accurate of
the two on parameters. `ml` is the engine to reach for.

## Limits, and what is left

`--infer` is off by default and should stay off by default. A whole program
already has its answers in its call sites, and the measurement above says so:
1.2 points of parameter accuracy, no change to what runs. The flag is for the
boundary, and at the boundary an annotation still beats a prediction 16 kernels
to 4.

Three things the numbers point at, in the order they are worth doing:

1. **The extension surface is as binding as the predictor.** Of the thirteen
   kernels `ml` does not build: four are body features the *annotated* build
   refuses too (`a, b = b, a + b`, a string method); four are typed
   `list[int]` where the boundary accepts only `list[float]` — a correct
   enough type that the ABI has no room for; and five are a parameter the
   predictor abstained on, left dynamic, which an extension cannot export.
   Only the middle four are a prediction problem, and widening the boundary to
   `list[int]` fixes them without touching a model.
2. **`int` where `float` was meant is the whole misprediction story** — every
   one of the five refused calls. The label vocabulary separates them and the
   models are simply wrong about which; a numeric-widening prior for a
   parameter whose body divides, or an `int`→`float` retreat when both are
   plausible, would cost little and recover three kernels.
3. **`ml` is the engine worth keeping.** It is more accurate on parameters
   (72.4% against 70.4%), has fewer wrong types (21 against 29), and costs
   nothing measurable at compile time where `dl` costs a third more. `dl` earns
   its place only on the corpus cross-validation (67.5% against 58.9%), which
   is the one measurement made on a different distribution from the shipping
   one; that disagreement is worth resolving before either model is retrained.
