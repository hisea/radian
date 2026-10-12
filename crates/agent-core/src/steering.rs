use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::message::Message;

/// What happens to not-yet-started tool calls when a steering message arrives mid-batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SteerMode {
    /// Finish the batch, then inject.
    #[default]
    AfterBatch,
    /// Let started calls finish; skip the rest with a `skipped_by_steering` result.
    SkipRemaining,
}

#[derive(Debug, thiserror::Error)]
pub enum SteerError {
    /// The turn already ended. The message is handed back.
    #[error("the turn has finished")]
    RunFinished(Message),
    #[error("steering messages must be user messages")]
    NotUserMessage(Message),
}

#[derive(Debug, Default)]
struct Queue {
    messages: VecDeque<Message>,
    closed: bool,
}

/// Sends user messages into a running turn. Cheap to clone and usable from any task.
///
/// Messages are written to the transcript only at safe points: after a tool batch, when the
/// model would otherwise finish, or before the first model request.
#[derive(Debug, Clone, Default)]
pub struct SteeringHandle {
    queue: Arc<Mutex<Queue>>,
}

impl SteeringHandle {
    pub fn steer(&self, message: impl Into<Message>) -> Result<(), SteerError> {
        let message = message.into();
        if !message.is_user() {
            return Err(SteerError::NotUserMessage(message));
        }
        let mut queue = self.lock();
        if queue.closed {
            return Err(SteerError::RunFinished(message));
        }
        queue.messages.push_back(message);
        Ok(())
    }

    /// Puts messages that were drained but could not be applied back in front of the queue,
    /// so that `close` returns them to the host instead of losing them.
    pub(crate) fn requeue_front(&self, messages: Vec<Message>) {
        let mut queue = self.lock();
        for message in messages.into_iter().rev() {
            queue.messages.push_front(message);
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.lock().messages.is_empty()
    }

    pub(crate) fn drain(&self) -> Vec<Message> {
        self.lock().messages.drain(..).collect()
    }

    /// Rejects further messages and returns those never applied.
    pub(crate) fn close(&self) -> Vec<Message> {
        let mut queue = self.lock();
        queue.closed = true;
        queue.messages.drain(..).collect()
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        // The lock is never held across user code, so poisoning can only come from a panic
        // inside this module; the queue is still consistent then.
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}
