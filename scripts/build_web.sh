#!/usr/bin/env sh
# Full web build. Runs inside the builder container (see Makefile `web`).
set -eu

cargo build --profile web --target wasm32-unknown-unknown

rm -rf dist
wasm-bindgen --target web --out-dir dist \
	target/wasm32-unknown-unknown/web/boopweb.wasm

if command -v wasm-opt >/dev/null 2>&1; then
	# Rust emits bulk-memory / trunc_sat; older binaryen needs them enabled.
	wasm-opt -Oz \
		--enable-bulk-memory \
		--enable-nontrapping-float-to-int \
		--enable-sign-ext \
		--enable-mutable-globals \
		dist/boopweb_bg.wasm -o dist/boopweb_bg.wasm
fi

cp web/index.html web/manifest.webmanifest web/sw.js web/_headers dist/
mkdir -p dist/icons
cp -R web/icons/. dist/icons/
# Fonts are embedded in the wasm binary (see src/main.rs), not served.

echo
echo "wasm binary:"
ls -lh dist/boopweb_bg.wasm
echo
echo "dist/ is ready to deploy (Cloudflare Pages: output directory = dist)"
