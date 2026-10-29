use crate::cache::PageCache;
use crate::config::Config;
use crate::sitemap::Sitemaps;
use crate::engine::Renderer;
use crate::head::{assemble, head_tags, Head, Site};
use crate::mirror::{parse_filter, Mirror};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use nostr::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

const ON_DEMAND_TIMEOUT: Duration = Duration::from_secs(3);

pub struct App {
    pub config: Config,
    pub mirror: Mirror,
    pub renderer: Option<Renderer>,
    pub pages: PageCache,
    pub sitemaps: Sitemaps,
}

pub type Shared = Arc<App>;

impl App {
    pub fn new(config: Config, mirror: Mirror, renderer: Option<Renderer>) -> Shared {
        Arc::new(App { config, mirror, renderer, pages: PageCache::new(Duration::from_secs(30), Duration::from_secs(900), 96 * 1024 * 1024), sitemaps: Sitemaps::default() })
    }
}

#[derive(Deserialize)]
struct Rendered {
    html: String,
    status: u16,
    #[serde(default)]
    head: Head,
    #[serde(default)]
    data: Value,
}

pub fn router(app: Shared) -> Router {
    let assets = ServeDir::new(app.config.client_dir()).append_index_html_on_directories(false);
    Router::new()
        .route("/sitemap.xml", get(crate::sitemap::index))
        .route("/sitemap-pages.xml", get(crate::sitemap::pages))
        .route("/sitemap-{file}", get(crate::sitemap::file))
        .fallback_service(assets.fallback(get(page).with_state(app.clone())))
        .layer(axum::middleware::map_response(cache_assets))
        .layer(CompressionLayer::new())
        .with_state(app)
}

async fn cache_assets(uri: axum::extract::OriginalUri, mut res: Response) -> Response {
    if uri.0.path().starts_with("/assets/") && res.status() == StatusCode::OK {
        res.headers_mut().insert(header::CACHE_CONTROL, "public, max-age=31536000, immutable".parse().unwrap());
    }
    res
}

pub fn origin(app: &App, headers: &HeaderMap) -> String {
    if let Some(u) = &app.config.public_url {
        return u.trim_end_matches('/').to_string();
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let scheme = headers.get("x-forwarded-proto").and_then(|h| h.to_str().ok()).filter(|p| *p == "https").unwrap_or("http");
    format!("{scheme}://{host}")
}

fn shell(app: &App) -> Option<String> {
    std::fs::read_to_string(app.config.client_dir().join("index.html")).ok()
}

fn plain_shell(shell: &str) -> String {
    shell.replace("<!--app-html-->", "")
}

fn html(status: u16, body: String) -> Response {
    let mut res = (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
    if status == 200 {
        res.headers_mut().insert(header::CACHE_CONTROL, "public, max-age=30".parse().unwrap());
    }
    res
}

async fn page(State(app): State<Shared>, headers: HeaderMap, req: Request) -> Response {
    let url = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_else(|| "/".into());
    page_for(app, headers, url).await
}

async fn resolve_misses(app: &App, misses: &[Value]) -> bool {
    let mut blocking = Vec::new();
    for m in misses.iter().filter(|m| m.get("search").is_none()) {
        let only_profiles = m.get("kinds").and_then(Value::as_array).is_some_and(|k| k == &[json!(0)]);
        if only_profiles {
            if let Some(authors) = m.get("authors").and_then(Value::as_array) {
                app.mirror.want_profiles(authors.iter().filter_map(|a| a.as_str().and_then(|a| PublicKey::from_hex(a).ok())));
            }
        } else if app.mirror.should_try(&m.to_string()) {
            if let Some((filter, _)) = parse_filter(m) {
                blocking.push(filter);
            }
        }
    }
    !blocking.is_empty() && app.mirror.fetch(blocking, ON_DEMAND_TIMEOUT).await > 0
}

async fn render(app: &App, origin: &str, shell_html: &str, url: &str) -> Option<(String, u16)> {
    let renderer = app.renderer.as_ref()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let input = json!({ "url": url, "now": now, "origin": origin }).to_string();
    let mut out = match renderer.call("ssrRender", input.clone()).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("render {url}: {e}");
            return None;
        }
    };
    if !out.misses.is_empty() && resolve_misses(app, &out.misses).await {
        match renderer.call("ssrRender", input).await {
            Ok(o) => out = o,
            Err(e) => tracing::error!("re-render {url}: {e}"),
        }
    }
    let rendered: Rendered = match serde_json::from_str(&out.text) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("render output {url}: {e}");
            return None;
        }
    };
    let site = Site { name: &app.config.site_name, description: &app.config.description, default_image: &app.config.default_image, origin };
    let tags = head_tags(&rendered.head, &site, rendered.status);
    Some((assemble(shell_html, &rendered.html, &tags, &rendered.data), rendered.status))
}

pub async fn page_for(app: Shared, headers: HeaderMap, url: String) -> Response {
    let Some(shell_html) = shell(&app) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "client build missing").into_response();
    };
    if app.renderer.is_none() {
        return html(200, plain_shell(&shell_html));
    }
    let origin = origin(&app, &headers);
    let produce = {
        let (app, origin, shell_html, url) = (app.clone(), origin.clone(), shell_html.clone(), url.clone());
        move || {
            let (app, origin, shell_html, url) = (app.clone(), origin.clone(), shell_html.clone(), url.clone());
            async move { render(&app, &origin, &shell_html, &url).await }
        }
    };
    let cacheable = app.mirror.priority_loaded() && !url.contains('?');
    let result = if cacheable {
        app.pages.get_or_render(&format!("{origin}{url}"), produce).await.map(|c| (c.html.as_str().to_string(), c.status))
    } else {
        produce().await
    };
    match result {
        Some((body, status)) => html(status, body),
        None => html(200, plain_shell(&shell_html)),
    }
}

