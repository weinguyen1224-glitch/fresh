#!/usr/bin/env bash
# Run the source-built fresh with config/data isolated to the source tree,
# so it does NOT touch ~/.config/fresh or ~/.local/share/fresh.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

export XDG_CONFIG_HOME="$ROOT/.fresh-config"
export XDG_DATA_HOME="$ROOT/.fresh-data"

exec "$ROOT/target/release/fresh" "$@"
