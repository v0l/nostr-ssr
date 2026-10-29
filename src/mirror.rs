use crate::config::{Backfill, Config};
use crate::filters::{self, ResultFilter};
use nostr_memory::MemoryDatabase;
use nostr_relay_builder::LocalRelay;
use nostr_sdk::prelude::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PAGE: usize = 500;
const PROFILE_BATCH: usize = 100;
const NEGATIVE_TTL: Duration = Duration::from_secs(600);
const SEARCHED_TAGS: [&str; 8] = ["title", "name", "summary", "description", "alt", "file", "t", "d"];

#[derive(Clone)]
pub struct Mirror {
    pub database: Arc<MemoryDatabase>,
    pub relay: LocalRelay,
    client: Client,
    kinds: Vec<Kind>,
    filters: Arc<Vec<Arc<dyn ResultFilter>>>,
    priority_loaded: Arc<AtomicBool>,
    pending_profiles: Arc<Mutex<HashSet<PublicKey>>>,
    tried: Arc<Mutex<HashMap<String, Instant>>>,
}

pub fn search_tokens(q: &str) -> Vec<String> {
    q.split(|c: char| c == '+' || c.is_whitespace()).map(|t| t.trim_end_matches('*').to_lowercase()).filter(|t| !t.is_empty()).collect()
}

fn search_text(ev: &Event) -> String {
    let mut text = ev.content.to_lowercase();
    for tag in ev.tags.iter() {
        if SEARCHED_TAGS.contains(&tag.kind().to_string().as_str()) {
            if let Some(v) = tag.content() {
                text.push('\n');
                text.push_str(&v.to_lowercase());
            }
        }
    }
    text
}

pub fn parse_filter(raw: &Value) -> Option<(Filter, Option<String>)> {
    let mut obj = raw.as_object()?.clone();
    obj.remove("relays");
    let search = obj.remove("search").and_then(|s| s.as_str().map(str::to_string));
    let filter: Filter = serde_json::from_value(Value::Object(obj)).ok()?;
    Some((filter, search))
}

impl Mirror {
    pub fn new(max_events: usize, relay_port: Option<u16>, kinds: &[u16]) -> Mirror {
        let database = Arc::new(match NonZeroUsize::new(max_events) {
            Some(max) => MemoryDatabase::bounded(max),
            None => MemoryDatabase::unbounded(),
        });
        let mut builder = LocalRelay::builder().database(database.clone());
        if let Some(port) = relay_port {
            builder = builder.port(port);
        }
        let relay = builder.build();
        let client = Client::builder().database(database.clone()).build();
        Mirror {
            database,
            relay,
            client,
            kinds: kinds.iter().copied().map(Kind::from).collect(),
            filters: Default::default(),
            priority_loaded: Default::default(),
            pending_profiles: Default::default(),
            tried: Default::default(),
        }
    }

    pub fn from_config(config: &Config) -> anyhow::Result<Mirror> {
        Ok(Mirror::new(config.max_events, config.relay_port, &config.kinds).with_filters(filters::build(&config.filters)?))
    }

    pub fn with_filters(mut self, filters: Vec<Arc<dyn ResultFilter>>) -> Mirror {
        self.filters = Arc::new(filters);
        self
    }

    pub fn priority_loaded(&self) -> bool {
        self.priority_loaded.load(Ordering::Acquire)
    }

    pub fn mark_loaded(&self) {
        self.priority_loaded.store(true, Ordering::Release);
    }

