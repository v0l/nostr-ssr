use crate::config::{Config, OpenGraph};
use crate::head::escape_html;
use crate::mirror::{parse_filter, Mirror};
use nostr::nips::nip19::{Nip19, Nip19Coordinate, Nip19Event};
use nostr::prelude::*;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const NIP05_TTL: Duration = Duration::from_secs(24 * 3600);
const NIP05_NEGATIVE_TTL: Duration = Duration::from_secs(600);
const NIP05_TIMEOUT: Duration = Duration::from_secs(5);
const ENTITY_PREFIXES: [&str; 5] = ["npub1", "nprofile1", "naddr1", "nevent1", "note1"];

#[derive(Default)]
pub struct State {
    nip05: Mutex<HashMap<String, (Instant, Option<PublicKey>)>>,
    client: reqwest::Client,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tags {
    pub metas: Vec<(String, String)>,
    pub title: Option<String>,
    pub canonical: Option<String>,
    pub json_ld: Option<String>,
}

pub struct Profile {
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
}

fn avatar(og: &OpenGraph, pubkey: &PublicKey) -> String {
    og.avatar.replace("{pubkey}", &pubkey.to_hex())
}

fn tag_value<'a>(ev: &'a Event, name: &str) -> Option<&'a str> {
    ev.tags.iter().find(|t| t.as_slice().first().map(String::as_str) == Some(name)).and_then(|t| t.as_slice().get(1)).map(String::as_str)
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn last_segment(path: &str) -> Option<&str> {
    path.split(['?', '#']).next()?.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty())
}

pub fn is_entity_path(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or("");
    ENTITY_PREFIXES.iter().any(|p| path.starts_with(&format!("/{p}")) || path.starts_with(&format!("/e/{p}")) || path.starts_with(&format!("/p/{p}")))
}

pub fn is_address_path(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.strip_prefix('/').is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}

pub fn parse_address(name: &str, domain: &str) -> Option<(String, String)> {
    let name = name.to_lowercase();
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')) {
        return None;
    }
    let domain = domain.to_lowercase();
    if !domain.contains('.') || domain.ends_with(".local") || domain.ends_with(".internal") || domain.ends_with(".localhost") || domain.split('.').all(|l| l.parse::<u8>().is_ok()) {
        return None;
    }
    Some((name, domain))
}

async fn lookup(mirror: &Mirror, filter: Value, key: &str) -> Vec<Event> {
    let found = mirror.query(std::slice::from_ref(&filter)).await;
    if !found.is_empty() || !mirror.should_try(key) {
        return found;
    }
    if let Some((f, _)) = parse_filter(&filter) {
        mirror.fetch(vec![f], FETCH_TIMEOUT).await;
    }
    mirror.query(&[filter]).await
}

async fn profile(mirror: &Mirror, pubkey: &PublicKey) -> Option<Profile> {
    let ev = lookup(mirror, json!({"kinds": [0], "authors": [pubkey.to_hex()], "limit": 1}), &format!("og-profile:{pubkey}")).await.into_iter().next()?;
    let v: Value = serde_json::from_str(&ev.content).ok()?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    Some(Profile { name: text("name"), about: text("about"), picture: text("picture") })
}

fn pair(key: &str, value: &str) -> (String, String) {
    (key.to_string(), value.to_string())
}

pub fn profile_tags(og: &OpenGraph, pubkey: &PublicKey, profile: Option<&Profile>) -> Tags {
    let name = profile.and_then(|p| p.name.as_deref()).unwrap_or("Nostrich");
    let title = profile.map(|_| format!("{name}'s Profile")).unwrap_or_default();
    let description = profile.and_then(|p| p.about.as_deref()).map(|a| clip(a, 160)).unwrap_or_default();
    let image = profile.and_then(|p| p.picture.clone()).unwrap_or_else(|| avatar(og, pubkey));
    let json_ld = profile.map(|_| json!({"@context": "https://schema.org", "@type": "Person", "name": title, "description": description, "image": image}).to_string());
    Tags {
        metas: vec![
            pair("og:type", "profile"),
            pair("og:title", &title),
            pair("og:description", &description),
            pair("og:image", &image),
            pair("twitter:card", "summary"),
            pair("twitter:title", &title),
            pair("twitter:description", &description),
            pair("twitter:image", &image),
        ],
        title: Some(title).filter(|t| !t.is_empty()),
        canonical: None,
        json_ld,
    }
}

