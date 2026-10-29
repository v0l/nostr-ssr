use nostr::prelude::*;
use std::collections::HashMap;

pub const VIDEO_KIND: u16 = 21;
pub const SHORTS_KIND: u16 = 22;
pub const OLD_VIDEO_KIND: u16 = 34235;
pub const OLD_SHORTS_KIND: u16 = 34236;
pub const IMAGE_KIND: u16 = 20;
pub const MEDIA_KINDS: [u16; 5] = [IMAGE_KIND, VIDEO_KIND, SHORTS_KIND, OLD_VIDEO_KIND, OLD_SHORTS_KIND];

pub type Meta = HashMap<String, String>;

#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    pub id: String,
    pub pubkey: String,
    pub kind: u16,
    pub created_at: u64,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub metas: Vec<Meta>,
    pub url: String,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub path: String,
    pub key: String,
    pub hash: Option<String>,
    pub hosts: Vec<String>,
}

impl Media {
    pub fn is_short(&self) -> bool {
        self.kind == SHORTS_KIND || self.kind == OLD_SHORTS_KIND
    }

    pub fn is_image(&self) -> bool {
        self.kind == IMAGE_KIND
    }

    pub fn identifier(&self) -> Option<&str> {
        self.metas.iter().find_map(|m| m.get("x")).or_else(|| self.metas.iter().find_map(|m| m.get("url"))).map(String::as_str)
    }

    pub fn kind_priority(&self) -> u8 {
        match self.kind {
            IMAGE_KIND => 5,
            VIDEO_KIND => 4,
            SHORTS_KIND => 3,
            OLD_VIDEO_KIND => 2,
            _ => 1,
        }
    }
}

fn parse_imeta(tag: &[String]) -> Meta {
    let mut meta = Meta::new();
    for pair in tag.iter().skip(1) {
        if let Some((k, v)) = pair.split_once(' ') {
            if k == "fallback" {
                let all = meta.entry("fallback".to_string()).or_default();
                if !all.is_empty() {
                    all.push('\n');
                }
                all.push_str(v);
                continue;
            }
            meta.insert(k.to_string(), v.to_string());
        }
    }
    meta
}

pub fn origin_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").map(|r| ("https://", r)).or_else(|| url.strip_prefix("http://").map(|r| ("http://", r)))?;
    let host = rest.1.split(['/', '?', '#']).next().filter(|h| !h.is_empty())?;
    Some(format!("{}{}", rest.0, host))
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn extension_of(url: &str) -> Option<&str> {
    let path = url.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    let (_, ext) = last.rsplit_once('.')?;
    (!ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(ext)
}

pub fn blossom_url(origin: &str, sha256: &str, ext: Option<&str>) -> String {
    match ext {
        Some(e) => format!("{origin}/{sha256}.{e}"),
        None => format!("{origin}/{sha256}"),
    }
}

fn hosts_for(primary: Option<&Meta>, url: &str, key_hash: Option<&str>) -> Vec<String> {
    let (Some(meta), Some(hash)) = (primary, key_hash) else { return Vec::new() };
    let mut origins: Vec<String> = Vec::new();
    if let Some(o) = meta.get("service").and_then(|s| origin_of(s)) {
        origins.push(o);
    }
    for fb in meta.get("fallback").map(String::as_str).unwrap_or("").lines() {
        if let Some(o) = origin_of(fb) {
            if !origins.contains(&o) {
                origins.push(o);
            }
        }
    }
    let ext = extension_of(url);
    origins.iter().map(|o| blossom_url(o, hash, ext)).filter(|u| u != url).collect()
}

pub fn url_hash(url: &str) -> Option<String> {
    let bytes = url.as_bytes();
    (0..bytes.len().saturating_sub(63)).find(|i| is_sha256(&url[*i..i + 64]) && url.is_char_boundary(*i)).map(|i| url[i..i + 64].to_lowercase())
}

fn width(m: &Meta) -> u32 {
    m.get("dim").and_then(|d| d.split('x').next()).and_then(|w| w.parse().ok()).unwrap_or(0)
}

fn best_url(metas: &[Meta], accept: impl Fn(&Meta) -> bool) -> Option<String> {
    metas.iter().filter(|m| m.contains_key("url") && accept(m)).max_by_key(|m| width(m)).map(|m| m["url"].clone())
}

fn is_hls(m: &Meta) -> bool {
    m.get("m").is_some_and(|m| matches!(m.to_lowercase().as_str(), "application/x-mpegurl" | "application/vnd.apple.mpegurl"))
}

pub fn tag_value<'a>(ev: &'a Event, name: &str) -> Option<&'a str> {
    ev.tags.iter().find(|t| t.as_slice().first().map(String::as_str) == Some(name)).and_then(|t| t.as_slice().get(1)).map(String::as_str)
}

