//! The JSON-RPC 2.0 layer over stdio (ADR 0049): the bounded line reader, and one writer task
//! that owns stdout.
//!
//! The writer's queue is bounded ([`super::ACP_OUTBOX_CAPACITY`] messages): a producer waits
//! for room instead of dropping a message or growing the queue without bound. Async producers
//! (the read loop, the prompt task, the bridge) wait with `send`. The live-event sink is
//! synchronous and runs inside the turn loop on a multi-thread runtime, so it waits through
//! `block_in_place` and `blocking_send`: the writer keeps draining on another worker while the
//! turn stalls.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{ACP_MAX_INPUT_LINE_BYTES, ACP_OUTBOX_CAPACITY};

/// The read buffer [`LineReader::new`] gives the underlying reader. A line is accumulated in the
/// reader's own buffer, so this only bounds one `fill_buf` chunk.
const READ_BUFFER_BYTES: usize = 64 * 1024;

/// One line from the protocol input.
pub(crate) enum IncomingLine {
    /// A complete line; the newline and a trailing `\r` are dropped.
    Line(String),
    /// A line longer than the reader's limit. It was discarded as it was read, never held whole.
    TooLong,
    /// End of input, including an over-long line that has no newline before it.
    Eof,
}

/// The error message for an over-long input line. It names the limit, which the manual's drift
/// test reads from [`super::ACP_MAX_INPUT_LINE_BYTES`].
pub(crate) fn input_line_limit_message() -> String {
    format!(
        "an input line is longer than the {ACP_MAX_INPUT_LINE_BYTES} byte limit; the line was discarded"
    )
}

/// Reads newline-delimited input with a per-line bound. A line longer than `limit` is discarded
/// as it is read: the accumulated buffer never grows past `limit`, so an over-long line is
/// never held whole.
pub(crate) struct LineReader<R> {
    reader: BufReader<R>,
    limit: usize,
    /// The bytes of the line being read, emptied before each line. Its length never passes
    /// `limit`, so an over-long line is never held whole.
    line: Vec<u8>,
    /// The longest the accumulated line ever got. Tests assert it never passes the limit,
    /// which is what "never held whole" means without measuring RSS.
    #[cfg(test)]
    longest: usize,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub(crate) fn new(reader: R, limit: usize) -> Self {
        Self::with_capacity(reader, limit, READ_BUFFER_BYTES)
    }

    fn with_capacity(reader: R, limit: usize, capacity: usize) -> Self {
        Self {
            reader: BufReader::with_capacity(capacity, reader),
            limit,
            line: Vec::new(),
            #[cfg(test)]
            longest: 0,
        }
    }

    /// Reads one line, bounded at the limit.
    pub(crate) async fn next_line(&mut self) -> std::io::Result<IncomingLine> {
        self.line.clear();
        let mut over = false;
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                // End of input. A partial line is served as `lines()` served one, but an
                // over-long line that never ended is just end of input.
                if over || self.line.is_empty() {
                    return Ok(IncomingLine::Eof);
                }
                return self.take_line();
            }
            let newline = chunk.iter().position(|byte| *byte == b'\n');
            let take = newline.unwrap_or(chunk.len());
            let done = newline.is_some();
            if !over {
                if take <= self.limit - self.line.len() {
                    append_line(&mut self.line, &chunk[..take], self.limit);
                    #[cfg(test)]
                    {
                        self.longest = self.longest.max(self.line.len());
                    }
                } else {
                    // Past the limit: drop what was collected and discard the rest as it
                    // arrives, so the over-long line is never held whole.
                    over = true;
                    self.line.clear();
                }
            }
            let consumed = take + usize::from(done);
            self.reader.consume(consumed);
            if done {
                if over {
                    return Ok(IncomingLine::TooLong);
                }
                return self.take_line();
            }
        }
    }

    /// Turns the accumulated bytes into a line, dropping a trailing `\r`.
    fn take_line(&mut self) -> std::io::Result<IncomingLine> {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let bytes = std::mem::take(&mut self.line);
        String::from_utf8(bytes)
            .map(IncomingLine::Line)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the input line is not valid UTF-8",
                )
            })
    }

    /// The longest the accumulated line ever got. Test-only, so the bounded-buffer property is
    /// asserted directly instead of by measuring the process's memory.
    #[cfg(test)]
    fn longest_line(&self) -> usize {
        self.longest
    }
}

