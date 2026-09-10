# Predicting types at compile time: what the measurements say

**Status: research findings, nothing implemented. Revised 2026-09-10 after
adversarial verification — three headline numbers were corrected and one
conclusion was overturned. See [§7](#7-verification-what-survived-and-what-did-not).** Companion to
[the dynamism plan](2026-09-10-dynamism-at-native-speed.md). Six local
experiments plus a literature review. Every number below is either measured
here or cited; where a claim could not be verified it says so.

The question asked was how to rank four options — no typing at all, a
heuristic predictor, classical ML, and deep learning — for recovering type
information at compile time.

**The short answer is that the ranking was the wrong question.** Two
measurements moved the decision, and neither is about predictors.

---

## 1. The two findings that decide it

### PyRs's representation space is already almost complete

Across six heavily-annotated real projects (black, flask, httpx, rich,
pydantic, mypy — 31,899 parameters, 25,996 annotated):

| Annotation kind | Share of annotated params | Has a static PyRs representation? |
|---|---:|---|
| User class defined in the project | 32.0% | yes — `Ty::Class` |
| Scalar (`int`/`float`/`str`/`bool`) | 26.2% | yes |
| Union / `Optional` | 14.1% | yes — `Ty::Union`, **measured free** |
| Container | 9.7% | yes |
| Named class / enum / protocol | 8.0% | yes |
| **`Any` / `object`** | **4.7%** | **no — must box** |
| Callable | 0.6% | yes — `Ty::Closure` |
| other / unresolved | 4.5% | — |

**90.8% of annotated parameters fall inside the representation space PyRs
already has. Only 4.67% are genuinely dynamic.**

This reframes everything. The compiler does not need a model to *invent* a
type vocabulary — it needs to stop boxing the types it can already name.
Which leads directly to the second finding.

### The dynamic path is slow because of representation, not ignorance

Same 20M-iteration loop, `-O2`:

| Version | Time | Allocated | vs CPython |
|---|---:|---:|---:|
| `i: int` | 0.025s | 0 B | 33.5× |
| `v: int \| str = i` | 0.024s | 0 B | **34.9×** |
| `v: object = i` | 0.591s | 320 MB / 305 GCs | 1.4× |

A **two-member union is free**. `Ty::Union` lowers to an LLVM `{i32, i64}`
in two registers; `Ty::Any` lowers to a heap `PyrsUnionBox` with the same
two fields. Perfect type prediction on the third row would still leave it
boxed. **No predictor, of any sophistication, fixes this.**

---

## 2. Experiments

### E1 — How monomorphic is real Python, per argument, weighted by execution?

Instrumented `sys.monitoring` PY_START, recording the set of concrete types
reaching each parameter of each function, plus how often the site ran.

| Workload | Arg sites | Observations | Mono (by site) | Mono (by execution) | ≤4 types (by execution) |
|---|---:|---:|---:|---:|---:|
| black formatting 24 files | 586 | 54,532 | 86.5% | 64.4% | 94.8% |
| json encode/decode | 173 | 23,581 | 87.9% | 97.4% | 98.6% |
| rich rendering | 426 | 86,062 | 85.2% | 57.3% | 78.2% |
| **total** | **1,185** | **164,175** | **86.2%** | **65.4%** | **86.6%** |

Two things matter here.

**Hot sites are *more* polymorphic than average** — 86.2% of sites are
monomorphic but only 65.4% of executions land on a monomorphic site. Any
evaluation that counts sites equally will overstate the win. The metric must
be execution-weighted.

**A 4-way cache covers what a 1-way cache does not.** Monomorphic
speculation captures 65% of executions; ≤4 types captures 86.6%. That gap is
the argument for a small polymorphic inline cache over a single guarded
type, and it recurs in the literature (see §3).

Published comparison: Åkerblom & Wrigstad (DLS 2015, 36 apps, >7M sites)
report **96% monomorphic, ~94% of hot sites**. That is not in conflict —
they measure the *receiver* of `o.m()`; this measures *arguments*, which are
naturally more varied. Both matter to PyRs: receiver monomorphism enables
devirtualization, argument monomorphism enables unboxing.

### E2 — Where does type information actually live?

For all 12,601 annotated parameters in CPython's own stdlib:

| Evidence available | Share |
|---|---:|
| Only passed to another call → needs the call graph | 43.3% |
| Stored / passed through — no local evidence at all | 34.9% |
| Structural only (operator / index / iterate) | 10.5% |
| Weak | 9.1% |
| A method unique to one builtin type pins it exactly | **2.3%** |

Body-local inference — what `try_infer_param_from_body` does — reaches
about **2%** of real parameters. Nearly 80% of the information is in the
call graph.

### E3 — So how much does call-graph propagation actually recover?

Hid every annotation across the six-project corpus (17,117 parameters),
seeded only from literals, constructor calls and known builtin returns, then
iterated a worklist to a fixpoint.

**26.1%** determined, at **71%** agreement with the hidden annotations.

E2 said the information is structurally in the call graph; E3 says the
*seeds are too sparse to extract it*. The fixpoint runs out of fuel. This is
a lower bound — the simulation is much weaker than a real compiler's flow
analysis — but the direction is the finding.

### E4 — Does a seed cascade? (Hypothesis: yes. Answer: no.)

If seeds are the bottleneck, one predicted type should determine several
parameters downstream. Injecting ground truth for a fraction of parameters
and re-running the fixpoint:

| Seeds injected | Determined | Gain over the 4,460 baseline | **Marginal leverage** |
|---:|---:|---:|---:|
| 702 (5%) | 5,183 | 723 | **1.03×** |
| 1,404 (10%) | 5,795 | 1,335 | 0.95× |
| 3,511 (25%) | 7,435 | 2,975 | 0.85× |
| 7,022 (50%) | 9,904 | 5,444 | 0.78× |
| 14,044 (100%) | 14,085 | 9,625 | 0.69× |

**A seed buys approximately itself, and the return declines.** The
amplification hypothesis is refuted. A predictor's value is its direct hit
rate — there is no downstream multiplier to bank on.

Also note the ceiling: even with 100% ground-truth seeding, propagation
determines only 82.3% of parameters. The rest are never called within the
corpus or are called only with untypeable expressions.

### E5 — Heuristic vs classical ML, leave-one-project-out

Ten-way classification (`int/float/str/bool/list/dict/set/tuple/None/OTHER`)
— the question a compiler asks, not the papers' 1,000+ type vocabulary.
Trained on five projects, tested on the sixth. This is a much harder
generalization test than the usual random file split.

| Held out | n | Majority | Heuristic | Logistic reg | HistGB |
|---|---:|---:|---:|---:|---:|
| black | 1,745 | 51.0% | 53.6% | 57.7% | 52.4% |
| flask | 579 | 72.5% | 66.1% | 82.7% | 77.7% |
| httpx | 922 | 74.9% | 73.8% | 70.7% | 76.1% |
| rich | 1,824 | 57.6% | 60.4% | 64.0% | 59.7% |
| pydantic | 6,568 | 72.0% | 71.2% | 70.7% | 71.4% |
| mypy | 14,358 | 64.9% | 67.2% | 75.1% | 66.5% |
| **weighted** | **25,996** | **65.8%** | **67.0%** | **72.1%** | **66.9%** |

A linear model beats hand-written rules by ~5 points. Gradient boosting adds
nothing over it. Note how strong the majority baseline is: on this corpus
"just box it" is right 65.8% of the time, because most annotations in real
library code are user classes and unions — which, per §1, PyRs represents
statically anyway.

### E6 — Can the model be distilled into a shippable table?

Grok's literature review found a real gap: **no published work distills a
type-inference model into a lookup table and reports the loss.** So:
teacher = the logistic regression; student = an identifier→type table voted
by the teacher over unlabelled code. Held out mypy.

| | Accuracy | Artifact |
|---|---:|---:|
| Majority baseline | 64.9% | 0 |
| **Teacher (logistic regression)** | **75.1%** | **323 KB** |
| Student (distilled table, 588 entries) | 62.9% | 13 KB |
| Table counted straight from labels | 68.1% | 29 KB |

**Distillation into a name table loses 7 points and lands below the majority
baseline.** The teacher's advantage comes from *usage* features — methods
called, operators applied — which a name table structurally cannot hold.

But the result that matters is the artifact column: **the teacher is already
323 KB.** There is nothing to distill. A linear model over cheap syntactic
features is small enough to ship, deterministic, and microseconds to
evaluate.

---

## 3. What the literature says

Researched with three parallel Grok 4.6 agents; sources cited inline.

### Nobody has achieved "CPython dynamism + AOT speed"

> Every system that kept full dynamism and stayed compatible — PEP 659,
> Pyston, untyped Cython, Nuitka, mypyc on `Any` — lands at **~1.1–2×**.
> The 6–41× band is typed AOT / nopython JIT.

PyRs's measured 1.4× on the `Any` path is not a bug. **It is the industry
norm for that representation.** The speed comes from types, in every system
that has it.

| System | Mechanism | Cited speedup | On type miss | Standalone binary |
|---|---|---|---|---|
| mypyc | AOT C ext, gradual types | 1.5–5× typed; ~4× on mypy | `TypeError` | no |
| Cython typed | AOT to C types | 4× typed vars; 150× `cdef` | `TypeError` | no (libpython) |
| Nuitka | AOT calling libpython | ~1.74× (EASE 2025) | full Python semantics | packaging only |
| **Codon** | AOT LLVM + inference | **~39× mean (EASE 2025)** | **compile-time reject** | **yes** |
| Numba `@njit` | function JIT | ~11.6× mean | compile failure | no |
| Static Python (Cinder) | typed bytecode + JIT | 1.25× shallow, 3.3× refined | `TypeError` | no |
| PyPy | tracing JIT | 1.76× harmonic steady-state (IISWC 2020) | deopt + recompile | no |
| CPython 3.11 PEP 659 | inline caches, no JIT | 1.25× geomean vs 3.10 | rewrite opcode back | no |

Codon is the closest commercial proof that PyRs's current position —
standalone native binary, reject what you cannot type — is viable.

**Static Python is the closest architectural relative**, and its production
result is sobering: Instagram converted **541 modules over a year** for
**+3.7% RPS** (Lu et al., arXiv:2206.13831 §6). Its guards raise `TypeError`
rather than falling back to a generic path — the opposite of what the
dynamism plan proposes, and the reason its adoption cost was so high.

### Learned type inference: the honest numbers

Accuracy figures across papers are **not comparable** — they differ in
metric (exact match vs match-up-to-parametric vs "adjusted"), split
granularity (file-level leaks, project-level does not), and label vocabulary.

The best available proxy for what PyRs needs is TIGER's "Ele" bucket
(`int|float|str|bool|bytes`), project-level split, 10k ManyTypes4Py sample
(arXiv:2407.02095):

| Model | Top-1 exact on Ele |
|---|---:|
| Type4Py | 95.1% |
| TIGER | 94.5% |
| CodeT5-fine-tuned | 94.0% |
| TypeGen | 89.8% |

Type4Py's widely-cited **100%** is on `{str,int,list,bool,float}` under a
**file-level** split with `Any`/`None` dropped — an upper bound, not a
planning number. Independent re-evaluations put its *overall* exact match at
50.3% (TypeT5, project split) and 44.5% (TOSEM 2024, developer-only ground
truth).

**The single most decision-relevant number in the whole literature review**
is DeepTyper's comparison against an identifier-frequency baseline
(FSE 2018, Table 2):

| | Top-1 | Top-5 |
|---|---:|---:|
| Identifier MLE baseline | 37.5% | 78.9% |
| DeepTyper (neural) | 56.9% | 81.1% |
| **Neural advantage** | **+19.4 pp** | **+2.2 pp** |

**At top-5 the neural advantage nearly vanishes.** Combine that with E1's
finding that a ≤4-type cache covers 86.6% of executions where a 1-type guard
covers 65.4%, and the two independent results converge on the same design: a
**small polymorphic cache seeded by a cheap predictor captures nearly all the
value a neural network would.**

### Shipping ML in a compiler: MLGO is the precedent

LLVM's MLGO is the only production ML-in-compiler system of this shape
(arXiv:2101.04808, llvm.org/docs/MLGO.html):

| | |
|---|---|
| Model | Tiny MLP, 2 hidden layers (40, 20), **11 scalar features** |
| Shipped as | TensorFlow SavedModel **AOT-compiled at compiler build time** into a `.h` + `.o` of matrix-multiply loops with constant weights. **No TF runtime in the shipped compiler.** |
| Size | **~115 KB**, 0.08% of clang; ~1% total compile time |
| Benefit | up to 7% `.text` vs `-Oz`; regalloc 0.3–1.5% QPS |
| Deterministic | **yes, by construction** — "online training is an anti-goal" |

Their discipline is the part to copy: **correctness is never the model's
job.** The model picks a policy; legality checks stay in the compiler. A hard
cap bounds what a bad policy can do.

Also relevant: **GraalSP**, an ML static profiler enabled by default in
Oracle GraalVM `-O2`, worth **+7.46%** — and still far behind real PGO.
That is the honest ceiling for predicting what a profile would have told you.

### Reproducibility and provenance

- Android's PGO policy is the applicable analogue: *"profiles should be
  collected offline and checked in alongside the code to ensure reproducible
  builds."* Treat a type profile or a policy artifact as a **checked-in,
  hashed build input**.
- Floating-point nondeterminism is a solved non-issue for MLGO-style AOT CPU
  inference (single-threaded, fixed weights, no atomics, target triple fixed
  at compiler build time) — and is **re-introduced** if you evaluate a model
  graph at compile time. Don't.
- If the shipped artifact is **weights trained on scraped GitHub code**, that
  inherits Copilot-class provenance questions (*Doe v. GitHub*,
  N.D. Cal. 4:22-cv-06823) even though the model never emits code. A model
  trained only on **typeshed** plus permissively-licensed corpora avoids
  this entirely, and typeshed is hand-curated ground truth besides.

---

## 4. The revised ranking

Not four competing options — a pipeline, ordered by information quality per
unit of compile-time cost. What changed from the earlier draft is that
**tiers 1 and 2 got much bigger and tier 4 got much smaller.**

| Rank | Tier | Evidence | Verdict |
|---|---|---|---|
| **1** | **Unbox `Any` to `{i32,i64}`** | measured 23.6× tax; union is free | Not a prediction strategy at all, and worth more than every predictor combined. Do this first or nothing else matters. |
| **2** | **Honor annotations already present** | 81.5% of params annotated in modern code; 90.8% of those already representable | Nearly free. The corpus is already typed; PyRs must stop discarding that. |
| **3** | **Call-graph fixpoint + typeshed lookup** | E2: ~80% of info is in the call graph; E3: seeds are the bottleneck | Sound, exact, no guard needed. Typeshed supplies the seeds E3 showed are missing. |
| **4** | **Profile feedback** (`pyrs profile`) | GraalVM `.iprof` precedent; ground truth for what ran | Also the labelled dataset any later model needs. |
| **5** | **Linear model, ~300 KB, shipped like MLGO** | E5: 72.1% vs 67.0% heuristic; E6: nothing to distill | The right predictor. Beats the heuristic, beats gradient boosting, small enough to ship, deterministic. |
| **6** | **Deep learning, offline only** | 94–95% on scalars but +2.2 pp at top-5 over a name baseline | Use it to *generate* tier-5's features and training data. Never in the compile loop. |
| **7** | **Generic kernel + guard** | the always-correct floor | Not a fallback of last resort — the thing that lets tiers 1–6 be optimizations instead of requirements. |

### On deep learning specifically

Your three arguments were right, and one of them is the strongest point in
this whole document:

- *"Many models to transfer from"* — true, and the reason to use DL
  **offline**.
- *"It gets better with time"* — true, and preserved: regenerate the tier-5
  artifact as models improve. It is a data file, versioned and hashed like
  the Unicode tables `make hygiene` already checks.
- *"We are not constrained to the CPython stdlib for training data"* —
  **correct, and it was the flaw in my first measurement.** Typeshed alone is
  5,330 hand-curated stub files of exact signatures.

The objection was never to deep learning. It is to a 500 MB model inside a
6.5 MB compiler that compiles a program in 74 ms. MLGO shows the resolution
is standard practice: train offline, ship the policy.

The one place the evidence genuinely undercuts the DL case is DeepTyper's
+2.2 pp at top-5. With a 4-way polymorphic cache — which E1 says PyRs wants
regardless — most of the neural advantage is already captured by a cheap
predictor.

---

## 5. Where I was wrong

Recorded because the corrections are the useful part.

1. **"Seeds cascade through the call graph."** Refuted by E4: marginal
   leverage is 1.03× and declines to 0.69×. A prediction is worth its own
   site and nothing more.
2. **"43% of params need the call graph, so the call-graph fixpoint is the
   big win."** Half right. The information is there (E2), but propagation
   alone recovers only 26% (E3) because seeds are sparse. The fixpoint needs
   typeshed and profiles to be worth building.
3. **"Most annotations aren't predictable, so box them."** An artifact of my
   own label mapping, which collapsed user classes and unions into
   "unpredictable" — when PyRs represents both statically. Correcting it took
   the representable share from 34% to **90.8%**.

---

## 6. Recommendation

Build tiers 1–3. Measure the dynamism tax again. **Then** decide whether any
predictor is warranted, because after tier 3 the residue may not justify one
— and after tier 4 there will be real data to decide with instead of a guess.

The trap to avoid is building the predictor first, measuring it against
today's weak baseline, and concluding it is essential.

## Reproducing

Scripts in the session scratchpad: `monomorph.py` + `run_workload.py` (E1),
`propagate.py` (E2/E3), `leverage.py` (E4), `predict.py` (E5),
`distill.py` (E6), `repr_space.py` (§1). Corpus: black, flask, httpx, rich,
pydantic, mypy at HEAD, plus CPython 3.14.7's stdlib and typeshed.


---

## 7. Verification: what survived and what did not

Three Claude agents were told to attack the experiments and two Grok 4.6
agents were told to falsify the literature claims. The results below replace
the corresponding numbers above.

### Confirmed, and strengthened

**Representation is the bottleneck — now proven three independent ways.**
Re-ran the benchmark across union widths:

| Variant | best of 3 | GC allocated |
|---|---:|---:|
| plain `int` | 0.0233s | 0 B |
| `int \| str` | 0.0228s | 0 B |
| `int \| str \| float` | 0.0230s | 0 B |
| `int \| str \| float \| bool \| None` | **0.0229s** | **0 B** |
| `object` (`Any`) | **0.6539s** | **320 MB** |

**A five-member union costs the same as a plain int.** Union *width* is
free, so "`Any` is the open union" is a measured property, not an analogy.
And the emitted LLVM confirms the mechanism: the `object` and 5-union loops
are byte-identical except for one extra call to `@pyrs_union_box_new`. That
single call is the entire 28.6× gap.

Independently corroborated by the literature review: *"the 6–41× band is
almost entirely unboxing + killing the `PyObject*` protocol, not AOT vs
interpreter."*

**PyRs's own inference is stronger than assumed.** Stripping every parameter
and return annotation from `examples/*.py`: **7 of 11 still compile.** Two of
the four failures are *wrong* inferences rather than missing ones (`mean`
inferred `int`, received `float`) — the "fails on conflict instead of
widening" defect, not an absence of information.

**Gradient boosting still loses to logistic regression** on a corrected rerun
(67.8% vs 72.1%), so E5's ranking stands.

### Corrected

**E1's execution-weighted monomorphism was wrong, and the instrument had a
fatal bug.** The workload filter tested `SELF_DIR in filename` where
`SELF_DIR` was the scratchpad root — and the virtualenv lived underneath it,
so *every* site-packages file was disabled on first call. **All 164,175
observations came from CPython's stdlib**; the "three workloads" were three
ways of exercising one library. (Separately, the installed `black` is
mypyc-compiled to a `.so`, so `PY_START` never fires for it regardless.)

Re-run with the filter fixed, six workloads including numeric/sklearn, graph
algorithms and a test suite — 5,366 sites and 3,204,527 observations, 20x the
original data:

| | claimed | corrected |
|---|---:|---:|
| Monomorphic **sites** | 86.2% | **86.6%** |
| Monomorphic **executions** | 65.4% | **79.5%** |
| ≤4-type cache coverage | 86.6% | **96.1%** |

- **Site-level monomorphism survives and is stronger than claimed** — 86.6%,
  and stable at 85.0–87.8% across all six workloads. It held against every
  attack.
- **The 65.4% execution figure does not survive.** It was an artifact of the
  stdlib-only sample. More importantly, the corrected 79.5% should never be
  quoted alone: per-workload it ranges **62.0% to 98.1%** (stdev 15.3 pp), and
  choosing a different three of the six moves it from 63.4% to 94.7%. It is a
  property of the workload mix, not of Python.
- **The 4-type cache figure was a serious understatement**: 96.1% pooled,
  90.5% worst case.

The instrument itself was validated: an independent `sys.settrace`
re-implementation reproduced the rates exactly; the `_getframe(1)` guard
dropped 0 of 3,542,896 callbacks; `__name__` conflation affected 0.11% of
sites and flipped none; and excluding C calls biases the result by **±0.0 pp**
(74.3% of call executions are C calls, and their monomorphism is 87.5% —
identical to Python callees).


**"90.8% representable" was too lenient.** The classifier counted any
capitalised base name as representable. Under a strict definition
(TypeVar / Protocol / ABC / abstract std types / parameterized generics /
unresolved names all excluded):

| Definition | 6 libraries | 4 apps + scientific |
|---|---:|---:|
| Lenient (the original claim) | 89.9% | 92.8% |
| `Any` anywhere disqualifies | 86.1% | 87.2% |
| **Strict** | **73.4%** | **68.6%** |
| Ultra (no parameterization at all) | 63.4% | 63.3% |

The honest figure is **~69–73%**, not 90.8%. The truth sits above the strict
row, because a large share of strict rejections are `unresolved-Upper` —
third-party class names the audit script could not resolve without the full
import graph, which a real whole-program compiler *would* resolve.

**"Only 4.67% is `Any`" counted top level only.** Counting `Any` nested
anywhere (`dict[str, Any]`): **8.90%** on the libraries, **7.84%** on the
cross-domain set — roughly double.

### Invalidated, and one over-correction

**E3 is worse than reported, and its two halves came from different
programs.** The pair "26.1% coverage at 71% agreement" never existed: 26.1%
is `leverage.py` (whole corpus, tracks locals) and 71% is `propagate.py` (per
repo, no local tracking, 12.6% coverage). Three further defects:

- **Return annotations were an un-hidden oracle.** Both scripts seed
  `known_ret` from `n.returns`, so "hide every annotation" was false.
  Removing the leak: 4,460 -> 3,695 determinations.
- **The denominator was 1.86x too small** — 27,517 definition sites collapse
  into 14,900 `setdefault` entries, hiding 8,521 parameters.
- **Agreement was lenient and repo-skewed.** Strict is 63.9%, and mypy is
  55.8% of the sample; **excluding mypy, agreement is 53.6%**.

Corrected: whole-program propagation determines **14.0%** of the 31,899 real
parameters — **11.6%** with the return-annotation oracle also removed — at
**63.9% strict** agreement. The direction of the original finding holds and
the correction makes it *worse*, not better.

**E4 survives; withdrawing it was an over-correction on my part.** A
synthetic benchmark with hand-computed answers shows `leverage.py`
propagates correctly to depth 4 (one level per round) and yields **exactly
4.00x** marginal leverage when the call graph supports it. The algorithm is
not too weak. (It was `propagate.py` — a different script, with dead
local-tracking code — that resolves only one hop.)

The published 1.03x was a seed-accounting artifact: seeds counted as
determinations, and ~31.6% of sampled seeds were already free. Measured
properly, with seeds drawn only from parameters not already determined:
**~1.5x at low density, decaying to ~1.0x at saturation** — a seed
determines itself plus roughly half a parameter downstream.

The structural reason is the durable finding: **61.4% of parameters are dead
ends**, never forwarded as a bare name into any resolvable in-corpus call, so
knowing them can determine nothing else. That share is if anything
understated, since the measurement uses the same bare-name matching that
overstates fan-out.

Three compounding defects behind it:

- **Function resolution by bare name.** 23.7% of names have more than one
  definition, and 58.7% of all definition sites live under a conflated name
  (`__init__` x1169, `f` x316, `foo` x245). 63% of call resolutions are
  `anything.foo()` matched purely on the attribute. `setdefault` keeps the
  first definition and discards 8,521 parameters belonging to later ones, so
  the denominator understates the real parameter count by 1.86x.
- **The ground truth is conflated too.** 4.1% of `(name, param)` keys carry
  conflicting annotations across definitions, so the ceiling on measurable
  agreement is ~93.3%, not 100%.
- **The agreement rule was lenient.** `endswith` matching inflated it:
  strict exact agreement is **68.8%**, not the reported 71.7% (e.g. truth
  `str | None` counted as agreeing with a prediction of `None`).

### Overturned


**"A cheap predictor plus a 4-way cache captures nearly all the value a deep
learning predictor would" is FALSE for an AOT compiler.** Three findings
killed it:

1. **Category error.** DeepTyper's top-5 is "the gold *static annotation* is
   in the model's top 5." A cache slot is a *runtime* type at a call site.
   Different objects. The argument conflated them.
2. **Someone did run the decisive experiment.** Ye et al., *Concrete Type
   Inference for Code Optimization using Machine Learning with SMT Solving*,
   OOPSLA 2023 ([10.1145/3622825](https://doi.org/10.1145/3622825)) fed
   predicted types to **AOT** backends and measured wall-clock speedup:
   frequency-model **17.6×** vs GPT-4 **26.4×** (Numba-AOT geomean), and
   **18.3×** vs **62.2×** (C++ backend). The gap concentrates in kernels the
   frequency model never types at all.
3. **The tail is where cheap predictors collapse.** On *unseen user-defined*
   types under a project split, Type4Py scores 6.9% top-1 / 10.9% top-5
   against TIGER's 67.5% / 85.0% (TIGER, arXiv:2407.02095). User-defined
   classes are ~32% of annotated parameters.

The distinction that matters: **a polymorphic inline cache is a JIT
mechanism and a profile requires a prior run.** For a profile-guided method
JIT the original claim is mostly right. **PyRs is an AOT compiler with cold
start, so it is the case where the claim fails.**

One limit worth keeping: Ye et al.'s baseline is *untyped* Python, not
annotations + call graph + profile. It shows a model beats **nothing**; it
does not show a model beats tiers 2–4. And its own null result is the
strongest confirmation of tier 1 — when GPT-4 mispredicted on `ipnsw`,
**both backends reported 1.0×, no speedup at all.** Prediction without a
fast representation buys nothing.

### Also corrected in the literature claims

"Everything that keeps full dynamism lands at 1.1–2×" was too tight — Nuitka
reports **3.3–3.7×** on pystone. And SBCL / Chez / LispWorks *are* AOT native
compilers for dynamic languages. The claim survives only in its precise
form: no system has full **CPython-level** dynamism (arbitrary instance
attributes, `eval`, metaclasses, monkey-patching) together with 6–41× AOT
native speed. Codon states the trade directly: its restrictions "are
ultimately what allow Codon to compile to native code without any runtime
performance overhead."


### Two more reversals, both against my own conclusion

**"A linear model beats hand-written rules" — REVERSED.** Eleven hand-written
rules with no training beat the logistic regression: **72.8% vs 72.1%**
weighted, 71.0% vs 70.2% unweighted. RandomForest (73.6%) beats both. And the
diagnostic that settles it: logistic regression stripped of name and default
features scores **60.5%, below the 65.8% majority baseline** — the usage
features contribute nothing net. The honest statement is "parameter defaults
and a handful of name tokens predict the representation bucket," which needs
no model. (Only 4.3% of parameters even have a recognised literal default,
and that alone buys 3.1 pp.)

Two threats to E5 were cleared: no feature leakage (116 high-purity features
inspected, none encode the annotation), and the vectorizer is fit on training
data only. The `[:,:600]` truncation was a real bug — gradient boosting is
70.8%, not 66.9%.

**"There is nothing to distill" — REVERSED.** A **39-leaf decision tree at
~2 KB matches the 323 KB teacher exactly** (72.0% weighted both, and beats it
unweighted at 71.0% vs 70.4%). Twelve leaves at 0.6 KB reach 71.2%. The
depth-4 tree's root split is `def_bool` and its leaves are `iterated=>list`,
`n_name=>str`, `def_bool=>bool`. The original 62.9% was measuring the
student's handicap, not the teacher's knowledge — there was never 75% of
learned signal to preserve.

### Where the representation space actually runs out

Per-project strict representability spans **51.4% to 88.4%**, and the low end
is not random:

| project | lenient | strict |
|---|---:|---:|
| black | 89.9%+ | 88.4% |
| pandas | 91.2% | 75.0% |
| zulip (Django app) | 95.9% | 71.6% |
| httpx | — | 58.4% |
| **xarray** | **94.2%** | **51.4%** |

**Scientific and array code is where PyRs's representation space runs out** —
xarray's 43-point gap is abstract container protocols and unresolvable
array-protocol names. Application code (zulip, 71.6%) behaves like library
code; numeric code does not. That matters because it is exactly the domain
the dynamism plan targets.

Also resolved while auditing: **`Optional[SomeClass]` costs the same as a
scalar union.** `codegen/src/emit.rs:66` gives every `Ty::Union` the layout
`{i32, i64}`, `Ty::Class` enters the payload by `ptrtoint`, and
`optional_of` is just `union_of(&[t, Ty::None])`. 16 bytes, no allocation.
The real constraint is downstream: dispatch on the class payload wants a
monomorphic class, which is the 60.9% flat-layout figure, not a union-layout
issue.
