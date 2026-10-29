use crate::config::{Config, SitemapSource};
use crate::head::escape_html;
use crate::media::Media;
use crate::web::{origin, Shared};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use nostr::nips::nip19::{Nip19Coordinate, Nip19Event, Nip19Profile, ToBech32};
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
    loc: Option<String>,
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
    if let Some(loc) = &e.loc {
        return Some(loc.clone());
    }
    let id = match source.encoding.as_str() {
        "note" => e.id.to_bech32().ok()?,
        "naddr" => Nip19Coordinate::new(Coordinate::new(e.kind, e.pubkey).identifier(e.identifier.as_deref()?), Vec::<RelayUrl>::new()).to_bech32().ok()?,
        "npub" => e.pubkey.to_bech32().ok()?,
        "nprofile" => Nip19Profile::new(e.pubkey, Vec::<RelayUrl>::new()).to_bech32().ok()?,
        _ => Nip19Event::new(e.id).author(e.pubkey).kind(e.kind).to_bech32().ok()?,
    };
    Some(format!("{}{id}", source.prefix))
}

fn entry(ev: &Event) -> Entry {
    Entry { id: ev.id, pubkey: ev.pubkey, kind: ev.kind, created_at: ev.created_at.as_secs(), identifier: ev.tags.identifier().map(|d| d.to_string()), loc: None }
}

fn authors(source: &SitemapSource, events: &[Event]) -> Vec<Entry> {
    let mut latest: std::collections::HashMap<PublicKey, &Event> = std::collections::HashMap::new();
    for ev in events {
        let slot = latest.entry(ev.pubkey).or_insert(ev);
        if ev.created_at > slot.created_at {
            *slot = ev;
        }
    }
    let _ = source;
    latest.into_values().map(entry).collect()
}