    pub async fn query(&self, raw: &[Value]) -> Vec<Event> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        self.filters.iter().for_each(|f| f.prepare());
        for (mut filter, search) in raw.iter().filter_map(parse_filter) {
            let limit = filter.limit;
            let wanted: Option<Vec<Kind>> = filter.kinds.as_ref().map(|k| k.iter().copied().collect());
            let post_filtered = filters::may_apply(&self.filters, wanted.as_deref());
            if search.is_some() || post_filtered {
                filter.limit = None;
            }
            let Ok(events) = self.database.query(filter).await else { continue };
            let tokens = search.as_deref().map(search_tokens);
            let mut taken = 0;
            for ev in events.iter() {
                if limit.is_some_and(|l| taken >= l) {
                    break;
                }
                if post_filtered && !filters::keep_all(&self.filters, ev) {
                    continue;
                }
                if let Some(tokens) = &tokens {
                    let text = search_text(ev);
                    if !tokens.iter().all(|t| text.contains(t.as_str())) {
                        continue;
                    }
                }
                if seen.insert(ev.id) {
                    out.push(ev.clone());
                    taken += 1;
                }
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        out
    }

    pub async fn count(&self, filter: &Value) -> usize {
        match parse_filter(filter) {
            Some((f, None)) => self.database.count(f).await.unwrap_or(0),
            _ => 0,
        }
    }

    pub async fn len(&self) -> usize {
        self.database.count(Filter::new()).await.unwrap_or(0)
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    async fn note_author(&self, ev: &Event) {
        if ev.kind == Kind::Metadata {
            return;
        }
        let have = self.database.query(Filter::new().kind(Kind::Metadata).author(ev.pubkey).limit(1)).await.map(|e| !e.is_empty()).unwrap_or(false);
        if !have {
            self.pending_profiles.lock().unwrap().insert(ev.pubkey);
        }
    }

    pub fn want_profiles(&self, pubkeys: impl IntoIterator<Item = PublicKey>) {
        for pk in pubkeys {
            if self.should_try(&format!("profile:{pk}")) {
                self.pending_profiles.lock().unwrap().insert(pk);
            }
        }
    }

    pub fn should_try(&self, key: &str) -> bool {
        let mut tried = self.tried.lock().unwrap();
        tried.retain(|_, at| at.elapsed() < NEGATIVE_TTL);
        if tried.contains_key(key) {
            return false;
        }
        tried.insert(key.to_string(), Instant::now());
        true
    }

    pub async fn fetch(&self, filters: Vec<Filter>, timeout: Duration) -> usize {
        let before = self.len().await;
        let _ = self.client.fetch_events(filters).timeout(timeout).await;
        self.len().await.saturating_sub(before)
    }

    pub async fn follow(&self, urls: &[String], backfills: &[Backfill]) -> anyhow::Result<()> {
        if urls.is_empty() {
            self.mark_loaded();
            return Ok(());
        }
        for url in urls {
            self.client.add_relay(url.as_str()).await?;
        }
        self.client.connect().and_wait(Duration::from_secs(15)).await;
        tokio::spawn(forward_to_relay(self.clone()));
        let mut live = Filter::new().since(Timestamp::now());
        if !self.kinds.is_empty() {
            live = live.kinds(self.kinds.iter().copied());
        }
        self.client.subscribe(vec![live]).await?;
        let me = self.clone();
        tokio::spawn(async move { me.profile_loop().await });
        for b in backfills.iter().filter(|b| b.priority) {
            self.backfill(b).await?;
        }
        self.mark_loaded();
        for b in backfills.iter().filter(|b| !b.priority) {
            self.backfill(b).await?;
        }
        Ok(())
    }

    async fn backfill(&self, spec: &Backfill) -> anyhow::Result<()> {
        let Some((base, _)) = parse_filter(&spec.filter) else {
            anyhow::bail!("bad backfill filter {}", spec.filter);
        };
        let mut until: Option<Timestamp> = None;
        let mut fetched = 0;
        while fetched < spec.max {
            let mut filter = base.clone().limit(PAGE);
            if let Some(u) = until {
                filter = filter.until(u);
            }
            let events = self.client.fetch_events(filter).timeout(Duration::from_secs(30)).await?;
            let mut oldest = Timestamp::max();
            for ev in events.iter() {
                oldest = oldest.min(ev.created_at);
            }
            if events.len() < PAGE {
                break;
            }
            fetched += events.len();
            until = Some(if until == Some(oldest) { oldest - 1 } else { oldest });
        }
        Ok(())
    }

    async fn profile_loop(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            loop {
                let batch: Vec<PublicKey> = {
                    let mut pending = self.pending_profiles.lock().unwrap();
                    let take: Vec<PublicKey> = pending.iter().take(PROFILE_BATCH).copied().collect();
                    for pk in &take {
                        pending.remove(pk);
                    }
                    take
                };
                if batch.is_empty() {
                    break;
                }
                self.fetch(vec![Filter::new().kind(Kind::Metadata).authors(batch)], Duration::from_secs(8)).await;
            }
        }
    }
}

async fn forward_to_relay(mirror: Mirror) {
    let mut notifications = mirror.client.notifications();
    while let Some(n) = notifications.next().await {
        if let ClientNotification::Event { event, .. } = n {
            mirror.relay.notify_event((*event).clone());
            mirror.note_author(&event).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_relay_builder::MockRelay;
    use serde_json::json;

    pub fn keys(n: u8) -> Keys {
        Keys::new(SecretKey::from_slice(&[n.max(1); 32]).unwrap())
    }

    fn note(n: u8, at: u64, title: &str, tags: Vec<Vec<&str>>) -> Event {
        let mut all = vec![vec!["title", title]];
        all.extend(tags);
        EventBuilder::new(Kind::from(2003), "desc").tags(all.into_iter().map(|t| Tag::parse(t).unwrap())).custom_created_at(Timestamp::from_secs(at)).finalize(&keys(n)).unwrap()
    }

    async fn filled(events: &[Event]) -> Mirror {
        let m = Mirror::new(0, None, &[]);
        for ev in events {
            m.database.save_event(ev).await.unwrap();
        }
        m
    }

    fn backfill(filter: Value, priority: bool) -> Backfill {
        Backfill { filter, max: 5000, priority }
    }

    #[tokio::test]
    async fn queries_filter_by_tag_author_and_limit_newest_first() {
        let m = filled(&[note(1, 10, "One", vec![vec!["t", "movie"]]), note(1, 11, "Two", vec![vec!["t", "tv"]]), note(2, 12, "Three", vec![vec!["t", "movie"]])]).await;
        let got = m.query(&[json!({"kinds": [2003], "#t": ["movie"]})]).await;
        assert_eq!(got.iter().map(|e| e.created_at.as_secs()).collect::<Vec<_>>(), vec![12, 10]);
        let author = keys(1).public_key().to_hex();
        assert_eq!(m.query(&[json!({"kinds": [2003], "authors": [author], "limit": 1})]).await.len(), 1);
    }

    #[tokio::test]
    async fn search_matches_every_token_against_content_and_tags() {
        let m = filled(&[note(1, 10, "Ubuntu 24.04 Desktop", vec![]), note(1, 11, "Debian Server", vec![])]).await;
        assert_eq!(m.query(&[json!({"kinds": [2003], "search": "ubuntu+desktop*"})]).await.len(), 1);
        assert_eq!(m.query(&[json!({"kinds": [2003], "search": "SERVER*"})]).await.len(), 1);
        assert_eq!(m.query(&[json!({"kinds": [2003], "search": "desc"})]).await.len(), 2);
        assert_eq!(m.query(&[json!({"kinds": [2003], "search": "fedora*"})]).await.len(), 0);
    }

    #[tokio::test]
    async fn a_newer_profile_replaces_the_older_one() {
        let old = EventBuilder::new(Kind::Metadata, "{\"name\":\"old\"}").custom_created_at(Timestamp::from_secs(10)).finalize(&keys(1)).unwrap();
        let new = EventBuilder::new(Kind::Metadata, "{\"name\":\"new\"}").custom_created_at(Timestamp::from_secs(20)).finalize(&keys(1)).unwrap();
        let m = filled(&[old, new]).await;
        let got = m.query(&[json!({"kinds": [0], "authors": [keys(1).public_key().to_hex()]})]).await;
        assert_eq!(got.len(), 1);
        assert!(got[0].content.contains("new"));
    }

    #[tokio::test]
    async fn follow_backfills_past_one_page_then_streams_live_events() {
        let upstream = MockRelay::run().await.unwrap();
        for n in 1..=620u64 {
            upstream.add_event(note(1, 1_000_000 + n, &format!("t{n}"), vec![])).await.unwrap();
        }
        let m = Mirror::new(0, None, &[2003]);
        m.follow(&[upstream.url().await.to_string()], &[backfill(json!({"kinds": [2003]}), true)]).await.unwrap();
        assert!(m.priority_loaded());
        assert_eq!(m.query(&[json!({"kinds": [2003], "limit": 1000})]).await.len(), 620);

        upstream.add_event(note(1, Timestamp::now().as_secs(), "live", vec![])).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while m.len().await < 621 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("live event reached the mirror");
    }

    #[tokio::test]
    async fn priority_backfills_finish_before_the_rest_starts() {
        let upstream = MockRelay::run().await.unwrap();
        upstream.add_event(note(1, 100, "a", vec![])).await.unwrap();
        upstream.add_event(EventBuilder::new(Kind::Metadata, "{}").custom_created_at(Timestamp::from_secs(90)).finalize(&keys(1)).unwrap()).await.unwrap();
        let m = Mirror::new(0, None, &[]);
        m.follow(&[upstream.url().await.to_string()], &[backfill(json!({"kinds": [2003]}), false), backfill(json!({"kinds": [0]}), true)]).await.unwrap();
        assert_eq!(m.len().await, 2);
    }

    #[tokio::test]
    async fn authors_of_live_events_get_their_profiles_fetched() {
        let upstream = MockRelay::run().await.unwrap();
        upstream.add_event(EventBuilder::new(Kind::Metadata, "{\"name\":\"alice\"}").custom_created_at(Timestamp::from_secs(90)).finalize(&keys(1)).unwrap()).await.unwrap();
        let m = Mirror::new(0, None, &[]);
        m.follow(&[upstream.url().await.to_string()], &[]).await.unwrap();
        m.note_author(&note(1, 100, "a", vec![])).await;
        let profile = json!({"kinds": [0], "authors": [keys(1).public_key().to_hex()]});
        tokio::time::timeout(Duration::from_secs(10), async {
            while m.query(&[profile.clone()]).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("profile fetched");
    }

    #[tokio::test]
    async fn post_filters_drop_results_before_the_limit_is_applied() {
        let rule: crate::filters::Rule = serde_json::from_str(r#"{"kinds":[2003],"deny_tags":{"t":["dead"]}}"#).unwrap();
        let m = Mirror::new(0, None, &[]).with_filters(crate::filters::build(&[rule]).unwrap());
        for (n, at, tag) in [(1u8, 10u64, "ok"), (2, 11, "dead"), (3, 12, "dead")] {
            m.database.save_event(&note(n, at, "t", vec![vec!["t", tag]])).await.unwrap();
        }
        let got = m.query(&[json!({"kinds": [2003], "limit": 2})]).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].created_at.as_secs(), 10);
    }

    #[tokio::test]
    async fn negative_cache_stops_repeat_lookups() {
        let m = Mirror::new(0, None, &[]);
        assert!(m.should_try("k"));
        assert!(!m.should_try("k"));
        assert!(m.should_try("other"));
    }

    #[tokio::test]
    async fn the_local_relay_serves_what_was_mirrored() {
        let m = Mirror::new(0, None, &[]);
        let ev = note(1, 5, "served", vec![]);
        m.database.save_event(&ev).await.unwrap();
        m.relay.run().await.unwrap();
        let client = Client::default();
        client.add_relay(m.relay.url().await).await.unwrap();
        client.connect().await;
        let got = client.fetch_events(Filter::new().id(ev.id)).timeout(Duration::from_secs(5)).await.unwrap();
        assert_eq!(got.len(), 1);
    }
}
