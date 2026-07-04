# md-mcp developer tasks. `make check` is the pre-commit / pre-merge / pre-push
# gate: formatting, lints, unit + in-process protocol tests, then the stdio
# end-to-end suite against a freshly built release binary.

.PHONY: check fmt lint test e2e build clean

check: fmt lint test e2e ## Full gate: run before committing, merging, or pushing

fmt: ## Verify formatting (cargo fmt --check)
	cargo fmt --all --check

lint: ## Lint all targets, warnings as configured (clippy)
	cargo clippy --all-targets --quiet

test: ## Unit + in-process protocol tests
	cargo test --workspace --quiet

e2e: build ## stdio black-box end-to-end suite (functional + hardening)
	python3 tests/e2e/run.py --no-build

build: ## Build the release server binary
	cargo build --release -p md-server --quiet

clean: ## Remove build artifacts
	cargo clean
