# nostr-ssr

One server for the SSR sites (dtan, snort, zap.stream and others). It mirrors events from upstream relays into an
in-memory relay, and renders each site's SSR bundle in embedded QuickJS against that store with no network during render.

Each site ships `dist/client/index.html` (with `<!--app-html-->` or an empty `<div id="root">`) and `dist/ssr/render.js`,
mounted into the image at `DIST`. Site behaviour is a JSON file (`CONFIG`, see `configs/`); env vars `SITE_NAME`,
`PUBLIC_URL`, `RELAYS`, `MAX_EVENTS`, `DIST`, `SSR_BUNDLE`, `BIND`, `RELAY_PORT`, `SSR_WORKERS` override it.

## Bundle contract

- `globalThis.ssrRender(json)` receives `{"url"}` and returns JSON
  `{html, status, head: {title, description, path, type, image, published, modified, noindex, jsonLd}, data}`.
  `data` is embedded as `<script id="ssr-data" type="application/json">`.
- `relayQuery(filtersJson) -> eventsJson` is provided natively (NIP-01 filters, plus `search`). Empty results are
  reported as misses; the server fetches them from upstream, then renders again.
- Optional `ssrPaths()` (or `ssrCategories()`) returns extra sitemap paths.
- `hostLog(level, msg)` is provided for `console`.

## Config

`relays`, `kinds` (live subscription scope), `backfill` (`filter`, `max`, `priority`; priority ones run first and
mark the store loaded, which enables page caching), `sitemap` (`filter`, `prefix`, `encoding`: `nevent`, `note`, `naddr`),
`static_paths`, `warm`, `max_events`, `sitemap_chunk`, `relay_port` (expose the mirror as a relay).

Routes: pages, `/sitemap.xml`, `/sitemap-pages.xml`, `/sitemap-events-N.xml`, static client assets.

Test: `cargo test`. Run: `CONFIG=configs/dtan.json DIST=../dtan/dist cargo run`.
