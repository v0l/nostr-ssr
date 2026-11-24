use nostr::nips::nip19::{Nip19Event, Nip19Profile, ToBech32};
use nostr::prelude::*;
use nostr_relay_builder::LocalRelay;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const WORDS: [&str; 24] = ["nostr", "bitcoin", "zap", "relay", "gm", "pura", "vida", "lightning", "freedom", "client", "note", "the", "a", "is", "and", "of", "to", "we", "build", "open", "protocol", "keys", "sats", "plebs"];

fn text(rng: &mut Rng, min: u64, max: u64) -> String {
    let len = min + rng.below(max - min);
    let mut s = String::with_capacity(len as usize + 16);
    while (s.len() as u64) < len {
        s.push_str(WORDS[rng.below(WORDS.len() as u64) as usize]);
        s.push(' ');
        if rng.below(40) == 0 {
            s.push_str("#");
            s.push_str(WORDS[rng.below(8) as usize]);
            s.push(' ');
        }
    }
    s
}

fn profile(rng: &mut Rng, keys: &Keys, at: u64) -> Event {
    let max = if rng.below(20) == 0 { 6000 } else { 600 };
    let about = text(rng, 40, max);
    let content = serde_json::json!({ "name": format!("pleb{}", rng.below(1_000_000)), "about": about, "picture": format!("https://img.example/{}.jpg", rng.next()), "nip05": "x@example.com", "lud16": "x@example.com" }).to_string();
    EventBuilder::new(Kind::Metadata, content).custom_created_at(Timestamp::from_secs(at)).finalize(keys).unwrap()
}

fn note(rng: &mut Rng, keys: &Keys, at: u64, refs: &[(EventId, PublicKey)]) -> Event {
    let mut tags = Vec::new();
    for _ in 0..rng.below(8) {
        if refs.is_empty() {
            break;
        }
        let (id, pk) = refs[rng.below(refs.len() as u64) as usize];
        tags.push(Tag::event(id));
        tags.push(Tag::public_key(pk));
    }
    if rng.below(4) == 0 {
        tags.push(Tag::hashtag(WORDS[rng.below(8) as usize]));
    }
    let max = if rng.below(30) == 0 { 4000 } else { 500 };
    let body = text(rng, 20, max);
    EventBuilder::text_note(body).tags(tags).custom_created_at(Timestamp::from_secs(at)).finalize(keys).unwrap()
}

fn article(rng: &mut Rng, keys: &Keys, at: u64) -> Event {
    let tags = [Tag::identifier(format!("a{}", rng.next())), Tag::parse(["title", &text(rng, 10, 60)]).unwrap(), Tag::parse(["summary", &text(rng, 40, 200)]).unwrap()];
    EventBuilder::new(Kind::LongFormTextNote, text(rng, 2000, 20000)).tags(tags).custom_created_at(Timestamp::from_secs(at)).finalize(keys).unwrap()
}

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned()).unwrap_or_else(|| default.to_string())
}

fn rss_kib(pid: &str) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse().ok()).unwrap_or(0)
}

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    ok: AtomicU64,
    shed: AtomicU64,
    other: AtomicU64,
    slow: AtomicU64,
    total_ms: AtomicU64,
    max_ms: AtomicU64,
}