fn live_tags(og: &OpenGraph, ev: &Event, host_profile: Option<&Profile>, host: &PublicKey) -> Tags {
    let host_name = host_profile.and_then(|p| p.name.as_deref()).unwrap_or("Nostrich");
    let stream = tag_value(ev, "streaming").or_else(|| tag_value(ev, "recording")).unwrap_or("");
    let image = tag_value(ev, "image").map(str::to_string).or_else(|| host_profile.and_then(|p| p.picture.clone())).unwrap_or_else(|| avatar(og, host));
    let title = tag_value(ev, "title").unwrap_or("");
    let heading = format!("{host_name} is streaming");
    let mut metas = vec![
        pair("og:type", "video.other"),
        pair("og:title", &heading),
        pair("og:description", title),
        pair("og:image", &image),
        pair("og:video", stream),
        pair("og:video:secure_url", stream),
        pair("og:video:type", "application/vnd.apple.mpegurl"),
        pair("twitter:card", "player"),
        pair("twitter:title", &heading),
        pair("twitter:description", title),
        pair("twitter:image", &image),
    ];
    if let Some(site) = &og.twitter_site {
        metas.push(pair("twitter:site", site));
    }
    if let Some(player) = &og.player {
        let nevent = Nip19Event::new(ev.id).author(ev.pubkey).kind(ev.kind).to_bech32().unwrap_or_default();
        metas.push(pair("twitter:player", &player.replace("{nevent}", &nevent)));
        metas.push(pair("twitter:player:width", "640"));
        metas.push(pair("twitter:player:height", "480"));
        metas.push(pair("twitter:text:player_height", "480"));
    }
    let json_ld = json!({"@context": "https://schema.org", "@type": "BroadcastEvent", "name": title, "description": title, "streamURL": stream, "performer": {"@type": "Person", "name": host_name}}).to_string();
    Tags { metas, title: Some(heading), canonical: None, json_ld: Some(json_ld) }
}

fn clip_tags(og: &OpenGraph, ev: &Event, author: Option<&Profile>) -> Tags {
    let name = author.and_then(|p| p.name.as_deref()).unwrap_or("Nostrich");
    let stream = tag_value(ev, "r").unwrap_or("");
    let image = tag_value(ev, "image").map(str::to_string).or_else(|| author.and_then(|p| p.picture.clone())).unwrap_or_else(|| avatar(og, &ev.pubkey));
    let title = tag_value(ev, "title").unwrap_or("");
    let heading = format!("{name} created a clip");
    let metas = vec![
        pair("og:type", "video.other"),
        pair("og:title", &heading),
        pair("og:description", title),
        pair("og:image", &image),
        pair("og:video", stream),
        pair("og:video:secure_url", stream),
        pair("og:video:type", "video/mp4"),
        pair("twitter:card", "summary_large_image"),
        pair("twitter:title", &heading),
        pair("twitter:description", title),
        pair("twitter:image", &image),
    ];
    let json_ld = json!({"@context": "https://schema.org", "@type": "Clip", "name": title, "description": title, "video": {"@type": "VideoObject", "contentURL": stream}, "actor": {"@type": "Person", "name": name}}).to_string();
    Tags { metas, title: Some(heading), canonical: None, json_ld: Some(json_ld) }
}

