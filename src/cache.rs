use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const FAILURE_TTL: Duration = Duration::from_secs(30);
const MAX_FAILED_KEYS: usize = 16384;

#[derive(Clone)]
pub struct Cached {
    pub html: Arc<String>,
    pub status: u16,
    pub at: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Page(String, u16),
    Busy,
    Failed,
}

#[derive(Clone)]
pub enum Served {
    Page(Cached),
    Busy,
    Failed,
}

impl Served {
    pub fn page(self) -> Option<Cached> {
        match self {
            Served::Page(c) => Some(c),
            _ => None,
        }
    }
}

struct Inner {
    entries: HashMap<String, Cached>,
    bytes: usize,
    failed: HashMap<String, Instant>,
}

type Flight = watch::Receiver<Option<Served>>;

#[derive(Clone)]
pub struct PageCache {
    inner: Arc<Mutex<Inner>>,
    flights: Arc<Mutex<HashMap<String, Flight>>>,
    fresh: Duration,
    stale: Duration,
    failure_ttl: Duration,
    max_bytes: usize,
}

struct Landing {
    flights: Arc<Mutex<HashMap<String, Flight>>>,
    key: String,
}

impl Drop for Landing {
    fn drop(&mut self) {
        self.flights.lock().unwrap().remove(&self.key);
    }
}

impl PageCache {
    pub fn new(fresh: Duration, stale: Duration, max_bytes: usize) -> Self {
        PageCache {
            inner: Arc::new(Mutex::new(Inner { entries: HashMap::new(), bytes: 0, failed: HashMap::new() })),
            flights: Default::default(),
            fresh,
            stale,
            failure_ttl: FAILURE_TTL,
            max_bytes,
        }
    }

    pub fn with_failure_ttl(mut self, ttl: Duration) -> Self {
        self.failure_ttl = ttl;
        self
    }

    fn get(&self, key: &str) -> Option<Cached> {
        self.inner.lock().unwrap().entries.get(key).cloned()
    }

