use crate::mirror::Mirror;
use crate::plugin::{enrich_data, Plugin};
use rquickjs::{Context, Function, Runtime};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime};

const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
const STACK_LIMIT: usize = 4 * 1024 * 1024;

struct Job {
    function: &'static str,
    input: String,
    reply: tokio::sync::oneshot::Sender<Result<Output, String>>,
    _slot: tokio::sync::OwnedSemaphorePermit,
}

pub struct Output {
    pub text: String,
    pub misses: Vec<Value>,
}

#[derive(Clone)]
pub struct Renderer {
    tx: mpsc::SyncSender<Job>,
    slots: Arc<tokio::sync::Semaphore>,
    wait: Duration,
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub workers: usize,
    pub queue: usize,
    pub wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { workers: 2, queue: 64, wait: Duration::from_millis(2000) }
    }
}

#[derive(Debug, PartialEq)]
pub enum CallError {
    Busy,
    Failed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Busy => write!(f, "renderer busy"),
            CallError::Failed(e) => write!(f, "{e}"),
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn run_filters(mirror: &Mirror, handle: &tokio::runtime::Handle, filters_json: &str, misses: &Mutex<Vec<Value>>) -> String {
    let parsed: Vec<Value> = serde_json::from_str(filters_json).unwrap_or_default();
    let events = handle.block_on(mirror.query(&parsed));
    if events.is_empty() {
        misses.lock().unwrap().extend(parsed);
    }
    let mut out = String::from("[");
    for (i, ev) in events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&ev.as_json());
    }
    out.push(']');
    out
}

pub fn count_filter(mirror: &Mirror, handle: &tokio::runtime::Handle, filter_json: &str) -> f64 {
    match serde_json::from_str::<Value>(filter_json) {
        Ok(filter) => handle.block_on(mirror.count(&filter)) as f64,
        Err(_) => 0.0,
    }
}

const MAX_LOAD_JOBS: usize = 1_000_000;

fn drain_jobs(rt: &Runtime) -> Result<(), String> {
    for _ in 0..MAX_LOAD_JOBS {
        match rt.execute_pending_job() {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => return Err(format!("pending job failed: {e:?}")),
        }
    }
    Err("pending jobs never settled".into())
}

struct Engine {
    _rt: Runtime,
    ctx: Context,
    loaded: Option<SystemTime>,
    misses: Arc<Mutex<Vec<Value>>>,
}

impl Engine {
    fn load(bundle: &Path, mirror: Mirror, handle: tokio::runtime::Handle, plugins: &[Arc<dyn Plugin>]) -> Result<Engine, String> {
        let source = std::fs::read_to_string(bundle).map_err(|e| format!("{}: {e}", bundle.display()))?;
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(MEMORY_LIMIT);
        rt.set_max_stack_size(STACK_LIMIT);
        let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
        let misses: Arc<Mutex<Vec<Value>>> = Default::default();
        let m = misses.clone();
        ctx.with(|ctx| {
            let hidden_mirror = mirror.clone();
            let count_mirror = mirror.clone();
            let count_handle = handle.clone();
            let count = Function::new(ctx.clone(), move |filter: String| -> f64 { count_filter(&count_mirror, &count_handle, &filter) }).map_err(|e| e.to_string())?;
            ctx.globals().set("relayCount", count).map_err(|e| e.to_string())?;
            let query = Function::new(ctx.clone(), move |filters: String| -> String { run_filters(&mirror, &handle, &filters, &m) }).map_err(|e| e.to_string())?;
            let hidden = Function::new(ctx.clone(), move || -> String { serde_json::to_string(&hidden_mirror.hidden_keys()).unwrap_or_else(|_| "[]".into()) }).map_err(|e| e.to_string())?;
            ctx.globals().set("relayHidden", hidden).map_err(|e| e.to_string())?;
            ctx.globals().set("relayQuery", query).map_err(|e| e.to_string())?;
            let log = Function::new(ctx.clone(), |level: String, msg: String| tracing::warn!("ssr {level}: {msg}")).map_err(|e| e.to_string())?;
            ctx.globals().set("hostLog", log).map_err(|e| e.to_string())?;
            for p in plugins {
                for f in p.js_functions() {
                    let (name, handler) = (f.name, f.handler.clone());
                    let already = ctx.globals().get::<_, Function>(name).is_ok();
                    if already {
                        continue;
                    }
                    let fn_ = Function::new(ctx.clone(), move |arg: String| handler(&arg)).map_err(|e| e.to_string())?;
                    ctx.globals().set(name, fn_).map_err(|e| e.to_string())?;
                }
            }
            ctx.eval::<(), _>(source.as_str()).map_err(|e| {
                let detail = ctx.catch().into_exception().map(|x| format!("{x}")).unwrap_or_else(|| e.to_string());
                format!("loading {}: {detail}", bundle.display())
            })?;
            Ok::<_, String>(())
        })?;
        drain_jobs(&rt).map_err(|e| format!("loading {}: {e}", bundle.display()))?;
        ctx.with(|ctx| ctx.globals().get::<_, Function>("ssrRender").map(|_| ()).map_err(|_| format!("{} does not define ssrRender", bundle.display())))?;
        Ok(Engine { _rt: rt, ctx, loaded: modified(bundle), misses })
    }

