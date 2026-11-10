#!/usr/bin/env bash
# Generic Linux build for llm_proxy — native x86_64 release.
# No WSL, no machine-specific deploy. Output: dist/llm_proxy (+ dist/llm_proxy.toml).
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release

mkdir -p dist
cp -f target/release/llm_proxy dist/llm_proxy
if [ -f llm_proxy.toml ]; then
  cp -f llm_proxy.toml dist/
fi
echo "Done: dist/llm_proxy"
