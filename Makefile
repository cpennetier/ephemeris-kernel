.PHONY: test-rust develop build guard test

# Full Rust suites. `pyo3`/`python` features are disabled: extension-module
# mode does not link standalone (build through maturin instead).
test-rust:
	cargo test --no-default-features --manifest-path crates/ephemeris-des/Cargo.toml --lib
	cargo test --no-default-features --features arrow_only --manifest-path crates/ephemeris-ledger/Cargo.toml
	cargo test --no-default-features --features arrow_only --manifest-path crates/ephemeris-fsm/Cargo.toml

develop:
	maturin develop --release
	python scripts/route-native-extension.py

build:
	maturin build --release

guard:
	pytest tests -q

test: test-rust guard