fn post_tags(og: &OpenGraph, ev: &Event, author: Option<&Profile>) -> Tags {
    let name = author.and_then(|p| p.name.as_deref()).unwrap_or("Nostrich");
    let heading = format!("{name}: {}", clip(&ev.content, 160));
    let image = author.and_then(|p| p.picture.clone()).unwrap_or_else(|| avatar(og, &ev.pubkey));
    let profile_name = author.and_then(|p| p.name.as_deref()).unwrap_or("");
    let published = crate::sitemap::iso(ev.created_at.as_secs() as i64);
    let metas = vec![
        pair("og:type", "article"),
        pair("og:title", &heading),
        pair("og:description", ""),
        pair("og:image", &image),
        pair("og:article:published_time", &published),
        pair("og:article:author:username", profile_name),
        pair("twitter:card", "summary"),
        pair("twitter:title", &heading),
        pair("twitter:description", ""),
        pair("twitter:image", &image),
    ];
    let json_ld = json!({"@context": "https://schema.org", "@type": "Comment", "name": profile_name, "description": ev.content.replace('\n', " "), "datePublished": published, "author": {"@type": "Person", "name": profile_name}}).to_string();
    Tags { metas, title: Some(heading), canonical: None, json_ld: Some(json_ld) }
}

pub async fn event_tags(og: &OpenGraph, mirror: &Mirror, ev: &Event, origin: &str) -> Tags {
    let mut tags = match ev.kind.as_u16() {
        30311 => {
            let host = ev
                .tags
                .iter()
                .find(|t| {
                    let s = t.as_slice();
                    s.first().map(String::as_str) == Some("p") && s.len() > 3 && s[3].eq_ignore_ascii_case("host")
                })
                .and_then(|t| t.as_slice().get(1).and_then(|h| PublicKey::from_hex(h).ok()))
                .unwrap_or(ev.pubkey);
            let p = profile(mirror, &host).await;
            live_tags(og, ev, p.as_ref(), &host)
        }
        1313 => {
            let p = profile(mirror, &ev.pubkey).await;
            clip_tags(og, ev, p.as_ref())
        }
        _ => {
            let p = profile(mirror, &ev.pubkey).await;
            post_tags(og, ev, p.as_ref())
        }
    };
    let link = match ev.coordinate() {
        Some(c) => Nip19Coordinate::new(c, Vec::<RelayUrl>::new()).to_bech32().ok(),
        None => Nip19Event::from(ev).to_bech32().ok(),
    };
    tags.canonical = link.map(|l| format!("{}/{l}", origin.trim_end_matches('/')));
    tags
}

impl State {
    async fn resolve_address(&self, og: &OpenGraph, name: &str, domain: &str, host: &str) -> Option<PublicKey> {
        let (name, domain) = parse_address(name, domain)?;
        let key = format!("{name}@{domain}");
        if let Some((at, found)) = self.nip05.lock().unwrap().get(&key) {
            if at.elapsed() < if found.is_some() { NIP05_TTL } else { NIP05_NEGATIVE_TTL } {
                return *found;
            }
        }
        let url = match &og.nip05_url {
            Some(t) => t.replace("{name}", &name),
            None => format!("https://{domain}/.well-known/nostr.json?name={name}"),
        };
        let mut req = self.client.get(url).timeout(NIP05_TIMEOUT);
        for (k, v) in &og.nip05_headers {
            req = req.header(k.as_str(), v.replace("{host}", host));
        }
        let found = async {
            let body: Value = req.send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
            PublicKey::from_hex(body.get("names")?.get(&name)?.as_str()?).ok()
        }
        .await;
        self.nip05.lock().unwrap().insert(key, (Instant::now(), found));
        found
    }
}

