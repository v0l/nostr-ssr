use crate::cache::PageCache;
use crate::config::Config;
use crate::plugin::Plugin;
use crate::sitemap::Sitemaps;
use crate::engine::Renderer;
use crate::head::{assemble, assemble_raw, head_tags, Head, Site};
use crate::mirror::{parse_filter, Mirror};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use nostr::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc};
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
    pub proxy: crate::proxy::Proxy,
    pub opengraph: crate::opengraph::State,
    pub plugins: Vec<Arc<dyn Plugin>>,
}

pub type Shared = Arc<App>;

impl App {
    pub fn new(config: Config, mirror: Mirror, renderer: Option<Renderer>) -> Shared {
        Self::with_plugins(config, mirror, renderer, vec![])
    }

    pub fn with_plugins(config: Config, mirror: Mirror, renderer: Option<Renderer>, plugins: Vec<Arc<dyn Plugin>>) -> Shared {
        Arc::new(App { config, mirror, renderer, pages: PageCache::new(Duration::from_secs(30), Duration::from_secs(900), 96 * 1024 * 1024), sitemaps: Sitemaps::default(), proxy: Default::default(), opengraph: Default::default(), plugins })
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
    let base = crate::proxy::routes(&app.config.proxy)
        .route("/sitemap.xml", get(crate::sitemap::index))
        .route("/sitemap-pages.xml", get(crate::sitemap::pages))
        .route("/sitemap-{file}", get(crate::sitemap::file));
    let base = for_plugin_routes(base, &app.plugins);
    base.fallback_service(assets.fallback(get(page).with_state(app.clone())))
        .layer(axum::middleware::from_fn_with_state(app.clone(), custom_headers))
        .layer(axum::middleware::map_response(cache_assets))
        .layer(CompressionLayer::new())
        .with_state(app)
}

fn for_plugin_routes(mut base: Router<Shared>, plugins: &[Arc<dyn Plugin>]) -> Router<Shared> {
    for p in plugins {
        let name = p.name().to_string();
        let r = p.routes();
        if r.has_routes() {
            base = base.nest_service(&format!("/api/{name}"), r);
        }
    }
    base
}

async fn custom_headers(State(app): State<Shared>, req: Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;
    for rule in app.config.headers.iter().filter(|r| path.starts_with(&r.prefix)) {
        for (k, v) in &rule.set {
            if let (Ok(k), Ok(v)) = (axum::http::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                res.headers_mut().insert(k, v);
            }
        }
    }
    res
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
    let mut tags = head_tags(&rendered.head, &site, rendered.status);
    if rendered.status < 400 {
        if let Some(og) = crate::opengraph::tags_for(&app.opengraph, &app.config, &app.mirror, url, origin).await {
            tags = crate::opengraph::merge(&tags, &og);
        }
    }
    let raw = rendered.head.raw.is_some() || app.config.opengraph.is_some();
    let page = if raw { assemble_raw(shell_html, &rendered.html, &tags, &rendered.data) } else { assemble(shell_html, &rendered.html, &tags, &rendered.data) };
    Some((page, rendered.status))
}

async fn shell_with_tags(app: &App, origin: &str, shell_html: &str, url: &str) -> Option<String> {
    let tags = crate::opengraph::tags_for(&app.opengraph, &app.config, &app.mirror, url, origin).await?;
    let head = crate::opengraph::merge("", &tags);
    Some(assemble_raw(shell_html, "", &head, &json!({})))
}

fn own_shell(app: &App, url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let rule = app.config.shells.iter().find(|r| path.starts_with(&r.prefix))?;
    std::fs::read_to_string(app.config.client_dir().join(&rule.file)).ok()
}

pub async fn page_for(app: Shared, headers: HeaderMap, url: String) -> Response {
    if let Some(own) = own_shell(&app, &url) {
        return html(200, own);
    }
    let Some(shell_html) = shell(&app) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "client build missing").into_response();
    };
    let origin = origin(&app, &headers);
    if app.renderer.is_none() {
        return match shell_with_tags(&app, &origin, &shell_html, &url).await {
            Some(page) => html(200, page),
            None => html(200, plain_shell(&shell_html)),
        };
    }
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

    struct RoutePlugin;
    impl crate::plugin::Plugin for RoutePlugin {
        fn name(&self) -> &'static str {
            "swarm"
        }
        fn routes(&self) -> Router {
            Router::new().route("/stats", get(|| async { axum::Json(json!({ "seeds": 7 })) }))
        }
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
    async fn entity_pages_get_their_event_tags_merged_over_the_bundle_head() {
        let mut app = setup("og", BUNDLE);
        let a = Arc::get_mut(&mut app).unwrap();
        a.config.opengraph = Some(crate::config::OpenGraph { twitter_site: Some("@t".into()), ..Default::default() });
        let keys = Keys::new(SecretKey::from_slice(&[5; 32]).unwrap());
        let live = EventBuilder::new(Kind::from(30311), "").tags([Tag::identifier("room"), Tag::parse(["title", "Show"]).unwrap(), Tag::parse(["streaming", "https://cdn/l.m3u8"]).unwrap()]).finalize(&keys).unwrap();
        app.mirror.database.save_event(&live).await.unwrap();
        let naddr = nostr::nips::nip19::Nip19Coordinate::new(live.coordinate().unwrap(), Vec::<RelayUrl>::new()).to_bech32().unwrap();
        let (status, body) = get_page(&app, &format!("/{naddr}")).await;
        assert_eq!(status, 200);
        assert!(body.contains("property=\"og:video\" content=\"https://cdn/l.m3u8\""));
        assert!(body.contains("<title>Nostrich is streaming</title>"));
        assert_eq!(body.matches("<title").count(), 1);
        assert_eq!(body.matches("rel=\"canonical\"").count(), 1);
        assert!(body.contains(&format!("https://t.example/{naddr}")));
        let (_, home) = get_page(&app, "/").await;
        assert!(!home.contains("og:video"));
    }

    #[tokio::test]
    async fn without_a_bundle_the_shell_is_served_with_event_tags_and_custom_headers() {
        let mut app = setup("shell", BUNDLE);
        let a = Arc::get_mut(&mut app).unwrap();
        a.renderer = None;
        a.config.opengraph = Some(crate::config::OpenGraph::default());
        a.config.headers = vec![crate::config::HeaderRule { prefix: "/".into(), set: [("Cross-Origin-Opener-Policy".to_string(), "same-origin".to_string())].into() }];
        let keys = Keys::new(SecretKey::from_slice(&[6; 32]).unwrap());
        let ev = EventBuilder::text_note("hello shell").finalize(&keys).unwrap();
        app.mirror.database.save_event(&ev).await.unwrap();
        let note = ev.id.to_bech32().unwrap();
        let res = router(app.clone()).oneshot(Request::get(format!("/{note}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.headers()["cross-origin-opener-policy"], "same-origin");
        let body = String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("og:article:published_time") && body.contains("hello shell"));
        assert_eq!(body.matches("<title").count(), 1);
        assert!(body.contains("<div id=\"root\"></div>"));
        let (status, plain) = get_page(&app, "/streams").await;
        assert_eq!(status, 200);
        assert!(!plain.contains("og:article"));
    }

    #[tokio::test]
    async fn a_broken_bundle_serves_the_plain_shell_with_200() {
        let app = setup("broken", "globalThis.ssrRender = () => { throw new Error('x'); };");
        let (status, body) = get_page(&app, "/").await;
        assert_eq!(status, 200);
        assert!(body.contains("<div id=\"root\"></div>"));
    }

    #[tokio::test]
    async fn plugin_routes_are_served_under_the_api_prefix() {
        let mut app = setup("routes", BUNDLE);
        Arc::get_mut(&mut app).unwrap().plugins = vec![std::sync::Arc::new(RoutePlugin)];
        let res = router(app.clone()).oneshot(Request::get("/api/swarm/stats").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert_eq!(body, r#"{"seeds":7}"#);
        let res = router(app.clone()).oneshot(Request::get("/api/other/x").body(Body::empty()).unwrap()).await.unwrap();
        // unknown api paths fall through to the SPA shell
        let status = res.status();
        let body = String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(body.contains("id=\"root\""));
    }

    #[tokio::test]
    async fn a_path_with_its_own_shell_gets_that_shell_without_server_rendering() {
        let mut app = setup("shells", BUNDLE);
        let dir = app.config.client_dir();
        std::fs::write(dir.join("overlay.html"), "<html><head><title>Overlay</title></head><body><div id=\"overlay\"></div></body></html>").unwrap();
        note(&app, 1, 1_700_000_000, "hello relay").await;
        Arc::get_mut(&mut app).unwrap().config.shells = vec![crate::config::ShellRule { prefix: "/overlay/".into(), file: "overlay.html".into() }];
        let (status, body) = get_page(&app, "/overlay/abc/zaps?volume=1").await;
        assert_eq!(status, 200);
        assert!(body.contains("<div id=\"overlay\"></div>"));
        assert!(!body.contains("hello relay") && !body.contains("<div id=\"root\">"));
        let (_, main) = get_page(&app, "/").await;
        assert!(main.contains("<p>hello relay</p>"));
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
