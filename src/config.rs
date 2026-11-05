use crate::filters::Rule;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Backfill {
    pub filter: Value,
    pub max: usize,
    pub priority: bool,
}

impl Default for Backfill {
    fn default() -> Self {
        Backfill { filter: Value::Object(Default::default()), max: 5000, priority: false }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProxyRule {
    pub prefix: String,
    pub upstream: String,
    pub forward_path: bool,
    pub headers: std::collections::HashMap<String, String>,
    pub resolve_nip05: Option<String>,
    pub resolve_headers: std::collections::HashMap<String, String>,
    pub timeout_secs: u64,
}

impl Default for ProxyRule {
    fn default() -> Self {
        ProxyRule { prefix: String::new(), upstream: String::new(), forward_path: false, headers: Default::default(), resolve_nip05: None, resolve_headers: Default::default(), timeout_secs: 10 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ShellRule {
    pub prefix: String,
    pub file: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HeaderRule {
    pub prefix: String,
    pub set: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OpenGraph {
    pub twitter_site: Option<String>,
    pub player: Option<String>,
    pub avatar: String,
    pub nip05_url: Option<String>,
    pub nip05_headers: std::collections::HashMap<String, String>,
    pub nip05_domain: Option<String>,
    pub skip_paths: Vec<String>,
}

impl Default for OpenGraph {
    fn default() -> Self {
        OpenGraph {
            twitter_site: None,
            player: None,
            avatar: "https://nostr-api.v0l.io/api/v1/avatar/cyberpunks/{pubkey}.webp".into(),
            nip05_url: None,
            nip05_headers: Default::default(),
            nip05_domain: None,
            skip_paths: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Health {
    pub filter: Value,
    pub interval_secs: u64,
    pub allow_private: bool,
}

impl Default for Health {
    fn default() -> Self {
        Health { filter: serde_json::json!({"kinds": [20, 21, 22, 34235, 34236]}), interval_secs: 60, allow_private: false }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SitemapSource {
    pub name: String,
    pub filter: Value,
    pub group: String,
    pub media: bool,
    pub min_count: usize,
    pub prefix: String,
    pub encoding: String,
    pub chunk: Option<usize>,
    pub max: Option<usize>,
    pub changefreq: String,
    pub priority: String,
    pub family_friendly: Option<bool>,
    pub view_kind: Option<u16>,
}

impl Default for SitemapSource {
    fn default() -> Self {
        SitemapSource {
            name: "events".into(),
            filter: Value::Object(Default::default()),
            group: String::new(),
            media: false,
            min_count: 1,
            prefix: "/e/".into(),
            encoding: "nevent".into(),
            chunk: None,
            max: None,
            changefreq: "monthly".into(),
            priority: "0.7".into(),
            family_friendly: None,
            view_kind: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub site_name: String,
    pub description: String,
    pub public_url: Option<String>,
    pub default_image: String,
    pub relays: Vec<String>,
    pub live: bool,
    pub kinds: Vec<u16>,
    pub backfill: Vec<Backfill>,
    pub sitemap: Vec<SitemapSource>,
    pub sitemap_ttl_secs: u64,
    pub filters: Vec<Rule>,
    pub health: Option<Health>,
    pub proxy: Vec<ProxyRule>,
    pub opengraph: Option<OpenGraph>,
    pub headers: Vec<HeaderRule>,
    pub shells: Vec<ShellRule>,
    pub static_paths: Vec<String>,
    pub max_events: usize,
    pub sitemap_chunk: usize,
    pub dist: PathBuf,
    pub bundle: Option<PathBuf>,
    pub bind: String,
    pub relay_port: Option<u16>,
    pub workers: usize,
    pub warm: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            site_name: "Nostr".into(),
            description: String::new(),
            public_url: None,
            default_image: "/banner.jpg".into(),
            relays: vec![],
            live: true,
            kinds: vec![],
            backfill: vec![],
            sitemap: vec![],
            sitemap_ttl_secs: 300,
            filters: vec![],
            health: None,
            proxy: vec![],
            opengraph: None,
            headers: vec![],
            shells: vec![],
            static_paths: vec!["/".into()],
            max_events: 200_000,
            sitemap_chunk: 40_000,
            dist: "dist".into(),
            bundle: None,
            bind: "0.0.0.0:3000".into(),
            relay_port: None,
            workers: 2,
            warm: vec![],
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        let mut config: Config = match std::env::var("CONFIG") {
            Ok(path) => serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?)?,
            Err(_) => Config::default(),
        };
        config.apply_env(|k| std::env::var(k).ok());
        Ok(config)
    }

    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        let text = |k: &str| get(k).filter(|v| !v.is_empty());
        if let Some(v) = text("SITE_NAME") {
            self.site_name = v;
        }
        if let Some(v) = text("PUBLIC_URL") {
            self.public_url = Some(v);
        }
        if let Some(v) = text("RELAYS") {
            self.relays = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        if let Some(v) = text("MAX_EVENTS").and_then(|v| v.parse().ok()) {
            self.max_events = v;
        }
        if let Some(v) = text("DIST") {
            self.dist = v.into();
        }
        if let Some(v) = text("SSR_BUNDLE") {
            self.bundle = Some(v.into());
        }
        if let Some(v) = text("BIND") {
            self.bind = v;
        }
        if let Some(v) = text("RELAY_PORT").and_then(|v| v.parse().ok()) {
            self.relay_port = Some(v);
        }
        if let Some(v) = text("SSR_WORKERS").and_then(|v| v.parse().ok()) {
            self.workers = v;
        }
    }

    pub fn bundle_path(&self) -> PathBuf {
        self.bundle.clone().unwrap_or_else(|| self.dist.join("ssr/render.js"))
    }

    pub fn client_dir(&self) -> PathBuf {
        let nested = self.dist.join("client");
        if nested.join("index.html").exists() {
            nested
        } else {
            self.dist.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_json_file_keeps_defaults_and_env_overrides_it() {
        let mut c: Config = serde_json::from_str(r#"{"site_name":"DTAN.XYZ","kinds":[2003],"backfill":[{"filter":{"kinds":[2003]},"priority":true}]}"#).unwrap();
        assert_eq!(c.max_events, 200_000);
        assert_eq!(c.backfill[0].max, 5000);
        assert!(c.backfill[0].priority);
        c.apply_env(|k| (k == "RELAYS").then(|| "wss://a, wss://b".to_string()));
        assert_eq!(c.relays, vec!["wss://a", "wss://b"]);
        assert_eq!(c.site_name, "DTAN.XYZ");
    }
}