pub fn event_path(ev: &Event) -> String {
    let link = if ev.kind.is_addressable() {
        let d = tag_value(ev, "d").unwrap_or("").to_string();
        Nip19Coordinate::new(Coordinate::new(ev.kind, ev.pubkey).identifier(d), Vec::<RelayUrl>::new()).to_bech32()
    } else {
        Nip19Event::new(ev.id).author(ev.pubkey).kind(ev.kind).to_bech32()
    };
    format!("/e/{}", link.unwrap_or_default())
}

pub fn profile_path(pubkey: &PublicKey) -> String {
    format!("/p/{}", Nip19Profile::new(*pubkey, Vec::<RelayUrl>::new()).to_bech32().unwrap_or_default())
}

pub fn profile_path_hex(pubkey: &str) -> String {
    PublicKey::from_hex(pubkey).map(|p| profile_path(&p)).unwrap_or_else(|_| format!("/p/{pubkey}"))
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn parse(ev: &Event) -> Option<Media> {
    let kind = ev.kind.as_u16();
    if !MEDIA_KINDS.contains(&kind) || ev.created_at.as_secs() == 0 {
        return None;
    }
    let metas: Vec<Meta> = ev.tags.iter().filter(|t| t.kind().to_string() == "imeta").map(|t| parse_imeta(t.as_slice())).collect();
    let image = kind == IMAGE_KIND;
    let url = if image {
        best_url(&metas, |m| m.get("m").is_none_or(|m| m.starts_with("image/")))
    } else {
        best_url(&metas, |m| m.get("m").is_some_and(|m| m.starts_with("video/")))
            .or_else(|| best_url(&metas, is_hls))
            .or_else(|| ev.tags.iter().find(|t| t.kind().to_string() == "url").and_then(|t| t.content()).map(String::from))
    }?;
    let mut tags: Vec<String> = Vec::new();
    for t in ev.tags.iter().filter(|t| t.kind().to_string() == "t") {
        if let Some(v) = t.content() {
            if !tags.iter().any(|x| x == v) {
                tags.push(v.to_string());
            }
        }
    }
    let thumbnail = metas.iter().find_map(|m| m.get("image")).cloned().or_else(|| image.then(|| url.clone()));
    let primary = metas.iter().find(|m| m.get("url") == Some(&url));
    let explicit_hash = primary.and_then(|m| m.get("x")).cloned();
    let key = if image {
        explicit_hash.clone().or_else(|| url_hash(&url)).unwrap_or_else(|| url.clone())
    } else {
        explicit_hash.clone().unwrap_or_else(|| url.clone())
    };
    let hash = explicit_hash.or_else(|| image.then(|| url_hash(&url)).flatten()).filter(|h| is_sha256(h)).map(|h| h.to_lowercase());
    let hosts = hosts_for(primary, &url, hash.as_deref());
    Some(Media {
        id: ev.id.to_hex(),
        pubkey: ev.pubkey.to_hex(),
        kind,
        created_at: ev.created_at.as_secs(),
        title: tag_value(ev, "title").unwrap_or("").to_string(),
        content: ev.content.clone(),
        tags,
        thumbnail,
        duration: metas.iter().find_map(|m| m.get("duration")).and_then(|d| d.parse().ok()),
        metas,
        path: event_path(ev),
        key,
        hash,
        hosts,
        url,
    })
}

pub fn deduplicate(items: Vec<Media>) -> Vec<Media> {
    let mut groups: HashMap<String, Media> = HashMap::new();
    for m in items {
        let Some(key) = m.identifier().map(String::from) else { continue };
        match groups.get(&key) {
            Some(cur) if (cur.kind_priority(), cur.created_at, &cur.id) >= (m.kind_priority(), m.created_at, &m.id) => {}
            _ => {
                groups.insert(key, m);
            }
        }
    }
    let mut out: Vec<Media> = groups.into_values().collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(keys: &Keys, kind: u16, at: u64, x: &str, extra: Vec<Tag>) -> Event {
        let mut tags = vec![
            Tag::parse(["imeta", &format!("url https://cdn/{x}.mp4"), "m video/mp4", &format!("x {x}"), "dim 640x360", "image https://cdn/t.jpg", "duration 12.5"]).unwrap(),
            Tag::parse(["imeta", &format!("url https://cdn/{x}-hd.mp4"), "m video/mp4", "dim 1920x1080"]).unwrap(),
            Tag::parse(["t", "demo"]).unwrap(),
            Tag::parse(["t", "demo"]).unwrap(),
            Tag::parse(["title", "A title"]).unwrap(),
        ];
        tags.extend(extra);
        EventBuilder::new(Kind::from(kind), "body").tags(tags).custom_created_at(Timestamp::from_secs(at)).finalize(keys).unwrap()
    }

    #[test]
    fn picks_the_widest_video_and_reads_metadata() {
        let keys = Keys::generate();
        let m = parse(&video(&keys, 21, 100, "aa", vec![])).unwrap();
        assert_eq!(m.url, "https://cdn/aa-hd.mp4");
        assert_eq!(m.thumbnail.as_deref(), Some("https://cdn/t.jpg"));
        assert_eq!(m.duration, Some(12.5));
        assert_eq!(m.title, "A title");
        assert_eq!(m.tags, ["demo"]);
        assert!(m.path.starts_with("/e/nevent1"));
    }

    #[test]
    fn events_without_a_playable_url_are_rejected() {
        let keys = Keys::generate();
        let ev = EventBuilder::new(Kind::from(21), "").tag(Tag::parse(["t", "demo"]).unwrap()).finalize(&keys).unwrap();
        assert!(parse(&ev).is_none());
        let note = EventBuilder::text_note("hi").finalize(&keys).unwrap();
        assert!(parse(&note).is_none());
    }

    #[test]
    fn legacy_events_are_addressed_with_naddr() {
        let keys = Keys::generate();
        let m = parse(&video(&keys, 34235, 100, "aa", vec![Tag::identifier("slug")])).unwrap();
        assert!(m.path.starts_with("/e/naddr1"));
    }

    #[test]
    fn duplicates_keep_the_preferred_kind_then_newest() {
        let keys = Keys::generate();
        let old = parse(&video(&keys, 34235, 300, "same", vec![Tag::identifier("a")])).unwrap();
        let new = parse(&video(&keys, 21, 100, "same", vec![])).unwrap();
        let other = parse(&video(&keys, 21, 200, "other", vec![])).unwrap();
        let out = deduplicate(vec![old, new.clone(), other.clone()]);
        assert_eq!(out.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), [other.id, new.id]);
    }

    #[test]
    fn image_events_use_the_image_as_thumbnail() {
        let keys = Keys::generate();
        let ev = EventBuilder::new(Kind::from(20), "").tag(Tag::parse(["imeta", "url https://cdn/i.jpg", "m image/jpeg", "dim 10x10"]).unwrap()).finalize(&keys).unwrap();
        let m = parse(&ev).unwrap();
        assert_eq!(m.thumbnail.as_deref(), Some("https://cdn/i.jpg"));
        assert!(m.is_image());
    }
}

#[cfg(test)]
pub mod tests_support {
    use super::*;

    pub fn media(id: &str) -> Media {
        Media {
            id: id.into(),
            pubkey: "p".into(),
            kind: 21,
            created_at: 1,
            title: String::new(),
            content: String::new(),
            tags: vec![],
            metas: vec![],
            url: format!("https://cdn/{id}.mp4"),
            thumbnail: None,
            duration: None,
            path: format!("/e/{id}"),
            key: id.into(),
            hash: None,
            hosts: vec![],
        }
    }
}
