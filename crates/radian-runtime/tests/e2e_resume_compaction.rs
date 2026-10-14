//! End-to-end: journal → resume → compaction → **the request a real adapter sends**.
//!
//! The other tests each cover one link: `compact_control.rs` the compaction commit,
//! `budget.rs` resume, the adapters' `http.rs` the request body. This one chains them with
//! the **real adapter** and looks at the last HTTP request after a resume, which is where
//! ADR 0018 (revision 6: the user's words survive verbatim, the read-back uses `origin`),
//! ADR 0019 (the replacement commit) and ADR 0005 (host meta invisible to adapters) have to
//! hold together. Both adapters run the same scenario.
//!
//! Scenario: three turns (the first user message is short and carries JSON special characters,
//! so it can survive a compaction verbatim; the second and third assistant replies are the
//! huge ones, and steer the compaction boundary), one manual compaction, close; resume, a
//! fourth turn; compact again, resume, a fifth turn. Assertions read the recorded request
//! bodies (parsed, not raw — the special characters are JSON-escaped on the wire) and the
//! journal's revision records.
//!
//! **Journal self-check coverage**: this test lives in `radian-runtime`, so the AGENTS.md
//! command `cargo test -p agent-core --features journal-selfcheck` does *not* run it (that
//! builds agent-core's own tests only). `radian-runtime` forwards the same feature, so
//! `cargo test -p radian-runtime --features journal-selfcheck --test e2e_resume_compaction`
//! runs it with the self-check armed, and that run is part of this slice's verification. The
//! plain `cargo test --workspace` run covers it without the feature.

mod common;

use std::path::Path;
use std::sync::Arc;

use agent_core::{Agent, AgentConfig, ContextBudget, InputSource, NoTools, TurnOptions};
use common::{Recorder, TempDir, created};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};

/// The first user message: the JSON special characters must survive a compaction **verbatim**,
/// so it stays short (a long message would legitimately be cut to the verbatim budget, ADR
/// 0018 revision 6).
fn first_message() -> &'static str {
    "\"引号\" \\ 反斜杠 {\"k\":1} TASK-ONE"
}

/// A huge assistant reply used to steer the compaction boundary, made of a distinct filler
/// character so the two are distinguishable in an assertion. The plan walks the turn units
/// backwards and stops at the first unit that does not fit the keep target:
/// - the second reply is huge, so the first compaction summarizes units 1 and 2 (both user
///   messages are short, so they survive **verbatim** in `origin`) and keeps unit 3;
/// - the third reply is huge too, so the second compaction (now over `[summary1, unit3]`)
///   summarizes `summary1` and unit 3 and keeps the fourth turn's unit.
fn big_reply(filler: char) -> String {
    filler.to_string().repeat(16_000)
}

const A1: &str = "ASSISTANT-ONE-REPLY";
const M2: &str = "SECOND-USER-MESSAGE-2";
const M3: &str = "THIRD-USER-MESSAGE-3";
const M4: &str = "FOURTH-USER-MESSAGE-4";
const M5: &str = "FIFTH-USER-MESSAGE-5";
const SUMMARY1: &str = "SUMMARY-ONE-TEXT";
const SUMMARY2: &str = "SUMMARY-TWO-TEXT";

/// Every string in a JSON document, in document order. Adapter-agnostic: it does not care
/// whether a message's text sits in a `content` string or a `text` block.
fn all_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => out.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                all_strings(item, out);
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values() {
                all_strings(value, out);
            }
        }
        _ => {}
    }
}

/// The request's text as one searchable string: the parsed document's strings, joined. The
/// raw body cannot be searched for the user's words: JSON escapes them on the wire.
fn request_text(body: &str) -> String {
    let value: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|error| panic!("body is not JSON: {error}"));
    let mut strings = Vec::new();
    all_strings(&value, &mut strings);
    strings.join("\n")
}

/// Whether a JSON key appears anywhere in the document (the host-meta leak check).
fn has_key(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::Array(items) => items.iter().any(|item| has_key(item, needle)),
        serde_json::Value::Object(fields) => {
            fields.contains_key(needle) || fields.values().any(|value| has_key(value, needle))
        }
        _ => false,
    }
}

