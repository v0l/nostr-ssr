# Configuration

The config is one JSON file, read from the path in `CONFIG`. Every key is optional. Without `CONFIG` the defaults apply.

## Environment variables

These override the file.

| Variable | Config key | Default | Notes |
| --- | --- | --- | --- |
| `CONFIG` | none | none | Path to the JSON file |
| `SITE_NAME` | `site_name` | `Nostr` | Used in titles and `og:site_name` |
| `PUBLIC_URL` | `public_url` | request host | Origin for canonical links and sitemaps. Set it behind a proxy |
| `RELAYS` | `relays` | none | Comma separated `wss://` URLs |
| `MAX_EVENTS` | `max_events` | `200000` | Mirror capacity, see [memory](#memory) |
| `DIST` | `dist` | `dist` | Folder with `client/` and `ssr/` |
| `SSR_BUNDLE` | `bundle` | `DIST/ssr/render.js` | Render script path |
| `BIND` | `bind` | `0.0.0.0:3000` | Listen address |
| `RELAY_PORT` | `relay_port` | off | Serve the mirror as a relay on this port |
| `SSR_WORKERS` | `workers` | `2` | QuickJS render threads |
| `CACHE_MAX_MB` | `cache_max_mb` | `64` | Page cache size, see [memory](#memory) |
| `RUST_LOG` | none | `info` with nostr-sdk subscription noise off | Log level |

## Example

```json
{
  "site_name": "Demo",
  "description": "Shown when a page sets none",
  "public_url": "https://example.com",
  "default_image": "/banner.jpg",
  "relays": ["wss://relay.primal.net", "wss://relay.damus.io"],
  "kinds": [1, 30023],
  "backfill": [
    {"filter": {"kinds": [30023]}, "max": 5000, "priority": true},
    {"filter": {"kinds": [1]}, "max": 20000}
  ],
  "max_events": 100000,
  "static_paths": ["/", "/about"],
  "warm": ["/"],
  "sitemap": [
    {"name": "articles", "filter": {"kinds": [30023]}, "prefix": "/a/", "encoding": "naddr"}
  ]
}
```

`kinds` is what the live subscription follows. `backfill` loads history once at startup. Filters marked `priority` run first, and once they finish the mirror counts as loaded, which turns on page caching and the sitemap snapshot. `warm` lists paths rendered in the background so the cache is hot.

## Top level keys

| Key | Default | Meaning |
| --- | --- | --- |
| `site_name` | `Nostr` | Site name |
| `description` | empty | Fallback description |
| `public_url` | none | Public origin |
| `default_image` | `/banner.jpg` | Fallback `og:image` |
| `relays` | `[]` | Upstream relays. With none, the mirror counts as loaded at once |
| `live` | `true` | Follow upstream live. `false` stops the stream and backfills, so the mirror only holds what pages fetch on demand |
| `kinds` | `[]` | Kinds followed live |
| `backfill` | `[]` | List of `{filter, max, priority}`. `max` defaults to 5000 |
| `max_events` | `200000` | Mirror capacity |
| `max_event_bytes` | `65536` | Events whose content plus tags are larger are not mirrored. `0` turns it off |
| `static_paths` | `["/"]` | Paths listed in `/sitemap-pages.xml` |
| `warm` | `[]` | Paths pre-rendered in the background |
| `sitemap` | `[]` | Sitemap sources, see [Sitemaps](#sitemaps) |
| `sitemap_ttl_secs` | `300` | How long a sitemap snapshot is reused |
| `sitemap_chunk` | `40000` | Default URLs per sitemap file, capped at 50000 |
| `filters` | `[]` | [Result filters](#result-filters) |
| `health` | off | [Media liveness checker](#media-health) |
| `proxy` | `[]` | [Proxy rules](#proxy-rules) |
| `opengraph` | off | [OpenGraph tags](#opengraph) |
| `headers` | `[]` | Response headers by path prefix |
| `shells` | `[]` | Alternate client HTML by path prefix |
| `dist`, `bundle`, `bind`, `relay_port`, `workers`, `cache_max_mb` | see env table | Same as the env vars |
| `render_queue` | `32` | Renders that may wait for a worker. More callers wait `render_wait_ms` for a slot |
| `render_wait_ms` | `2000` | How long a page waits for a queue slot before it is shed with `503` and `Retry-After: 5` |
| `renders_per_engine` | `2000` | A worker reloads its QuickJS engine after this many renders. `0` turns it off |
| `engine_heap_mb` | `96` | A worker reloads its engine when the QuickJS heap passes this after a render. `0` turns it off |

## Routes

| Route | Served by |
| --- | --- |
| `/healthz` | JSON from memory: `ok`, `loaded`, `cached_pages`, `cached_bytes`, `rss_bytes`, `backed_off_relays`. Never renders, so use it for probes |
| `/sitemap.xml` | Sitemap index: the pages file plus every chunk of every source |
| `/sitemap-pages.xml` | `static_paths` plus `ssrPaths()` from the bundle |
| `/sitemap-<name>-<n>.xml` | Chunk `n` (from 1) of a sitemap source |
| `/api/<name>/...` | Routes of a [plugin](bundle.md#what-the-server-gives-you) |
| proxy prefixes | Your `proxy` rules |
| anything else | A file from `DIST/client`, or a rendered page |

Pages are cached for 30 seconds fresh and 15 minutes stale while revalidating, and only once the mirror is loaded. Requests with a query string are never cached. Concurrent requests for one URL, query string or not, share a single render. Files under `/assets/` get a one year immutable cache header. Probe `/healthz`, not `/`: a page probe waits on render capacity and fails under load. `initial sync complete events=N` in the log marks the end of the first sync.

When a render fails or returns bad JSON the plain client shell is served with status 200, so the SPA still works, and that URL is not rendered again for 30 seconds. When the render queue stays full for `render_wait_ms`, an uncached page gets the plain shell with status 503 and `Retry-After: 5`; a page that is in the cache, even an expired one, is served from it.

A page that reads events the mirror lacks is rendered, the missing events are fetched from the upstream relays, and it is rendered once more. The render worker is free while the fetch runs. All lookups for one page share a 3 second budget, and a lookup by id returns as soon as every id is found. A relay that answers `rate-limited` is skipped for on-demand fetches and backfills for 15 seconds, doubling on each repeat up to 10 minutes, with one log line per backoff.

## Result filters

`filters` is a list of rules applied to every query result (renders, on demand lookups and sitemaps), after the store answers and before `limit`. A limit of 20 still returns 20 kept events. A rule with `kinds` only touches those kinds.

| Field | Meaning |
| --- | --- |
| `kinds` | Kinds the rule applies to. Empty means all |
| `deny_authors` | Hex pubkeys to drop |
| `deny_ids` | Hex event ids to drop |
| `deny_tags` | `{"t": ["spam"]}` drops events with any listed value for that tag |
| `require_tags` | Tag names an event must have |
| `deny_content` | Regex. Matching events are dropped |
| `deny_file` | Path to a JSON array, or lines, of event ids. Reloaded when the file changes, for example a dead media list written by a checker |

```json
"filters": [
  {"kinds": [1], "deny_tags": {"t": ["spam"]}, "deny_content": "(?i)free money"},
  {"deny_authors": ["<hex pubkey>"]}
]
```

Custom logic implements the `ResultFilter` trait in `src/filters.rs` and is passed to `Mirror::with_filters`.

## Sitemaps

Each entry in `sitemap` is one source.

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | `events` | File name part: `/sitemap-<name>-<n>.xml` |
| `filter` | `{}` | NIP-01 filter selecting events |
| `prefix` | `/e/` | Path prefix put before the encoded id |
| `encoding` | `nevent` | `nevent`, `note`, `naddr`, `npub`, `nprofile`, or `auto` (`naddr` for addressable kinds, else `nevent`) |
| `chunk` | `sitemap_chunk` | URLs per file, capped at 50000 |
| `max` | none | Newest N events only |
| `changefreq` | `monthly` | Written to every URL |
| `priority` | `0.7` | Written to every URL |
| `group` | none | `author` (one URL per author) or `tag` (one URL per `t` tag, most used first) |
| `min_count` | `1` | With `group: "tag"`, drop tags used fewer times |
| `media` | `false` | Write video sitemap entries, with duplicates of the same media collapsed |
| `family_friendly` | none | With `media`, written as `<video:family_friendly>` |
| `view_kind` | none | With `media`, the kind of view events that tag a video by `e` (or `a`). The count is written as `<video:view_count>`, so that kind must be in `kinds` and `backfill` |

Files are numbered oldest first, so existing files stay byte-stable as events arrive and only the last file grows. The index carries a `lastmod` per file. The snapshot holds ids only and is rebuilt once per `sitemap_ttl_secs`.

## Proxy rules

`proxy` forwards paths to another server, which replaces CDN page rules and edge functions.

| Key | Default | Meaning |
| --- | --- | --- |
| `prefix` | none | Ends in `/`: matches one path segment under it. Otherwise an exact path |
| `upstream` | none | URL the remainder is appended to. The query string is forwarded |
| `forward_path` | `false` | Append the whole request path instead of the remainder |
| `headers` | `{}` | Sent upstream. `{host}` is replaced with the request host (`x-forwarded-host` first, port stripped) |
| `timeout_secs` | `10` | Upstream timeout |
| `resolve_nip05` | none | URL template with `{name}`. A segment that is not a 64 char hex pubkey is looked up as a nostr address and the pubkey substituted, falling back to the raw segment |
| `resolve_headers` | `{}` | Headers for the lookup |

Only GET, HEAD and OPTIONS are served. Responses get `access-control-allow-origin: *` and `cache-control: no-store`. Rules do not depend on the domain, so one config serves every domain pointed at the server.

```json
"proxy": [
  {"prefix": "/.well-known/nostr.json", "upstream": "https://api.example.com", "forward_path": true,
   "headers": {"X-Proxy-Host": "{host}"}},
  {"prefix": "/.well-known/lnurlp/", "upstream": "https://pay.example.com/.well-known/lnurlp/",
   "resolve_nip05": "https://api.example.com/.well-known/nostr.json?name={name}"}
]
```

## OpenGraph

`opengraph` gives entity pages their own previews without a bundle. For entity paths (`/npub1...`, `/e/nevent1...`, `/p/nprofile1...`, `/naddr1...`, `/note1...`) and bare nostr address paths (`/alice`), the event or profile is looked up in the mirror (fetched from the relays on a miss) and its tags are merged over the bundle's head, replacing matching `og:*` and `twitter:*` metas, the title, the canonical link and the JSON-LD.

Kinds: 30311 live streams (`og:video` from `streaming` or `recording`, host from the `p` tag with role `host`, player card), 1313 clips (`og:video` from the `r` tag), profiles (kind 0), and everything else as an article.

| Key | Meaning |
| --- | --- |
| `twitter_site` | Handle for `twitter:site` |
| `player` | Player URL template with `{nevent}` |
| `avatar` | Avatar URL template with `{pubkey}` |
| `nip05_url` | Lookup URL template with `{name}` |
| `nip05_headers` | Headers for the lookup |
| `nip05_domain` | Domain bare addresses belong to |
| `skip_paths` | Bare paths that are real routes, not addresses |

## Sites without a bundle

If there is no `render.js` the server serves the client shell and still merges `opengraph` tags, so a plain SPA gets per event previews without a renderer.

`headers` adds response headers by path prefix, for example a `content-security-policy` that nginx used to set:

```json
"headers": [{"prefix": "/", "set": {"x-frame-options": "SAMEORIGIN"}}]
```

`shells` serves a different client file for a path prefix, as is and without rendering, for a light entry point such as an overlay or an embed:

```json
"shells": [{"prefix": "/overlay/", "file": "overlay.html"}]
```

## Media health

`health` turns on a checker that probes media URLs in matching events and hides events whose media is definitely gone. Timeouts, DNS errors, 5xx and 429 never count as dead.

| Key | Default | Meaning |
| --- | --- | --- |
| `filter` | kinds 20, 21, 22, 34235, 34236 | Events to check |
| `interval_secs` | `60` | Sweep interval |
| `allow_private` | `false` | Allow URLs on private addresses |

## Memory

The mirror keeps events in memory, so `max_events` sets the footprint, and it is the part that grows: with `live` on, the mirror fills from its first sync up to `max_events` and only then stops growing. In a replay of a social site (kinds 0, 1, 5 and 30023, 20 live events a second, 4 page requests a second) the container settled at about 375 MiB with `max_events` 30000 and 480 MiB with 60000; with 150000 it was still climbing past 560 MiB at 97000 events after 30 minutes, about 4 KiB per event once indexes and the relay client are counted. Size the memory limit for a full mirror, not for the count after the first sync. On top of that each render worker holds a QuickJS heap that is reloaded after `renders_per_engine` renders or past `engine_heap_mb`, and the page cache holds up to `cache_max_mb`. `/healthz` reports `events`, `cached_bytes` and `rss_bytes`. `examples/replay.rs` runs the replay against any image.
