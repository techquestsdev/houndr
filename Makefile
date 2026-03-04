.PHONY: build release run test bench lint fmt clean docker docker-run audit help

BIN      := houndr-server
CONFIG   ?= config.toml
IMAGE    := houndr
PORT     ?= 6080

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## Build in debug mode
	cargo build

release: ## Build in release mode
	cargo build --release

run: build ## Run the server (debug)
	cargo run --bin $(BIN) -- --config $(CONFIG)

run-release: release ## Run the server (release)
	./target/release/$(BIN) --config $(CONFIG)

test: ## Run all tests
	cargo test

bench: ## Run benchmarks
	cargo bench -p houndr-index

lint: ## Run clippy lints
	cargo clippy --workspace -- -D warnings

fmt: ## Format code
	cargo fmt --all

fmt-check: ## Check formatting
	cargo fmt --all -- --check

check: fmt-check lint test ## Run all checks (format, lint, test)

audit: ## Run dependency security audit
	cargo install cargo-audit --quiet 2>/dev/null || true
	cargo audit

clean: ## Remove build artifacts
	cargo clean

docker: ## Build Docker image
	docker build -t $(IMAGE) .

docker-run: docker ## Build and run in Docker
	docker run -p $(PORT):6080 -v ./$(CONFIG):/app/config.toml $(IMAGE)
