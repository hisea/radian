//! Test doubles for the journal seam (feature `testing`).
//!
//! [`RecordingJournal`] remembers everything, [`FailingJournal`] fails the Nth append, and
//! [`SlowJournal`] delays each append. All of them can share a [`SequenceLog`] with the
//! event sink under test, so a test can prove what happened in which order.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::journal::{JournalError, JournalRecord, TranscriptJournal};
use crate::message::Message;

/// A shared, ordered log for cross-cutting ordering assertions (journal vs. events).
#[derive(Clone, Default)]
pub struct SequenceLog(std::sync::Arc<Mutex<Vec<String>>>);

impl SequenceLog {
    pub fn note(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }

    pub fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct Inner {
    records: Mutex<Vec<JournalRecord>>,
    log: Option<SequenceLog>,
    calls: AtomicUsize,
    /// 1-based; this append fails.
    fail_at: Option<usize>,
    delay: Option<Duration>,
}

impl Inner {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError> {
        let nth = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let key = match &record {
            JournalRecord::Message { index, .. } => format!("{index}"),
            JournalRecord::TranscriptReplaced { revision, .. } => format!("replaced:{revision}"),
            JournalRecord::ToolExecutionStarted { call_id, .. } => format!("started:{call_id}"),
            JournalRecord::ToolExecutionSettled { call_id, .. } => format!("settled:{call_id}"),
        };
        if let Some(delay) = self.delay {
            if let Some(log) = &self.log {
                log.note(format!("journal-start:{key}"));
            }
            tokio::time::sleep(delay).await;
        }
        if self.fail_at == Some(nth) {
            return Err(JournalError(format!("injected failure at append {nth}")));
        }
        self.records.lock().unwrap().push(record);
        if let Some(log) = &self.log {
            log.note(format!("journal:{key}"));
        }
        Ok(())
    }
}

macro_rules! journal_double {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Default)]
        pub struct $name(std::sync::Arc<Inner>);

        impl $name {
            fn inner(&self) -> &Inner {
                &self.0
            }
        }

        impl TranscriptJournal for $name {
            fn append(
                &self,
                record: JournalRecord,
            ) -> BoxFuture<'static, Result<(), JournalError>> {
                let inner = std::sync::Arc::clone(&self.0);
                Box::pin(async move { inner.append(record).await })
            }
        }
    };
}

journal_double!(RecordingJournal, "Records every append; never fails.");
journal_double!(
    FailingJournal,
    "Fails one chosen append (1-based); records the rest."
);
journal_double!(SlowJournal, "Delays every append; never fails.");

impl RecordingJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes `journal:<index>` to `log` after each append lands.
    pub fn with_log(log: SequenceLog) -> Self {
        Self(std::sync::Arc::new(Inner {
            log: Some(log),
            ..Default::default()
        }))
    }

    pub fn records(&self) -> Vec<JournalRecord> {
        self.inner().records.lock().unwrap().clone()
    }

    /// The recorded messages in order, without their indexes or marker records.
    pub fn messages(&self) -> Vec<Message> {
        self.records()
            .into_iter()
            .filter_map(|r| match r {
                JournalRecord::Message { message, .. } => Some(message),
                _ => None,
            })
            .collect()
    }

    pub fn calls(&self) -> usize {
        self.inner().calls.load(Ordering::SeqCst)
    }
}

impl FailingJournal {
    /// Fails the `nth` append (1-based) and records every other one.
    pub fn fail_at(nth: usize) -> Self {
        Self(std::sync::Arc::new(Inner {
            fail_at: Some(nth),
            ..Default::default()
        }))
    }

    pub fn records(&self) -> Vec<JournalRecord> {
        self.inner().records.lock().unwrap().clone()
    }

    /// How many appends were attempted, failed ones included.
    pub fn calls(&self) -> usize {
        self.inner().calls.load(Ordering::SeqCst)
    }
}

impl SlowJournal {
    /// Every append takes `delay` (virtual-time friendly).
    pub fn new(delay: Duration) -> Self {
        Self(std::sync::Arc::new(Inner {
            delay: Some(delay),
            ..Default::default()
        }))
    }

    /// Like [`SlowJournal::new`], and notes `journal-start:<index>` when an append begins
    /// and `journal:<index>` when it lands, so a test can wait for an append in flight.
    pub fn with_log(delay: Duration, log: SequenceLog) -> Self {
        Self(std::sync::Arc::new(Inner {
            delay: Some(delay),
            log: Some(log),
            ..Default::default()
        }))
    }

    pub fn records(&self) -> Vec<JournalRecord> {
        self.inner().records.lock().unwrap().clone()
    }

    pub fn calls(&self) -> usize {
        self.inner().calls.load(Ordering::SeqCst)
    }
}