/// Drives `inputs` as turns, waiting for each turn to finish.
async fn turns(inbox: &Arc<Inbox>, recorder: &Arc<Recorder>, inputs: &[&str]) {
    for input in inputs {
        inbox.followup(*input, InputSource::User).await.unwrap();
        recorder.wait_finished().await;
    }
}

/// The configuration shared by every driver: `bytes_per_token 1` keeps the arithmetic in
/// bytes, so the 16k assistant replies are far over the keep target and a manual compaction
/// has a prefix to summarize.
fn agent_config() -> AgentConfig {
    AgentConfig {
        context_budget: Some(ContextBudget::new(3_000).with_bytes_per_token(1)),
        auto_compact: false,
        ..Default::default()
    }
}

/// The committed replacement revisions, in journal order.
fn committed_revisions(path: &Path) -> Vec<u64> {
    Session::open(path)
        .unwrap()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::TranscriptReplacementCommitted { revision, .. } => Some(*revision),
            _ => None,
        })
        .collect()
}

/// Opens a driver over `journal`, spawns it, and returns the handle plus the recorder.
#[allow(clippy::type_complexity)]
async fn open<M: agent_core::Model + 'static>(
    agent: &Arc<Agent<M, NoTools, ()>>,
    journal: Arc<SessionJournal>,
) -> (
    tokio::task::JoinHandle<Result<(), radian_runtime::DriverError>>,
    Arc<Inbox>,
    Arc<Recorder>,
) {
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let mut driver = Driver::open(
        agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    let handle = tokio::spawn(async move { driver.run().await });
    (handle, inbox, recorder)
}

/// Closes the inbox and awaits the driver (releasing the journal's single-writer lock).
async fn stop(
    inbox: Arc<Inbox>,
    handle: tokio::task::JoinHandle<Result<(), radian_runtime::DriverError>>,
) {
    inbox.close();
    let result = handle.await.expect("the driver task does not panic");
    result.expect("the driver exits cleanly");
}

/// The shared scenario, also producing the request bodies of one driver (indexed by driver).
macro_rules! e2e_scenario {
    ($name:ident, $server:path, $model:path, $reply:path, $bodies:path) => {
        #[tokio::test]
        async fn $name() {
            let m1 = first_message();
            let a2 = big_reply('y');
            let a3 = big_reply('w');
            let server = <$server>::start(vec![
                $reply(A1),
                $reply(&a2),
                $reply(&a3),
                $reply(SUMMARY1),
                $reply("FOURTH-ASSISTANT-REPLY"),
                $reply(SUMMARY2),
                $reply("FIFTH-ASSISTANT-REPLY"),
            ])
            .await;
            let dir = TempDir::new("e2e-resume-compaction");
            let path = dir.file("s.jsonl");

            // ---- driver 1: three turns, one manual compaction, close ----
            let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
            let agent = Arc::new(Agent::new($model(&server), NoTools).with_config(agent_config()));
            let (handle, inbox, recorder) = open(&agent, Arc::clone(&journal)).await;
            turns(&inbox, &recorder, &[m1, M2, M3]).await;
            let outcome = inbox
                .compact(None)
                .await
                .expect("the first compaction commits");
            assert!(
                matches!(
                    outcome,
                    agent_core::CompactionOutcome::Compacted { revision: 1, .. }
                ),
                "{outcome:?}"
            );
            stop(inbox, handle).await;
            drop(journal);

            // ---- driver 2: resume, a fourth turn, a second compaction, close ----
            let journal = Arc::new(SessionJournal::resume(&path, "sid").unwrap());
            let (handle, inbox, recorder) = open(&agent, Arc::clone(&journal)).await;
            turns(&inbox, &recorder, &[M4]).await;

            // The fourth turn's request is the last one so far: ADR 0018's read-back
            // and ADR 0019's replacement commit, observed on the wire.
            let bodies = $bodies(&server);
            let body = bodies.last().expect("turn 4 sent a request").clone();
            let text = request_text(&body);
            assert!(text.contains(SUMMARY1), "the summary is sent");
            assert!(text.contains(m1), "the first user message verbatim");
            assert!(text.contains(M2), "the second user message verbatim");
            assert!(text.contains(M3), "the third user message verbatim");
            assert!(
                !text.contains(A1) && !text.contains(&a2),
                "the compressed-away assistant replies must not be sent"
            );
            assert!(
                text.contains(&a3),
                "the kept (huge) assistant reply stays, unlike the compressed one"
            );
            assert!(
                text.find(SUMMARY1).unwrap() < text.find(M4).unwrap(),
                "the summary precedes the new message"
            );
            // ADR 0005: host meta is invisible to adapters. The transcript messages do carry
            // `meta.source = user` and the summary carries `meta.origin.user_messages`, so a
            // leak would show one of these keys.
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            for key in ["meta", "origin", "user_messages"] {
                assert!(
                    !has_key(&value, key),
                    "host `{key}` leaked into the request"
                );
            }

            let outcome = inbox
                .compact(None)
                .await
                .expect("the second compaction commits");
            assert!(
                matches!(
                    outcome,
                    agent_core::CompactionOutcome::Compacted { revision: 2, .. }
                ),
                "{outcome:?}"
            );
            stop(inbox, handle).await;
            drop(journal);

            // ---- driver 3: resume again, a fifth turn ----
            let journal = Arc::new(SessionJournal::resume(&path, "sid").unwrap());
            let (handle, inbox, recorder) = open(&agent, Arc::clone(&journal)).await;
            turns(&inbox, &recorder, &[M5]).await;
            let bodies = $bodies(&server);
            let text = request_text(bodies.last().expect("turn 5 sent a request"));
            assert!(
                text.contains(m1),
                "the first user's words survive the second compaction"
            );
            assert!(text.contains(SUMMARY2), "the rolling summary is sent");
            assert!(text.contains(M5), "the fifth message is sent");
            stop(inbox, handle).await;
            drop(journal);

            // ADR 0019: the revisions strictly increase (1 then 2).
            let revisions = committed_revisions(&path);
            assert_eq!(revisions, vec![1, 2], "revisions: {revisions:?}");
        }
    };
}

// ---------- Anthropic ----------

fn anthropic_model(
    server: &agent_provider_anthropic::testing::MockServer,
) -> agent_provider_anthropic::Anthropic {
    use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
    Anthropic::new(
        AnthropicConfig::new(&server.base_url, "claude-test")
            .with_api_key(ApiKey::new("sk-ant-e2e")),
    )
    .expect("a local mock config is valid")
}

fn anthropic_reply(text: &str) -> agent_provider_anthropic::testing::Reply {
    use agent_provider_anthropic::testing::{
        Reply, block_stop, message_delta, message_start, message_stop, text_delta, text_start,
    };
    Reply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, text),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])
}

