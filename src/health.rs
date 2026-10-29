use crate::media::{blossom_url, extension_of, origin_of, Media};
use reqwest::header::{CONTENT_TYPE, LOCATION, RANGE};
use reqwest::{Client, Method, StatusCode, Url};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_REDIRECTS: usize = 3;
const ALIVE_TTL: Duration = Duration::from_secs(6 * 3600);
const DEAD_TTL: Duration = Duration::from_secs(30 * 60);
const CONCURRENCY: usize = 16;
const PER_HOST: usize = 4;
const HOST_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Expect {
    Video,
    Image,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub key: String,
    pub urls: Vec<String>,
    pub expect: Expect,
}

#[derive(Clone, Copy)]
struct Verdict {
    dead: bool,
    at: Instant,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Probe {
    Alive,
    Gone,
    Unknown,
}

const UNKNOWN_RETRY: Duration = Duration::from_secs(2 * 60);

#[derive(Clone)]
pub struct Health {
    client: Client,
    verdicts: Arc<RwLock<HashMap<String, Verdict>>>,
    unknown_at: Arc<RwLock<HashMap<String, Instant>>>,
    hosts: Arc<RwLock<HashMap<String, (bool, Instant)>>>,
    allow_private: bool,
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast() || v4.octets()[0] == 0 || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

fn content_type_ok(expect: Expect, content_type: &str) -> bool {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    match expect {
        Expect::Video => ct.starts_with("video/") || ct.contains("mpegurl") || ct == "application/octet-stream" || ct == "binary/octet-stream" || ct == "application/mp4" || ct == "application/vnd.apple.mpegurl",
        Expect::Image => ct.starts_with("image/") || ct == "application/octet-stream" || ct == "binary/octet-stream",
    }
}

impl Health {
    pub fn new(allow_private: bool) -> Health {
        let client = Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(PROBE_TIMEOUT).connect_timeout(Duration::from_secs(5)).user_agent("Mozilla/5.0 (compatible; media-check)").build().expect("http client");
        Health { client, verdicts: Default::default(), unknown_at: Default::default(), hosts: Default::default(), allow_private }
    }

    pub fn is_dead(&self, key: &str) -> bool {
        self.verdicts.read().unwrap().get(key).is_some_and(|v| v.dead)
    }

    pub fn dead_among<'a>(&self, keys: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        let g = self.verdicts.read().unwrap();
        keys.into_iter().filter(|k| g.get(*k).is_some_and(|v| v.dead)).cloned().collect()
    }

    pub fn dead_keys(&self) -> Vec<String> {
        self.verdicts.read().unwrap().iter().filter(|(_, v)| v.dead).map(|(k, _)| k.clone()).collect()
    }

    fn due(&self, key: &str) -> bool {
        if let Some(at) = self.unknown_at.read().unwrap().get(key) {
            if at.elapsed() < UNKNOWN_RETRY {
                return false;
            }
        }
        match self.verdicts.read().unwrap().get(key) {
            None => true,
            Some(v) => v.at.elapsed() >= if v.dead { DEAD_TTL } else { ALIVE_TTL },
        }
    }

    fn record(&self, key: &str, dead: bool) {
        self.verdicts.write().unwrap().insert(key.to_string(), Verdict { dead, at: Instant::now() });
    }