fn tags(source: &SitemapSource, events: &[Event]) -> Vec<Entry> {
    let mut stats: std::collections::HashMap<String, (usize, &Event)> = std::collections::HashMap::new();
    for ev in events {
        let mut seen = std::collections::HashSet::new();
        for t in ev.tags.iter().filter(|t| t.kind().to_string() == "t").filter_map(|t| t.content()) {
            let t = t.trim().to_lowercase();
            if t.is_empty() || !seen.insert(t.clone()) {
                continue;
            }
            let slot = stats.entry(t).or_insert((0, ev));
            slot.0 += 1;
            if ev.created_at > slot.1.created_at {
                slot.1 = ev;
            }
        }
    }
    let mut ranked: Vec<(String, usize, &Event)> = stats.into_iter().filter(|(_, (n, _))| *n >= source.min_count).map(|(t, (n, e))| (t, n, e)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .map(|(t, _, ev)| Entry { loc: Some(format!("{}{}", source.prefix, crate::media::url_encode(&t))), ..entry(ev) })
        .collect()
}

fn deduplicated(events: Vec<Event>) -> Vec<Event> {
    let mut by_key: std::collections::HashMap<String, (Media, Event)> = std::collections::HashMap::new();
    let mut loose = Vec::new();
    for ev in events {
        match crate::media::parse(&ev) {
            Some(m) => match m.identifier().map(String::from) {
                Some(key) => {
                    let better = by_key.get(&key).is_none_or(|(cur, _)| (cur.kind_priority(), cur.created_at, cur.id.clone()) < (m.kind_priority(), m.created_at, m.id.clone()));
                    if better {
                        by_key.insert(key, (m, ev));
                    }
                }
                None => {}
            },
            None => loose.push(ev),
        }
    }
    by_key.into_values().map(|(_, e)| e).chain(loose).collect()
}

async fn collect(app: &crate::web::App, source: &SitemapSource) -> SourceSnapshot {
    let mut filter = source.filter.clone();
    if let (Some(max), Some(obj)) = (source.max, filter.as_object_mut()) {
        obj.insert("limit".into(), max.into());
    }
    let mut events = app.mirror.query(&[filter]).await;
    if source.media {
        events = deduplicated(events);
    }
    let mut entries: Vec<Entry> = match source.group.as_str() {
        "author" => authors(source, &events),
        "tag" => tags(source, &events),
        _ => events.iter().map(entry).collect(),
    };
    drop(events);
    if source.encoding == "npub" && source.group.is_empty() {
        entries.sort_by_key(|e| e.pubkey);
        entries.dedup_by_key(|e| e.pubkey);
    }
    if source.group == "tag" {
        return SourceSnapshot { name: source.name.clone(), chunk: chunk_size(&app.config, source), entries };
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

fn xml_text(s: &str) -> String {
    escape_html(&s.chars().filter(|c| (*c as u32) >= 0x20 || matches!(c, '\t' | '\n' | '\r')).collect::<String>()).replace('\'', "&apos;")
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}...", s.chars().take(n.saturating_sub(3)).collect::<String>())
    } else {
        s.to_string()
    }
}

fn is_http(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

fn video_extra(origin: &str, v: &Media, uploader: Option<(&str, String)>) -> Option<String> {
    let thumb = v.thumbnail.as_deref().filter(|t| is_http(t))?;
    if !is_http(&v.url) {
        return None;
    }
    let first = if v.title.is_empty() { &v.content } else { &v.title };
    let title = clip(if first.is_empty() { "Untitled" } else { first }, 100);
    let desc_src = if !v.content.is_empty() { &v.content } else if !v.title.is_empty() { &v.title } else { &title };
    let mut out = String::from("    <video:video>\n");
    out += &format!("      <video:thumbnail_loc>{}</video:thumbnail_loc>\n", xml_text(thumb));
    out += &format!("      <video:title>{}</video:title>\n", xml_text(&title));
    out += &format!("      <video:description>{}</video:description>\n", xml_text(&clip(desc_src, 2048)));
    out += &format!("      <video:content_loc>{}</video:content_loc>\n", xml_text(&v.url));
    if let Some(d) = v.duration.filter(|d| *d >= 1.0) {
        out += &format!("      <video:duration>{}</video:duration>\n", (d.round() as u64).min(28_800));
    }
    out += &format!("      <video:publication_date>{}</video:publication_date>\n", iso(v.created_at as i64));
    for t in v.tags.iter().take(32) {
        out += &format!("      <video:tag>{}</video:tag>\n", xml_text(t));
    }
    if let Some((name, profile)) = uploader {
        out += &format!("      <video:uploader info=\"{}\">{}</video:uploader>\n", xml_text(&format!("{origin}{profile}")), xml_text(&clip(name, 100)));
    }
    out += "    </video:video>\n";
    Some(out)
}

fn image_extra(img: &Media) -> Option<String> {
    let mut urls: Vec<&str> = Vec::new();
    for u in img.metas.iter().filter_map(|m| m.get("url")).map(String::as_str).filter(|u| is_http(u)) {
        if !urls.contains(&u) {
            urls.push(u);
        }
    }
    if urls.is_empty() {
        return None;
    }
    Some(urls.iter().take(1000).map(|u| format!("    <image:image>\n      <image:loc>{}</image:loc>\n    </image:image>\n", xml_text(u))).collect())
}

async fn media_extras(app: &crate::web::App, origin: &str, entries: &[Entry]) -> std::collections::HashMap<EventId, Option<String>> {
    let ids: Vec<EventId> = entries.iter().map(|e| e.id).collect();
    let events = app.mirror.query(&[serde_json::json!({"ids": ids.iter().map(|i| i.to_hex()).collect::<Vec<_>>()})]).await;
    let parsed: Vec<(EventId, Media)> = events.iter().filter_map(|ev| crate::media::parse(ev).map(|m| (ev.id, m))).collect();
    let pubkeys: Vec<String> = parsed.iter().filter(|(_, m)| !m.is_image()).map(|(_, m)| m.pubkey.clone()).collect::<std::collections::HashSet<_>>().into_iter().collect();
    let names = profile_names(app, &pubkeys).await;
    parsed
        .into_iter()
        .filter_map(|(id, m)| {
            let extra = if m.is_image() {
                image_extra(&m)
            } else {
                let uploader = names.get(&m.pubkey).map(|n| (n.as_str(), crate::media::profile_path_hex(&m.pubkey)));
                video_extra(origin, &m, uploader)
            };
            Some((id, extra))
        })
        .collect()
}

async fn profile_names(app: &crate::web::App, pubkeys: &[String]) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    for chunk in pubkeys.chunks(500) {
        let events = app.mirror.query(&[serde_json::json!({"kinds": [0], "authors": chunk})]).await;
        for ev in events.iter() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&ev.content) else { continue };
            if let Some(n) = ["display_name", "name"].iter().filter_map(|k| v.get(k)?.as_str()).find(|s| !s.is_empty()) {
                names.insert(ev.pubkey.to_hex(), n.to_string());
            }
        }
    }
    names
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
    let extras = if source.media { media_extras(&app, &origin, entries).await } else { Default::default() };
    let mut body = String::with_capacity(entries.len() * 160);
    for e in entries {
        let Some(path) = path_for(source, e) else { continue };
        if source.media && !extras.contains_key(&e.id) {
            continue;
        }
        body.push_str(&format!(
            "  <url>\n    <loc>{}</loc>\n    <lastmod>{}</lastmod>\n    <changefreq>{}</changefreq>\n    <priority>{}</priority>\n{}  </url>\n",
            xml_text(&format!("{origin}{path}")),
            iso(e.created_at as i64),
            xml_text(&source.changefreq),
            xml_text(&source.priority),
            extras.get(&e.id).and_then(|x| x.as_deref()).unwrap_or("")
        ));
    }
    xml(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" xmlns:video=\"http://www.google.com/schemas/sitemap-video/1.1\" xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\">\n{body}</urlset>\n"))
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

    #[tokio::test]
    async fn media_sources_add_video_and_image_extensions_and_skip_unplayable_events() {
        let mut videos = source("videos", 21, 100);
        videos.media = true;
        videos.encoding = "nevent".into();
        let mut images = source("images", 20, 100);
        images.media = true;
        let app = app("media", vec![videos, images], "[]");
        app.mirror.database.save_event(&EventBuilder::new(Kind::Metadata, "{\"name\":\"Alice\"}").finalize(&Keys::new(SecretKey::from_slice(&[1; 32]).unwrap())).unwrap()).await.unwrap();
        save(&app, 1, 21, 100, vec![vec!["title", "Clip & more"], vec!["imeta", "url https://cdn/a.mp4", "m video/mp4", "x aa", "image https://cdn/a.jpg", "duration 12"], vec!["t", "demo"]]).await;
        save(&app, 2, 21, 101, vec![]).await;
        save(&app, 3, 20, 102, vec![vec!["imeta", "url https://cdn/i.jpg", "m image/jpeg", "x bb"]]).await;
        let (_, v) = get(&app, "/sitemap-videos-1.xml").await;
        assert!(v.contains("<video:content_loc>https://cdn/a.mp4</video:content_loc>"));
        assert!(v.contains("<video:title>Clip &amp; more</video:title>"));
        assert!(v.contains("<video:uploader info=\"https://t.example/p/nprofile1"));
        assert_eq!(v.matches("<url>").count(), 1);
        let (_, i) = get(&app, "/sitemap-images-1.xml").await;
        assert!(i.contains("<image:loc>https://cdn/i.jpg</image:loc>"));
    }

    #[tokio::test]
    async fn duplicate_media_collapses_to_the_preferred_kind() {
        let mut videos = source("videos", 21, 100);
        videos.filter = json!({"kinds": [21, 34235]});
        videos.media = true;
        let app = app("dupes", vec![videos], "[]");
        let tag = |id: &'static str| vec![vec!["imeta", "url https://cdn/same.mp4", "m video/mp4", id]];
        save(&app, 1, 34235, 300, [tag("x same"), vec![vec!["d", "a"]]].concat()).await;
        let kept = save(&app, 1, 21, 100, tag("x same")).await;
        let (_, body) = get(&app, "/sitemap-videos-1.xml").await;
        assert_eq!(body.matches("<url>").count(), 1);
        assert!(body.contains(&kept.id.to_bech32().unwrap_or_default()) || body.contains("nevent1"));
    }

    #[tokio::test]
    async fn tag_sources_rank_by_count_and_drop_rare_tags() {
        let mut tags = source("tags", 21, 100);
        tags.group = "tag".into();
        tags.prefix = "/t/".into();
        tags.min_count = 2;
        let app = app("tags", vec![tags], "[]");
        save(&app, 1, 21, 100, vec![vec!["t", "Popular"], vec!["t", "rare"]]).await;
        save(&app, 2, 21, 101, vec![vec!["t", "popular"], vec!["t", "a b"]]).await;
        save(&app, 3, 21, 102, vec![vec!["t", "popular"], vec!["t", "a b"]]).await;
        let (_, body) = get(&app, "/sitemap-tags-1.xml").await;
        let popular = body.find("/t/popular").unwrap();
        assert!(popular < body.find("/t/a%20b").unwrap());
        assert!(!body.contains("rare"));
    }

    #[tokio::test]
    async fn author_sources_list_each_author_once_with_their_latest_post_date() {
        let mut authors = source("profiles", 21, 100);
        authors.group = "author".into();
        authors.encoding = "nprofile".into();
        authors.prefix = "/p/".into();
        let app = app("authors", vec![authors], "[]");
        save(&app, 1, 21, 100, vec![]).await;
        save(&app, 1, 21, 200, vec![]).await;
        save(&app, 2, 21, 150, vec![]).await;
        let (_, body) = get(&app, "/sitemap-profiles-1.xml").await;
        assert_eq!(body.matches("/p/nprofile1").count(), 2);
        assert!(body.contains("<lastmod>1970-01-01T00:03:20Z</lastmod>"));
    }
}
