COMPOSE := docker compose -f dev/docker-compose.yml

build: ## Release build
	cargo build --release

test: ## Unit and UI tests
	cargo test

test-it: ## Integration tests against the dev stack (needs dev-up + dev-bootstrap)
	LAZYGRAFANA_IT=1 cargo test --test integration

lint: ## Formatting and clippy
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

run: ## Run against the dev stack
	cargo run -- --config dev/lazygrafana.yml

.PHONY: build test test-it lint run load-up load-seed load-clean load-down dev-up dev-up-all dev-bootstrap dev-bootstrap-compat dev-down dev-reset dev-logs dev-ps

dev-up: ## Start the core dev stack (Grafana 13, Prometheus, Loki, Alloy, log generators)
	$(COMPOSE) up -d

dev-up-all: ## Also start Grafana 11 (:3011) and the image renderer
	$(COMPOSE) --profile compat --profile renderer up -d

dev-bootstrap: ## Issue tokens, seed folders/orgs/annotations, backfill 7d of metrics
	./dev/bootstrap.sh

dev-bootstrap-compat: ## Seed the Grafana 11 instance (requires dev-up-all)
	GRAFANA_URL=http://localhost:3011 SECRETS_PREFIX=compat- SKIP_BACKFILL=1 ./dev/bootstrap.sh

dev-down: ## Stop the stack, keep data
	$(COMPOSE) --profile compat --profile renderer down

dev-reset: ## Stop the stack and wipe all data and tokens
	$(COMPOSE) --profile compat --profile renderer down -v
	rm -rf dev/.secrets

dev-logs: ## Follow logs of all services
	$(COMPOSE) logs -f --tail=50

dev-ps: ## Show service status
	$(COMPOSE) ps

load-up: ## Start the load profile (avalanche metrics + log floods)
	$(COMPOSE) --profile load up -d

load-seed: ## Seed ~300 dashboards, heavy dashboards and ~100 alert rules
	./dev/load/seed.py

load-clean: ## Remove the seeded load-test content
	./dev/load/seed.py --clean

load-down: ## Stop the load profile services
	$(COMPOSE) --profile load stop avalanche avalanche-wide load-logs load-logs-apache
