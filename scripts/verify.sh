#!/usr/bin/env sh
set -eu

echo "[1/5] rustfmt"
cargo fmt --all --check

echo "[2/5] check all targets"
cargo check --workspace --all-targets

echo "[3/5] unit and documentation tests"
cargo test --workspace

echo "[4/5] clippy with warnings denied"
cargo clippy --workspace --all-targets -- -D warnings

echo "[5/5] compile benchmark executables"
cargo bench --workspace --no-run

echo "RIAPS workspace verification passed."