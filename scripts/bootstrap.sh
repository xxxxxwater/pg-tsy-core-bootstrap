#!/usr/bin/env bash
set -euo pipefail
python3 -m venv research/.venv
research/.venv/bin/pip install -e 'research[dev]'
echo "Python environment ready. Install Rust stable separately if cargo is unavailable."