    async fn host_is_public(&self, host: &str, port: u16) -> Option<bool> {
        if let Some((ok, at)) = self.hosts.read().unwrap().get(host) {
            if at.elapsed() < HOST_TTL {
                return Some(*ok);
            }
        }
        let addrs = match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, port))).await {
            Ok(Ok(addrs)) => addrs.collect::<Vec<_>>(),
            _ => return None,
        };
        let ok = !addrs.is_empty() && addrs.iter().all(|a| public_ip(a.ip()));
        self.hosts.write().unwrap().insert(host.to_string(), (ok, Instant::now()));
        Some(ok)
    }

    async fn request(&self, method: Method, start: &str, expect: Expect, ranged: bool) -> Option<Probe> {
        let mut url = Url::parse(start).ok()?;
        for _ in 0..=MAX_REDIRECTS {
            if !matches!(url.scheme(), "http" | "https") {
                return Some(Probe::Gone);
            }
            if !self.allow_private {
                let host = url.host_str()?.to_string();
                match self.host_is_public(&host, url.port_or_known_default().unwrap_or(443)).await {
                    Some(true) => {}
                    Some(false) => return Some(Probe::Gone),
                    None => return Some(Probe::Unknown),
                }
            }
            let mut req = self.client.request(method.clone(), url.clone());
            if ranged {
                req = req.header(RANGE, "bytes=0-0");
            }
            let res = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(%url, "probe error: {e}");
                    return Some(if e.is_timeout() || e.is_connect() { Probe::Unknown } else { Probe::Gone });
                }
            };
            let status = res.status();
            if status.is_redirection() {
                let next = res.headers().get(LOCATION).and_then(|l| l.to_str().ok()).and_then(|l| url.join(l).ok());
                match next {
                    Some(n) => {
                        url = n;
                        continue;
                    }
                    None => return Some(Probe::Gone),
                }
            }
            if matches!(status, StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED | StatusCode::FORBIDDEN) && method == Method::HEAD {
                return None;
            }
            if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
                return Some(Probe::Unknown);
            }
            let ct = res.headers().get(CONTENT_TYPE).and_then(|c| c.to_str().ok()).unwrap_or("").to_string();
            tracing::debug!(%url, %status, %ct, "probe response");
            let ok = (status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT) && content_type_ok(expect, &ct);
            return Some(if ok { Probe::Alive } else { Probe::Gone });
        }
        Some(Probe::Gone)
    }

    pub async fn probe_url(&self, url: &str, expect: Expect) -> Probe {
        match self.request(Method::HEAD, url, expect, false).await {
            Some(p) => p,
            None => self.request(Method::GET, url, expect, true).await.unwrap_or(Probe::Gone),
        }
    }

    pub async fn url_alive(&self, url: &str, expect: Expect) -> bool {
        self.probe_url(url, expect).await == Probe::Alive
    }

    pub async fn judge(&self, urls: &[String], expect: Expect) -> Probe {
        let mut unknown = false;
        for u in urls {
            match self.probe_url(u, expect).await {
                Probe::Alive => return Probe::Alive,
                Probe::Unknown => unknown = true,
                Probe::Gone => {}
            }
        }
        if unknown {
            Probe::Unknown
        } else {
            Probe::Gone
        }
    }

    pub async fn sweep(&self, mut candidates: Vec<Candidate>) -> usize {
        candidates.retain(|c| self.due(&c.key));
        let total = candidates.len();
        let overall = Arc::new(Semaphore::new(CONCURRENCY));
        let hosts: Arc<std::sync::Mutex<HashMap<String, Arc<Semaphore>>>> = Default::default();
        let mut tasks = tokio::task::JoinSet::new();
        for c in candidates {
            let permit = overall.clone().acquire_owned().await.unwrap();
            let me = self.clone();
            let host_gate = {
                let host = c.urls.first().and_then(|u| origin_of(u)).unwrap_or_default();
                hosts.lock().unwrap().entry(host).or_insert_with(|| Arc::new(Semaphore::new(PER_HOST))).clone()
            };
            tasks.spawn(async move {
                let _host = host_gate.acquire_owned().await.unwrap();
                match me.judge(&c.urls, c.expect).await {
                    Probe::Alive => me.record(&c.key, false),
                    Probe::Gone => me.record(&c.key, true),
                    Probe::Unknown => {
                        me.unknown_at.write().unwrap().insert(c.key.clone(), Instant::now());
                    }
                }
                drop(permit);
            });
        }
        while tasks.join_next().await.is_some() {}
        total
    }
}