pub async fn warm(app: &Shared, paths: &[String]) {
    let Some(shell_html) = shell(app) else { return };
    let origin = app.config.public_url.clone().unwrap_or_default().trim_end_matches('/').to_string();
    for path in paths {
        let (a, o, s, u) = (app.clone(), origin.clone(), shell_html.clone(), path.clone());
        app.pages
            .refresh(&format!("{origin}{path}"), move || {
                let (a, o, s, u) = (a.clone(), o.clone(), s.clone(), u.clone());
                async move { render(&a, &o, &s, &u).await }
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use nostr_database::NostrDatabase;
    use std::path::PathBuf;
    use tower::ServiceExt;

    const SHELL: &str = "<html><head><title>Old</title></head><body><div id=\"root\"><!--app-html--></div></body></html>";
    const BUNDLE: &str = r#"
globalThis.ssrRender = (json) => {
  const { url } = JSON.parse(json);
  if (url.startsWith("/nope")) return JSON.stringify({ html: "<h1>404</h1>", status: 404, head: { title: "Not found" }, data: {} });
  const notes = JSON.parse(relayQuery(JSON.stringify([{ kinds: [1], limit: 10 }])));
  const body = notes.map((n) => "<p>" + n.content + "</p>").join("");
  return JSON.stringify({ html: body, status: 200, head: { title: "Notes", path: url }, data: { count: notes.length } });
};
globalThis.ssrPaths = () => JSON.stringify(["/about"]);
"#;

    fn setup(name: &str, bundle: &str) -> Shared {
        let dir = std::env::temp_dir().join(format!("nostr-ssr-web-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("client/assets")).unwrap();
        std::fs::create_dir_all(dir.join("ssr")).unwrap();
        std::fs::write(dir.join("client/index.html"), SHELL).unwrap();
        std::fs::write(dir.join("client/assets/a.js"), "x").unwrap();
        std::fs::write(dir.join("ssr/render.js"), bundle).unwrap();
        let config = Config {
            site_name: "Test".into(),
            public_url: Some("https://t.example".into()),
            dist: PathBuf::from(&dir),
            sitemap: vec![crate::config::SitemapSource { filter: json!({"kinds": [1]}), encoding: "note".into(), chunk: Some(2), ..Default::default() }],
            sitemap_ttl_secs: 0,
            ..Config::default()
        };
        let mirror = Mirror::new(0, None, &[]);
        let renderer = Renderer::start(config.bundle_path(), 1, mirror.clone());
        App::new(config, mirror, Some(renderer))
    }

    async fn get_page(app: &Shared, uri: &str) -> (u16, String) {
        let res = router(app.clone()).oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        (status, String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap())
    }

    async fn note(app: &Shared, n: u8, at: u64, text: &str) -> Event {
        let keys = Keys::new(SecretKey::from_slice(&[n; 32]).unwrap());
        let ev = EventBuilder::text_note(text).custom_created_at(Timestamp::from_secs(at)).finalize(&keys).unwrap();
        app.mirror.database.save_event(&ev).await.unwrap();
        ev
    }

    #[tokio::test]
    async fn pages_are_rendered_from_the_in_memory_relay_with_head_and_data() {
        let app = setup("page", BUNDLE);
        note(&app, 1, 1_700_000_000, "hello relay").await;
        let (status, body) = get_page(&app, "/").await;
        assert_eq!(status, 200);
        assert!(body.contains("<p>hello relay</p>"));
        assert!(body.contains("<title>Notes | Test</title>"));
        assert!(body.contains("rel=\"canonical\" href=\"https://t.example/\""));
        assert!(body.contains("\"count\":1"));
        assert!(!body.contains("<title>Old"));
    }

    #[tokio::test]
    async fn the_bundle_decides_the_status_and_error_pages_are_noindex() {
        let app = setup("status", BUNDLE);
        let (status, body) = get_page(&app, "/nope").await;
        assert_eq!(status, 404);
        assert!(body.contains("noindex") && !body.contains("canonical"));
    }

    #[tokio::test]
    async fn a_broken_bundle_serves_the_plain_shell_with_200() {
        let app = setup("broken", "globalThis.ssrRender = () => { throw new Error('x'); };");
        let (status, body) = get_page(&app, "/").await;
        assert_eq!(status, 200);
        assert!(body.contains("<div id=\"root\"></div>"));
    }

    #[tokio::test]
    async fn built_assets_are_cached_immutably_and_pages_are_not() {
        let app = setup("assets", BUNDLE);
        let res = router(app.clone()).oneshot(Request::get("/assets/a.js").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
    }

    #[tokio::test]
    async fn pages_are_cached_once_loaded_and_new_events_show_after_it_goes_stale() {
        let app = setup("cache", BUNDLE);
        app.mirror.mark_loaded();
        note(&app, 1, 10, "first").await;
        assert!(get_page(&app, "/").await.1.contains("first"));
        note(&app, 2, 11, "second").await;
        assert!(!get_page(&app, "/").await.1.contains("second"));
        assert!(get_page(&app, "/?fresh=1").await.1.contains("second"));
    }

    #[tokio::test]
    async fn the_origin_follows_x_forwarded_proto_when_no_public_url_is_set() {
        let mut app = setup("origin", BUNDLE);
        Arc::get_mut(&mut app).unwrap().config.public_url = None;
        let res = router(app).oneshot(Request::get("/sitemap.xml").header("host", "x.example").header("x-forwarded-proto", "https").body(Body::empty()).unwrap()).await.unwrap();
        let body = String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("<loc>https://x.example/sitemap-pages.xml</loc>"));
    }
}
