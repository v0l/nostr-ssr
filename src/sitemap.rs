use crate::config::{Config, SitemapSource};
use crate::head::escape_html;
use crate::web::{origin, Shared};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use nostr::nips::nip19::{Nip19Coordinate, Nip19Event, ToBech32};
use nostr::prelude::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const MAX_URLS_PER_FILE: usize = 50_000;

struct Entry {
    id: EventId,
    pubkey: PublicKey,
    kind: Kind,
    created_at: u64,
    identifier: Option<String>,
}

pub struct SourceSnapshot {
    name: String,
    chunk: usize,
    entries: Vec<Entry>,
}

pub struct Snapshot {
    sources: Vec<SourceSnapshot>,
}

#[derive(Default, Clone)]
pub struct Sitemaps {
    slot: Arc<Mutex<Option<(Instant, Arc<Snapshot>)>>>,
}

fn chunk_size(config: &Config, source: &SitemapSource) -> usize {
    source.chunk.unwrap_or(config.sitemap_chunk).clamp(1, MAX_URLS_PER_FILE)
}

fn path_for(source: &SitemapSource, e: &Entry) -> Option<String> {
    let id = match source.encoding.as_str() {
        "note" => e.id.to_bech32().ok()?,
        "naddr" => Nip19Coordinate::new(Coordinate::new(e.kind, e.pubkey).identifier(e.identifier.as_deref()?), Vec::<RelayUrl>::new()).to_bech32().ok()?,
        "npub" => e.pubkey.to_bech32().ok()?,
        _ => Nip19Event::new(e.id).author(e.pubkey).kind(e.kind).to_bech32().ok()?,
    };
    Some(format!("{}{id}", source.prefix))
}

async fn collect(app: &crate::web::App, source: &SitemapSource) -> SourceSnapshot {
    let mut filter = source.filter.clone();
    if let (Some(max), Some(obj)) = (source.max, filter.as_object_mut()) {
        obj.insert("limit".into(), max.into());
    }
    let events = app.mirror.query(&[filter]).await;
    let mut entries: Vec<Entry> = events
        .iter()
        .map(|ev| Entry { id: ev.id, pubkey: ev.pubkey, kind: ev.kind, created_at: ev.created_at.as_secs(), identifier: ev.tags.identifier().map(|d| d.to_string()) })
        .collect();
    drop(events);
    if source.encoding == "npub" {
        entries.sort_by_key(|e| e.pubkey);
        entries.dedup_by_key(|e| e.pubkey);
    }
    entries.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    SourceSnapshot { name: source.name.clone(), chunk: chunk_size(&app.config, source), entries }
}

async fn snapshot(app: &crate::web::App) -> Arc<Snapshot> {
    let mut slot = app.sitemaps.slot.lock().await;
    let ttl = Duration::from_secs(app.config.sitemap_ttl_secs);
    if let Some((at, s)) = slot.as_ref() {
        if at.elapsed() < ttl {
            return s.clone();
        }
    }
    let mut sources = Vec::new();
    for source in &app.config.sitemap {
        sources.push(collect(app, source).await);
    }
    let built = Arc::new(Snapshot { sources });
    if app.mirror.priority_loaded() {
        *slot = Some((Instant::now(), built.clone()));
    }
    built
}

