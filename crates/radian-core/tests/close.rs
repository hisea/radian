mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use radian_core::*;

#[tokio::test]
async fn failed_close_can_be_retried() {
    let tools = MemTools::new(vec![]);
    tools.log.close_failures_left.store(1, Ordering::SeqCst);
    let mut chat = agent(ScriptedModel::new([]), tools.clone())
        .open_session()
        .await
        .unwrap();

    assert!(chat.close().await.is_err());
    assert!(!chat.is_closed());
    // A session that started closing takes no new turns.
    assert!(matches!(
        chat.start_turn("hi").err(),
        Some(SessionError::Closed)
    ));

    chat.close().await.unwrap();
    assert!(chat.is_closed());
    assert!(tools.log.released.load(Ordering::SeqCst));

    // Once closed, close() is a no-op.
    chat.close().await.unwrap();
    assert_eq!(tools.log.close_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn dropped_close_can_be_retried() {
    let tools = MemTools::new(vec![]);
    tools.log.close_delay_ms.store(100, Ordering::SeqCst);
    let mut chat = agent(ScriptedModel::new([]), tools.clone())
        .open_session()
        .await
        .unwrap();

    let attempt = tokio::time::timeout(Duration::from_millis(10), chat.close()).await;

    assert!(attempt.is_err(), "the first close should have been dropped");
    assert!(!chat.is_closed());
    assert!(!tools.log.released.load(Ordering::SeqCst));
    assert!(matches!(
        chat.start_turn("hi").err(),
        Some(SessionError::Closed)
    ));

    chat.close().await.unwrap();
    assert!(chat.is_closed());
    assert!(tools.log.released.load(Ordering::SeqCst));
    assert_eq!(tools.log.close_calls.load(Ordering::SeqCst), 2);
}
