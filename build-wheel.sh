#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if [ -z "${WASI_SDK_PATH:-}" ]; then
    read -rp "Enter WASI SDK path [/opt/wasi-sdk]: " WASI_SDK_PATH
    WASI_SDK_PATH="${WASI_SDK_PATH:-/opt/wasi-sdk}"
fi
export WASI_SDK_PATH

echo "==> Creating venv and installing maturin via uv..."
uv venv .venv
uv pip install --python .venv/bin/python maturin

echo "==> Building wheel with maturin..."
.venv/bin/maturin build --release --features pyo3 --out dist

echo ""
echo "==> Done! Artifacts are in dist/:"
ls -lh dist/
