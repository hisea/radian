//! Host-side latency baseline for a turn (plan Phase 7): a whole turn and one tool round
//! trip, with an **in-process** model and tool that answer immediately. Network, the HTTP
//! adapter and the terminal are excluded; the end-to-end (mock HTTP + pty) numbers,
//! including first-token, are in `radian-tui/tests/perf_startup.rs`.
//!
//! Measurement, not an assertion: `#[ignore]`d. Run:
//!
//! ```sh
//! cargo test --release -p radian-runtime --test perf_runtime -- --ignored --nocapture
//! ```

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_core::{Agent, InputSource, ModelResponse, TurnOptions};
use radian_runtime::{Driver, Inbox};
use radian_session::SessionJournal;
use serde_json::json;

struct Env {
    inbox: Arc<Inbox>,
    recorder: Arc<Recorder>,
    #[allow(dead_code)]
    dir: TempDir,
}

async fn open(
    label: &str,
    replies: Vec<ModelResponse>,
) -> (Driver<Arc<ScriptedModel>, MemTools, ()>, Env) {
    let dir = TempDir::new(label);
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "perf", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let model = ScriptedModel::new(replies);
    let agent = Arc::new(Agent::new(Arc::clone(&model), MemTools::new()));
    let driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn radian_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    (
        driver,
        Env {
            inbox,
            recorder,
            dir,
        },
    )
}

fn median_p95(mut samples: Vec<Duration>) -> (Duration, Duration, Duration) {
    samples.sort();
    let n = samples.len();
    (
        samples[n / 2],
        samples[(n * 95 / 100).min(n - 1)],
        samples[n - 1],
    )
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// One model call, no tool: the turn the host runs for a plain answer (in-process).
#[tokio::test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
async fn perf_single_call_turn() {
    let samples = 100;
    let mut turns = Vec::with_capacity(samples);
    for i in 0..samples {
        let (mut driver, env) = open(&format!("perf-one-{i}"), vec![text("hello")]).await;
        let handle = tokio::spawn(async move { driver.run().await });
        let start = Instant::now();
        env.inbox.followup("hi", InputSource::User).await.unwrap();
        env.recorder.wait_finished().await;
        turns.push(start.elapsed());
        handle.abort();
    }
    let (m, p, x) = median_p95(turns);
    println!(
        "single-call turn (in-process): median {:.3} ms  p95 {:.3} ms  max {:.3} ms",
        ms(m),
        ms(p),
        ms(x)
    );
}

/// One tool call then a final answer: two model calls and one immediate tool.
#[tokio::test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
async fn perf_tool_round_trip() {
    let samples = 100;
    let mut turns = Vec::with_capacity(samples);
    for i in 0..samples {
        let script = vec![
            calls(vec![call("c1", "echo", json!({"a": 1}))]),
            text("done"),
        ];
        let (mut driver, env) = open(&format!("perf-tool-{i}"), script).await;
        let handle = tokio::spawn(async move { driver.run().await });
        let start = Instant::now();
        env.inbox.followup("go", InputSource::User).await.unwrap();
        env.recorder.wait_finished().await;
        turns.push(start.elapsed());
        handle.abort();
    }
    let (m, p, x) = median_p95(turns);
    println!(
        "tool round trip (in-process, 2 calls + 1 tool): median {:.3} ms  p95 {:.3} ms  max {:.3} ms",
        ms(m),
        ms(p),
        ms(x)
    );
}
