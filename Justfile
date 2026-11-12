default:
    @just --list

build:
    cargo build --all-features

build-release:
    cargo build --release --all-features

build-optimized:
    #!/usr/bin/env bash
    set -e
    export RUSTFLAGS="-C target-cpu=native -C opt-level=3"
    cargo build --release --all-features

test:
    cargo test --all-features

test-verbose:
    cargo test --all-features -- --nocapture

test-unit:
    cargo test --lib --all-features

test-coverage:
    #!/usr/bin/env bash
    set -e
    cargo llvm-cov --all-features --workspace --html
    echo "Coverage report: target/llvm-cov/html/index.html"

test-sequential:
    cargo test --all-features -- --test-threads=1

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- \
        -D warnings \
        -W clippy::nursery \
        -W clippy::pedantic

# The clippy set that the pre-commit hook and CI both run.
lint-strict:
    #!/usr/bin/env bash
    cargo clippy --all-targets --all-features -- \
        -D warnings \
        -W clippy::arithmetic_side_effects \
        -W clippy::expect_used \
        -W clippy::float_arithmetic \
        -W clippy::indexing_slicing \
        -W clippy::mem_forget \
        -W clippy::nursery \
        -W clippy::panic \
        -W clippy::pedantic \
        -W clippy::unwrap_used

# The e2e scenarios in e2e/, against a release build or the binary DOTMAN_BIN names.
e2e *args:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -z "${DOTMAN_BIN:-}" ]]; then
        cargo build --release --locked
        DOTMAN_BIN="$(realpath "${CARGO_TARGET_DIR:-target}")/release/dotman"
    fi
    DOTMAN_BIN="$DOTMAN_BIN" e2e/run.sh {{ args }}

# The same, keeping each scenario's temp root for debugging.
e2e-keep *args:
    E2E_KEEP=1 just e2e {{ args }}

fix:
    cargo fix --allow-dirty --allow-staged --all-features

audit:
    cargo audit

unused-deps:
    cargo machete

profile-perf:
    #!/usr/bin/env bash
    set -e
    cargo build --profile release-with-debug
    perf record --call-graph=dwarf target/release-with-debug/dotman --help
    perf report

profile-flame:
    cargo flamegraph --bin dotman -- --help

profile-memory:
    #!/usr/bin/env bash
    set -e
    cargo build --profile release-with-debug
    valgrind --tool=massif target/release-with-debug/dotman --help

docs:
    cargo doc --no-deps --all-features --open

docs-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

generate:
    cargo run -p xtask -- generate

install-manpages: generate
    #!/usr/bin/env bash
    set -e
    echo "Installing man pages to /usr/share/man/man1/ (requires sudo)..."
    sudo install -Dm644 man/*.1 -t /usr/share/man/man1/
    sudo mandb
    echo "Man pages installed successfully!"

watch:
    cargo watch -x "build --all-features"

watch-test:
    cargo watch -x "test --all-features"

setup:
    #!/usr/bin/env bash
    set -e
    cargo install cargo-watch
    cargo install cargo-audit
    cargo install cargo-machete
    cargo install cargo-llvm-cov
    cargo install flamegraph

install:
    cargo install --path . --force

uninstall:
    cargo uninstall dotman-cli

# Bumps Cargo.toml, Cargo.lock, the PKGBUILD and install.sh's fallback version. The
# src/bootstrap/install.sh.in template pins CARGO_PKG_VERSION itself and is not touched.
release-prepare version:
    #!/usr/bin/env bash
    set -e
    sed -i 's/^version = ".*"/version = "{{ version }}"/' Cargo.toml
    sed -i 's/^pkgver=.*/pkgver={{ version }}/' packaging/arch/PKGBUILD
    sed -i 's/^FALLBACK_VERSION=.*/FALLBACK_VERSION="{{ version }}"/' install/install.sh
    cargo check --workspace
    just test
    just lint
    just audit

clean:
    cargo clean
    rm -rf dist/

pre-commit: lint-strict

ci: fmt-check lint test audit docs-check

dev: clean build-optimized test lint docs
