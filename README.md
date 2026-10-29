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
`static_paths`, `warm`, `filters`, `sitemap_ttl_secs`, `sitemap_chunk`, `max_events`, `sitemap_chunk`, `relay_port` (expose the mirror as a relay).

## Result filters

`filters` is a list of rules applied to every query result (renders, on-demand lookups and sitemaps) after the store
answers and before `limit`, so a limit of 20 still returns 20 kept events. A rule with `kinds` only touches those kinds.
Fields: `deny_authors`, `deny_ids`, `deny_tags` (`{"t": ["spam"]}`), `require_tags`, `deny_content` (regex),
`deny_file` (JSON array or lines of event ids, reloaded when the file changes, e.g. a dead-media list written by a checker).
Custom logic implements the `ResultFilter` trait in `src/filters.rs` and is passed to `Mirror::with_filters`.

## Sitemaps

Each `sitemap` source has its own `name`, `filter`, `prefix`, `encoding` (`nevent`, `note`, `naddr`, `npub`), `chunk`
(default `sitemap_chunk`, capped at the protocol limit of 50000), `max` (newest N only), `changefreq`, `priority`.
Files are `/sitemap-<name>-<n>.xml`, oldest first, so existing files stay byte-stable as new events arrive and only
the last file grows. The index carries a `lastmod` per file. The snapshot is built once per `sitemap_ttl_secs` and only
holds ids, not events, so requests do not re-query the store.

Routes: pages, `/sitemap.xml`, `/sitemap-pages.xml`, `/sitemap-NAME-N.xml`, static client assets.

Test: `cargo test`. Run: `CONFIG=configs/dtan.json DIST=../dtan/dist cargo run`.