    fn run(&self, function: &str, input: &str, plugins: &[Arc<dyn Plugin>]) -> Result<Output, String> {
        self.misses.lock().unwrap().clear();
        let url: String = serde_json::from_str::<serde_json::Value>(input)
            .ok()
            .and_then(|v| v.get("url").and_then(serde_json::Value::as_str).map(String::from))
            .unwrap_or_default();
        let text = self.ctx.with(|ctx| {
            let f: Function = ctx.globals().get(function).map_err(|_| format!("bundle does not define {function}"))?;
            let failed = |e: rquickjs::Error| {
                let detail = ctx.catch().into_exception().map(|x| format!("{x}")).unwrap_or_else(|| e.to_string());
                format!("{function} failed: {detail}")
            };
            let value: rquickjs::Value = f.call((input,)).map_err(failed)?;
            match value.as_promise() {
                Some(promise) => promise.finish::<String>().map_err(failed),
                None => rquickjs::FromJs::from_js(&ctx, value).map_err(failed),
            }
        })?;
        let data = if function == "ssrRender" && !plugins.is_empty() {
            render_data(&text, &url, plugins)?
        } else {
            text
        };
        Ok(Output { text: data, misses: std::mem::take(&mut *self.misses.lock().unwrap()) })
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Rendered {
    #[serde(default)]
    html: serde_json::Value,
    #[serde(default = "default_status")]
    status: u16,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    data: serde_json::Value,
    #[serde(flatten)]
    rest: serde_json::Value,
}

fn default_status() -> u16 {
    200
}

fn render_data(text: &str, url: &str, plugins: &[Arc<dyn Plugin>]) -> Result<String, String> {
    let mut r: Rendered = serde_json::from_str(text).map_err(|e| format!("render output: {e}"))?;
    if r.data.is_object() {
        enrich_data(&mut r.data, url, r.status, plugins);
    }
    serde_json::to_string(&r).map_err(|e| e.to_string())
}

impl Renderer {
    pub fn start(bundle: PathBuf, workers: usize, mirror: Mirror) -> Renderer {
        Self::start_with_plugins(bundle, workers, mirror, vec![])
    }

    pub fn start_with_plugins(bundle: PathBuf, workers: usize, mirror: Mirror, plugins: Vec<Arc<dyn Plugin>>) -> Renderer {
        Self::start_with(bundle, Limits { workers, ..Limits::default() }, mirror, plugins)
    }

    pub fn start_with(bundle: PathBuf, limits: Limits, mirror: Mirror, plugins: Vec<Arc<dyn Plugin>>) -> Renderer {
        let handle = tokio::runtime::Handle::current();
        let workers = limits.workers.max(1);
        let capacity = workers + limits.queue;
        let (tx, rx) = mpsc::sync_channel::<Job>(capacity);
        let rx = Arc::new(Mutex::new(rx));
        for n in 0..workers {
            let rx = rx.clone();
            let bundle = bundle.clone();
            let mirror = mirror.clone();
            let handle = handle.clone();
            let plugins = plugins.clone();
            std::thread::Builder::new()
                .name(format!("ssr-{n}"))
                .spawn(move || {
                    let mut engine: Option<Engine> = None;
                    loop {
                        let job = match rx.lock().unwrap().recv() {
                            Ok(j) => j,
                            Err(_) => return,
                        };
                        if job.reply.is_closed() {
                            continue;
                        }
                        if engine.as_ref().map(|e| e.loaded != modified(&bundle)).unwrap_or(true) {
                            engine = match Engine::load(&bundle, mirror.clone(), handle.clone(), &plugins) {
                                Ok(e) => Some(e),
                                Err(e) => {
                                    let _ = job.reply.send(Err(e));
                                    continue;
                                }
                            };
                        }
                        let result = engine.as_ref().map(|e| e.run(job.function, &job.input, &plugins)).unwrap_or_else(|| Err("no engine".into()));
                        if result.is_err() {
                            engine = None;
                        }
                        let _ = job.reply.send(result);
                    }
                })
                .expect("spawn ssr worker");
        }
        Renderer { tx, slots: Arc::new(tokio::sync::Semaphore::new(capacity)), wait: limits.wait }
    }

    pub async fn call(&self, function: &'static str, input: String) -> Result<Output, CallError> {
        let slot = match tokio::time::timeout(self.wait, self.slots.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            _ => return Err(CallError::Busy),
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.try_send(Job { function, input, reply, _slot: slot }).map_err(|_| CallError::Busy)?;
        rx.await.map_err(|e| CallError::Failed(e.to_string()))?.map_err(CallError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{JsFunction, Plugin};
    use nostr::event::FinalizeEvent;
    use nostr_database::NostrDatabase;
    use serde_json::json;

    fn bundle(name: &str, source: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("nostr-ssr-{name}-{}.js", std::process::id()));
        std::fs::write(&path, source).unwrap();
        path
    }

    struct TestPlugin;
    impl Plugin for TestPlugin {
        fn name(&self) -> &'static str {
            "test"
        }
        fn js_functions(&self) -> Vec<JsFunction> {
            vec![JsFunction {
                name: "swarmStats",
                handler: std::sync::Arc::new(|arg: &str| {
                    let v: serde_json::Value = serde_json::from_str(arg).unwrap_or_default();
                    let ih = v.get("info_hash").and_then(serde_json::Value::as_str).unwrap_or("");
                    json!({ "seeds": ih.len(), "leeches": ih.len() / 2 }).to_string()
                }),
            }]
        }
        fn enrich(&self, _: &str, _: u16, data: &serde_json::Value) -> Option<serde_json::Value> {
            if data.get("fromPlugin").is_none() {
                Some(json!({ "fromPlugin": true }))
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn a_bundle_reads_the_mirror_through_relay_query_and_misses_are_reported() {
        let mirror = Mirror::new(0, None, &[]);
        let keys = nostr::Keys::generate();
        let ev = nostr::EventBuilder::text_note("hello").finalize(&keys).unwrap();
        mirror.database.save_event(&ev).await.unwrap();
        let path = bundle("query", "globalThis.ssrRender = (s) => JSON.stringify({found: JSON.parse(relayQuery(s)).length});");
        let r = Renderer::start(path, 1, mirror);
        let hit = r.call("ssrRender", json!([{"kinds": [1]}]).to_string()).await.unwrap();
        assert_eq!(hit.text, "{\"found\":1}");
        assert!(hit.misses.is_empty());
        let miss = r.call("ssrRender", json!([{"kinds": [30023]}]).to_string()).await.unwrap();
        assert_eq!(miss.text, "{\"found\":0}");
        assert_eq!(miss.misses, vec![json!({"kinds": [30023]})]);
    }

    #[tokio::test]
    async fn a_bundle_counts_mirror_events_through_relay_count() {
        let mirror = Mirror::new(0, None, &[]);
        let target = "a".repeat(64);
        for _ in 0..3 {
            let keys = nostr::Keys::generate();
            let ev = nostr::EventBuilder::new(nostr::Kind::Custom(7700), "").tag(nostr::Tag::parse(["e", target.as_str()]).unwrap()).finalize(&keys).unwrap();
            mirror.database.save_event(&ev).await.unwrap();
        }
        let r = Renderer::start(bundle("count", "globalThis.ssrRender = (s) => String(relayCount(s));"), 1, mirror);
        assert_eq!(r.call("ssrRender", json!({"kinds": [7700], "#e": [target]}).to_string()).await.unwrap().text, "3");
        assert_eq!(r.call("ssrRender", json!({"kinds": [7700], "#e": ["b".repeat(64)]}).to_string()).await.unwrap().text, "0");
        assert_eq!(r.call("ssrRender", "not json".into()).await.unwrap().text, "0");
    }

    #[tokio::test]
    async fn a_bundle_can_read_the_keys_its_result_filters_hide() {
        struct Hide;
        impl crate::filters::ResultFilter for Hide {
            fn keep(&self, _: &nostr::Event) -> bool {
                true
            }
            fn hidden_keys(&self) -> Vec<String> {
                vec!["b".into(), "a".into()]
            }
        }
        let mirror = Mirror::new(0, None, &[]).with_filters(vec![std::sync::Arc::new(Hide)]);
        let r = Renderer::start(bundle("hidden", "globalThis.ssrRender = () => relayHidden();"), 1, mirror);
        assert_eq!(r.call("ssrRender", "".into()).await.unwrap().text, "[\"a\",\"b\"]");
    }

    #[tokio::test]
    async fn a_throwing_bundle_names_the_error_and_recovers_when_fixed() {
        let path = bundle("throws", "globalThis.ssrRender = () => { throw new Error('boom'); };");
        let r = Renderer::start(path.clone(), 1, Mirror::new(0, None, &[]));
        let err = r.call("ssrRender", "x".into()).await.err().unwrap().to_string();
        assert!(err.contains("boom"), "{err}");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&path, "globalThis.ssrRender = () => 'fixed';").unwrap();
        assert_eq!(r.call("ssrRender", "x".into()).await.unwrap().text, "fixed");
    }

    #[tokio::test]
    async fn a_bundle_can_finish_async_setup_before_its_first_render() {
        let source = "Promise.resolve('ready').then(v => v + '!').then(v => { globalThis.ssrRender = () => v; });";
        let r = Renderer::start(bundle("async-setup", source), 1, Mirror::new(0, None, &[]));
        assert_eq!(r.call("ssrRender", "x".into()).await.unwrap().text, "ready!");
    }

    #[tokio::test]
    async fn a_render_may_return_a_promise() {
        let source = "globalThis.ssrRender = async (s) => { const v = await Promise.resolve(s); return v + JSON.parse(relayQuery('[]')).length; };";
        let r = Renderer::start(bundle("async-render", source), 1, Mirror::new(0, None, &[]));
        assert_eq!(r.call("ssrRender", "n".into()).await.unwrap().text, "n0");
    }

    #[tokio::test]
    async fn a_render_promise_that_never_settles_is_an_error() {
        let r = Renderer::start(bundle("stuck-render", "globalThis.ssrRender = () => new Promise(() => {});"), 1, Mirror::new(0, None, &[]));
        assert!(r.call("ssrRender", "x".into()).await.is_err());
    }

    #[tokio::test]
    async fn plugins_expose_js_functions_and_enrich_the_hydration_data() {
        let mirror = Mirror::new(0, None, &[]);
        let path = bundle("plugin", r#"
globalThis.ssrRender = (json) => {
  const { url } = JSON.parse(json);
  if (url.startsWith("/e/")) {
    const swarm = JSON.parse(swarmStats(JSON.stringify({ info_hash: "abc123" })));
    return JSON.stringify({ html: "<div>" + swarm.seeds + "s" + swarm.leeches + "l</div>", status: 200, head: { title: "T" }, data: { queries: {} } });
  }
  return JSON.stringify({ html: "<p>home</p>", status: 200, head: { title: "H" }, data: { queries: {} } });
};
"#);
        let r = Renderer::start_with_plugins(path, 1, mirror, vec![std::sync::Arc::new(TestPlugin)]);
        let home = r.call("ssrRender", json!({ "url": "/", "now": 0, "origin": "https://x" }).to_string()).await.unwrap();
        let hd: serde_json::Value = serde_json::from_str(&home.text).unwrap();
        assert_eq!(hd["html"], "<p>home</p>");
        let e = r.call("ssrRender", json!({ "url": "/e/nevent1abc", "now": 0, "origin": "https://x" }).to_string()).await.unwrap();
        let ev: serde_json::Value = serde_json::from_str(&e.text).unwrap();
        assert_eq!(ev["data"]["queries"], json!({}));
        assert_eq!(ev["data"]["fromPlugin"], true);
        assert!(ev["html"].as_str().unwrap().contains("6s3l"));
    }

    #[tokio::test]
    async fn a_bundle_without_hydration_data_still_renders_with_plugins_loaded() {
        let path = bundle("nodata", r#"globalThis.ssrRender = () => JSON.stringify({ html: "<p>x</p>", status: 200, head: {} });"#);
        let r = Renderer::start_with_plugins(path, 1, Mirror::new(0, None, &[]), vec![std::sync::Arc::new(TestPlugin)]);
        let out = r.call("ssrRender", json!({ "url": "/" }).to_string()).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["html"], "<p>x</p>");
        assert!(v.get("data").is_none());
    }

    const SPIN: &str = "globalThis.ssrRender = (s) => { const until = Date.now() + 300; while (Date.now() < until) {} return s; };";

    fn limits(workers: usize, queue: usize, wait_ms: u64) -> Limits {
        Limits { workers, queue, wait: std::time::Duration::from_millis(wait_ms), ..Limits::default() }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_full_queue_waits_briefly_for_a_slot_then_reports_busy() {
        let r = Renderer::start_with(bundle("full-queue", SPIN), limits(1, 1, 100), Mirror::new(0, None, &[]), vec![]);
        let calls: Vec<_> = (0..3)
            .map(|n| {
                let r = r.clone();
                tokio::spawn(async move { r.call("ssrRender", format!("{n}")).await })
            })
            .collect();
        let started = std::time::Instant::now();
        let mut results = Vec::new();
        for c in calls {
            results.push(c.await.unwrap());
        }
        let busy: Vec<_> = results.iter().filter(|r| matches!(r, Err(CallError::Busy))).collect();
        assert_eq!(busy.len(), 1, "one call is shed");
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 2);
        assert!(started.elapsed() >= std::time::Duration::from_millis(550));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_queued_call_takes_a_slot_that_frees_within_the_wait() {
        let r = Renderer::start_with(bundle("wait-queue", SPIN), limits(1, 1, 2000), Mirror::new(0, None, &[]), vec![]);
        let calls: Vec<_> = (0..3)
            .map(|n| {
                let r = r.clone();
                tokio::spawn(async move { r.call("ssrRender", format!("{n}")).await })
            })
            .collect();
        for c in calls {
            assert!(c.await.unwrap().is_ok());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_job_whose_caller_gave_up_is_skipped() {
        let path = bundle("gave-up", "globalThis.n = 0; globalThis.ssrRender = (s) => { const until = Date.now() + 200; while (Date.now() < until) {} globalThis.n++; return String(globalThis.n); };");
        let r = Renderer::start_with(path, limits(1, 4, 2000), Mirror::new(0, None, &[]), vec![]);
        let first = {
            let r = r.clone();
            tokio::spawn(async move { r.call("ssrRender", "a".into()).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let r2 = r.clone();
        let abandoned = tokio::spawn(async move { r2.call("ssrRender", "b".into()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        abandoned.abort();
        assert_eq!(first.await.unwrap().unwrap().text, "1");
        assert_eq!(r.call("ssrRender", "c".into()).await.unwrap().text, "2");
    }

    #[tokio::test]
    async fn a_missing_bundle_or_function_is_an_error_not_a_panic() {
        let r = Renderer::start("/nonexistent/render.js".into(), 1, Mirror::new(0, None, &[]));
        assert!(r.call("ssrRender", "x".into()).await.is_err());
        let r = Renderer::start(bundle("nofn", "globalThis.other = 1;"), 1, Mirror::new(0, None, &[]));
        assert!(r.call("ssrRender", "x".into()).await.is_err());
    }
}
