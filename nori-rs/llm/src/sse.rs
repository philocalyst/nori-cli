//! Server-sent events, read one at a time with a stall timeout.

use std::pin::Pin;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use serde_json::Value;

use crate::ApiError;

type Inner = Pin<
    Box<
        dyn Stream<
                Item = Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<reqwest::Error>,
                >,
            > + Send,
    >,
>;

/// One event: its name (`message` when the server gave none) and its data parsed as JSON.
pub(crate) struct Event {
    pub(crate) name: String,
    pub(crate) data: Value,
    /// The data as sent, for `[DONE]` and the like.
    pub(crate) raw: String,
}

/// The events of one streamed response.
pub(crate) struct Events {
    inner: Inner,
    chunk: Duration,
}

impl Events {
    pub(crate) fn new(response: reqwest::Response, chunk: Duration) -> Self {
        Self {
            inner: Box::pin(response.bytes_stream().eventsource()),
            chunk,
        }
    }

    /// The next event; `None` when the stream ended.
    pub(crate) async fn next(&mut self) -> Result<Option<Event>, ApiError> {
        loop {
            let next = tokio::time::timeout(self.chunk, self.inner.next())
                .await
                .map_err(|_| ApiError::timeout(self.chunk, "the next part of the answer"))?;
            let event = match next {
                None => return Ok(None),
                Some(Ok(event)) => event,
                Some(Err(eventsource_stream::EventStreamError::Transport(error))) => {
                    return Err(ApiError::disconnected(ApiError::from_transport(error)));
                }
                Some(Err(error)) => return Err(ApiError::disconnected(error)),
            };
            if event.data.is_empty() {
                continue;
            }
            let data = serde_json::from_str(&event.data).unwrap_or(Value::Null);
            return Ok(Some(Event {
                name: if event.event.is_empty() {
                    "message".to_owned()
                } else {
                    event.event
                },
                data,
                raw: event.data,
            }));
        }
    }
}