fn anthropic_bodies(server: &agent_provider_anthropic::testing::MockServer) -> Vec<String> {
    server
        .requests()
        .into_iter()
        .map(|request| request.body)
        .collect()
}

e2e_scenario!(
    anthropic_resume_after_compaction_sends_the_summary_and_the_originals,
    agent_provider_anthropic::testing::MockServer,
    anthropic_model,
    anthropic_reply,
    anthropic_bodies
);

// ---------- OpenAI-compatible ----------

fn openai_model(
    server: &agent_provider_openai_compatible::testing::MockServer,
) -> agent_provider_openai_compatible::OpenAiCompatible {
    use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
    OpenAiCompatible::new(
        OpenAiCompatibleConfig::new(&server.base_url, "test-model")
            .with_api_key(ApiKey::new("sk-e2e")),
    )
    .expect("a local mock config is valid")
}

fn openai_reply(text: &str) -> agent_provider_openai_compatible::testing::Reply {
    use agent_provider_openai_compatible::testing::{Reply, chunk};
    Reply::sse(&[
        chunk(
            serde_json::json!({"role": "assistant", "content": text}),
            None,
        ),
        chunk(serde_json::json!({}), Some("stop")),
    ])
}

fn openai_bodies(server: &agent_provider_openai_compatible::testing::MockServer) -> Vec<String> {
    server
        .requests()
        .into_iter()
        .map(|request| request.body)
        .collect()
}

e2e_scenario!(
    openai_compatible_resume_after_compaction_sends_the_summary_and_the_originals,
    agent_provider_openai_compatible::testing::MockServer,
    openai_model,
    openai_reply,
    openai_bodies
);
