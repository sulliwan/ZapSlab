// ZapSlab service worker: offline app shell.
//
// BUILD_ID is injected at build time (scripts/build_web.sh) from the wasm
// binary's content hash. It names the cache (so each deploy gets a fresh cache
// and the previous shell is purged on activate) and pins the hashed wasm URL
// (keeping the immutable cache header on `.wasm` safe across deploys). The page
// compares the deployed build id in /version.json against the id it booted with
// to offer a reload; the service worker only serves and caches the current
// shell.

const BUILD_ID = "__BUILD_ID__";
const CACHE = `zapslab-${BUILD_ID}`;
const WASM = `/zapslab_bg.${BUILD_ID}.wasm`;
const SHELL = [
  "/",
  "/index.html",
  "/zapslab.js",
  WASM,
  "/manifest.webmanifest",
  "/icons/icon-192.png",
  "/icons/icon-512.png",
  "/icons/maskable-512.png",
  "/icons/apple-touch-icon.png",
  "/version.json",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key)))
      )
      .then(() => self.clients.claim())
  );
});

// A reload after downloading a new shell hands control to the new worker so the
// page is served by the fresh cache.
self.addEventListener("message", (event) => {
  if (event.data && event.data.type === "SKIP_WAITING") {
    self.skipWaiting();
  }
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  if (new URL(req.url).origin !== self.location.origin) return;

  const { pathname } = new URL(req.url);

  // The deployed build id must never be served stale: it drives the page's
  // update check. Bypass the HTTP cache and fall back to the pruned shell copy
  // only when offline.
  if (pathname === "/version.json") {
    event.respondWith(
      fetch(req, { cache: "no-store" }).catch(() => caches.match(req))
    );
    return;
  }

  // Navigations: network-first so deploys show up; offline falls back to shell.
  if (req.mode === "navigate") {
    event.respondWith(
      fetch(req)
        .then((res) => {
          const copy = res.clone();
          caches.open(CACHE).then((cache) => cache.put("/", copy));
          return res;
        })
        .catch(() => caches.match("/index.html"))
    );
    return;
  }

  // Application code must never be served stale: try the network first and
  // update the cache, falling back to the cached copy only when offline. The
  // wasm URL is unique per build, so the immutable HTTP cache stays correct.
  if (pathname.endsWith(".js") || pathname.endsWith(".wasm")) {
    event.respondWith(
      fetch(req)
        .then((res) => {
          const copy = res.clone();
          caches.open(CACHE).then((cache) => cache.put(req, copy));
          return res;
        })
        .catch(() => caches.match(req))
    );
    return;
  }

  // Everything else: cache-first, fill from network on miss.
  event.respondWith(
    caches.match(req).then(
      (hit) =>
        hit ||
        fetch(req).then((res) => {
          const copy = res.clone();
          caches.open(CACHE).then((cache) => cache.put(req, copy));
          return res;
        })
    )
  );
});