/// Appends to the line buffer without letting its capacity pass `limit`. The line itself can
/// never be longer than the limit (the caller checks), so capping the capacity there keeps the
/// reader's memory bounded by the limit even while the `Vec` grows.
fn append_line(line: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    let needed = line.len() + bytes.len();
    debug_assert!(needed <= limit, "the caller bounds the line at the limit");
    if line.capacity() < needed {
        let target = needed.max(line.capacity().saturating_mul(2)).min(limit);
        line.reserve_exact(target - line.len());
    }
    line.extend_from_slice(bytes);
}

/// The sending end of the stdout writer.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: mpsc::Sender<Value>,
}

impl Outbox {
    /// Sends one message, waiting for room in the bounded queue. No message is ever dropped.
    async fn send(&self, message: Value) {
        let _ = self.tx.send(message).await;
    }

    /// The synchronous producer's path (the live-event sink). `block_in_place` moves the turn
    /// off its worker while it waits, so the writer task keeps draining stdout. The ACP process
    /// always builds a multi-thread runtime (`run_acp_blocking`), which this requires.
    fn send_blocking(&self, message: Value) {
        tokio::task::block_in_place(|| {
            let _ = self.tx.blocking_send(message);
        });
    }

    /// A notification: no id, no response.
    pub(crate) async fn notify(&self, method: &str, params: Value) {
        self.send(notification(method, params)).await;
    }

    /// [`Outbox::notify`] for a producer that runs in a synchronous context.
    pub(crate) fn notify_blocking(&self, method: &str, params: Value) {
        self.send_blocking(notification(method, params));
    }

    /// A successful response to the client's request `id`.
    pub(crate) async fn respond(&self, id: &Value, result: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await;
    }

    /// An error response to the client's request `id`.
    pub(crate) async fn respond_error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await;
    }

    /// A request from the host to the client.
    pub(crate) async fn request(&self, id: i64, method: &str, params: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
    }
}

fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// Starts the task that serializes every outgoing message, one JSON object per line. A write
/// failure (the client closed stdout) ends the task; later sends return an error and are
/// ignored, so a producer never waits for a writer that is gone.
pub(crate) fn spawn_writer() -> (Outbox, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(ACP_OUTBOX_CAPACITY);
    let task = tokio::spawn(write_messages(tokio::io::stdout(), rx));
    (Outbox { tx }, task)
}

/// Drains the queue to `out` in order. `write_all` blocks when the client stops reading, which
/// is what fills the bounded queue and stalls the producer.
async fn write_messages<W: AsyncWrite + Unpin>(mut out: W, mut rx: mpsc::Receiver<Value>) {
    while let Some(message) = rx.recv().await {
        let Ok(mut bytes) = serde_json::to_vec(&message) else {
            continue;
        };
        bytes.push(b'\n');
        if out.write_all(&bytes).await.is_err() {
            return;
        }
        let _ = out.flush().await;
    }
}

/// One incoming JSON-RPC message, classified. A response has an `id` and a `result` or
/// `error` and no `method`; a request has both `id` and `method`; a notification has a
/// `method` and no `id`.
pub(crate) enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
    Invalid(String),
}

/// A JSON-RPC error the client returned for one of the host's requests.
#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub(crate) fn message(&self) -> String {
        format!("{} (code {})", self.message, self.code)
    }
}

/// Classifies one parsed line. Unknown top-level shapes are `Invalid`, never silently dropped.
pub(crate) fn classify(value: Value) -> Incoming {
    let Some(object) = value.as_object() else {
        return Incoming::Invalid("the message is not a JSON object".to_owned());
    };
    let method = object.get("method").and_then(Value::as_str);
    let id = object.get("id");
    match (method, id) {
        (Some(method), Some(id)) => Incoming::Request {
            id: id.clone(),
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        },
        (Some(method), None) => Incoming::Notification {
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        },
        (None, Some(id)) => {
            if let Some(error) = object.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the client returned an error")
                    .to_owned();
                Incoming::Response {
                    id: id.clone(),
                    result: Err(RpcError { code, message }),
                }
            } else {
                Incoming::Response {
                    id: id.clone(),
                    result: Ok(object.get("result").cloned().unwrap_or(Value::Null)),
                }
            }
        }
        (None, None) => Incoming::Invalid("the message has neither a method nor an id".to_owned()),
    }
}