pub async fn tags_for(state: &State, config: &Config, mirror: &Mirror, path: &str, origin: &str) -> Option<Tags> {
    let og = config.opengraph.as_ref()?;
    let id = last_segment(path)?;
    let bare = path.split(['?', '#']).next().unwrap_or("");
    let address = !is_entity_path(path) && is_address_path(path) && !og.skip_paths.iter().chain(config.static_paths.iter()).any(|p| p == bare);
    if !is_entity_path(path) && !address {
        return None;
    }
    let target = if address {
        let host = origin.split("://").nth(1).unwrap_or(origin).split(':').next().unwrap_or("");
        let domain = og.nip05_domain.clone().unwrap_or_else(|| host.to_string());
        Nip19::Pubkey(state.resolve_address(og, id, &domain, host).await?)
    } else {
        Nip19::from_bech32(id).ok()?
    };
    match target {
        Nip19::Pubkey(pk) => Some(profile_tags(og, &pk, profile(mirror, &pk).await.as_ref())),
        Nip19::Profile(p) => Some(profile_tags(og, &p.public_key, profile(mirror, &p.public_key).await.as_ref())),
        Nip19::EventId(_) | Nip19::Event(_) | Nip19::Coordinate(_) => {
            let (filter, key) = match &target {
                Nip19::EventId(i) => (json!({"ids": [i.to_hex()]}), i.to_hex()),
                Nip19::Event(e) => (json!({"ids": [e.event_id.to_hex()]}), e.event_id.to_hex()),
                Nip19::Coordinate(c) => (json!({"kinds": [c.kind.as_u16()], "authors": [c.public_key.to_hex()], "#d": [c.identifier]}), format!("{}:{}:{}", c.kind.as_u16(), c.public_key, c.identifier)),
                _ => return None,
            };
            let ev = lookup(mirror, filter, &format!("og-event:{key}")).await.into_iter().next()?;
            Some(event_tags(og, mirror, &ev, origin).await)
        }
        _ => None,
    }
}

fn strip(head: &str, pattern: &str) -> String {
    Regex::new(pattern).map(|re| re.replace_all(head, "").into_owned()).unwrap_or_else(|_| head.to_string())
}

