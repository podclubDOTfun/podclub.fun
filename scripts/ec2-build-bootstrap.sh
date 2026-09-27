#!/usr/bin/env bash
# podclub.fun — EC2 build-box bootstrap.
# Runs ON a fresh Ubuntu 22.04/24.04 EC2 instance.
# Installs the NEAR contract toolchain that a minimal shell can't provide
# (rustup + wasm32 target + cargo-near + near CLI), then verifies it.
#
# Usage on the instance:
#   curl -fsSL <raw-url>/ec2-build-bootstrap.sh | bash
# or: scp this file over, then `bash ec2-build-bootstrap.sh`
set -euo pipefail

echo "==> System packages"
sudo apt-get update -y
sudo apt-get install -y build-essential pkg-config libssl-dev git curl clang

echo "==> rustup + stable Rust"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup default stable
rustup update stable

echo "==> wasm32 target (the piece the base environment is missing)"
rustup target add wasm32-unknown-unknown

echo "==> cargo-near (builds reproducible NEP-141/router wasm)"
cargo install cargo-near --locked

echo "==> near CLI (near-cli-rs) for deploy/view/call"
cargo install near-cli-rs --locked

echo
echo "==> Versions"
rustc --version
cargo --version
cargo near --version || true
near --version || true
rustup target list --installed | grep wasm32 || true

echo
echo "Done. Next: git clone the repo, then \`cd contracts && cargo near build\`."
echo "NOTE: never put a mainnet private key in this repo or in shell history."
echo "      Log in interactively with \`near account import-account\` when deploying."
