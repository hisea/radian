//! The host-side pending-request queue (ADR 0023 U7).
//!
//! An approval and a structured question (ADR 0024 Q1/Q4) are both "the model is waiting for
//! the human". The queue keeps them in **arrival order**, each tagged with the session that
//! asked, and hands the UI one request at a time. It is synchronous and thread-safe: the
//! permission layer's `Approver` and the tool's `Prompter` push from a tokio worker, the UI
//! reads the head from its own thread. [`PendingRequest`]/[`PendingResponse`] are what let one
//! queue hold both kinds.
//!
//! # Lifecycle
//!
//! Every row of ADR 0023 U7 / ADR 0024 Q4 is one mechanism:
//!
//! | Situation | What happens | Who |
//! |---|---|---|
//! | a request arrives | `push` appends it, returns the waiting end; the arrival order is the display order | the queue |
//! | the user answers the head | `answer(id, resp)` completes that element and removes it | the UI |
//! | the wait gives up (turn cancelled, UI abandoned it) | the receiver is dropped; `head`/`len` drop the element the next time they run, and `PendingAnswer::drop` wakes the UI immediately | the permission layer + the queue |
//! | the session closes with requests pending | `abort_session` (approval-only queue) denies; `abort_session_all` (shared queue) denies approvals and gives questions `PromptError::Gone` | the host |
//! | the UI exits / the host shuts down | the queue is dropped, so every remaining waiting end sees the sender dropped ("abandoned"), exactly as before | `Drop` |
//! | a late answer (the wait is gone, or the element was aborted) | `answer` returns [`Answered::Late`] and changes nothing | the queue |
//! | an answer for an id that was never issued | `answer` returns [`Answered::Unknown`] | the queue |
//! | a crash | no result; ADR 0002 `repair` handles the call | recovery rules |
//!
//! Nothing here changes the fail-closed approval semantics (ADR 0012): an aborted session
//! denies, it never allows.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context as TaskContext, Poll};

use futures_util::future::BoxFuture;
use radian_coding::{PromptError, Prompter, QuestionAnswer, QuestionRequest};
use radian_permissions::{ApprovalError, ApprovalOutcome, ApprovalRequest, Approver};
use tokio::sync::{oneshot, watch};

/// One pending request: an approval or a structured question. ADR 0023 U7 gives approvals and
/// questions **one** queue so the UI shows them in a single arrival order; this enum is what
/// lets one `PendingQueue` hold both without either side knowing about the other's type.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingRequest {
    Approval(ApprovalRequest),
    Question(QuestionRequest),
}

/// The answer to a [`PendingRequest`]. A question's answer is itself a `Result`: a decline or
/// a gone prompter is an outcome of asking, not a panic (ADR 0024 Q4).
#[derive(Debug, Clone, PartialEq)]
pub enum PendingResponse {
    Approval(ApprovalOutcome),
    Question(Result<QuestionAnswer, PromptError>),
}

/// The request the UI should show: its id, the session that asked, and the request itself.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingView<Req> {
    pub request_id: u64,
    pub origin: String,
    pub request: Req,
}

/// Why an `answer` did not complete a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answered {
    /// The request was already completed or abandoned; the answer is silently dropped.
    Late,
    /// No request with that id was ever issued.
    Unknown,
}

impl std::fmt::Display for Answered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Answered::Late => "the request is no longer pending",
            Answered::Unknown => "no such pending request",
        })
    }
}

impl std::error::Error for Answered {}

/// The waiting end of one queued request. Awaiting it yields the answer; dropping it before
/// the answer is the "the wait gave up" case (turn cancelled or UI abandoned).
pub struct PendingAnswer<Resp> {
    rx: Option<oneshot::Receiver<Resp>>,
    /// Wakes the UI when this waiting end is dropped, so `head`/`len` re-run and drop the
    /// element instead of showing a request nobody is waiting for.
    version: watch::Sender<u64>,
}

impl<Resp> PendingAnswer<Resp> {
    /// Waits for the answer. `Err(Abandoned)` means the queue (or the sender) was dropped.
    pub async fn wait(mut self) -> Result<Resp, Abandoned> {
        let rx = self.rx.take().expect("a pending answer is awaited once");
        rx.await.map_err(|_| Abandoned)
    }
}

