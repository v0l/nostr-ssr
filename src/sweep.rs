use crate::filters::ResultFilter;
use crate::health::{candidate, Health};
use crate::media;
use crate::mirror::{parse_filter, Mirror};
use nostr::prelude::*;
use nostr_database::NostrDatabase;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub struct DeadMedia {
    health: Health,
    kinds: Vec<Kind>,
}

impl DeadMedia {
    pub fn new(health: Health) -> DeadMedia {
        DeadMedia { health, kinds: media::MEDIA_KINDS.map(Kind::from).to_vec() }
    }
}

impl ResultFilter for DeadMedia {
    fn kinds(&self) -> Option<&[Kind]> {
        Some(&self.kinds)
    }

    fn keep(&self, event: &Event) -> bool {
        media::parse(event).is_none_or(|m| !self.health.is_dead(&m.key))
    }
}

pub fn filter(health: Health) -> Arc<dyn ResultFilter> {
    Arc::new(DeadMedia::new(health))
}

async fn author_servers(mirror: &Mirror, pubkeys: &[String]) -> HashMap<String, Vec<String>> {
    let authors: Vec<PublicKey> = pubkeys.iter().filter_map(|p| PublicKey::from_hex(p).ok()).collect();
    let mut out = HashMap::new();
    for chunk in authors.chunks(500) {
        let events = mirror.database.query(Filter::new().kind(Kind::from(10063)).authors(chunk.iter().copied())).await.unwrap_or_default();
        for ev in events.iter() {
            let servers = ev.tags.iter().filter(|t| t.as_slice().first().map(String::as_str) == Some("server")).filter_map(|t| t.as_slice().get(1).cloned()).collect();
            out.insert(ev.pubkey.to_hex(), servers);
        }
    }
    out
}

pub async fn check_media(mirror: &Mirror, health: &Health, filter: &Value) -> usize {
    let Some((filter, _)) = parse_filter(filter) else { return 0 };
    let events = mirror.database.query(filter).await.unwrap_or_default();
    let all = media::deduplicate(events.iter().filter_map(media::parse).collect());
    let mut authors: Vec<String> = all.iter().map(|m| m.pubkey.clone()).collect();
    authors.sort();
    authors.dedup();
    let servers = author_servers(mirror, &authors).await;
    let none = Vec::new();
    let candidates = all.iter().map(|m| candidate(m, servers.get(&m.pubkey).unwrap_or(&none))).collect();
    health.sweep(candidates).await
}

pub async fn run(mirror: Mirror, health: Health, filter: Value, interval: Duration) {
    while !mirror.priority_loaded() {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    loop {
        let checked = check_media(&mirror, &health, &filter).await;
        let dead = health.dead_keys().len();
        tracing::info!(checked, dead, "media health sweep");
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use nostr::event::FinalizeEvent;
    use serde_json::json;

    async fn media_server() -> String {
        let app = Router::new()
            .route("/alive.mp4", get(|| async { ([("content-type", "video/mp4")], "x") }))
            .route("/gone.mp4", get(|| async { StatusCode::NOT_FOUND }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn video(base: &str, name: &str, at: u64) -> Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::from(21), "")
            .tag(Tag::parse(["imeta", &format!("url {base}/{name}.mp4"), "m video/mp4"]).unwrap())
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(&keys)
            .unwrap()
    }

    #[tokio::test]
    async fn a_sweep_marks_gone_media_dead_and_the_filter_then_hides_it_from_queries() {
        let base = media_server().await;
        let health = Health::new(true);
        let mirror = Mirror::new(0, None, &[]).with_filters(vec![filter(health.clone())]);
        let alive = video(&base, "alive", 10);
        let gone = video(&base, "gone", 11);
        mirror.database.save_event(&alive).await.unwrap();
        mirror.database.save_event(&gone).await.unwrap();
        assert_eq!(mirror.query(&[json!({"kinds": [21]})]).await.len(), 2);
        let checked = check_media(&mirror, &health, &json!({"kinds": [21]})).await;
        assert_eq!(checked, 2);
        let shown = mirror.query(&[json!({"kinds": [21], "limit": 5})]).await;
        assert_eq!(shown.iter().map(|e| e.id).collect::<Vec<_>>(), vec![alive.id]);
    }
}