    fn put(&self, key: &str, value: Cached) {
        let mut g = self.inner.lock().unwrap();
        if value.html.len() > self.max_bytes {
            return;
        }
        if let Some(old) = g.entries.insert(key.to_string(), value.clone()) {
            g.bytes -= old.html.len();
        }
        g.bytes += value.html.len();
        while g.bytes > self.max_bytes {
            let Some(oldest) = g.entries.iter().min_by_key(|(_, v)| v.at).map(|(k, _)| k.clone()) else { break };
            if let Some(removed) = g.entries.remove(&oldest) {
                g.bytes -= removed.html.len();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap().bytes
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn recently_failed(&self, key: &str) -> bool {
        self.inner.lock().unwrap().failed.get(key).is_some_and(|at| at.elapsed() < self.failure_ttl)
    }

    fn mark_failed(&self, key: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.failed.len() >= MAX_FAILED_KEYS {
            let ttl = self.failure_ttl;
            g.failed.retain(|_, at| at.elapsed() < ttl);
            if g.failed.len() >= MAX_FAILED_KEYS {
                g.failed.clear();
            }
        }
        g.failed.insert(key.to_string(), Instant::now());
    }

    fn settle(&self, key: &str, store: bool, outcome: Outcome) -> Served {
        match outcome {
            Outcome::Page(html, status) => {
                let value = Cached { html: Arc::new(html), status, at: Instant::now() };
                if store && status == 200 {
                    self.put(key, value.clone());
                }
                Served::Page(value)
            }
            Outcome::Busy => Served::Busy,
            Outcome::Failed => {
                self.mark_failed(key);
                Served::Failed
            }
        }
    }

    async fn fly<F, Fut>(&self, key: &str, store: bool, force: bool, render: F) -> Served
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Outcome> + Send + 'static,
    {
        let mut rx = {
            let mut flights = self.flights.lock().unwrap();
            match flights.get(key) {
                Some(rx) => rx.clone(),
                None => {
                    if store && !force {
                        if let Some(hit) = self.get(key).filter(|h| h.at.elapsed() < self.fresh) {
                            return Served::Page(hit);
                        }
                    }
                    if self.recently_failed(key) {
                        return Served::Failed;
                    }
                    let (tx, rx) = watch::channel(None);
                    flights.insert(key.to_string(), rx.clone());
                    let me = self.clone();
                    let landing = Landing { flights: self.flights.clone(), key: key.to_string() };
                    tokio::spawn(async move {
                        let outcome = render().await;
                        let served = me.settle(&landing.key, store, outcome);
                        drop(landing);
                        let _ = tx.send(Some(served));
                    });
                    rx
                }
            }
        };
        let settled = rx.wait_for(Option::is_some).await.ok().and_then(|v| v.clone());
        settled.unwrap_or(Served::Failed)
    }

    pub async fn get_or_render<F, Fut>(&self, key: &str, store: bool, render: F) -> Served
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Outcome> + Send + 'static,
    {
        let held = if store { self.get(key) } else { None };
        if let Some(hit) = &held {
            let age = hit.at.elapsed();
            if age < self.fresh {
                return Served::Page(hit.clone());
            }
            if age < self.stale {
                self.refresh_in_background(key, render);
                return Served::Page(hit.clone());
            }
        }
        match self.fly(key, store, false, render).await {
            Served::Page(c) => Served::Page(c),
            other => held.map(Served::Page).unwrap_or(other),
        }
    }

    pub async fn refresh<F, Fut>(&self, key: &str, render: F)
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Outcome> + Send + 'static,
    {
        if let Some(hit) = self.get(key) {
            if hit.at.elapsed() < self.fresh / 2 {
                return;
            }
        }
        self.inner.lock().unwrap().failed.remove(key);
        self.fly(key, true, true, render).await;
    }

    fn refresh_in_background<F, Fut>(&self, key: &str, render: F)
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Outcome> + Send + 'static,
    {
        if self.recently_failed(key) || self.flights.lock().unwrap().contains_key(key) {
            return;
        }
        let me = self.clone();
        let key = key.to_string();
        tokio::spawn(async move {
            me.fly(&key, true, true, render).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Render = std::pin::Pin<Box<dyn Future<Output = Outcome> + Send>>;

    fn counting(outcome: impl Fn(usize) -> Outcome + Send + Sync + Clone + 'static) -> (Arc<AtomicUsize>, impl Fn() -> Render + Clone + Send + Sync + 'static) {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        (n, move || {
            let (n, outcome) = (n2.clone(), outcome.clone());
            Box::pin(async move {
                let v = n.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_millis(30)).await;
                outcome(v)
            })
        })
    }

    fn counter() -> (Arc<AtomicUsize>, impl Fn() -> Render + Clone + Send + Sync + 'static) {
        counting(|v| Outcome::Page(format!("render {v}"), 200))
    }

    fn html(s: Served) -> String {
        s.page().map(|c| c.html.to_string()).unwrap_or_default()
    }

    #[tokio::test]
    async fn fresh_pages_are_served_without_rendering_again() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        assert_eq!(html(cache.get_or_render("/a", true, render.clone()).await), "render 1");
        assert_eq!(html(cache.get_or_render("/a", true, render.clone()).await), "render 1");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_render() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (c, r) = (cache.clone(), render.clone());
            tasks.push(tokio::spawn(async move { html(c.get_or_render("/a", true, r).await) }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), "render 1");
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn two_hundred_concurrent_requests_for_an_uncacheable_url_share_one_render() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        let mut tasks = Vec::new();
        for _ in 0..200 {
            let (c, r) = (cache.clone(), render.clone());
            tasks.push(tokio::spawn(async move { html(c.get_or_render("/a?x=1", false, r).await) }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), "render 1");
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn a_failed_render_is_remembered_and_not_retried_until_it_expires() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20).with_failure_ttl(Duration::from_millis(200));
        let (n, render) = counting(|_| Outcome::Failed);
        let mut tasks = Vec::new();
        for _ in 0..200 {
            let (c, r) = (cache.clone(), render.clone());
            tasks.push(tokio::spawn(async move { matches!(c.get_or_render("/broken", true, r).await, Served::Failed) }));
        }
        for t in tasks {
            assert!(t.await.unwrap());
        }
        assert!(matches!(cache.get_or_render("/broken", true, render.clone()).await, Served::Failed));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(250)).await;
        cache.get_or_render("/broken", true, render.clone()).await;
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_busy_renderer_is_not_remembered_as_a_failure() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        let (n, render) = counting(|v| if v == 1 { Outcome::Busy } else { Outcome::Page("ok".into(), 200) });
        assert!(matches!(cache.get_or_render("/a", true, render.clone()).await, Served::Busy));
        assert_eq!(html(cache.get_or_render("/a", true, render.clone()).await), "ok");
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_expired_page_is_served_when_the_renderer_is_busy_or_failing() {
        let cache = PageCache::new(Duration::from_millis(10), Duration::from_millis(20), 1 << 20);
        let (_, render) = counting(|v| if v == 1 { Outcome::Page("old".into(), 200) } else { Outcome::Busy });
        cache.get_or_render("/a", true, render.clone()).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(html(cache.get_or_render("/a", true, render.clone()).await), "old");
    }

    #[tokio::test]
    async fn a_render_finishes_and_is_cached_even_when_every_caller_gave_up() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        let (c, r) = (cache.clone(), render.clone());
        let t = tokio::spawn(async move { c.get_or_render("/a", true, r).await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        t.abort();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(html(cache.get_or_render("/a", true, render).await), "render 1");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_pages_are_served_immediately_and_refreshed_behind() {
        let cache = PageCache::new(Duration::from_millis(20), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        cache.get_or_render("/a", true, render.clone()).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        let started = Instant::now();
        assert_eq!(html(cache.get_or_render("/a", true, render.clone()).await), "render 1");
        assert!(started.elapsed() < Duration::from_millis(20), "stale hit did not wait for the render");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(html(cache.get_or_render("/a", true, render.clone()).await), "render 2");
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn warming_replaces_a_page_before_visitors_see_it_go_stale() {
        let cache = PageCache::new(Duration::from_millis(100), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        cache.refresh("/a", render.clone()).await;
        cache.refresh("/a", render.clone()).await;
        assert_eq!(n.load(Ordering::SeqCst), 1, "a recent page is not re-rendered");
        tokio::time::sleep(Duration::from_millis(70)).await;
        cache.refresh("/a", render.clone()).await;
        assert_eq!(html(cache.get_or_render("/a", true, render).await), "render 2");
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn only_successful_pages_are_cached_and_size_is_bounded() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 25);
        let missing = || async { Outcome::Page("nope".to_string(), 404) };
        cache.get_or_render("/missing", true, missing).await;
        assert!(cache.is_empty());
        let big = |s: &'static str| move || async move { Outcome::Page(s.repeat(10), 200) };
        cache.get_or_render("/1", true, big("a")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        cache.get_or_render("/2", true, big("b")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        cache.get_or_render("/3", true, big("c")).await;
        assert!(cache.len() <= 2);
        assert!(cache.bytes() <= 25);
        assert!(cache.get("/3").is_some());
        cache.get_or_render("/huge", true, big("abc")).await;
        assert!(cache.get("/huge").is_none() && cache.bytes() <= 25);
    }

    #[tokio::test]
    async fn the_failure_memory_stays_bounded_under_a_crawl_of_distinct_urls() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        for n in 0..(MAX_FAILED_KEYS * 3) {
            cache.mark_failed(&format!("/{n}"));
        }
        assert!(cache.inner.lock().unwrap().failed.len() <= MAX_FAILED_KEYS);
        assert!(cache.flights.lock().unwrap().is_empty());
    }
}