pub fn merge(head: &str, tags: &Tags) -> String {
    let mut out = head.to_string();
    for (key, _) in &tags.metas {
        out = strip(&out, &format!(r#"<meta\s[^>]*?(?:property|name)="{}"[^>]*?/?>"#, regex::escape(key)));
    }
    if tags.title.is_some() {
        out = strip(&out, r"<title[^>]*>[^<]*</title>");
    }
    if tags.canonical.is_some() {
        out = strip(&out, r#"<link\s[^>]*?rel="canonical"[^>]*?/?>"#);
    }
    if tags.json_ld.is_some() {
        out = strip(&out, r#"(?s)<script\s[^>]*?type="application/ld\+json"[^>]*>.*?</script>"#);
    }
    let mut added = Vec::new();
    for (key, value) in &tags.metas {
        added.push(format!("<meta property=\"{}\" content=\"{}\" />", escape_html(key), escape_html(value)));
    }
    if let Some(t) = &tags.title {
        added.push(format!("<title>{}</title>", escape_html(t)));
    }
    if let Some(c) = &tags.canonical {
        added.push(format!("<link rel=\"canonical\" href=\"{}\" />", escape_html(c)));
    }
    if let Some(ld) = &tags.json_ld {
        added.push(format!("<script type=\"application/ld+json\">{}</script>", ld.replace("</", "<\\/")));
    }
    format!("{}\n    {}", out.trim_end(), added.join("\n    "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::event::FinalizeEvent;
    use nostr_database::NostrDatabase;

    fn keys(n: u8) -> Keys {
        Keys::new(SecretKey::from_slice(&[n; 32]).unwrap())
    }

    fn og() -> OpenGraph {
        OpenGraph { twitter_site: Some("@zap_stream".into()), player: Some("https://zap.stream/embed/{nevent}".into()), ..OpenGraph::default() }
    }

    async fn save(m: &Mirror, ev: &Event) {
        m.database.save_event(ev).await.unwrap();
    }

    fn profile_event(n: u8, json: &str) -> Event {
        EventBuilder::new(Kind::Metadata, json).finalize(&keys(n)).unwrap()
    }

    fn event(n: u8, kind: u16, content: &str, tags: Vec<Vec<&str>>) -> Event {
        EventBuilder::new(Kind::from(kind), content).tags(tags.into_iter().map(|t| Tag::parse(t).unwrap())).custom_created_at(Timestamp::from_secs(1_700_000_000)).finalize(&keys(n)).unwrap()
    }

    fn meta<'a>(t: &'a Tags, key: &str) -> Option<&'a str> {
        t.metas.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[tokio::test]
    async fn a_live_stream_is_tagged_as_a_playable_video_with_the_host_name_stream_url_and_player() {
        let m = Mirror::new(0, None, &[]);
        let host = keys(2);
        save(&m, &profile_event(2, r#"{"name":"alice","picture":"https://x/a.png"}"#)).await;
        let ev = event(1, 30311, "", vec![vec!["d", "room"], vec!["title", "Late show"], vec!["streaming", "https://cdn/live.m3u8"], vec!["image", "https://x/cover.png"], vec!["p", &host.public_key().to_hex(), "", "Host"]]);
        let t = event_tags(&og(), &m, &ev, "https://zap.stream").await;
        assert_eq!(meta(&t, "og:type"), Some("video.other"));
        assert_eq!(meta(&t, "og:title"), Some("alice is streaming"));
        assert_eq!(meta(&t, "og:description"), Some("Late show"));
        assert_eq!(meta(&t, "og:image"), Some("https://x/cover.png"));
        assert_eq!(meta(&t, "og:video"), Some("https://cdn/live.m3u8"));
        assert_eq!(meta(&t, "og:video:secure_url"), Some("https://cdn/live.m3u8"));
        assert_eq!(meta(&t, "og:video:type"), Some("application/vnd.apple.mpegurl"));
        assert_eq!(meta(&t, "twitter:card"), Some("player"));
        assert_eq!(meta(&t, "twitter:site"), Some("@zap_stream"));
        assert!(meta(&t, "twitter:player").unwrap().starts_with("https://zap.stream/embed/nevent1"));
        assert_eq!(meta(&t, "twitter:player:width"), Some("640"));
        assert!(t.json_ld.as_deref().unwrap().contains("\"BroadcastEvent\""));
        assert!(t.canonical.as_deref().unwrap().starts_with("https://zap.stream/naddr1"));
    }

    #[tokio::test]
    async fn an_ended_stream_falls_back_to_the_recording_and_the_host_avatar_when_there_is_no_image() {
        let m = Mirror::new(0, None, &[]);
        let ev = event(1, 30311, "", vec![vec!["d", "room"], vec!["recording", "https://cdn/rec.m3u8"]]);
        let t = event_tags(&og(), &m, &ev, "https://zap.stream").await;
        assert_eq!(meta(&t, "og:video"), Some("https://cdn/rec.m3u8"));
        assert_eq!(meta(&t, "og:title"), Some("Nostrich is streaming"));
        assert!(meta(&t, "og:image").unwrap().contains(&keys(1).public_key().to_hex()));
    }

    #[tokio::test]
    async fn a_stream_clip_is_tagged_as_an_mp4_video_from_its_r_tag() {
        let m = Mirror::new(0, None, &[]);
        save(&m, &profile_event(1, r#"{"name":"bob"}"#)).await;
        let ev = event(1, 1313, "", vec![vec!["r", "https://cdn/clip.mp4"], vec!["title", "Big moment"]]);
        let t = event_tags(&og(), &m, &ev, "https://zap.stream").await;
        assert_eq!(meta(&t, "og:title"), Some("bob created a clip"));
        assert_eq!(meta(&t, "og:video"), Some("https://cdn/clip.mp4"));
        assert_eq!(meta(&t, "og:video:type"), Some("video/mp4"));
        assert_eq!(meta(&t, "twitter:card"), Some("summary_large_image"));
        assert!(t.json_ld.as_deref().unwrap().contains("\"Clip\""));
        assert!(t.canonical.as_deref().unwrap().starts_with("https://zap.stream/nevent1"));
    }

    #[tokio::test]
    async fn other_kinds_are_tagged_as_articles_with_the_author_and_trimmed_content() {
        let m = Mirror::new(0, None, &[]);
        save(&m, &profile_event(1, r#"{"name":"carol","picture":"https://x/c.png"}"#)).await;
        let ev = event(1, 1, &"x".repeat(300), vec![]);
        let t = event_tags(&og(), &m, &ev, "https://zap.stream").await;
        assert_eq!(meta(&t, "og:type"), Some("article"));
        assert_eq!(meta(&t, "og:title").unwrap().chars().count(), "carol: ".len() + 160);
        assert_eq!(meta(&t, "og:image"), Some("https://x/c.png"));
        assert_eq!(meta(&t, "og:article:author:username"), Some("carol"));
        assert!(meta(&t, "og:article:published_time").unwrap().starts_with("2023-11-14T"));
    }

    #[tokio::test]
    async fn a_profile_link_is_tagged_with_the_profile_name_about_and_picture() {
        let m = Mirror::new(0, None, &[]);
        let pk = keys(3).public_key();
        save(&m, &profile_event(3, r#"{"name":"dave","about":"hello there","picture":"https://x/d.png"}"#)).await;
        let state = State::default();
        let config = Config { opengraph: Some(og()), ..Config::default() };
        let t = tags_for(&state, &config, &m, &format!("/{}", pk.to_bech32().unwrap()), "https://zap.stream").await.unwrap();
        assert_eq!(meta(&t, "og:type"), Some("profile"));
        assert_eq!(meta(&t, "og:title"), Some("dave's Profile"));
        assert_eq!(meta(&t, "og:description"), Some("hello there"));
        assert_eq!(meta(&t, "og:image"), Some("https://x/d.png"));
        assert!(t.json_ld.as_deref().unwrap().contains("\"Person\""));
    }

    #[tokio::test]
    async fn event_links_look_the_event_up_in_the_mirror_by_id_and_by_coordinate() {
        let m = Mirror::new(0, None, &[]);
        let live = event(1, 30311, "", vec![vec!["d", "room"], vec!["title", "T"], vec!["streaming", "https://cdn/l.m3u8"]]);
        save(&m, &live).await;
        let state = State::default();
        let config = Config { opengraph: Some(og()), ..Config::default() };
        let naddr = Nip19Coordinate::new(live.coordinate().unwrap(), Vec::<RelayUrl>::new()).to_bech32().unwrap();
        let t = tags_for(&state, &config, &m, &format!("/{naddr}"), "https://zap.stream").await.unwrap();
        assert_eq!(meta(&t, "og:video"), Some("https://cdn/l.m3u8"));
        let note = Nip19Event::new(live.id).to_bech32().unwrap();
        let t = tags_for(&state, &config, &m, &format!("/e/{note}"), "https://zap.stream").await.unwrap();
        assert_eq!(meta(&t, "og:video"), Some("https://cdn/l.m3u8"));
    }

    #[tokio::test]
    async fn unknown_events_unparseable_ids_and_unconfigured_sites_get_no_tags() {
        let m = Mirror::new(0, None, &[]);
        let state = State::default();
        let config = Config { opengraph: Some(og()), ..Config::default() };
        let ghost = Nip19Event::new(event(9, 1, "x", vec![]).id).to_bech32().unwrap();
        assert!(tags_for(&state, &config, &m, &format!("/{ghost}"), "https://zap.stream").await.is_none());
        assert!(tags_for(&state, &config, &m, "/note1garbage", "https://zap.stream").await.is_none());
        assert!(tags_for(&state, &config, &m, "/", "https://zap.stream").await.is_none());
        assert!(tags_for(&state, &config, &m, "/streams", "https://zap.stream").await.is_none());
        assert!(tags_for(&state, &Config::default(), &m, &format!("/{ghost}"), "https://zap.stream").await.is_none());
    }

    #[tokio::test]
    async fn a_bare_nostr_address_path_resolves_through_nip05_and_is_cached() {
        use axum::routing::get;
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let pk = keys(4).public_key().to_hex();
        let pk2 = pk.clone();
        let app = axum::Router::new().route("/nostr.json", get(move |q: axum::extract::Query<HashMap<String, String>>| {
            let (h, pk) = (h.clone(), pk2.clone());
            async move {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                axum::Json(if q.get("name").map(String::as_str) == Some("erin") { json!({"names": {"erin": pk}}) } else { json!({"names": {}}) })
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let m = Mirror::new(0, None, &[]);
        save(&m, &profile_event(4, r#"{"name":"erin"}"#)).await;
        let state = State::default();
        let mut o = og();
        o.nip05_url = Some(format!("http://{addr}/nostr.json?name={{name}}"));
        o.nip05_domain = Some("zap.stream".into());
        let config = Config { opengraph: Some(o), static_paths: vec!["/".into(), "/streams".into()], ..Config::default() };
        let t = tags_for(&state, &config, &m, "/erin", "https://zap.stream").await.unwrap();
        assert_eq!(meta(&t, "og:title"), Some("erin's Profile"));
        assert!(tags_for(&state, &config, &m, "/erin", "https://zap.stream").await.is_some());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(tags_for(&state, &config, &m, "/nobody", "https://zap.stream").await.is_none());
        assert!(tags_for(&state, &config, &m, "/streams", "https://zap.stream").await.is_none());
    }

    #[test]
    fn nostr_address_validation_rejects_private_domains_and_odd_names() {
        assert!(parse_address("alice", "example.com").is_some());
        assert!(parse_address("Alice", "Example.COM").is_some());
        assert!(parse_address("a b", "example.com").is_none());
        assert!(parse_address("a", "localhost").is_none());
        assert!(parse_address("a", "nas.local").is_none());
        assert!(parse_address("a", "192.168.1.1").is_none());
    }

    #[test]
    fn path_classification_matches_the_old_pages_middleware() {
        assert!(is_entity_path("/npub1abc") && is_entity_path("/e/nevent1abc") && is_entity_path("/p/nprofile1abc") && is_entity_path("/naddr1abc?x=1"));
        assert!(!is_entity_path("/streams") && !is_entity_path("/x/npub1abc"));
        assert!(is_address_path("/alice_1") && !is_address_path("/") && !is_address_path("/a/b") && !is_address_path("/a-b"));
        assert_eq!(last_segment("/e/nevent1abc?x=1"), Some("nevent1abc"));
    }

    #[test]
    fn merging_replaces_matching_head_tags_once_and_keeps_the_rest_escaped() {
        let head = "<title>Home - zap.stream</title>\n    <meta property=\"og:title\" content=\"old\" />\n    <meta name=\"description\" content=\"keep\" />\n    <link rel=\"canonical\" href=\"https://zap.stream\" />";
        let tags = Tags { metas: vec![pair("og:title", "A & B \"q\"")], title: Some("A & B".into()), canonical: Some("https://zap.stream/naddr1x".into()), json_ld: Some("{\"a\":\"</script>\"}".into()) };
        let out = merge(head, &tags);
        assert_eq!(out.matches("og:title").count(), 1);
        assert_eq!(out.matches("<title>").count(), 1);
        assert!(out.contains("<title>A &amp; B</title>"));
        assert!(out.contains("content=\"A &amp; B &quot;q&quot;\""));
        assert_eq!(out.matches("rel=\"canonical\"").count(), 1);
        assert!(out.contains("naddr1x") && out.contains("name=\"description\" content=\"keep\""));
        assert_eq!(out.matches("</script>").count(), 1);
    }
}
