# RELEASE PROCESS (single command after committing your changes):
#   make release VERSION=x.y.z
#
# This validates versions, runs full CI, tags, and pushes. CI then builds
# the .deb package and publishes to the APT repo automatically.
#
# If you need to bump version separately (e.g., before committing):
#   make bump-version VERSION=x.y.z
#
# PRE-1.0 VERSIONING:
# - Patch (0.1.x): bug fixes, new features, security fixes, improvements
# - Minor (0.x.0): breaking config/protocol changes requiring simultaneous update

.PHONY: build build-release build-web build-rust \
        dev run test lint fmt check ci pre-push install-hooks version-check bump-version \
        install uninstall deploy clean setup doctor help \
        docker-build-2404 docker-build-2604 docker-check-2404 docker-check-2604 \
        desktop-electron desktop-electron-dev desktop-tauri desktop-tauri-dev

CARGO := cargo
NPM := npm
INSTALL_DIR := /usr/local/bin
CONFIG_DIR := /etc/beam
WEB_INSTALL_DIR := /usr/share/beam/web/dist

help:
	@echo "Beam Remote Desktop"
	@echo ""
	@echo "Development:"
	@echo "  make dev            Build everything (debug) and run server"
	@echo "  make build          Build everything (debug)"
	@echo "  make build-release  Build everything (release)"
	@echo "  make test           Run all tests"
	@echo "  make lint           Run clippy + TypeScript type check"
	@echo "  make fmt            Format all Rust code"
	@echo "  make check          Full pre-commit check (fmt + lint + test)"
	@echo "  make pre-push       Fast pre-push checks (fmt + type-check, no link)"
	@echo "  make ci             Run exact CI checks (verify before pushing)"
	@echo "  make install-hooks  Install git pre-push hook calling 'make pre-push'"
	@echo ""
	@echo "Deployment:"
	@echo "  sudo make install   Build and install to system"
	@echo "  make build-release && sudo make deploy"
	@echo "                      Build as user, deploy as root"
	@echo "  sudo make uninstall Remove from system"
	@echo ""
	@echo "Release:"
	@echo "  make bump-version VERSION=x.y.z  Bump version in all files"
	@echo "  make release VERSION=x.y.z       Run CI, tag, push (triggers APT build)"
	@echo ""
	@echo "Setup:"
	@echo "  make setup          Check and install dev dependencies"
	@echo "  make doctor         Check system readiness"

# === Development ===

build: build-web build-rust

build-rust:
	$(CARGO) build --workspace

build-release: build-web
	$(CARGO) build --release --workspace

build-web:
	cd web && $(NPM) install --silent && $(NPM) run build

# Build everything, put agent in PATH, run server
dev: build
	@echo ""
	@echo "Starting Beam server (debug build)..."
	@echo "  Web client: https://localhost:8444"
	@echo "  Log in with your Linux username and password"
	@echo ""
	PATH="$(CURDIR)/target/debug:$$PATH" \
	RUST_LOG=$${RUST_LOG:-info} \
	$(CARGO) run -p beam-server

# Run from release build
run: build-release
	PATH="$(CURDIR)/target/release:$$PATH" \
	RUST_LOG=$${RUST_LOG:-info} \
	./target/release/beam-server

# === Testing ===

test:
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit
	cd web && $(NPM) test

# === Code Quality ===

lint:
	$(CARGO) clippy --workspace -- -D warnings
	cd web && npx tsc --noEmit

fmt:
	$(CARGO) fmt --all

check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace -- -D warnings
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit
	cd web && $(NPM) test
	@echo ""
	@echo "All checks passed."

ci:
	@echo "Running CI checks (mirrors .github/workflows/ci.yml)..."
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace -- -D warnings
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit && $(NPM) test && $(NPM) run build
	@echo ""
	@echo "All CI checks passed."

# Fast pre-push: fmt + type-check only. No clippy/test/web-build because
# those need libclang+pkg-config+gstreamer-dev installed (linker step).
# GitHub Actions runs the full `make ci` on every PR — that's the
# canonical correctness gate. This target keeps the dev loop quick and
# unblocks contributors who haven't installed system dev headers.
pre-push:
	@echo "Running fast pre-push checks (fmt + type-check only)..."
	$(CARGO) fmt --all -- --check
	cd web && npx tsc --noEmit
	@echo ""
	@echo "Pre-push checks passed. CI will run the full clippy+test+build suite."

