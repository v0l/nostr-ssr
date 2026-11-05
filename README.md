# nostr-ssr

One server for the SSR sites (dtan, snort, zap.stream and others). It mirrors events from upstream relays into an
in-memory relay, and renders each site's SSR bundle in embedded QuickJS against that store with no network during render.

Each site ships `dist/client/index.html` (with `<!--app-html-->` or an empty `<div id="root">`) and `dist/ssr/render.js`,
mounted into the image at `DIST`. Site behaviour is a JSON file (`CONFIG`, see `configs/`); env vars `SITE_NAME`,
`PUBLIC_URL`, `RELAYS`, `MAX_EVENTS`, `DIST`, `SSR_BUNDLE`, `BIND`, `RELAY_PORT`, `SSR_WORKERS` override it.

## Bundle contract

- `globalThis.ssrRender(json)` receives `{"url"}` and returns JSON
  `{html, status, head: {title, description, path, type, image, published, modified, noindex, jsonLd, meta}, data}`.
  `jsonLd` may be one object or a list (one script each). `meta` is a list of `[name, content]` pairs written after
  the fixed tags (`og:`, `video:` and `article:` names as `property`), skipping names the fixed tags already set.
  `data` is embedded as `<script id="ssr-data" type="application/json">`. `ssrRender` may return a promise; it is
  settled from the QuickJS job queue (no timers, no network), and promises left pending when the bundle loads are settled
  before the first render.
- `relayQuery(filtersJson) -> eventsJson` is provided natively (NIP-01 filters, plus `search`). Empty results are
  reported as misses; the server fetches them from upstream, then renders again.
- `relayCount(filterJson) -> number` counts mirror events matching one filter (0 for a bad filter). It never fetches.
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

Media sources (`"media": true`) can also set `family_friendly` (`true`/`false`, written as `<video:family_friendly>`)
and `view_kind`, the kind of view events tagging a video by `e` (or `a` for addressable kinds). Its mirror count is
written as `<video:view_count>`, so that kind must be in `kinds` and `backfill`.

## Proxy rules

`proxy` replaces the Cloudflare Pages functions and page rules that forward paths to another server. Each rule has
`prefix` (ends in `/` for one path segment under it, otherwise an exact path), `upstream` (URL the remainder is appended
to; the query string is forwarded), `headers` (sent upstream), `timeout_secs`, and optionally `resolve_nip05` (a URL
template with `{name}`) plus `resolve_headers`. With `resolve_nip05`, a segment that is not a 64 char hex pubkey is looked up
as a nostr address first and the pubkey is substituted, falling back to the raw segment. Only GET, HEAD and OPTIONS are
served; responses get `access-control-allow-origin: *` and `cache-control: no-store`. `forward_path: true` appends the request path (not just the remainder) to `upstream`, and `{host}` in any header value is replaced with the request host (`x-forwarded-host` first, port stripped). Example (zap.stream lnurlp):

```json
{"prefix": "/.well-known/lnurlp/", "upstream": "https://api-core.zap.stream/.well-known/lnurlp/",
 "resolve_nip05": "https://api.snort.social/.well-known/nostr.json?name={name}",
 "resolve_headers": {"X-Proxy-Host": "zap.stream"}}
```

The `snort-social-nip05` worker routes (snort.social nostr.json and lnurlp, zap.stream nostr.json, nostr.gg nostr.json) all forward to `api.snort.social` with the request host. Rules are host agnostic, so one config serves every domain pointed at the server:

```json
[
  {"prefix": "/.well-known/nostr.json", "upstream": "https://api.snort.social", "forward_path": true, "headers": {"X-Proxy-Host": "{host}"}},
  {"prefix": "/.well-known/lnurlp/", "upstream": "https://api.snort.social", "forward_path": true, "headers": {"X-Proxy-Host": "{host}"}}
]
```

## OpenGraph tags

`opengraph` replaces the `nostr-rs-api` `/opengraph/<id>` rewrite that the Pages middleware called. For entity paths
(`/npub1...`, `/e/nevent1...`, `/p/nprofile1...`, `/naddr1...`, `/note1...`) and bare nostr address paths (`/alice`) the
event or profile is looked up in the mirror (fetched from upstream relays on a miss) and its tags are merged over the
bundle's head, replacing matching `og:*`/`twitter:*` metas, the title, the canonical link and the JSON-LD. Kinds: 30311 live
streams (`og:video` from `streaming` or `recording`, host from the `p` tag with role host, player card), 1313 clips
(`og:video` from the `r` tag, mp4), profiles (kind 0), everything else as an article. Fields: `twitter_site`,
`player` (template with `{nevent}`), `avatar` (template with `{pubkey}`), `nip05_url` (template with `{name}`),
`nip05_headers`, `nip05_domain`, `skip_paths` (bare paths that are real routes, not addresses).

## Sites without an SSR bundle

If `dist/ssr/render.js` does not exist the server serves the client shell and still merges `opengraph` tags for entity
and address paths, so a plain SPA (snort) gets per-event previews without writing a renderer. `live: false` stops the
relay stream and backfills, so the mirror only holds what pages fetch on demand. `headers` adds response headers by path prefix
(for example a Content-Security-Policy that nginx used to set).

`shells` serves a different client HTML file for a path prefix, as is and without rendering, for a light entry point
that should not load the main app (an OBS overlay, an embed):

```json
"shells": [{"prefix": "/overlay/", "file": "overlay.html"}]
```

Routes: pages, `/sitemap.xml`, `/sitemap-pages.xml`, `/sitemap-NAME-N.xml`, static client assets.

Test: `cargo test`. Run: `CONFIG=configs/dtan.json DIST=../dtan/dist cargo run`.
