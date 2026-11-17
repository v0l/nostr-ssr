# Bundle contract

A site gives nostr-ssr two build outputs. The server wraps them with head tags, sitemaps and caching.

## Files

| Path | What it is |
| --- | --- |
| `dist/client/index.html` | The normal SPA build. Put `<!--app-html-->` inside the root element, or leave `<div id="root"></div>` empty. |
| `dist/ssr/render.js` | One script that defines `globalThis.ssrRender`. Optional: without it the server serves the client shell with OpenGraph tags and headers only. |

`DIST` (or `dist` in the config) is the folder holding them. If `DIST/client/index.html` is missing, `DIST/index.html` is used instead. `SSR_BUNDLE` (or `bundle`) points at a render script somewhere else. The script is reloaded when its mtime changes.

The script runs in embedded QuickJS, not Node and not a browser. There are no timers, no `fetch`, no DOM. Build it as a single IIFE with everything inlined (see the Vite config in [zap.stream](https://github.com/v0l/zap.stream/blob/main/vite.ssr.config.ts) or `bun build --format=iife`).

## What you write: `ssrRender`

```js
globalThis.ssrRender = (json) => {
  const { url, now, origin } = JSON.parse(json);
  return JSON.stringify({ html, status, head, data });
};
```

Input fields:

| Field | Meaning |
| --- | --- |
| `url` | Path and query of the request, for example `/e/nevent1...?x=1` |
| `now` | Unix seconds, so renders do not call `Date.now()` |
| `origin` | Public origin, from `public_url` or the request host |

Output fields:

| Field | Meaning |
| --- | --- |
| `html` | Markup that goes into the root element |
| `status` | HTTP status. Anything 400 or above also adds `noindex` |
| `head` | Head tags, see below |
| `data` | Any JSON. Embedded as `<script id="ssr-data" type="application/json">` so the client can hydrate without refetching |

`ssrRender` may return a promise. It is settled from the QuickJS job queue, so only promises that need no timer or network can resolve. Promises left pending when the bundle loads are settled before the first render.

### `head`

| Key | Written as |
| --- | --- |
| `title` | `<title>TITLE \| site_name</title>`, plus `og:title` and `twitter:title` |
| `description` | `description`, `og:description`, `twitter:description` (falls back to the config `description`) |
| `path` | Canonical link and `og:url` |
| `type` | `og:type`, default `website` |
| `image` | `og:image` and `twitter:image`, made absolute (falls back to `default_image`) |
| `published`, `modified` | `article:published_time` and `article:modified_time` when `type` is `article` |
| `noindex` | `robots: noindex, follow` |
| `jsonLd` | One object, or a list with one script each. Relative `url` and `image` values are made absolute. Without it a `WebSite`, `CreativeWork` or `WebPage` block is generated from `path` |
| `meta` | List of `[name, content]` pairs written after the fixed tags. `og:`, `video:` and `article:` names use `property`. Names the fixed tags already set are skipped |
| `raw` | A full head string used as is, skipping everything above |

Tags in the client shell that the head replaces (`title`, `description`, `og:*`, `twitter:*`, canonical) are removed from the shell first.

## What the server gives you

These globals exist before your script is evaluated.

| Function | Behaviour |
| --- | --- |
| `relayQuery(filtersJson)` | Takes a JSON list of NIP-01 filters (plus `search`), returns a JSON list of events from the mirror. Result filters apply. |
| `relayCount(filterJson)` | Number of mirror events matching one filter. 0 for a bad filter. Never fetches. |
| `relayHidden()` | JSON list of keys hidden by the result filters, for bundles that need to know what was removed. |
| `hostLog(level, msg)` | Writes a warning line to the server log. Wire it to `console` if you want. |

Render is synchronous and makes no network calls. When `relayQuery` finds nothing, the filter is recorded as a miss. After the render the server fetches the miss from the upstream relays (3 second limit, tried once per filter for a while), then renders again. Only the second result is served.

Plugins linked into a custom binary can add more globals, enrich `data` and serve routes under `/api/<name>/`. See `src/plugin.rs`.

## Optional exports

| Function | Behaviour |
| --- | --- |
| `ssrPaths()` | Returns a JSON list of extra paths for `/sitemap-pages.xml`. `ssrCategories()` is read if `ssrPaths` is missing. |

## Minimal example

Preact:

```tsx
import { render } from "preact-render-to-string";

declare const relayQuery: (filters: string) => string;

function Notes({ notes }: { notes: { id: string; content: string }[] }) {
  return (
    <main>
      <h1>Latest notes</h1>
      <ul>{notes.map((n) => <li key={n.id}>{n.content.slice(0, 140)}</li>)}</ul>
    </main>
  );
}

(globalThis as any).ssrRender = (json: string) => {
  const { url } = JSON.parse(json);
  if (url !== "/") {
    return JSON.stringify({ html: render(<h1>Not found</h1>), status: 404, head: { title: "Not found" }, data: {} });
  }
  const notes = JSON.parse(relayQuery(JSON.stringify([{ kinds: [1], limit: 10 }])));
  return JSON.stringify({
    html: render(<Notes notes={notes} />),
    status: 200,
    head: { title: "Latest notes", path: "/" },
    data: { notes },
  });
};
```

Build it:

```bash
bun build src/entry-server.tsx --outfile=dist/ssr/render.js \
  --target=browser --format=iife --jsx=automatic --jsx-import-source=preact
```

For React use `react-dom/server.browser` (alias `react-dom/server` to it) and `renderToString`.

Keep client and server markup identical for the same `data`, so hydration does not throw the server HTML away.
