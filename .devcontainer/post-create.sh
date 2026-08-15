#!/usr/bin/env bash
set -euo pipefail

echo "== apt deps (protobuf-compiler) =="
sudo apt-get update -y
sudo apt-get install -y --no-install-recommends protobuf-compiler

echo "== bun =="
if ! command -v bun >/dev/null 2>&1; then
  curl -fsSL https://bun.sh/install | bash
  echo 'export PATH="$HOME/.bun/bin:$PATH"' >> "$HOME/.bashrc"
fi
export PATH="$HOME/.bun/bin:$PATH"

echo "== wasm-pack =="
if ! command -v wasm-pack >/dev/null 2>&1; then
  cargo install wasm-pack
fi

echo "== wasm captcha module =="
(cd crates/temps-captcha-wasm && bun run build)

echo "== web deps =="
(cd web && bun install)

echo "== git hooks =="
./scripts/setup-hooks.sh

echo "== gh: point at the upstream remote for PRs =="
git remote get-url upstream >/dev/null 2>&1 || git remote add upstream https://github.com/gotempsh/temps.git

echo "== claude code =="
npm install -g @anthropic-ai/claude-code
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ANTHROPIC_API_KEY is not set -- run 'claude login', or add it as a"
  echo "Codespaces secret (repo/user settings) and rebuild the container."
fi

echo "== cargo: prefetch workspace dependencies =="
cargo fetch --locked

# include_dir!() in temps-cli / example plugins panics on a missing dist dir.
# Real dist is produced by `bun run build` in web/; drop debug placeholders so
# the background build below (and any manual `cargo check`) doesn't need it.
echo "== prepare embedded web asset placeholders =="
dist_dirs="crates/temps-cli/dist"
for w in examples/*/web; do dist_dirs="$dist_dirs $w/dist"; done
for d in $dist_dirs; do
  mkdir -p "$d"
  [ -f "$d/index.html" ] || printf '<!doctype html><title>placeholder</title>\n' > "$d/index.html"
done

echo "== cargo: kick off incremental workspace build in the background =="
nohup cargo build --workspace --all-targets \
  > /tmp/temps-initial-build.log 2>&1 &
disown
echo "Building in the background -- tail -f /tmp/temps-initial-build.log to watch it."

echo "post-create complete"
