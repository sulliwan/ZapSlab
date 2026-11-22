#!/usr/bin/env sh
# Full web build. Runs inside the builder container (see Makefile `web`).
set -eu

cargo build --profile web --target wasm32-unknown-unknown

rm -rf dist
wasm-bindgen --target web --out-dir dist \
	target/wasm32-unknown-unknown/web/zapslab.wasm

if command -v wasm-opt >/dev/null 2>&1; then
	# Rust emits bulk-memory / trunc_sat; older binaryen needs them enabled.
	wasm-opt -Oz \
		--enable-bulk-memory \
		--enable-nontrapping-float-to-int \
		--enable-sign-ext \
		--enable-mutable-globals \
		dist/zapslab_bg.wasm -o dist/zapslab_bg.wasm
fi

# Stamp the build with a content hash of the final wasm binary. The hash names
# the wasm URL (so the immutable cache header never pins a stale binary) and the
# service-worker cache (so a deploy purges the previous shell). The client's
# update check compares this id rather than the semantic version, so re-releasing
# the same version with a different binary is still detected.
BUILD_ID=$(sha256sum dist/zapslab_bg.wasm | cut -c1-12)
APP_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

mv dist/zapslab_bg.wasm "dist/zapslab_bg.${BUILD_ID}.wasm"
# Point wasm-bindgen's glue at the hashed filename.
sed -i "s/zapslab_bg\.wasm/zapslab_bg.${BUILD_ID}.wasm/g" dist/zapslab.js

cp web/index.html web/manifest.webmanifest web/sw.js web/_headers dist/
mkdir -p dist/icons
cp -R web/icons/. dist/icons/
# Fonts are embedded in the wasm binary (see src/main.rs), not served.

# Inject the build id into the page (for the client-side update check) and the
# service worker (cache name + pinned wasm URL).
sed -i "s/__BUILD_ID__/${BUILD_ID}/g" dist/index.html dist/sw.js

printf '{"name":"ZapSlab","version":"%s","build":"%s"}\n' \
	"${APP_VERSION}" "${BUILD_ID}" > dist/version.json

echo
echo "build id: ${BUILD_ID} (version ${APP_VERSION})"
echo "wasm binary:"
ls -lh "dist/zapslab_bg.${BUILD_ID}.wasm"
echo
echo "dist/ is ready to deploy (Cloudflare Pages: output directory = dist)"
