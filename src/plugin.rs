use serde_json::Value;
use std::sync::Arc;

/// A global JS function a plugin exposes to the bundle during a render.
/// The handler runs on the render worker thread and may block on I/O
/// (e.g. a tracker announce). It receives the JSON-serialised first
/// argument and returns a JSON-serialised value.
pub struct JsFunction {
    pub name: &'static str,
    pub handler: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

/// Application-side SSR enrichment.
///
/// A plugin registers JS functions the bundle calls during a render
/// (server-side logic, e.g. answering a torrent tracker announce), can
/// add keys to the hydration `data` after a successful render, and can
/// serve its own HTTP routes under `/api/<name>/`.
///
/// Bundle keys always win; plugin keys fill in only where the bundle left
/// them unset. All domain logic lives in the plugin, not in `nostr-ssr`.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;

    fn js_functions(&self) -> Vec<JsFunction> {
        Vec::new()
    }

    fn enrich(&self, url: &str, status: u16, data: &Value) -> Option<Value> {
        let _ = (url, status, data);
        None
    }

    /// Stateless routes, served under `/api/<name>/`.
    fn routes(&self) -> axum::Router {
        axum::Router::new()
    }
}

/// Merges `extra` into `target`, creating the object if needed. Any key
/// the bundle already set is left untouched.
pub fn merge_into(target: &mut Value, extra: &Value) {
    if let (Value::Object(to), Value::Object(from)) = (target, extra) {
        for (k, v) in from {
            to.entry(k.clone()).or_insert(v.clone());
        }
    }
}

/// Runs `enrich` on every plugin and merges each result into `data`.
pub fn enrich_data(data: &mut Value, url: &str, status: u16, plugins: &[Arc<dyn Plugin>]) {
    for p in plugins {
        if let Some(extra) = p.enrich(url, status, data) {
            merge_into(data, &extra);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn merge_leaves_bundle_keys_untouched_and_adds_missing_ones() {
        let mut target = json!({"a": 1});
        merge_into(&mut target, &json!({"a": 2, "b": 3}));
        assert_eq!(target, json!({"a": 1, "b": 3}));
    }

    #[test]
    fn enrich_runs_every_plugin_and_fills_in_data() {
        struct P(usize);
        impl Plugin for P {
            fn name(&self) -> &'static str {
                "p"
            }
            fn enrich(&self, _: &str, _: u16, _: &Value) -> Option<Value> {
                Some(json!({format!("k{}", self.0): true}))
            }
        }
        let mut data = json!({"queries": {}});
        enrich_data(&mut data, "/e/x", 200, &[Arc::new(P(1)), Arc::new(P(2))]);
        assert_eq!(data, json!({"queries": {}, "k1": true, "k2": true}));
    }

    #[test]
    fn js_functions_are_registered_and_callable() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let f = JsFunction {
            name: "swarm",
            handler: Arc::new(|arg: &str| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                let v: Value = serde_json::from_str(arg).unwrap();
                json!({ "ok": true, "len": v.as_str().map(str::len).unwrap_or(0) }).to_string()
            }),
        };
        let args = json!("info_hash").to_string();
        assert_eq!((f.handler)(&args), json!({"ok":true,"len":9}).to_string());
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }
}
