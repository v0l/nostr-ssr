use nostr::prelude::*;
use regex::Regex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub trait ResultFilter: Send + Sync {
    fn kinds(&self) -> Option<&[Kind]> {
        None
    }

    fn prepare(&self) {}

    fn keep(&self, event: &Event) -> bool;
}

pub fn may_apply(filters: &[Arc<dyn ResultFilter>], wanted: Option<&[Kind]>) -> bool {
    filters.iter().any(|f| match (f.kinds(), wanted) {
        (Some(mine), Some(wanted)) => mine.iter().any(|k| wanted.contains(k)),
        _ => true,
    })
}

pub fn keep_all(filters: &[Arc<dyn ResultFilter>], event: &Event) -> bool {
    filters.iter().all(|f| f.kinds().is_some_and(|k| !k.contains(&event.kind)) || f.keep(event))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub kinds: Vec<u16>,
    pub deny_authors: Vec<String>,
    pub deny_ids: Vec<String>,
    pub deny_tags: HashMap<String, Vec<String>>,
    pub require_tags: Vec<String>,
    pub deny_content: Option<String>,
    pub deny_file: Option<PathBuf>,
}

struct DenyFile {
    path: PathBuf,
    loaded: Mutex<(Option<SystemTime>, Option<SystemTime>, HashSet<String>)>,
}

impl DenyFile {
    fn refresh(&self) {
        let mut state = self.loaded.lock().unwrap();
        if state.1.is_some_and(|checked| checked.elapsed().map(|e| e < Duration::from_secs(5)).unwrap_or(false)) {
            return;
        }
        state.1 = Some(SystemTime::now());
        let modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if modified == state.0 && modified.is_some() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&self.path) else { return };
        let entries: Vec<String> = serde_json::from_str(&text).unwrap_or_else(|_| text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect());
        state.0 = modified;
        state.2 = entries.into_iter().collect();
    }

    fn contains(&self, key: &str) -> bool {
        self.loaded.lock().unwrap().2.contains(key)
    }
}

pub struct RuleFilter {
    kinds: Vec<Kind>,
    authors: HashSet<String>,
    ids: HashSet<String>,
    tags: HashMap<String, HashSet<String>>,
    require: Vec<String>,
    content: Option<Regex>,
    file: Option<DenyFile>,
}

impl RuleFilter {
    pub fn new(rule: &Rule) -> anyhow::Result<RuleFilter> {
        Ok(RuleFilter {
            kinds: rule.kinds.iter().copied().map(Kind::from).collect(),
            authors: rule.deny_authors.iter().cloned().collect(),
            ids: rule.deny_ids.iter().cloned().collect(),
            tags: rule.deny_tags.iter().map(|(k, v)| (k.clone(), v.iter().cloned().collect())).collect(),
            require: rule.require_tags.clone(),
            content: rule.deny_content.as_deref().map(Regex::new).transpose()?,
            file: rule.deny_file.clone().map(|path| DenyFile { path, loaded: Mutex::new((None, None, HashSet::new())) }),
        })
    }
}

impl ResultFilter for RuleFilter {
    fn kinds(&self) -> Option<&[Kind]> {
        (!self.kinds.is_empty()).then_some(self.kinds.as_slice())
    }

    fn prepare(&self) {
        if let Some(file) = &self.file {
            file.refresh();
        }
    }

    fn keep(&self, event: &Event) -> bool {
        let id = event.id.to_hex();
        if self.ids.contains(&id) || self.authors.contains(&event.pubkey.to_hex()) {
            return false;
        }
        if self.file.as_ref().is_some_and(|f| f.contains(&id)) {
            return false;
        }
        if self.content.as_ref().is_some_and(|re| re.is_match(&event.content)) {
            return false;
        }
        if !self.require.iter().all(|name| event.tags.iter().any(|t| t.as_slice().first().map(String::as_str) == Some(name.as_str()))) {
            return false;
        }
        !event.tags.iter().any(|t| {
            let parts = t.as_slice();
            match (parts.first().and_then(|n| self.tags.get(n)), parts.get(1)) {
                (Some(denied), Some(value)) => denied.contains(value),
                _ => false,
            }
        })
    }
}

pub fn build(rules: &[Rule]) -> anyhow::Result<Vec<Arc<dyn ResultFilter>>> {
    rules.iter().map(|r| Ok(Arc::new(RuleFilter::new(r)?) as Arc<dyn ResultFilter>)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::event::FinalizeEvent;

    fn keys(n: u8) -> Keys {
        Keys::new(SecretKey::from_slice(&[n; 32]).unwrap())
    }

    fn event(n: u8, kind: u16, content: &str, tags: Vec<Vec<&str>>) -> Event {
        EventBuilder::new(Kind::from(kind), content).tags(tags.into_iter().map(|t| Tag::parse(t).unwrap())).finalize(&keys(n)).unwrap()
    }

    fn filters(json: &str) -> Vec<Arc<dyn ResultFilter>> {
        build(&serde_json::from_str::<Vec<Rule>>(json).unwrap()).unwrap()
    }

    #[test]
    fn rules_only_touch_the_kinds_they_name() {
        let f = filters(r#"[{"kinds":[21],"deny_tags":{"t":["spam"]}}]"#);
        assert!(!keep_all(&f, &event(1, 21, "", vec![vec!["t", "spam"]])));
        assert!(keep_all(&f, &event(1, 21, "", vec![vec!["t", "fine"]])));
        assert!(keep_all(&f, &event(1, 1, "", vec![vec!["t", "spam"]])));
        assert!(may_apply(&f, Some(&[Kind::from(21)])));
        assert!(!may_apply(&f, Some(&[Kind::from(1)])));
        assert!(may_apply(&f, None));
    }

    #[test]
    fn authors_ids_content_and_required_tags_can_exclude() {
        let bad = event(2, 1, "buy pills now", vec![]);
        let f = filters(&format!(r#"[{{"deny_authors":["{}"]}},{{"deny_ids":["{}"]}},{{"deny_content":"(?i)pills"}},{{"kinds":[21],"require_tags":["imeta"]}}]"#, keys(3).public_key().to_hex(), event(4, 1, "x", vec![]).id.to_hex()));
        assert!(!keep_all(&f, &bad));
        assert!(!keep_all(&f, &event(3, 1, "ok", vec![])));
        assert!(!keep_all(&f, &event(4, 1, "x", vec![])));
        assert!(!keep_all(&f, &event(5, 21, "no media", vec![])));
        assert!(keep_all(&f, &event(5, 21, "media", vec![vec!["imeta", "url https://x/a.mp4"]])));
        assert!(keep_all(&f, &event(5, 1, "plain", vec![])));
    }

    #[test]
    fn a_deny_file_is_picked_up_without_a_restart() {
        let path = std::env::temp_dir().join(format!("nostr-ssr-deny-{}.json", std::process::id()));
        let ev = event(1, 21, "v", vec![]);
        std::fs::write(&path, "[]").unwrap();
        let f = filters(&format!(r#"[{{"deny_file":{}}}]"#, serde_json::to_string(&path).unwrap()));
        f[0].prepare();
        assert!(keep_all(&f, &ev));
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&path, format!("[\"{}\"]", ev.id.to_hex())).unwrap();
        let rule = RuleFilter::new(&Rule { deny_file: Some(path), ..Default::default() }).unwrap();
        rule.prepare();
        assert!(!rule.keep(&ev));
    }
}