pub fn candidate(m: &Media, author_servers: &[String]) -> Candidate {
    let mut urls = vec![m.url.clone()];
    urls.extend(m.hosts.iter().cloned());
    if let Some(hash) = &m.hash {
        let ext = extension_of(&m.url);
        for server in author_servers {
            if let Some(origin) = origin_of(server) {
                let u = blossom_url(&origin, hash, ext);
                if !urls.contains(&u) {
                    urls.push(u);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    urls.retain(|u| seen.insert(u.clone()));
    Candidate { key: m.key.clone(), urls, expect: if m.is_image() { Expect::Image } else { Expect::Video } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, Method as M};
    use axum::response::IntoResponse;
    use axum::routing::{any, get};
    use axum::Router;

    async fn serve() -> String {
        let app = Router::new()
            .route("/ok.mp4", get(|| async { ([("content-type", "video/mp4")], "x") }))
            .route("/page", get(|| async { ([("content-type", "text/html")], "<html>") }))
            .route("/placeholder.jpg", get(|| async { ([("content-type", "image/jpeg")], "x") }))
            .route(
                "/no-head.mp4",
                any(|method: M, headers: HeaderMap| async move {
                    if method == M::HEAD {
                        return axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response();
                    }
                    assert!(headers.contains_key("range"));
                    (axum::http::StatusCode::PARTIAL_CONTENT, [("content-type", "video/mp4")], "x").into_response()
                }),
            )
            .route("/moved.mp4", get(|| async { (axum::http::StatusCode::FOUND, [("location", "/ok.mp4")]) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn cand(key: &str, urls: &[String], expect: Expect) -> Candidate {
        Candidate { key: key.into(), urls: urls.to_vec(), expect }
    }

    #[tokio::test]
    async fn judges_urls_by_status_and_content_type() {
        let base = serve().await;
        let h = Health::new(true);
        assert!(h.url_alive(&format!("{base}/ok.mp4"), Expect::Video).await);
        assert!(h.url_alive(&format!("{base}/moved.mp4"), Expect::Video).await);
        assert!(h.url_alive(&format!("{base}/no-head.mp4"), Expect::Video).await);
        assert!(!h.url_alive(&format!("{base}/page"), Expect::Video).await);
        assert!(!h.url_alive(&format!("{base}/missing.mp4"), Expect::Video).await);
        assert!(!h.url_alive(&format!("{base}/placeholder.jpg"), Expect::Video).await);
        assert!(h.url_alive(&format!("{base}/placeholder.jpg"), Expect::Image).await);
    }

    #[tokio::test]
    async fn a_video_is_dead_only_when_every_source_fails() {
        let base = serve().await;
        let h = Health::new(true);
        let checked = h
            .sweep(vec![
                cand("alive-via-fallback", &[format!("{base}/missing.mp4"), format!("{base}/ok.mp4")], Expect::Video),
                cand("dead", &[format!("{base}/missing.mp4"), format!("{base}/page")], Expect::Video),
                cand("alive", &[format!("{base}/ok.mp4")], Expect::Video),
            ])
            .await;
        assert_eq!(checked, 3);
        assert!(h.is_dead("dead"));
        assert!(!h.is_dead("alive-via-fallback"));
        assert!(!h.is_dead("alive"));
        assert!(!h.is_dead("never-checked"));
        let keys = ["dead".to_string(), "alive".to_string()];
        assert_eq!(h.dead_among(keys.iter()), ["dead"]);
    }

    #[tokio::test]
    async fn fresh_verdicts_are_not_rechecked() {
        let base = serve().await;
        let h = Health::new(true);
        let c = cand("k", &[format!("{base}/ok.mp4")], Expect::Video);
        assert_eq!(h.sweep(vec![c.clone()]).await, 1);
        assert_eq!(h.sweep(vec![c]).await, 0);
    }

    #[tokio::test]
    async fn private_and_loopback_addresses_are_never_fetched() {
        let base = serve().await;
        let h = Health::new(false);
        assert!(!h.url_alive(&format!("{base}/ok.mp4"), Expect::Video).await);
        assert!(!h.url_alive("http://169.254.169.254/latest/meta-data", Expect::Video).await);
        assert!(!h.url_alive("http://[::1]/x.mp4", Expect::Video).await);
        assert!(!h.url_alive("file:///etc/passwd", Expect::Video).await);
    }

    #[tokio::test]
    async fn unreachable_hosts_are_unknown_not_dead() {
        let h = Health::new(true);
        let c = cand("flaky", &["http://127.0.0.1:1/x.mp4".to_string()], Expect::Video);
        assert_eq!(h.judge(&c.urls, c.expect).await, Probe::Unknown);
        h.sweep(vec![c.clone()]).await;
        assert!(!h.is_dead("flaky"));
        assert_eq!(h.sweep(vec![c]).await, 0, "unknown results are retried later, not immediately");
    }

    #[test]
    fn candidates_include_imeta_hosts_and_author_servers_once() {
        let mut m = crate::media::tests_support::media("a");
        m.url = "https://one.example/abc.mp4".into();
        m.hash = Some("a".repeat(64));
        m.hosts = vec![format!("https://two.example/{}.mp4", "a".repeat(64))];
        let c = candidate(&m, &["https://three.example/".into(), "https://two.example".into()]);
        assert_eq!(c.urls.len(), 3);
        assert!(c.urls[2].starts_with("https://three.example/") && c.urls[2].ends_with(".mp4"));
    }
}