install-hooks:
	@bash scripts/install-hooks.sh

version-check:
	@CARGO_VER=$$(grep -A5 '^\[workspace\.package\]' Cargo.toml | grep '^version' | sed 's/.*"\(.*\)"/\1/'); \
	WEB_VER=$$(node -p "require('./web/package.json').version"); \
	ELECTRON_VER=$$(node -p "require('./desktop-electron/package.json').version" 2>/dev/null || echo ""); \
	TAURI_VER=$$(grep '^version' desktop-tauri/Cargo.toml 2>/dev/null | head -1 | sed 's/.*"\(.*\)"/\1/' || echo ""); \
	echo "Cargo.toml version:                $$CARGO_VER"; \
	echo "web/package.json version:          $$WEB_VER"; \
	echo "desktop-electron/package.json:     $${ELECTRON_VER:-<not present>}"; \
	echo "desktop-tauri/Cargo.toml:          $${TAURI_VER:-<not present>}"; \
	if [ "$$CARGO_VER" != "$$WEB_VER" ]; then \
		echo "ERROR: Version mismatch! Cargo.toml ($$CARGO_VER) != web/package.json ($$WEB_VER)"; \
		exit 1; \
	fi; \
	if [ -n "$$ELECTRON_VER" ] && [ "$$CARGO_VER" != "$$ELECTRON_VER" ]; then \
		echo "ERROR: Version mismatch! Cargo.toml ($$CARGO_VER) != desktop-electron/package.json ($$ELECTRON_VER)"; \
		exit 1; \
	fi; \
	if [ -n "$$TAURI_VER" ] && [ "$$CARGO_VER" != "$$TAURI_VER" ]; then \
		echo "ERROR: Version mismatch! Cargo.toml ($$CARGO_VER) != desktop-tauri/Cargo.toml ($$TAURI_VER)"; \
		exit 1; \
	fi; \
	if [ -n "$$GITHUB_REF_NAME" ]; then \
		case "$$GITHUB_REF_NAME" in \
			v*) \
				TAG_VER=$${GITHUB_REF_NAME#v}; \
				echo "Git tag version: $$TAG_VER"; \
				if [ "$$CARGO_VER" != "$$TAG_VER" ]; then \
					echo "ERROR: Version mismatch! Cargo.toml ($$CARGO_VER) != git tag ($$TAG_VER)"; \
					exit 1; \
				fi;; \
		esac; \
	fi; \
	echo "Version check passed: $$CARGO_VER"

# Usage: make bump-version VERSION=0.2.0
# Updates: Cargo.toml (workspace), web/package.json, desktop-electron/package.json,
# desktop-tauri/Cargo.toml. version-check fails the build if any drift remains.
bump-version:
	@if [ -z "$(VERSION)" ]; then echo "Usage: make bump-version VERSION=x.y.z"; exit 1; fi
	@echo "Bumping version to $(VERSION)..."
	@sed -i '/^\[workspace\.package\]/,/^\[/ s/^version = ".*"/version = "$(VERSION)"/' Cargo.toml
	@node -e "const fs=require('fs'),p=JSON.parse(fs.readFileSync('web/package.json','utf8')); p.version='$(VERSION)'; fs.writeFileSync('web/package.json',JSON.stringify(p,null,2)+'\n')"
	@if [ -f desktop-electron/package.json ]; then \
		node -e "const fs=require('fs'),p=JSON.parse(fs.readFileSync('desktop-electron/package.json','utf8')); p.version='$(VERSION)'; fs.writeFileSync('desktop-electron/package.json',JSON.stringify(p,null,2)+'\n')"; \
	fi
	@if [ -f desktop-tauri/Cargo.toml ]; then \
		sed -i '/^\[package\]/,/^\[/ s/^version = ".*"/version = "$(VERSION)"/' desktop-tauri/Cargo.toml; \
	fi
	@if [ -f desktop-tauri/tauri.conf.json ]; then \
		node -e "const fs=require('fs'),p=JSON.parse(fs.readFileSync('desktop-tauri/tauri.conf.json','utf8')); p.version='$(VERSION)'; fs.writeFileSync('desktop-tauri/tauri.conf.json',JSON.stringify(p,null,2)+'\n')"; \
	fi
	@cd web && npm install --package-lock-only --silent 2>/dev/null || true
	@$(CARGO) check --quiet
	@$(MAKE) version-check

# Full release: bump, check, commit, tag, push. Requires VERSION and CHANGELOG already updated.
# Usage: make release VERSION=0.2.0
release:
	@if [ -z "$(VERSION)" ]; then echo "Usage: make release VERSION=x.y.z"; exit 1; fi
	@if [ -n "$$(git status --porcelain)" ]; then echo "ERROR: Working tree not clean. Commit or stash changes first."; exit 1; fi
	@CURRENT_VER=$$(grep -A5 '^\[workspace\.package\]' Cargo.toml | grep '^version' | sed 's/.*"\(.*\)"/\1/'); \
	if [ "$$CURRENT_VER" != "$(VERSION)" ]; then \
		echo "ERROR: Cargo.toml version ($$CURRENT_VER) != requested version ($(VERSION))"; \
		echo "Run 'make bump-version VERSION=$(VERSION)' first, update CHANGELOG.md, then commit."; \
		exit 1; \
	fi
	@$(MAKE) version-check
	@$(MAKE) ci
	@echo ""
	@echo "All checks passed. Tagging v$(VERSION) and pushing..."
	git tag v$(VERSION)
	git push && git push --tags
	@echo ""
	@echo "Release v$(VERSION) pushed. CI will build .deb and publish to APT repo."

# === Installation ===

install:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make install"; exit 1; fi
	./scripts/install.sh

uninstall:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make uninstall"; exit 1; fi
	./scripts/uninstall.sh

deploy:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make deploy"; exit 1; fi
	@if [ ! -f target/release/beam-server ] || [ ! -f target/release/beam-agent ]; then \
		echo "ERROR: Release binaries not found. Run 'make build-release' first."; exit 1; fi
	@if [ ! -d web/dist ]; then \
		echo "ERROR: web/dist not found. Run 'make build-release' first."; exit 1; fi
	@echo "Deploying Beam..."
	mkdir -p /var/lib/beam/sessions
	cp target/release/beam-server /tmp/beam-server-new && mv /tmp/beam-server-new $(INSTALL_DIR)/beam-server
	cp target/release/beam-agent /tmp/beam-agent-new && mv /tmp/beam-agent-new $(INSTALL_DIR)/beam-agent
	chmod 755 $(INSTALL_DIR)/beam-server $(INSTALL_DIR)/beam-agent
	rm -rf $(WEB_INSTALL_DIR)/*
	mkdir -p $(WEB_INSTALL_DIR)
	cp -r web/dist/* $(WEB_INSTALL_DIR)/
	setcap cap_sys_nice=ep $(INSTALL_DIR)/beam-agent 2>/dev/null || true
	systemctl restart beam
	@echo "Beam deployed and restarted."

# === Setup ===

setup:
	./scripts/dev-setup.sh

doctor:
	@scripts/beam-doctor

clean:
	$(CARGO) clean
	rm -rf web/node_modules web/dist

# === Containerised builds ===
# Use when you don't have Ubuntu locally (Synology DSM, TrueNAS, macOS, NixOS).
# Builds the same toolchain CI uses so results are reproducible.
# DOCKER overridable: e.g. DOCKER=/usr/local/bin/docker make docker-check-2404
DOCKER ?= docker

docker-build-2404:
	$(DOCKER) build -t beam-build-2404 -f docker/Dockerfile.dev-24.04 .

docker-build-2604:
	$(DOCKER) build -t beam-build-2604 -f docker/Dockerfile.dev-26.04 .

docker-check-2404: docker-build-2404
	$(DOCKER) run --rm \
		-v "$(CURDIR)":/work -w /work \
		-v beam-target-2404:/work/target \
		-v beam-cargo-registry:/cargo/registry \
		beam-build-2404 make check

docker-check-2604: docker-build-2604
	$(DOCKER) run --rm \
		-v "$(CURDIR)":/work -w /work \
		-v beam-target-2604:/work/target \
		-v beam-cargo-registry:/cargo/registry \
		beam-build-2604 make check

# === Desktop shells ===
# Both consume web/dist; they share the BeamNative contract declared in
# web/src/native-bridge.ts.
desktop-electron: build-web
	cd desktop-electron && npm ci && npm run dist:linux

desktop-electron-dev: build-web
	cd desktop-electron && npm install && npm run dev

desktop-tauri: build-web
	cd desktop-tauri && cargo tauri build

desktop-tauri-dev: build-web
	cd desktop-tauri && cargo tauri dev