impl<Resp> Future for PendingAnswer<Resp> {
    type Output = Result<Resp, Abandoned>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let rx = self.rx.as_mut().expect("a pending answer is polled once");
        match Pin::new(rx).poll(cx) {
            Poll::Ready(Ok(value)) => Poll::Ready(Ok(value)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(Abandoned)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<Resp> Drop for PendingAnswer<Resp> {
    fn drop(&mut self) {
        // Close the receiver **before** waking the UI. Fields drop only after this body runs,
        // so bumping first would let a woken UI call `head()` while `reply.is_closed()` is
        // still false, keep showing the request, and get no further wake-up (ADR 0023 U2:
        // change the state, then notify).
        drop(self.rx.take());
        self.bump();
    }
}

impl<Resp> PendingAnswer<Resp> {
    fn bump(&self) {
        self.version
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

/// The waiting end was dropped before an answer arrived (UI exited, session closed, queue
/// dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Abandoned;

impl std::fmt::Display for Abandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the pending request was abandoned")
    }
}

impl std::error::Error for Abandoned {}

struct Entry<Req, Resp> {
    request_id: u64,
    origin: String,
    request: Req,
    reply: oneshot::Sender<Resp>,
}

struct State<Req, Resp> {
    entries: VecDeque<Entry<Req, Resp>>,
}

/// The queue itself. Clone it through an `Arc`; every method takes `&self`.
pub struct PendingQueue<Req, Resp> {
    state: Mutex<State<Req, Resp>>,
    /// Bumped on every change that could alter the head or the count. A `watch` receiver
    /// `changed()`s once per version, so many changes coalesce into one wake-up.
    version: watch::Sender<u64>,
    /// The highest `request_id` handed out; 0 before the first `push`.
    last_issued: AtomicU64,
}

impl<Req, Resp> Default for PendingQueue<Req, Resp> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Req, Resp> PendingQueue<Req, Resp> {
    pub fn new() -> Self {
        let (version, _) = watch::channel(0u64);
        Self {
            state: Mutex::new(State {
                entries: VecDeque::new(),
            }),
            version,
            last_issued: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<Req, Resp>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.version
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// A receiver that changes once per queue change (coalesced). The UI reads the head, then
    /// waits on this; a change between the read and the wait is not lost because a `watch`
    /// observably bumps its version.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    /// Adds a request and returns its id and the waiting end. Arrival order is preserved.
    pub fn push(&self, origin: impl Into<String>, request: Req) -> (u64, PendingAnswer<Resp>) {
        let request_id = self.last_issued.fetch_add(1, Ordering::SeqCst) + 1;
        let (reply, rx) = oneshot::channel();
        {
            let mut state = self.lock();
            state.entries.push_back(Entry {
                request_id,
                origin: origin.into(),
                request,
                reply,
            });
        }
        self.bump();
        (
            request_id,
            PendingAnswer {
                rx: Some(rx),
                version: self.version.clone(),
            },
        )
    }

    /// The valid pending count, dropping any element whose waiting end is gone.
    pub fn len(&self) -> usize {
        let mut state = self.lock();
        if retain_open(&mut state) {
            drop(state);
            self.bump();
            state = self.lock();
        }
        state.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Completes one request. A late answer (the wait is gone, or the id was already
    /// answered/aborted) is [`Answered::Late`] and changes nothing; an id that was never
    /// issued is [`Answered::Unknown`]. Never panics and never completes twice.
    pub fn answer(&self, request_id: u64, response: Resp) -> Result<(), Answered> {
        let entry = {
            let mut state = self.lock();
            match state
                .entries
                .iter()
                .position(|entry| entry.request_id == request_id)
            {
                Some(position) => state.entries.remove(position).expect("just found"),
                None => {
                    return Err(if request_id > self.last_issued.load(Ordering::SeqCst) {
                        Answered::Unknown
                    } else {
                        Answered::Late
                    });
                }
            }
        };
        // The element is out of the queue either way; a closed receiver turns a successful
        // `send` into `Late`.
        let result = match entry.reply.send(response) {
            Ok(()) => Ok(()),
            Err(_) => Err(Answered::Late),
        };
        self.bump();
        result
    }

    /// Completes every request of `origin` with `response` and removes them. Other sessions are
    /// untouched. Returns how many were completed.
    pub fn abort_session(&self, origin: &str, response: Resp) -> usize
    where
        Resp: Clone,
    {
        let mut aborted = 0usize;
        let mut remaining = VecDeque::with_capacity(self.lock().entries.len());
        {
            let mut state = self.lock();
            while let Some(entry) = state.entries.pop_front() {
                if entry.origin == origin {
                    aborted += 1;
                    let _ = entry.reply.send(response.clone());
                } else {
                    remaining.push_back(entry);
                }
            }
            state.entries = remaining;
        }
        if aborted > 0 {
            self.bump();
        }
        aborted
    }
}

impl<Req: Clone, Resp> PendingQueue<Req, Resp> {
    /// The request the UI should show: the first valid entry in arrival order. Elements whose
    /// waiting end is gone are dropped here.
    pub fn head(&self) -> Option<PendingView<Req>> {
        let (head, changed) = {
            let mut state = self.lock();
            let changed = retain_open(&mut state);
            let head = state.entries.front().map(|entry| PendingView {
                request_id: entry.request_id,
                origin: entry.origin.clone(),
                request: entry.request.clone(),
            });
            (head, changed)
        };
        if changed {
            self.bump();
        }
        head
    }
}

impl PendingQueue<PendingRequest, PendingResponse> {
    /// Session close (ADR 0023 U7): completes **every** pending request of `origin` with its
    /// kind's abort outcome and removes them. An approval is denied (fail-closed, ADR 0012); a
    /// question gets [`PromptError::Gone`], which the tool turns into the `prompter_gone`
    /// failure. Other sessions are untouched. Returns how many were completed.
    pub fn abort_session_all(&self, origin: &str) -> usize {
        let mut aborted = 0usize;
        let mut remaining = VecDeque::with_capacity(self.lock().entries.len());
        {
            let mut state = self.lock();
            while let Some(entry) = state.entries.pop_front() {
                if entry.origin != origin {
                    remaining.push_back(entry);
                    continue;
                }
                aborted += 1;
                let response = match &entry.request {
                    PendingRequest::Approval(_) => {
                        PendingResponse::Approval(ApprovalOutcome::Deny {
                            reason: Some("session closed".into()),
                        })
                    }
                    PendingRequest::Question(_) => {
                        PendingResponse::Question(Err(PromptError::Gone))
                    }
                };
                let _ = entry.reply.send(response);
            }
            state.entries = remaining;
        }
        if aborted > 0 {
            self.bump();
        }
        aborted
    }
}

/// Drops elements whose waiting end is gone; returns whether anything was dropped.
fn retain_open<Req, Resp>(state: &mut State<Req, Resp>) -> bool {
    let before = state.entries.len();
    state.entries.retain(|entry| !entry.reply.is_closed());
    before != state.entries.len()
}

/// The approval-shaped view of the queue: [`Approver::decide`] pushes and waits. A session
/// closing aborts its own requests with the fail-closed denial (ADR 0012).
///
/// The type parameter is the queue it pushes into. The default is the approval-only queue, so
/// an existing `QueuedApprover::new(approval_queue, …)` is unchanged; the host passes the
/// shared [`PendingRequest`]/[`PendingResponse`] queue for approvals and questions together.
pub struct QueuedApprover<Q = PendingQueue<ApprovalRequest, ApprovalOutcome>> {
    queue: Arc<Q>,
    origin: String,
}

impl<Q> QueuedApprover<Q> {
    /// `origin` is the session id this approver answers for.
    pub fn new(queue: Arc<Q>, origin: impl Into<String>) -> Self {
        Self {
            queue,
            origin: origin.into(),
        }
    }
}

impl QueuedApprover<PendingQueue<ApprovalRequest, ApprovalOutcome>> {
    /// Completes every pending approval of this session with `Deny`, the fail-closed outcome
    /// (ADR 0012), and returns how many were denied. Call it when the session closes.
    pub fn abort_session(&self) -> usize {
        self.queue.abort_session(
            &self.origin,
            ApprovalOutcome::Deny {
                reason: Some("session closed".into()),
            },
        )
    }
}

impl QueuedApprover<PendingQueue<PendingRequest, PendingResponse>> {
    /// Completes every pending request of this session (approvals and questions) with its
    /// kind's abort outcome; see [`PendingQueue::abort_session_all`].
    pub fn abort_session(&self) -> usize {
        self.queue.abort_session_all(&self.origin)
    }
}

impl Approver for QueuedApprover<PendingQueue<ApprovalRequest, ApprovalOutcome>> {
    fn decide(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async move {
            let (_request_id, answer) = self.queue.push(self.origin.clone(), request);
            answer
                .wait()
                .await
                .map_err(|_| ApprovalError("the approval was abandoned".into()))
        })
    }
}

impl Approver for QueuedApprover<PendingQueue<PendingRequest, PendingResponse>> {
    fn decide(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async move {
            let (_request_id, answer) = self
                .queue
                .push(self.origin.clone(), PendingRequest::Approval(request));
            match answer.wait().await {
                Ok(PendingResponse::Approval(outcome)) => Ok(outcome),
                // A question response for an approval, or an abandoned waiter: an internal
                // failure, which the permission hook fails closed on (ADR 0012).
                Ok(PendingResponse::Question(_)) | Err(_) => {
                    Err(ApprovalError("the approval was abandoned".into()))
                }
            }
        })
    }
}

/// The question-shaped view of the shared queue: [`Prompter::ask`] pushes and waits. A session
/// closing aborts its own questions with [`PromptError::Gone`] (ADR 0024 Q4).
pub struct QueuedPrompter {
    queue: Arc<PendingQueue<PendingRequest, PendingResponse>>,
    origin: String,
}

impl QueuedPrompter {
    /// `origin` is the session id this prompter asks for; it is shown with the question so a
    /// multi-session UI can say who is asking (ADR 0024 Q2).
    pub fn new(
        queue: Arc<PendingQueue<PendingRequest, PendingResponse>>,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            queue,
            origin: origin.into(),
        }
    }

    /// Completes every pending request of this session with its kind's abort outcome and
    /// returns how many were completed. Call it when the session closes.
    pub fn abort_session(&self) -> usize {
        self.queue.abort_session_all(&self.origin)
    }
}

impl Prompter for QueuedPrompter {
    fn ask(&self, request: QuestionRequest) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>> {
        Box::pin(async move {
            let (_request_id, answer) = self
                .queue
                .push(self.origin.clone(), PendingRequest::Question(request));
            match answer.wait().await {
                Ok(PendingResponse::Question(result)) => result,
                // An approval response for a question, or an abandoned waiter (the UI exited
                // or the queue was dropped): the prompter is gone.
                Ok(PendingResponse::Approval(_)) | Err(_) => Err(PromptError::Gone),
            }
        })
    }
}
