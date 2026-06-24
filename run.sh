#!/usr/bin/env bash
# Run f9-talk straight from this source tree — no system install needed.
# Useful for development: edit code, run ./run.sh --build, test.
#
# Usage:
#   ./run.sh                       launch the existing release binary
#   ./run.sh --build               rebuild first, then launch
#   ./run.sh -v                    pass through to f9-talk (verbose logging)
#   ./run.sh --indicator-margin 80 or any other f9-talk flag
#
# What it does:
#  1. cd to the repo root (works no matter where you call it from)
#  2. cargo build --release if the binary is missing or --build was passed
#  3. kill any existing f9-talk so the abstract-socket lock doesn't block us
#  4. exec the binary inside `sg input` so the input group is active
#     (needed only until you log out + back in once)

set -euo pipefail
cd "$(dirname "$0")"

if [[ "${1:-}" == "--build" ]]; then
    shift
    cargo build --release
elif [[ ! -x ./target/release/f9-talk ]]; then
    echo "==> release binary missing — building"
    cargo build --release
fi

if pgrep -f 'f9-talk$' >/dev/null 2>&1; then
    echo "==> killing existing f9-talk instance"
    pkill -f 'f9-talk$' || true
    sleep 0.5
fi

# Forward args safely (handles spaces / quotes in user-supplied flags).
# Guard the no-arg case: `printf '%q ' "$@"` with zero args emits ''
# which f9-talk then sees as a literal empty argument.
if (( $# > 0 )); then
    escaped=$(printf ' %q' "$@")
else
    escaped=""
fi

echo "==> launching ./target/release/f9-talk $*"
exec sg input -c "RUST_LOG=info ./target/release/f9-talk${escaped}"
