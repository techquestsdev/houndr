.PHONY: build build-release run test bench lint fmt clean docker docker-run audit help release

BIN      := houndr-server
CONFIG   ?= config.toml
IMAGE    := houndr
PORT     ?= 6080

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## Build in debug mode
	cargo build

build-release: ## Build in release mode
	cargo build --release

run: build ## Run the server (debug)
	cargo run --bin $(BIN) -- --config $(CONFIG)

run-release: build-release ## Run the server (release)
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

release: ## Create a release (usage: make release VERSION=1.2.0)
	@if [ -z "$(VERSION)" ]; then \
		echo "Usage: make release VERSION=1.2.0"; \
		exit 1; \
	fi
	@if git diff --quiet && git diff --cached --quiet; then \
		true; \
	else \
		echo "Error: working tree is dirty — commit or stash changes first"; \
		exit 1; \
	fi
	@echo "Releasing v$(VERSION)..."
	sed -i '' 's/^version = ".*"/version = "$(VERSION)"/' Cargo.toml
	sed -i '' 's/^version: ".*"/version: "$(VERSION)"/' deploy/helm/houndr/Chart.yaml
	cargo check
	git add Cargo.toml Cargo.lock deploy/helm/houndr/Chart.yaml
	git commit -m "chore: bump version to $(VERSION)"
	git tag -s "v$(VERSION)" -m "v$(VERSION)"
	git push
	git push origin "v$(VERSION)"
	@echo ""
	@echo "v$(VERSION) pushed — release workflow triggered."
