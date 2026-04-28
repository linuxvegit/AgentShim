#!/usr/bin/env bash
# TODO: Run criterion benchmarks and open the HTML report.
#
# Suggested implementation:
#
#   set -euo pipefail
#   cargo bench --workspace 2>&1 | tee /tmp/bench.log
#   # Open report (Linux)
#   xdg-open target/criterion/report/index.html 2>/dev/null || true
#   # Open report (macOS)
#   open target/criterion/report/index.html 2>/dev/null || true
echo "Benchmarks not yet implemented — see crates/core/benches/ and crates/gateway/benches/"
