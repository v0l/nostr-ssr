use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Cached {
    pub html: Arc<String>,
    pub status: u16,
    pub at: Instant,
}

struct Inner {
    entries: HashMap<String, Cached>,
    bytes: usize,
}

#[derive(Clone)]
pub struct PageCache {
    inner: Arc<StdMutex<Inner>>,
    locks: Arc<StdMutex<HashMap<String, Arc<Mutex<()>>>>>,
    refreshing: Arc<StdMutex<std::collections::HashSet<String>>>,
    fresh: Duration,
    stale: Duration,
    max_bytes: usize,
}

impl PageCache {
    pub fn new(fresh: Duration, stale: Duration, max_bytes: usize) -> Self {
        PageCache {
            inner: Arc::new(StdMutex::new(Inner { entries: HashMap::new(), bytes: 0 })),
            locks: Default::default(),
            refreshing: Default::default(),
            fresh,
            stale,
            max_bytes,
        }
    }

    fn get(&self, key: &str) -> Option<Cached> {
        self.inner.lock().unwrap().entries.get(key).cloned()
    }

    fn put(&self, key: &str, value: Cached) {
        let mut g = self.inner.lock().unwrap();
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

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn key_lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.locks.lock().unwrap().entry(key.to_string()).or_default().clone()
    }

    pub async fn get_or_render<F, Fut>(&self, key: &str, render: F) -> Option<Cached>
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Option<(String, u16)>> + Send + 'static,
    {
        if let Some(hit) = self.get(key) {
            let age = hit.at.elapsed();
            if age < self.fresh {
                return Some(hit);
            }
            if age < self.stale {
                self.refresh_in_background(key, render);
                return Some(hit);
            }
        }
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;
        if let Some(hit) = self.get(key) {
            if hit.at.elapsed() < self.fresh {
                return Some(hit);
            }
        }
        let (html, status) = render().await?;
        let value = Cached { html: Arc::new(html), status, at: Instant::now() };
        if status == 200 {
            self.put(key, value.clone());
        }
        Some(value)
    }

    pub async fn refresh<F, Fut>(&self, key: &str, render: F)
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Option<(String, u16)>> + Send + 'static,
    {
        if let Some(hit) = self.get(key) {
            if hit.at.elapsed() < self.fresh / 2 {
                return;
            }
        }
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;
        if let Some((html, status)) = render().await {
            if status == 200 {
                self.put(key, Cached { html: Arc::new(html), status, at: Instant::now() });
            }
        }
    }

    fn refresh_in_background<F, Fut>(&self, key: &str, render: F)
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Option<(String, u16)>> + Send + 'static,
    {
        if !self.refreshing.lock().unwrap().insert(key.to_string()) {
            return;
        }
        let me = self.clone();
        let key = key.to_string();
        tokio::spawn(async move {
            if let Some((html, status)) = render().await {
                if status == 200 {
                    me.put(&key, Cached { html: Arc::new(html), status, at: Instant::now() });
                }
            }
            me.refreshing.lock().unwrap().remove(&key);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter() -> (Arc<AtomicUsize>, impl Fn() -> std::pin::Pin<Box<dyn Future<Output = Option<(String, u16)>> + Send>> + Clone + Send + Sync + 'static) {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        (n, move || {
            let n = n2.clone();
            Box::pin(async move {
                let v = n.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_millis(30)).await;
                Some((format!("render {v}"), 200))
            })
        })
    }

    #[tokio::test]
    async fn fresh_pages_are_served_without_rendering_again() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        assert_eq!(*cache.get_or_render("/a", render.clone()).await.unwrap().html, "render 1");
        assert_eq!(*cache.get_or_render("/a", render.clone()).await.unwrap().html, "render 1");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_render() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (c, r) = (cache.clone(), render.clone());
            tasks.push(tokio::spawn(async move { c.get_or_render("/a", r).await.unwrap().html }));
        }
        for t in tasks {
            assert_eq!(*t.await.unwrap(), "render 1");
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_pages_are_served_immediately_and_refreshed_behind() {
        let cache = PageCache::new(Duration::from_millis(20), Duration::from_secs(600), 1 << 20);
        let (n, render) = counter();
        cache.get_or_render("/a", render.clone()).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        let started = Instant::now();
        assert_eq!(*cache.get_or_render("/a", render.clone()).await.unwrap().html, "render 1");
        assert!(started.elapsed() < Duration::from_millis(20), "stale hit did not wait for the render");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*cache.get_or_render("/a", render.clone()).await.unwrap().html, "render 2");
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
        assert_eq!(*cache.get_or_render("/a", render).await.unwrap().html, "render 2");
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn only_successful_pages_are_cached_and_size_is_bounded() {
        let cache = PageCache::new(Duration::from_secs(60), Duration::from_secs(600), 25);
        let missing = || async { Some(("nope".to_string(), 404)) };
        cache.get_or_render("/missing", missing).await;
        assert!(cache.is_empty());
        let big = |s: &'static str| move || async move { Some((s.repeat(10), 200)) };
        cache.get_or_render("/1", big("a")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        cache.get_or_render("/2", big("b")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        cache.get_or_render("/3", big("c")).await;
        assert!(cache.len() <= 2);
        assert!(cache.get("/3").is_some());
    }
}
