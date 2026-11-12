//! Model streaming with bounded, classification-based retry (§6.3):
//! one attempt's shape, the retry wrapper, cancellation-aware
//! backoff, and the provider stream itself.

use super::Agent;
use super::TaskError;
use super::summary::{Accounting, duration_millis};
use crate::event::{EventKind, EventSink};
use crate::id::TaskId;
use crate::model::{ModelError, ModelErrorKind, ModelEvent, ModelRequest, ModelResponse};
use futures::StreamExt;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

impl Agent {
    /// Streams one model request with bounded retries, emitting provisional
    /// deltas and retry events as it goes.
    pub(super) async fn stream_with_retry(
        &self,
        request: &ModelRequest,
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
        accounting: &mut Accounting,
    ) -> Result<ModelResponse, TaskError> {
        let mut attempt: u32 = 1;
        loop {
            accounting.model_requests += 1;
            sink.emit(task_id, EventKind::ModelRequestStarted { attempt });

            match self.stream_once(request, task_id, sink, cancellation).await {
                Attempt::Completed(response) => return Ok(response),
                Attempt::Cancelled => return Err(TaskError::Cancelled),
                Attempt::Failed {
                    error,
                    response_started,
                } => {
                    // Cancellation wins over retry (§6.3): a cancelled
                    // request is not a failure to recover from.
                    if cancellation.is_cancelled() {
                        return Err(TaskError::Cancelled);
                    }
                    if self.retry.should_retry(attempt, &error) {
                        let delay = self.retry.delay(attempt, &error);
                        sink.emit(
                            task_id,
                            EventKind::ModelRetry {
                                attempt,
                                kind: error.kind,
                                delay_ms: duration_millis(delay),
                                response_started,
                            },
                        );
                        if self.sleep_or_cancel(delay, cancellation).await {
                            return Err(TaskError::Cancelled);
                        }
                        attempt = attempt.saturating_add(1);
                    } else {
                        return Err(TaskError::Model(error));
                    }
                }
            }
        }
    }

    /// One attempt: consumes the stream to a single terminal outcome,
    /// emitting deltas. The stream is dropped on cancellation, which is the
    /// only cancellation signal a `ModelStream` carries (§6.6).
    pub(super) async fn stream_once(
        &self,
        request: &ModelRequest,
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Attempt {
        let mut stream = self.provider.stream(request);
        let mut response_started = false;
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    drop(stream);
                    return Attempt::Cancelled;
                }
                next = stream.next() => match next {
                    Some(Ok(ModelEvent::Completed(response))) => {
                        sink.emit(
                            task_id,
                            EventKind::ModelCompleted {
                                response: response.clone(),
                            },
                        );
                        return Attempt::Completed(response);
                    }
                    Some(Ok(ModelEvent::TextDelta(text))) => {
                        response_started = true;
                        sink.emit(task_id, EventKind::TextDelta { text });
                    }
                    Some(Ok(ModelEvent::ReasoningDelta(text))) => {
                        response_started = true;
                        sink.emit(task_id, EventKind::ReasoningDelta { text });
                    }
                    Some(Err(error)) => {
                        drop(stream);
                        return Attempt::Failed { error, response_started };
                    }
                    None => {
                        // A stream that ends without `Completed` breaks the
                        // provider contract; treat it as a protocol error.
                        drop(stream);
                        return Attempt::Failed {
                            error: ModelError::new(
                                ModelErrorKind::Protocol,
                                "model stream ended without ModelCompleted",
                            ),
                            response_started,
                        };
                    }
                }
            }
        }
    }

    /// Waits for `delay` unless the task is cancelled first. Returns `true`
    /// when cancellation won.
    pub(super) async fn sleep_or_cancel(
        &self,
        delay: Duration,
        cancellation: &CancellationToken,
    ) -> bool {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => true,
            () = tokio::time::sleep(delay) => false,
        }
    }
}

/// How one model attempt ended.
// `Completed` is a plain data carrier sized by `ModelResponse`; boxing
// it would touch every construction site for no measured win. Allowed
// after clippy 1.98 began flagging it (only under workspace feature
// unification).
#[allow(clippy::large_enum_variant)]
pub(super) enum Attempt {
    /// The provider reported completion with an authoritative response.
    Completed(ModelResponse),
    /// The task's cancellation token won against the stream.
    Cancelled,
    /// The attempt failed; `response_started` records whether any delta had
    /// arrived before the failure.
    Failed {
        error: ModelError,
        response_started: bool,
    },
}