#[tokio::main]
async fn main() {
    let port: u16 = arg("--upstream-port", "7801").parse().unwrap();
    let events: usize = arg("--events", "150000").parse().unwrap();
    let live_per_sec: u64 = arg("--live", "10").parse().unwrap();
    let rps: u64 = arg("--rps", "2").parse().unwrap();
    let minutes: u64 = arg("--minutes", "30").parse().unwrap();
    let target = arg("--target", "");
    let pid = arg("--pid", "");
    let pid_file = arg("--pid-file", "");
    let mut rng = Rng(0x9e3779b97f4a7c15);

    let relay = LocalRelay::builder().port(port).max_filter_limit(5000).build();
    relay.run().await.unwrap();
    eprintln!("upstream {}", relay.url().await);

    let authors: Vec<Keys> = (0..4000).map(|_| Keys::generate()).collect();
    let now = Timestamp::now().as_secs();
    let mut refs: Vec<(EventId, PublicKey)> = Vec::new();
    let mut profiles = Vec::new();
    let started = Instant::now();
    for (i, k) in authors.iter().enumerate() {
        relay.add_event(profile(&mut rng, k, now - 86_400 - i as u64)).await.unwrap();
        profiles.push(k.public_key());
    }
    for i in 0..events.saturating_sub(authors.len()) {
        let k = &authors[rng.below(authors.len() as u64) as usize];
        let at = now - 60 - (i as u64 * 7) % (86_400 * 30);
        let ev = if i % 50 == 0 { article(&mut rng, k, at) } else { note(&mut rng, k, at, &refs) };
        if refs.len() < 20_000 {
            refs.push((ev.id, ev.pubkey));
        } else {
            let slot = rng.below(refs.len() as u64) as usize;
            refs[slot] = (ev.id, ev.pubkey);
        }
        relay.add_event(ev).await.unwrap();
    }
    eprintln!("seeded {events} events in {:?}", started.elapsed());
    let warmup: u64 = arg("--warmup", "0").parse().unwrap();
    tokio::time::sleep(Duration::from_secs(warmup)).await;
    if target.is_empty() {
        eprintln!("no --target, serving upstream only");
        tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
        return;
    }

    let live_relay = relay.clone();
    let mut live_rng = Rng(42);
    let live_refs = refs.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(1000 / live_per_sec.max(1)));
        loop {
            tick.tick().await;
            let fresh = Keys::generate();
            let at = Timestamp::now().as_secs();
            let _ = live_relay.add_event(profile(&mut live_rng, &fresh, at - 5)).await;
            let _ = live_relay.add_event(note(&mut live_rng, &fresh, at, &live_refs)).await;
        }
    });

    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let counters = Arc::new(Counters::default());
    let paths_rng = Arc::new(std::sync::Mutex::new(Rng(7)));
    let refs = Arc::new(refs);
    let profiles = Arc::new(profiles);
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    let sampler = {
        let counters = counters.clone();
        let (pid, pid_file) = (pid.clone(), pid_file.clone());
        tokio::spawn(async move {
            let begun = Instant::now();
            println!("min,rss_mib,sent,ok,shed,other,slow_over_1s,avg_ms,max_ms");
            loop {
                let pid = if pid_file.is_empty() { pid.clone() } else { std::fs::read_to_string(&pid_file).unwrap_or_default().trim().to_string() };
                let sent = counters.sent.load(Ordering::Relaxed);
                let done = counters.ok.load(Ordering::Relaxed) + counters.shed.load(Ordering::Relaxed) + counters.other.load(Ordering::Relaxed);
                println!(
                    "{:.1},{},{},{},{},{},{},{},{}",
                    begun.elapsed().as_secs_f64() / 60.0,
                    rss_kib(&pid) / 1024,
                    sent,
                    counters.ok.load(Ordering::Relaxed),
                    counters.shed.load(Ordering::Relaxed),
                    counters.other.load(Ordering::Relaxed),
                    counters.slow.load(Ordering::Relaxed),
                    counters.total_ms.load(Ordering::Relaxed) / done.max(1),
                    counters.max_ms.swap(0, Ordering::Relaxed)
                );
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        })
    };
    let mut tick = tokio::time::interval(Duration::from_millis(1000 / rps.max(1)));
    while Instant::now() < deadline {
        tick.tick().await;
        let path = {
            let mut r = paths_rng.lock().unwrap();
            match r.below(100) {
                0..=4 => "/".to_string(),
                5..=39 => {
                    let pk = profiles[r.below(profiles.len() as u64) as usize];
                    format!("/{}", Nip19Profile::new(pk, Vec::<RelayUrl>::new()).to_bech32().unwrap())
                }
                40..=49 => format!("/{}", Nip19Profile::new(Keys::generate().public_key(), Vec::<RelayUrl>::new()).to_bech32().unwrap()),
                50..=79 => {
                    let (id, pk) = refs[r.below(refs.len() as u64) as usize];
                    let nevent = Nip19Event::new(id).author(pk).to_bech32().unwrap();
                    if r.below(2) == 0 { format!("/e/{nevent}") } else { format!("/{nevent}") }
                }
                80..=89 => format!("/{}", EventId::from_byte_array(Keys::generate().public_key().to_bytes()).to_bech32().unwrap()),
                90..=94 => format!("/p/{}", profiles[r.below(profiles.len() as u64) as usize].to_bech32().unwrap()),
                _ => format!("/t/{}", WORDS[r.below(8) as usize]),
            }
        };
        let (client, counters, url) = (client.clone(), counters.clone(), format!("{target}{path}"));
        counters.sent.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let t = Instant::now();
            let status = client.get(&url).send().await.map(|r| r.status().as_u16()).unwrap_or(0);
            let ms = t.elapsed().as_millis() as u64;
            counters.total_ms.fetch_add(ms, Ordering::Relaxed);
            counters.max_ms.fetch_max(ms, Ordering::Relaxed);
            if ms > 1000 {
                counters.slow.fetch_add(1, Ordering::Relaxed);
            }
            match status {
                200 | 404 => counters.ok.fetch_add(1, Ordering::Relaxed),
                503 => counters.shed.fetch_add(1, Ordering::Relaxed),
                _ => counters.other.fetch_add(1, Ordering::Relaxed),
            };
        });
    }
    tokio::time::sleep(Duration::from_secs(11)).await;
    sampler.abort();
}
