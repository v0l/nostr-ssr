# AGENTS.md

Shared server for the Nostr SSR sites (dtan, snort, zap.stream and others). It mirrors events from upstream relays into
an in-memory relay and renders each site's SSR bundle in embedded QuickJS against that store. See README.md for the
config reference and the bundle contract.

## Public repo rules

This repo and its image `voidic/nostr-ssr` are public. Never put a site name, domain, relay, tag filter or config
that belongs to a private site in code, tests, fixtures, docs, config examples or commit messages. Site specific
config lives in that site's own repo as `ssr.json`. Use neutral fixtures (`demo`, `example.com`). Before committing,
check history too: `git log -p --all | grep -iE '<names>'`.

## Layout

- `src/main.rs` startup: config, mirror, health sweeper, renderer, warm loop, axum.
- `src/config.rs` the whole config schema. Every option is optional with a default; env vars override a few.
- `src/mirror.rs` in-memory relay, upstream follow and backfill, query with search and result filters, on demand fetch.
- `src/filters.rs` result filters applied inside `Mirror::query` before `limit`. `ResultFilter` trait, rule based filter.
- `src/engine.rs` QuickJS worker pool. Provides `relayQuery`, `relayHidden`, `hostLog` to the bundle. Reloads the bundle on mtime change.
- `src/web.rs` axum router, page rendering, miss resolution, cache, custom headers.
- `src/head.rs` head tag generation and shell assembly (`assemble`, `assemble_raw`).
- `src/opengraph.rs` per event and profile tags merged over the bundle head. Kinds 30311, 1313, profiles, others.
- `src/sitemap.rs` per source chunked sitemaps from an id snapshot.
- `src/proxy.rs` config driven path proxy rules (replaces CDN page rules and functions).
- `src/health.rs`, `src/sweep.rs`, `src/media.rs` media liveness checker and the filter that hides dead media.
- `src/cache.rs` page cache with stale while revalidate.

## Commands

```
cargo test
cargo build --release
CONFIG=configs/dtan.json DIST=../dtan/dist cargo run
docker build -t voidic/nostr-ssr:latest .
```

All tests run offline (mock relay, local axum upstreams). Keep it that way.

## Conventions

- No comments in code. Names carry the meaning. Do not add doc comments.
- Every bug fix and every new option gets a test beside the code in the same file, named after the behaviour.
- Render must stay synchronous and make no network calls. Data comes through `relayQuery`; misses are reported and the server fetches then re-renders.
- Never mark media dead on timeouts, DNS errors, 5xx or 429. Only a definite failure from every source counts.
- Sitemap files must stay stable as events arrive (oldest first, chunked), and never exceed 50000 URLs.
- Proxy rules only serve GET, HEAD and OPTIONS and one path segment under a prefix.
- Do not add `rust-alloc`-less QuickJS builds; `rust-alloc` is required for acceptable render times.
- Commit messages: a short subject, no body, no trailers.

## Publishing

The image is multi arch and must be pushed before any site image that builds on it:

```
docker buildx build --platform linux/amd64,linux/arm64 -t voidic/nostr-ssr:latest --push .
```

Site images build `FROM voidic/nostr-ssr:latest`. Never push a site image to a public registry unless the site is
public. Check the site's repo visibility first and ask which registry to use.

## Adding a site

1. Add `ssr.json` and a `Dockerfile.ssr` to the site repo (copy the dtan ones).
2. If the site has an SSR bundle, it exports `ssrRender(json)` returning `{html, status, head, data}` and reads data
   through `relayQuery`. Without a bundle the shell is served with opengraph tags and headers only.
3. Add proxy rules, headers and opengraph settings to its `ssr.json` instead of changing this repo.
4. Compare the output against the live site (titles, links, status codes, sitemap) before cutting over.
