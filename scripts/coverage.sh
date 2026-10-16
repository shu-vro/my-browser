#!/bin/sh
# Line coverage of the test suite (unit and e2e), as an HTML report in
# target/llvm-cov/html/index.html. Extra arguments go to cargo llvm-cov,
# e.g. `--open` or `--lcov --output-path lcov.info`.
#
# The e2e tests drive the instrumented browser binary; cargo llvm-cov sets
# LLVM_PROFILE_FILE so each launched browser writes its own profile, and a
# browser that quits normally (`quit`) counts. Needs a GUI session, like
# `cargo test`.
#
# Once: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
set -eu
cd "$(dirname "$0")/.."
command -v cargo-llvm-cov >/dev/null || {
  echo "needs cargo-llvm-cov: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview" >&2
  exit 1
}
cargo llvm-cov --release --html "$@"
echo "report: target/llvm-cov/html/index.html"
