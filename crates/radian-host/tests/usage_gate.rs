//! ADR 0027: the usage row follows the **current** model. The source refreshes and
//! shows only while the provider is opencode-go; switching away hides the row and stops the
//! network, switching back refreshes once. A local mock server counts the requests (never the
//! real endpoint or a real key).

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use radian_host::{ContributionStore, OpencodeGoUsage, Tick, spawn_usage_gated};

fn wait_for(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + deadline;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    check()
}

#[test]
fn the_usage_row_follows_the_current_model() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a mock port");
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            counter.fetch_add(1, Ordering::SeqCst);
            let body = r#"{"usage":{"rolling":{"percent":2}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let store = ContributionStore::new();
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let _task = spawn_usage_gated(
        runtime.handle(),
        Box::new(OpencodeGoUsage::with_url(
            "sk-secret",
            format!("http://{addr}/usage"),
        )),
        radian_host::OPENCODE_GO_MIN_PERIOD,
        Arc::clone(&store),
        Arc::new(|_: Tick| {}),
        gate_rx,
    );

    // Inactive at first: the row is absent and no request goes out.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "no request while inactive"
    );
    assert!(
        store.get(radian_host::USAGE_ITEM).is_none(),
        "no row while inactive"
    );

    // Activate: one request, the row appears.
    gate_tx.send(true).unwrap();
    assert!(
        wait_for(Duration::from_secs(3), || store
            .get(radian_host::USAGE_ITEM)
            .is_some()),
        "the row appears on activation"
    );
    let contribution = store.get(radian_host::USAGE_ITEM).expect("the row");
    let snapshot = contribution.usage.expect("the snapshot is the value");
    assert_eq!(snapshot.provider, "opencode-go");
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].label, "5h");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "one request on activation"
    );

    // Deactivate: the row disappears and no further request arrives.
    gate_tx.send(false).unwrap();
    assert!(
        wait_for(Duration::from_secs(3), || store
            .get(radian_host::USAGE_ITEM)
            .is_none()),
        "the row hides on deactivation"
    );
    let after_off = requests.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        after_off,
        "no request while inactive"
    );

    // Reactivate: one more request, the row returns.
    gate_tx.send(true).unwrap();
    assert!(
        wait_for(Duration::from_secs(3), || store
            .get(radian_host::USAGE_ITEM)
            .is_some()),
        "the row returns on reactivation"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        after_off + 1,
        "one request on reactivation"
    );
}