fn xml(body: String) -> Response {
    ([(header::CONTENT_TYPE, "application/xml; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], body).into_response()
}

pub fn iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub async fn index(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let origin = origin(&app, &headers);
    let snap = snapshot(&app).await;
    let mut body = format!("  <sitemap><loc>{origin}/sitemap-pages.xml</loc></sitemap>\n");
    for source in &snap.sources {
        for (n, chunk) in source.entries.chunks(source.chunk).enumerate() {
            let lastmod = chunk.iter().map(|e| e.created_at).max().map(|t| format!("<lastmod>{}</lastmod>", iso(t as i64))).unwrap_or_default();
            body.push_str(&format!("  <sitemap><loc>{}</loc>{lastmod}</sitemap>\n", escape_html(&format!("{origin}/sitemap-{}-{}.xml", source.name, n + 1))));
        }
    }
    xml(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n{body}</sitemapindex>\n"))
}

pub async fn pages(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let origin = origin(&app, &headers);
    let mut paths = app.config.static_paths.clone();
    if let Some(r) = &app.renderer {
        for function in ["ssrPaths", "ssrCategories"] {
            if let Ok(out) = r.call(function, String::new()).await {
                paths.extend(serde_json::from_str::<Vec<String>>(&out.text).unwrap_or_default());
                break;
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    let body: String = paths
        .iter()
        .map(|p| {
            let (freq, prio) = if p == "/" { ("hourly", "1.0") } else { ("daily", "0.5") };
            format!("  <url><loc>{}</loc><changefreq>{freq}</changefreq><priority>{prio}</priority></url>\n", escape_html(&format!("{origin}{p}")))
        })
        .collect();
    xml(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n{body}</urlset>\n"))
}

pub async fn file(State(app): State<Shared>, headers: HeaderMap, Path(file): Path<String>) -> Response {
    let Some((name, n)) = file.strip_suffix(".xml").and_then(|f| f.rsplit_once('-')).and_then(|(name, n)| Some((name, n.parse::<usize>().ok().filter(|n| *n >= 1)?))) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(source) = app.config.sitemap.iter().find(|s| s.name == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let origin = origin(&app, &headers);
    let snap = snapshot(&app).await;
    let Some(entries) = snap.sources.iter().find(|s| s.name == name).and_then(|s| s.entries.chunks(s.chunk).nth(n - 1)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut body = String::with_capacity(entries.len() * 160);
    for e in entries {
        let Some(path) = path_for(source, e) else { continue };
        body.push_str(&format!(
            "  <url><loc>{}</loc><lastmod>{}</lastmod><changefreq>{}</changefreq><priority>{}</priority></url>\n",
            escape_html(&format!("{origin}{path}")),
            iso(e.created_at as i64),
            escape_html(&source.changefreq),
            escape_html(&source.priority)
        ));
    }
    xml(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n{body}</urlset>\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Renderer;
    use crate::mirror::Mirror;
    use crate::web::{router, App};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use nostr::event::FinalizeEvent;
    use nostr_database::NostrDatabase;
    use serde_json::json;
    use tower::ServiceExt;

    fn app(name: &str, sources: Vec<SitemapSource>, filters: &str) -> Shared {
        let dir = std::env::temp_dir().join(format!("nostr-ssr-sm-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("client")).unwrap();
        std::fs::create_dir_all(dir.join("ssr")).unwrap();
        std::fs::write(dir.join("client/index.html"), "<html><head></head><body><div id=\"root\"></div></body></html>").unwrap();
        std::fs::write(dir.join("ssr/render.js"), "globalThis.ssrRender = () => '{}'; globalThis.ssrPaths = () => '[\"/about\",\"/\"]';").unwrap();
        let config = Config {
            public_url: Some("https://t.example".into()),
            dist: dir,
            sitemap: sources,
            sitemap_ttl_secs: 0,
            filters: serde_json::from_str(filters).unwrap(),
            ..Config::default()
        };
        let mirror = Mirror::from_config(&config).unwrap();
        let renderer = Renderer::start(config.bundle_path(), 1, mirror.clone());
        App::new(config, mirror, Some(renderer))
    }

    fn source(name: &str, kind: u16, chunk: usize) -> SitemapSource {
        SitemapSource { name: name.into(), filter: json!({"kinds": [kind]}), chunk: Some(chunk), encoding: "note".into(), ..Default::default() }
    }

    async fn save(app: &Shared, n: u8, kind: u16, at: u64, tags: Vec<Vec<&str>>) -> Event {
        let keys = Keys::new(SecretKey::from_slice(&[n; 32]).unwrap());
        let ev = EventBuilder::new(Kind::from(kind), "x").tags(tags.into_iter().map(|t| Tag::parse(t).unwrap())).custom_created_at(Timestamp::from_secs(at)).finalize(&keys).unwrap();
        app.mirror.database.save_event(&ev).await.unwrap();
        ev
    }

    async fn get(app: &Shared, uri: &str) -> (u16, String) {
        let res = router(app.clone()).oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        (res.status().as_u16(), String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap())
    }

    #[test]
    fn unix_seconds_format_as_utc_iso() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[tokio::test]
    async fn each_source_gets_its_own_chunked_files_listed_in_the_index_with_lastmod() {
        let app = app("chunks", vec![source("videos", 21, 2), source("notes", 1, 10)], "[]");
        let mut videos = Vec::new();
        for n in 1..=3u8 {
            videos.push(save(&app, n, 21, 1_700_000_000 + n as u64, vec![]).await);
        }
        save(&app, 9, 1, 1_700_000_100, vec![]).await;
        let (_, index) = get(&app, "/sitemap.xml").await;
        assert!(index.contains("sitemap-videos-1.xml") && index.contains("sitemap-videos-2.xml") && !index.contains("sitemap-videos-3.xml"));
        assert!(index.contains("sitemap-notes-1.xml") && index.contains("<lastmod>2023-11-14T22:15:00Z</lastmod>"));
        let (status, first) = get(&app, "/sitemap-videos-1.xml").await;
        assert_eq!(status, 200);
        assert!(first.contains(&videos[0].id.to_bech32().unwrap()) && first.contains(&videos[1].id.to_bech32().unwrap()));
        assert!(get(&app, "/sitemap-videos-2.xml").await.1.contains(&videos[2].id.to_bech32().unwrap()));
        for missing in ["/sitemap-videos-3.xml", "/sitemap-videos-0.xml", "/sitemap-other-1.xml", "/sitemap-videos-x.xml"] {
            assert_eq!(get(&app, missing).await.0, 404, "{missing}");
        }
    }

    #[tokio::test]
    async fn existing_files_do_not_shift_when_newer_events_arrive() {
        let app = app("stable", vec![source("e", 1, 2)], "[]");
        let a = save(&app, 1, 1, 100, vec![]).await;
        let b = save(&app, 2, 1, 200, vec![]).await;
        let before = get(&app, "/sitemap-e-1.xml").await.1;
        save(&app, 3, 1, 300, vec![]).await;
        let after = get(&app, "/sitemap-e-1.xml").await.1;
        assert_eq!(before, after);
        assert!(after.contains(&a.id.to_bech32().unwrap()) && after.contains(&b.id.to_bech32().unwrap()));
    }

    #[tokio::test]
    async fn result_filters_keep_denied_events_out_of_the_sitemap() {
        let app = app("filtered", vec![source("v", 21, 100)], r#"[{"kinds":[21],"deny_tags":{"t":["dead"]}}]"#);
        let keep = save(&app, 1, 21, 100, vec![vec!["t", "ok"]]).await;
        let drop = save(&app, 2, 21, 101, vec![vec!["t", "dead"]]).await;
        let (_, body) = get(&app, "/sitemap-v-1.xml").await;
        assert!(body.contains(&keep.id.to_bech32().unwrap()));
        assert!(!body.contains(&drop.id.to_bech32().unwrap()));
    }

    #[tokio::test]
    async fn max_keeps_only_the_newest_events_and_files_never_exceed_the_protocol_limit() {
        let mut capped = source("c", 1, 10);
        capped.max = Some(2);
        let app = app("max", vec![capped], "[]");
        let old = save(&app, 1, 1, 100, vec![]).await;
        save(&app, 2, 1, 200, vec![]).await;
        save(&app, 3, 1, 300, vec![]).await;
        assert!(!get(&app, "/sitemap-c-1.xml").await.1.contains(&old.id.to_bech32().unwrap()));
        let mut huge = source("h", 1, usize::MAX);
        huge.chunk = Some(usize::MAX);
        assert_eq!(chunk_size(&Config::default(), &huge), MAX_URLS_PER_FILE);
    }

    #[tokio::test]
    async fn addressable_and_profile_sources_use_naddr_and_npub_links_deduplicated() {
        let mut naddr = source("a", 30023, 10);
        naddr.encoding = "naddr".into();
        naddr.prefix = "/a/".into();
        let mut npub = source("p", 1, 10);
        npub.encoding = "npub".into();
        npub.prefix = "/p/".into();
        let app = app("enc", vec![naddr, npub], "[]");
        save(&app, 1, 30023, 100, vec![vec!["d", "slug"]]).await;
        save(&app, 1, 1, 101, vec![]).await;
        save(&app, 1, 1, 102, vec![]).await;
        assert!(get(&app, "/sitemap-a-1.xml").await.1.contains("/a/naddr1"));
        assert_eq!(get(&app, "/sitemap-p-1.xml").await.1.matches("/p/npub1").count(), 1);
    }

    #[tokio::test]
    async fn the_pages_sitemap_merges_static_and_bundle_paths_without_duplicates() {
        let app = app("pages", vec![], "[]");
        let (_, body) = get(&app, "/sitemap-pages.xml").await;
        assert_eq!(body.matches("<loc>https://t.example/</loc>").count(), 1);
        assert!(body.contains("<loc>https://t.example/about</loc>"));
    }
}
