# nostr-ssr

A server that makes a nostr web app show up properly on the web. It keeps a local mirror of relay events and serves your app's pages as real HTML with a title, description, OpenGraph tags and JSON-LD, plus a sitemap. Links unfurl in chat apps, and search engines see content instead of an empty `<div id="root">`.

It is one Rust binary. Your app's server side render script runs inside it in embedded QuickJS and reads events straight from the mirror, so a render makes no network calls and takes milliseconds. Pages the mirror cannot answer are fetched from the relays and rendered again.

Running on it today: [snort.social](https://snort.social), [zap.stream](https://zap.stream) and [dtan.xyz](https://dtan.xyz).

## Quick start

You need Docker. The repo carries a tiny demo site in `examples/demo`: a client shell, a 20 line render script and a config that follows a few public relays.

```bash
git clone https://github.com/v0l/nostr-ssr
cd nostr-ssr/examples/demo
docker run --rm -p 3000:3000 \
  -v "$PWD/dist:/app/dist:ro" \
  -v "$PWD/config.json:/app/config.json:ro" \
  -e CONFIG=/app/config.json \
  voidic/nostr-ssr:latest
```

Wait about ten seconds for the first sync (the log prints `initial sync complete`), then open <http://localhost:3000/> or fetch it:

```bash
curl -s localhost:3000/
```

You should see the latest notes in the HTML itself, with the head filled in:

```html
<title>Latest notes | Demo</title>
<meta property="og:title" content="Latest notes" />
<link rel="canonical" href="http://localhost:3000/" />
...
<div id="root"><h1>Latest notes</h1><ul><li>...</li></ul></div>
<script id="ssr-data" type="application/json">{"count":10}</script>
```

Other things to try:

```bash
curl -s localhost:3000/sitemap.xml
curl -s localhost:3000/sitemap-pages.xml
curl -si localhost:3000/nope | head -1
```

The sitemap index lists `/sitemap-pages.xml` and one file per chunk of each configured sitemap source, and `/nope` returns `404` because the render script said so.

`examples/demo/Dockerfile` bakes the same files into an image, and `examples/demo/compose.yaml` is the same run as a Compose service.

## Adding it to your app

Your app needs two build outputs, next to each other:

| File | What it is |
| --- | --- |
| `dist/client/index.html` | The normal SPA build, with `<!--app-html-->` in the root element (or an empty `<div id="root"></div>`) |
| `dist/ssr/render.js` | One script that defines `globalThis.ssrRender`. Optional |

Without `render.js` the server still serves the SPA and adds per event OpenGraph tags, which is enough to get link previews on a plain app.

`ssrRender` takes `{"url"}` as JSON and returns `{html, status, head, data}` as JSON. It reads events with `relayQuery`, which the server provides. In Preact:

```tsx
import { render } from "preact-render-to-string";

declare const relayQuery: (filters: string) => string;

(globalThis as any).ssrRender = (json: string) => {
  const { url } = JSON.parse(json);
  const notes = JSON.parse(relayQuery(JSON.stringify([{ kinds: [1], limit: 10 }])));
  return JSON.stringify({
    html: render(<Notes notes={notes} />),
    status: 200,
    head: { title: "Latest notes", path: url },
    data: { notes },
  });
};
```

Build it as a single script with everything inlined:

```bash
bun build src/entry-server.tsx --outfile=dist/ssr/render.js \
  --target=browser --format=iife --jsx=automatic --jsx-import-source=preact
```

The full contract (inputs, every `head` key, `relayCount`, misses, React notes) is in [docs/bundle.md](docs/bundle.md).

### The Dockerfile

Sites build `FROM voidic/nostr-ssr` and copy their build in. [zap.stream's `Dockerfile.ssr`](https://github.com/v0l/zap.stream/blob/main/Dockerfile.ssr) is the model:

```dockerfile
FROM oven/bun:alpine AS web
WORKDIR /app
COPY . .
RUN bun install && bun run build && bun run build:ssr

FROM voidic/nostr-ssr:latest
COPY --from=web /app/build /app/dist/client
COPY --from=web /app/build/ssr /app/dist/ssr
RUN rm -rf /app/dist/client/ssr
COPY ssr.json /app/ssr.json
ENV CONFIG=/app/ssr.json
```

The image defaults to `DIST=/app/dist` and `BIND=0.0.0.0:3000`. Put site specific settings (relays, kinds, backfill, sitemap, proxy rules, headers, OpenGraph) in `ssr.json` in your own repo. See the zap.stream `ssr.json` for a full example.

## Configuration

One JSON file, read from `CONFIG`. Every key is optional.

```json
{
  "site_name": "Demo",
  "description": "Shown when a page sets none",
  "public_url": "https://example.com",
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

`kinds` is followed live. `backfill` loads history at startup, and when the `priority` filters finish the mirror counts as loaded, which turns on page caching and sitemaps. `warm` pages are rendered in the background.

| Env var | Config key | Default | Meaning |
| --- | --- | --- | --- |
| `CONFIG` | none | none | Path to the JSON file |
| `SITE_NAME` | `site_name` | `Nostr` | Name in titles |
| `PUBLIC_URL` | `public_url` | request host | Origin for canonical links and sitemaps |
| `RELAYS` | `relays` | none | Comma separated relay URLs |
| `MAX_EVENTS` | `max_events` | `200000` | Mirror capacity |
| `DIST` | `dist` | `dist` | Folder with `client/` and `ssr/` |
| `SSR_BUNDLE` | `bundle` | `DIST/ssr/render.js` | Render script path |
| `BIND` | `bind` | `0.0.0.0:3000` | Listen address |
| `RELAY_PORT` | `relay_port` | off | Serve the mirror as a relay on this port |
| `SSR_WORKERS` | `workers` | `2` | Render threads |

Everything else (result filters, sitemap options, proxy rules, OpenGraph, headers, extra HTML shells, media health checks) is in [docs/config.md](docs/config.md).

## Deploying

The server needs outbound websocket access to your relays and nothing else. A Compose file:

```yaml
services:
  web:
    image: voidic/nostr-ssr:latest
    ports:
      - "3000:3000"
    environment:
      CONFIG: /app/config.json
      MAX_EVENTS: "100000"
    volumes:
      - ./dist:/app/dist:ro
      - ./config.json:/app/config.json:ro
    restart: unless-stopped
    mem_limit: 1g
```

On Kubernetes it is a plain Deployment: one container, port 3000, the memory limit below, and an Ingress for your domain. Set `PUBLIC_URL`, or pass `x-forwarded-proto` and `Host` from the proxy, so canonical links and sitemaps use the public address. Use `imagePullPolicy: Always` if you deploy `:latest`, and `kubectl rollout restart` after pushing a new image.

There is no `/health` or metrics route. Use `GET /` or `/sitemap.xml` as the probe, and watch the logs. `RUST_LOG=debug` raises the level. The mirror is in memory, so each pod syncs from the relays on start and serves the plain shell until `initial sync complete` is logged. Run one replica per site, or several that each sync on their own.

Memory is set mostly by `MAX_EVENTS`. The demo holding about 2500 events sat at 13 MiB. Each render worker may use up to 256 MiB of QuickJS heap and the page cache up to 96 MiB. A site with 300000 events and 2 workers fits in 2 GiB. Measure your own event mix after the first sync and set the limit from that.

## Development

```bash
cargo test
cargo build --release
CONFIG=examples/demo/config.json DIST=examples/demo/dist cargo run
docker build -t voidic/nostr-ssr:latest .
```

Tests run offline against a mock relay and local upstreams.

| Path | What it does |
| --- | --- |
| `src/main.rs`, `src/serve.rs` | Startup: config, mirror, health sweeper, renderer, warm loop, axum |
| `src/config.rs` | The config schema and env overrides |
| `src/mirror.rs` | In-memory relay, upstream follow and backfill, queries, on demand fetch |
| `src/engine.rs` | QuickJS worker pool and the functions the bundle can call |
| `src/web.rs`, `src/head.rs`, `src/cache.rs` | Routes, page assembly, head tags, page cache |
| `src/sitemap.rs`, `src/opengraph.rs`, `src/proxy.rs` | Sitemaps, per event tags, path proxy |
| `src/filters.rs`, `src/health.rs`, `src/sweep.rs`, `src/media.rs` | Result filters and the media checker |
| `src/plugin.rs` | Trait for apps that link the crate and add JS functions, data and routes |

See `AGENTS.md` for conventions.
