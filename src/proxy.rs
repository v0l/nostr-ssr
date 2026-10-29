use crate::config::ProxyRule;
use crate::web::Shared;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Proxy {
    client: Client,
}

impl Default for Proxy {
    fn default() -> Self {
        Proxy { client: Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("http client") }
    }
}

pub fn routes(rules: &[ProxyRule]) -> Router<Shared> {
    let mut router = Router::new();
    for rule in rules.iter().filter(|r| r.prefix.starts_with('/') && !r.upstream.is_empty()) {
        if rule.prefix.ends_with('/') {
            router = router.route(&format!("{}{{*rest}}", rule.prefix), any(handle));
        } else {
            router = router.route(&rule.prefix, any(handle));
        }
    }
    router
}

fn matching<'a>(rules: &'a [ProxyRule], path: &str) -> Option<(&'a ProxyRule, String)> {
    rules.iter().find_map(|r| {
        let rest = path.strip_prefix(r.prefix.as_str())?;
        let simple = r.prefix.ends_with('/') && !rest.is_empty() && !rest.contains('/') && !rest.contains("..") || !r.prefix.ends_with('/') && rest.is_empty();
        simple.then(|| (r, rest.to_string()))
    })
}

fn is_pubkey(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn resolve(client: &Client, rule: &ProxyRule, name: &str) -> Option<String> {
    let template = rule.resolve_nip05.as_deref()?;
    let url = template.replace("{name}", &urlencode(name));
    let mut req = client.get(url).timeout(RESOLVE_TIMEOUT);
    for (k, v) in &rule.resolve_headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let body: Value = req.send().await.ok()?.json().await.ok()?;
    body.get("names")?.as_object()?.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).and_then(|(_, v)| v.as_str()).map(str::to_string)
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn cors(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn handle(State(app): State<Shared>, method: Method, uri: Uri, _headers: HeaderMap) -> Response {
    if method == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        res.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, HEAD, OPTIONS"));
        res.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
        return cors(res);
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some((rule, rest)) = matching(&app.config.proxy, uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let client = &app.proxy.client;
    let target = if rule.resolve_nip05.is_some() && !rest.is_empty() && !is_pubkey(&rest) {
        resolve(client, rule, &rest).await.unwrap_or(rest)
    } else {
        rest
    };
    let mut url = format!("{}{}", rule.upstream, target);
    if let Some(q) = uri.query() {
        url.push('?');
        url.push_str(q);
    }
    let mut req = client.request(method, url).timeout(Duration::from_secs(rule.timeout_secs.max(1)));
    for (k, v) in &rule.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let upstream = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(path = uri.path(), "proxy failed: {e}");
            return cors(StatusCode::BAD_GATEWAY.into_response());
        }
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out = HeaderMap::new();
    for (k, v) in upstream.headers() {
        if matches!(k.as_str(), "content-length" | "transfer-encoding" | "connection" | "keep-alive") {
            continue;
        }
        if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_str().as_bytes()), HeaderValue::from_bytes(v.as_bytes())) {
            out.append(k, v);
        }
    }
    let body = upstream.bytes().await.unwrap_or_default();
    let mut res = (status, out, body).into_response();
    res = cors(res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::Renderer;
    use crate::mirror::Mirror;
    use crate::web::{router, App};
    use axum::body::{to_bytes, Body};
    use axum::extract::{Path, Query};
    use axum::http::Request;
    use axum::routing::get;
    use axum::Json;
    use serde_json::json;
    use std::collections::HashMap;
    use tower::ServiceExt;

    const PUBKEY: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    async fn upstream() -> String {
        let app = Router::new()
            .route("/lnurlp/{pubkey}", get(|Path(p): Path<String>, headers: HeaderMap| async move {
                let host = headers.get("x-proxy-host").and_then(|h| h.to_str().ok()).unwrap_or("none").to_string();
                ([("content-type", "application/json"), ("x-seen-host", "up")], Json(json!({"pubkey": p, "host": host})))
            }))
            .route("/nostr.json", get(|Query(q): Query<HashMap<String, String>>, headers: HeaderMap| async move {
                let name = q.get("name").cloned().unwrap_or_default();
                let mapped = if name.eq_ignore_ascii_case("Alice") { json!({"Alice": PUBKEY}) } else { json!({}) };
                let tagged = headers.get("x-proxy-host").is_some();
                Json(json!({"names": mapped, "tagged": tagged}))
            }))
            .route("/gone", get(|| async { StatusCode::NOT_FOUND }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn app(rules: Vec<ProxyRule>) -> Shared {
        let dir = std::env::temp_dir().join(format!("nostr-ssr-proxy-{}-{}", std::process::id(), rules.len()));
        std::fs::create_dir_all(dir.join("client")).unwrap();
        std::fs::write(dir.join("client/index.html"), "<html><head></head><body><div id=\"root\"></div></body></html>").unwrap();
        let config = Config { dist: dir, proxy: rules, ..Config::default() };
        let mirror = Mirror::new(0, None, &[]);
        let renderer = Renderer::start(config.bundle_path(), 1, mirror.clone());
        App::new(config, mirror, Some(renderer))
    }

    async fn get_(app: &Shared, uri: &str) -> (u16, HeaderMap, String) {
        let res = router(app.clone()).oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        (status, headers, String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap())
    }

    fn lnurlp(base: &str) -> ProxyRule {
        ProxyRule {
            prefix: "/.well-known/lnurlp/".into(),
            upstream: format!("{base}/lnurlp/"),
            headers: HashMap::from([("X-Proxy-Host".to_string(), "zap.stream".to_string())]),
            resolve_nip05: Some(format!("{base}/nostr.json?name={{name}}")),
            resolve_headers: HashMap::from([("X-Proxy-Host".to_string(), "zap.stream".to_string())]),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_lnurlp_request_for_a_pubkey_is_proxied_with_headers_and_open_cors() {
        let base = upstream().await;
        let app = app(vec![lnurlp(&base)]).await;
        let (status, headers, body) = get_(&app, &format!("/.well-known/lnurlp/{PUBKEY}")).await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["pubkey"], PUBKEY);
        assert_eq!(v["host"], "zap.stream");
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-seen-host"], "up");
    }

    #[tokio::test]
    async fn a_nostr_address_handle_is_resolved_to_its_pubkey_first_and_unknown_handles_pass_through() {
        let base = upstream().await;
        let app = app(vec![lnurlp(&base)]).await;
        let (_, _, body) = get_(&app, "/.well-known/lnurlp/alice").await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["pubkey"], PUBKEY);
        let (_, _, body) = get_(&app, "/.well-known/lnurlp/nobody").await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["pubkey"], "nobody");
    }

    #[tokio::test]
    async fn an_exact_path_rule_forwards_the_query_string_and_upstream_status() {
        let base = upstream().await;
        let rule = ProxyRule {
            prefix: "/.well-known/nostr.json".into(),
            upstream: format!("{base}/nostr.json"),
            headers: HashMap::from([("X-Proxy-Host".to_string(), "snort.social".to_string())]),
            ..Default::default()
        };
        let gone = ProxyRule { prefix: "/.well-known/gone".into(), upstream: format!("{base}/gone"), ..Default::default() };
        let app = app(vec![rule, gone]).await;
        let (status, headers, body) = get_(&app, "/.well-known/nostr.json?name=Alice").await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["names"]["Alice"], PUBKEY);
        assert_eq!(v["tagged"], true);
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert_eq!(get_(&app, "/.well-known/gone").await.0, 404);
    }

    #[tokio::test]
    async fn preflight_requests_are_answered_and_unsafe_methods_and_paths_are_refused() {
        let base = upstream().await;
        let app = app(vec![lnurlp(&base)]).await;
        let res = router(app.clone()).oneshot(Request::options(format!("/.well-known/lnurlp/{PUBKEY}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), 204);
        assert_eq!(res.headers()["access-control-allow-origin"], "*");
        let res = router(app.clone()).oneshot(Request::post(format!("/.well-known/lnurlp/{PUBKEY}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), 405);
        assert_ne!(get_(&app, "/.well-known/lnurlp/a/b").await.1.get("x-seen-host").map(|h| h.to_str().unwrap().to_string()), Some("up".to_string()));
        assert_ne!(get_(&app, "/.well-known/lnurlp/..%2Fsecret").await.1.get("x-seen-host").map(|h| h.to_str().unwrap().to_string()), Some("up".to_string()));
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_a_502_with_cors() {
        let rule = ProxyRule { prefix: "/.well-known/lnurlp/".into(), upstream: "http://127.0.0.1:1/".into(), timeout_secs: 1, ..Default::default() };
        let app = app(vec![rule]).await;
        let (status, headers, _) = get_(&app, &format!("/.well-known/lnurlp/{PUBKEY}")).await;
        assert_eq!(status, 502);
        assert_eq!(headers["access-control-allow-origin"], "*");
    }

    #[test]
    fn rules_match_only_single_segments_under_a_prefix_and_exact_paths() {
        let rules = vec![ProxyRule { prefix: "/a/".into(), upstream: "u".into(), ..Default::default() }, ProxyRule { prefix: "/b".into(), upstream: "u".into(), ..Default::default() }];
        assert_eq!(matching(&rules, "/a/x").map(|m| m.1), Some("x".to_string()));
        assert!(matching(&rules, "/a/x/y").is_none());
        assert!(matching(&rules, "/a/").is_none());
        assert!(matching(&rules, "/b").is_some());
        assert!(matching(&rules, "/b/x").is_none());
    }
}
