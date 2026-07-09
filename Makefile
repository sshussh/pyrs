# ---------------------------------------------------------------------------
# PyRs — Python compiler in Rust
#
#   make help                     list every target
#   make run                      compile and run main.py (the scratch file)
#   make run FILE=examples/fib.py any file works
#   make time FILE=...            race the native binary against python3
#   make ci                       the full gate: format + lints + tests
# ---------------------------------------------------------------------------

# Scratch program for compile/run/time/emit-llvm (main.py is gitignored)
FILE    ?= main.py
# Output binary, derived from FILE unless overridden
OUT     ?= $(basename $(notdir $(FILE)))
# PyRs optimization level (0-3)
O       ?= 2
# Benchmark repetitions (best-of-N)
RUNS    ?= 3

CARGO   := cargo
PYRS    := target/release/pyrs
PYTHON  := python3

SHELL   := bash
MAKEFLAGS += --no-print-directory
.DEFAULT_GOAL := help

# ---------------------------------------------------------------------------
##@ Compiler
# ---------------------------------------------------------------------------

.PHONY: build
build: ## Build the compiler (debug)
	$(CARGO) build

.PHONY: release
release: ## Build the compiler (release)
	$(CARGO) build --release

.PHONY: install
install: ## Install the PyRs binary into ~/.cargo/bin
	$(CARGO) install --path cli

# ---------------------------------------------------------------------------
##@ Using PyRs
# ---------------------------------------------------------------------------

.PHONY: compile
compile: release ## Compile FILE (default: main.py) to a native binary at OUT
	$(PYRS) compile -O $(O) -i $(FILE) -o $(OUT)

.PHONY: run
run: compile ## Compile and run FILE
	./$(OUT)

.PHONY: time
time: compile ## Race the compiled FILE against python3
	@printf '\n\033[1m-- pyrs (./%s, -O%s)\033[0m\n' '$(OUT)' '$(O)'
	@time ./$(OUT)
	@printf '\n\033[1m-- python3 (%s)\033[0m\n' '$(FILE)'
	@time $(PYTHON) $(FILE)

.PHONY: emit-llvm
emit-llvm: release ## Compile FILE and also write its LLVM IR to OUT.ll
	$(PYRS) compile -O $(O) --emit-llvm -i $(FILE) -o $(OUT)
	@echo "wrote $(OUT).ll"

# ---------------------------------------------------------------------------
##@ Quality
# ---------------------------------------------------------------------------

.PHONY: test
test: ## Run the full test suite (unit + end-to-end)
	$(CARGO) test --workspace

.PHONY: clippy
clippy: ## Lint with clippy; warnings are errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Format all Rust code
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Check formatting without changing anything
	$(CARGO) fmt --all -- --check

.PHONY: ci
ci: fmt-check clippy test examples ## Full gate: format + lints + tests + example parity

.PHONY: examples
examples: release ## Run every example and diff its output against python3
	@fail=0; \
	for ex in examples/*.py examples/modules/*.py; do \
	    got=$$($(PYRS) run -i $$ex 2>&1); \
	    want=$$($(PYTHON) $$ex 2>&1); \
	    if [ "$$got" = "$$want" ]; then \
	        printf '  \033[32mMATCH\033[0m  %s\n' "$$ex"; \
	    else \
	        printf '  \033[31mDIFFER\033[0m %s\n' "$$ex"; \
	        fail=1; \
	    fi; \
	done; \
	exit $$fail

.PHONY: bench
bench: release ## Benchmark suite vs CPython (best of N runs; set RUNS=N)
	RUNS=$(RUNS) PYRS=$(abspath $(PYRS)) ./benchmarks/run.sh

.PHONY: watch
watch: ## Re-run the tests on every change (needs cargo-watch)
	@command -v cargo-watch >/dev/null \
	    || { echo "cargo-watch not found; install with: cargo install cargo-watch"; exit 1; }
	cargo watch -x 'test --workspace'

# ---------------------------------------------------------------------------
##@ Housekeeping
# ---------------------------------------------------------------------------

.PHONY: doctor
doctor: ## Check that the required toolchain is installed
	@fail=0; \
	for tool in rustc cargo cmake cc llvm-config $(PYTHON); do \
	    if command -v $$tool >/dev/null 2>&1; then \
	        printf '  \033[32mok\033[0m      %-12s %s\n' "$$tool" \
	            "$$($$tool --version 2>/dev/null | head -1)"; \
	    else \
	        printf '  \033[31mmissing\033[0m %s\n' "$$tool"; \
	        fail=1; \
	    fi; \
	done; \
	exit $$fail

.PHONY: clean
clean: ## Remove build artifacts and scratch binaries
	$(CARGO) clean
	rm -f main a.out *.ll $(OUT)

.PHONY: help
help: ## Show this help
	@printf '\n\033[1mpyrs\033[0m — Python compiler in Rust\n'
	@printf '\nVariables: \033[36mFILE\033[0m=%s \033[36mOUT\033[0m=%s \033[36mO\033[0m=%s \033[36mRUNS\033[0m=%s\n' \
	    '$(FILE)' '$(OUT)' '$(O)' '$(RUNS)'
	@awk 'BEGIN { FS = ":.*?## " } \
	    /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5); next } \
	    /^[a-zA-Z0-9_-]+:.*?## / { printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2 }' \
	    $(MAKEFILE_LIST)
	@printf '\nExamples:\n'
	@printf '  make run FILE=examples/fib.py     compile and run an example\n'
	@printf '  make time FILE=main.py O=3        race pyrs -O3 against python3\n'
	@printf '  make bench RUNS=5                 slower but steadier benchmark\n\n'