/// The outcome of a `session/request_permission` response.
pub(crate) enum PermissionOutcome {
    Selected(String),
    Cancelled,
    Invalid(String),
}

/// Reads the protocol's nested outcome object:
/// `{"outcome": {"outcome": "selected", "optionId": "…"}}` or `{"outcome": {"outcome":
/// "cancelled"}}`.
pub(crate) fn permission_outcome(value: &Value) -> PermissionOutcome {
    let outcome = value.get("outcome").unwrap_or(value);
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("selected") => match outcome.get("optionId").and_then(Value::as_str) {
            Some(option) => PermissionOutcome::Selected(option.to_owned()),
            None => PermissionOutcome::Invalid("a selected outcome has no optionId".to_owned()),
        },
        Some("cancelled") => PermissionOutcome::Cancelled,
        other => PermissionOutcome::Invalid(format!("unknown permission outcome {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncRead, ReadBuf};
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;

    use super::{IncomingLine, LineReader, Outbox};
    use crate::ACP_OUTBOX_CAPACITY;

    /// An async reader that hands out at most `chunk` bytes per `poll_read`, so a multi-byte
    /// character is split across `fill_buf` calls deterministically.
    struct Chunked {
        bytes: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl Chunked {
        fn new(bytes: Vec<u8>, chunk: usize) -> Self {
            Self {
                bytes,
                pos: 0,
                chunk,
            }
        }
    }

    impl AsyncRead for Chunked {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let start = this.pos;
            let end = (start + this.chunk)
                .min(this.bytes.len())
                .min(start + buf.remaining());
            if start >= end {
                return Poll::Ready(Ok(()));
            }
            buf.put_slice(&this.bytes[start..end]);
            this.pos = end;
            Poll::Ready(Ok(()))
        }
    }

    /// ADR 0049: a line longer than the limit is discarded as it is read, so the accumulated
    /// buffer never passes the limit, and the next line is still served.
    #[tokio::test]
    async fn a_line_longer_than_the_limit_is_discarded_without_being_held() {
        let limit = 1024;
        let mut bytes = vec![b'x'; 64 * 1024];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = LineReader::with_capacity(Chunked::new(bytes, 2), limit, 7);
        assert!(
            matches!(reader.next_line().await.unwrap(), IncomingLine::TooLong),
            "an over-long line is reported as over the limit"
        );
        assert_eq!(
            reader.longest_line(),
            limit,
            "the accumulated line stops at the limit and is never held whole"
        );
        match reader.next_line().await.unwrap() {
            IncomingLine::Line(line) => assert_eq!(line, "{\"ok\":true}"),
            _ => panic!("the line after the over-long one is served"),
        }
    }

    /// ADR 0049: one line at most 64 MiB means a line exactly at the limit is accepted. The
    /// eight-byte reads put the newline in its own read, so the limit check sees a full buffer
    /// and an empty one, not a chunk that still has room.
    #[tokio::test]
    async fn a_line_exactly_at_the_limit_is_accepted() {
        let limit = 64;
        let mut bytes = vec![b'a'; limit];
        bytes.push(b'\n');
        let mut reader = LineReader::with_capacity(Chunked::new(bytes, 8), limit, 8);
        match reader.next_line().await.unwrap() {
            IncomingLine::Line(line) => assert_eq!(line.len(), limit),
            _ => panic!("a line exactly at the limit is accepted"),
        }
    }

    /// A multi-byte character split across the reads is kept: the reader collects bytes and
    /// validates UTF-8 only at the end of the line.
    #[tokio::test]
    async fn a_multibyte_character_split_across_reads_is_kept() {
        let text = "中".repeat(4_000);
        let mut bytes = serde_json::to_string(&text).unwrap().into_bytes();
        bytes.push(b'\n');
        // Two bytes per read split every three-byte character.
        let mut reader = LineReader::with_capacity(Chunked::new(bytes, 2), 1024 * 1024, 8);
        match reader.next_line().await.unwrap() {
            IncomingLine::Line(line) => {
                assert_eq!(serde_json::from_str::<String>(&line).unwrap(), text);
            }
            _ => panic!("the legal line is read"),
        }
    }

    /// ADR 0049: end of input in the middle of an over-long line is a normal end of input, not
    /// a parse error and not a truncated line served as one.
    #[tokio::test]
    async fn eof_in_the_middle_of_an_over_long_line_is_eof() {
        let bytes = vec![b'x'; 4096];
        let mut reader = LineReader::with_capacity(Chunked::new(bytes, 16), 128, 8);
        assert!(
            matches!(reader.next_line().await.unwrap(), IncomingLine::Eof),
            "an over-long line with no newline is just end of input"
        );
    }

    /// ADR 0049: the outgoing queue is bounded, and neither an async producer nor the
    /// synchronous live-event sink drops a message when it is full: both wait for the writer to
    /// drain, so the queue stays at its capacity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_outbox_makes_both_producers_wait_and_drops_nothing() {
        let (tx, mut rx) = mpsc::channel(ACP_OUTBOX_CAPACITY);
        let out = Outbox { tx };
        for id in 0..ACP_OUTBOX_CAPACITY {
            out.respond(&json!(id), json!(id)).await;
        }
        assert_eq!(
            rx.len(),
            ACP_OUTBOX_CAPACITY,
            "the queue holds exactly its capacity"
        );

        let (async_done, async_waiting) = oneshot::channel();
        let async_out = out.clone();
        let async_task = tokio::spawn(async move {
            async_out.respond(&json!("async"), json!("async")).await;
            let _ = async_done.send(());
        });

        let (sync_done, sync_waiting) = oneshot::channel();
        let sync_out = out.clone();
        let sync_task = tokio::spawn(async move {
            sync_out.notify_blocking("session/update", json!({ "n": "sync" }));
            let _ = sync_done.send(());
        });

        let mut async_waiting = async_waiting;
        let mut sync_waiting = sync_waiting;
        assert!(
            timeout(Duration::from_millis(300), &mut async_waiting)
                .await
                .is_err(),
            "an async producer must wait for room instead of growing the queue"
        );
        assert!(
            timeout(Duration::from_millis(300), &mut sync_waiting)
                .await
                .is_err(),
            "the synchronous event sink must wait for room instead of dropping the update"
        );
        assert_eq!(
            rx.len(),
            ACP_OUTBOX_CAPACITY,
            "the queue stays at its capacity while the producers wait"
        );

        // Draining admits both waiters; every message is still there, in order.
        let mut got = Vec::new();
        for _ in 0..ACP_OUTBOX_CAPACITY + 2 {
            let message = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("the queue drains")
                .expect("a message");
            got.push(message);
        }
        timeout(Duration::from_secs(5), async_waiting)
            .await
            .expect("the async producer resumes")
            .expect("the async producer signalled");
        timeout(Duration::from_secs(5), sync_waiting)
            .await
            .expect("the sync producer resumes")
            .expect("the sync producer signalled");
        async_task.await.unwrap();
        sync_task.await.unwrap();

        for (index, message) in got[..ACP_OUTBOX_CAPACITY].iter().enumerate() {
            assert_eq!(
                message["id"],
                json!(index),
                "the queued messages stayed in order"
            );
        }
        let tail: Vec<&Value> = got[ACP_OUTBOX_CAPACITY..].iter().collect();
        assert_eq!(
            tail.iter()
                .filter(|message| message["id"] == json!("async"))
                .count(),
            1,
            "the async message was delivered, not dropped: {tail:?}"
        );
        assert_eq!(
            tail.iter()
                .filter(|message| message["method"] == json!("session/update"))
                .count(),
            1,
            "the synchronous update was delivered, not dropped: {tail:?}"
        );
    }
}
