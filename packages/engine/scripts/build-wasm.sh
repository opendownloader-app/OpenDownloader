#!/usr/bin/env bash
# Builds crates/dl-core for wasm32-unknown-unknown and generates the wasm-bindgen
# JS/TS glue into packages/engine/src/wasm-gen/, where both the extension and
# the web app import it from. Run before either app's `vite build` — Vite never
# touches Rust, it only bundles the already-generated glue and emits
# dl_core_bg.wasm as a normal content-hashed asset.
set -euo pipefail

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

# The repo lives on a shared VM mount, which produces spurious archive/mmap
# failures when used as the cargo target dir. Build off-mount; only the final
# .wasm crosses back via wasm-bindgen's --out-dir.
: "${CARGO_TARGET_DIR:=/tmp/opendownloader-target}"
export CARGO_TARGET_DIR
export CARGO_INCREMENTAL=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(dirname "$SCRIPT_DIR")"
WORKSPACE_DIR="$(dirname "$(dirname "$PKG_DIR")")"
OUT_DIR="$PKG_DIR/src/wasm-gen"

# OPENDOWNLOADER_STORE_BUILD=1 drops the large-platform extractors (YouTube, Bilibili,
# TikTok, Douyin, Instagram, Facebook, WeChat) from the compiled wasm entirely.
#
# The Chrome Web Store and Edge Add-ons prohibit extensions that download from YouTube,
# so a build destined for them must not *contain* that code — a runtime setting could
# not be verified by a reviewer, and a feature flag can. Everything else, including the
# generic reader for pages that state their media in a `<video>` tag, is in both builds.
FEATURES=()
if [ "${OPENDOWNLOADER_STORE_BUILD:-0}" = "1" ]; then
  FEATURES+=(--no-default-features)
  echo "STORE BUILD: platform extractors excluded from the wasm"
fi

# `${FEATURES[@]+…}` rather than a bare `"${FEATURES[@]}"`: under `set -u`, macOS's
# bash 3.2 treats an empty array expansion as an unbound variable and aborts the build.
cargo build -p dl-core --target wasm32-unknown-unknown --release \
  ${FEATURES[@]+"${FEATURES[@]}"} \
  --manifest-path "$WORKSPACE_DIR/Cargo.toml"

# The wasm-bindgen CLI and the wasm-bindgen crate must be the same version or
# glue generation fails on a schema mismatch — the crate is pinned `=0.2.126`
# in the workspace manifest for exactly this reason.
if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "error: wasm-bindgen not on PATH — cargo install wasm-bindgen-cli --version 0.2.126 --locked" >&2
  exit 1
fi
wasm-bindgen \
  --target web \
  --out-dir "$OUT_DIR" \
  --out-name dl_core \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/dl_core.wasm"

# wasm-opt is a size optimisation, never a correctness requirement — so a
# failure here must not fail the build.
#
# It fails routinely for a reason worth naming: rustc emits bulk-memory
# instructions by default, and a wasm-opt older than that support rejects the
# module outright ("memory.copy operations require bulk memory operations"). An
# old copy shipped inside some unrelated node_modules dependency is enough to
# put one on PATH. So the optimisation is written to a temporary file and only
# swapped in if it both succeeded and validated; otherwise the unoptimised
# module — which is correct, just larger — is kept.
if command -v wasm-opt >/dev/null 2>&1; then
  WASM="$OUT_DIR/dl_core_bg.wasm"
  if wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int \
      --enable-sign-ext --enable-mutable-globals --enable-reference-types \
      -o "$WASM.opt" "$WASM" 2>/dev/null && [ -s "$WASM.opt" ]; then
    mv "$WASM.opt" "$WASM"
    echo "wasm-opt: optimized dl_core_bg.wasm"
  else
    rm -f "$WASM.opt"
    echo "wasm-opt: refused this module (likely too old for bulk memory) — keeping the unoptimised build"
  fi
else
  echo "wasm-opt not found on PATH — skipping (glue still works, just larger)"
fi

echo "wasm build complete: $OUT_DIR"
