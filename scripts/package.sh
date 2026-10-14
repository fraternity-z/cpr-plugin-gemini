#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target="x86_64-unknown-linux-gnu"
target_dir="${CARGO_TARGET_DIR:-$root/target}"

cargo build --manifest-path "$root/Cargo.toml" --locked --release --target "$target" --target-dir "$target_dir"
"${CPR_PLUGIN_CLI:-cpr-plugin}" package \
  --manifest "$root/plugin.json" \
  --binary "$target_dir/$target/release/cpr-plugin-gemini" \
  --target "$target" \
  --output-dir "$root/dist"
