//! Diagnostic example installed into ethlambda-engine, not a full beacon replay.
use ethlambda_engine::{EngineClient, ForkchoiceStateV1, JwtSecret, PayloadStatusValue};
use ethlambda_types::beacon::primitives::ExecutionBlockHash;
use serde_json::json;
use spawned_concurrency::{message::Message, tasks::{Actor, ActorStart, Backend, Context, Handler}};
use std::{hint::black_box, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

struct Begin {
    id: u64,
    sent: Instant,
    reply: tokio::sync::oneshot::Sender<[u64; 5]>,
}
impl Message for Begin { type Result = (); }
struct Answer {
    sent: Instant,
    dispatched: Instant,
    helper: Instant,
    answered: Instant,
    reply: tokio::sync::oneshot::Sender<[u64; 5]>,
}
impl Message for Answer { type Result = (); }
struct Critical { client: EngineClient }
impl Actor for Critical {}
fn ns(d: Duration) -> u64 { d.as_nanos().try_into().unwrap() }
impl Handler<Begin> for Critical {
    async fn handle(&mut self, msg: Begin, ctx: &Context<Self>) {
        let dispatched = Instant::now();
        let client = self.client.clone();
        let actor = ctx.actor_ref();
        tokio::spawn(async move {
            let helper = Instant::now();
            let mut hash = [0u8; 32];
            hash[24..].copy_from_slice(&msg.id.to_be_bytes());
            let state = ForkchoiceStateV1 {
                head_block_hash: ExecutionBlockHash::from(hash),
                safe_block_hash: ExecutionBlockHash::ZERO,
                finalized_block_hash: ExecutionBlockHash::ZERO,
            };
            let status = client.forkchoice_updated(&state).await.expect("mock RPC must succeed");
            assert_eq!(status.status, PayloadStatusValue::Syncing);
            let answered = Instant::now();
            actor.send(Answer { sent: msg.sent, dispatched, helper, answered, reply: msg.reply }).unwrap();
        });
    }
}
impl Handler<Answer> for Critical {
    async fn handle(&mut self, msg: Answer, _: &Context<Self>) {
        let applied = Instant::now();
        msg.reply.send([
            ns(msg.dispatched.duration_since(msg.sent)),
            ns(msg.helper.duration_since(msg.dispatched)),
            ns(msg.answered.duration_since(msg.helper)),
            ns(applied.duration_since(msg.answered)),
            ns(applied.duration_since(msg.sent)),
        ]).unwrap();
    }
}
struct Noise;
impl Message for Noise { type Result = u64; }
struct Background;
impl Actor for Background {}
impl Handler<Noise> for Background {
    async fn handle(&mut self, _: Noise, _: &Context<Self>) -> u64 {
        let mut x = 17u64;
        for _ in 0..50_000 { x = black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1)); }
        x
    }
}
fn stats(values: impl Iterator<Item=u64>) -> serde_json::Value {
    let mut v: Vec<_> = values.collect();
    v.sort_unstable();
    let n = v.len();
    assert!(n > 0);
    json!({"n": n, "mean_ns": v.iter().map(|&x| x as f64).sum::<f64>() / n as f64,
        "p50_ns": v[(n as f64 * 0.50).ceil() as usize - 1],
        "p99_ns": v[(n as f64 * 0.99).ceil() as usize - 1], "max_ns": v[n-1]})
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 9, "endpoint async|thread idle|busy seconds warmup sample_every concurrency gap_ms");
    let endpoint = args[1].clone();
    assert!(endpoint.starts_with("http://127.0.0.1:"));
    let backend = match args[2].as_str() { "async" => Backend::Async, "thread" => Backend::Thread, _ => panic!("bad backend") };
    let busy = match args[3].as_str() { "idle" => false, "busy" => true, _ => panic!("bad load") };
    let seconds: u64 = args[4].parse().unwrap();
    let warmup: u64 = args[5].parse().unwrap();
    let sample: u64 = args[6].parse().unwrap();
    let concurrency: usize = args[7].parse().unwrap();
    let gap_ms: u64 = args[8].parse().unwrap();
    assert!((1..=16).contains(&concurrency) && gap_ms <= 100);
    assert!((1..=10).contains(&seconds) && warmup <= 100);
    #[cfg(feature="probe-metrics")]
    spawned_concurrency::metrics::configure(spawned_concurrency::metrics::Config { timing_sample_every: sample }).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(3).enable_all().build().unwrap();
    rt.block_on(async {
        let actor = Critical { client: EngineClient::new(endpoint, JwtSecret::new([0x0f; 32])).unwrap() }.start_with_backend(backend);
        let backgrounds: Vec<_> = if busy { (0..16).map(|_| Background.start()).collect() } else { Vec::new() };
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for p in 0..if busy { 128 } else { 0 } {
            let background = backgrounds[p % backgrounds.len()].clone();
            let stop = stop.clone();
            workers.push(tokio::spawn(async move {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    black_box(background.request(Noise).await.unwrap());
                    n += 1;
                }
                n
            }));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut id = 0;
        for _ in 0..warmup {
            let (tx, rx) = tokio::sync::oneshot::channel();
            actor.send(Begin { id, sent: Instant::now(), reply: tx }).unwrap();
            tokio::time::timeout(Duration::from_secs(5), rx).await.unwrap().unwrap();
            id += 1;
        }
        let began = Instant::now();
        let deadline = began + Duration::from_secs(seconds);
        let mut streams = Vec::new();
        for producer in 0..concurrency {
            let actor = actor.clone();
            let mut next = id + producer as u64 * 1_000_000;
            streams.push(tokio::spawn(async move {
                let mut samples = Vec::new();
                while Instant::now() < deadline {
                    if gap_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(gap_ms)).await;
                        if Instant::now() >= deadline { break; }
                    }
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    actor.send(Begin { id: next, sent: Instant::now(), reply: tx }).unwrap();
                    samples.push(tokio::time::timeout(Duration::from_secs(5), rx).await.unwrap().unwrap());
                    next += 1;
                    assert!(samples.len() < 100_000, "bounded sample storage");
                }
                samples
            }));
        }
        let mut samples = Vec::new();
        for stream in streams { samples.extend(stream.await.unwrap()); }
        let elapsed = began.elapsed();
        stop.store(true, Ordering::Relaxed);
        let mut noise = 0;
        for job in workers { noise += job.await.unwrap(); }
        actor.context().stop();
        for a in &backgrounds { a.context().stop(); }
        actor.join().await;
        for a in backgrounds { a.join().await; }
        let fields = ["initial_queue", "helper_schedule", "engine_call", "answer_queue", "total"];
        let mut stages = serde_json::Map::new();
        for (i, name) in fields.iter().enumerate() { stages.insert(name.to_string(), stats(samples.iter().map(|v| v[i]))); }
        #[cfg(feature="probe-metrics")]
        {
            let s = spawned_concurrency::metrics::snapshot();
            assert_eq!(s.actors_active + s.handlers_active + s.requests_active + s.mailbox_depth, 0);
            assert_eq!(s.messages_enqueued_total, s.messages_handled_total + s.messages_discarded_total);
        }
        println!("{}", json!({"backend": args[2], "load": args[3], "metrics": cfg!(feature="probe-metrics"),
            "sample_every": sample, "concurrency": concurrency, "gap_ms": gap_ms, "requests": samples.len(), "warmup": warmup, "elapsed_s": elapsed.as_secs_f64(),
            "requests_per_second": samples.len() as f64/elapsed.as_secs_f64(), "background_completions_including_warmup": noise,
            "stages": stages, "samples_ns": samples}));
    });
}
