#!/usr/bin/env bash
set -euo pipefail

if grep -qE '^(crates/(mobius|mobius-ui|mobius-api|mobius-domain|mobius-engine|mobius-store|mobius-github|mobius-runner)/|crates/mobius-testkit/(src/|(tests/it/screenshots\.rs|Cargo\.toml)$)|(Cargo\.toml|Cargo\.lock|\.cargo/config\.toml)$|\.github/(workflows/screenshots\.yml|scripts/ui-changed\.sh)$)'; then
  echo true
else
  echo false
fi
