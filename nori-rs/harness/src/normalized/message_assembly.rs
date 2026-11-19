//! Assembly of streamed ACP chunks into whole messages.
//!
//! ACP chunk updates do not consistently carry message identities, so the turn
//! assembles them itself. One buffer is open at a time. A chunk continues the
//! open message only when it is the same stream and does not name a different
//! message ID; anything else closes the buffer first, so two messages, or a
//! message and a notice, are never concatenated into one.

use nori_protocol::MessageStream;

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenMessage {
    stream: MessageStream,
    message_id: Option<String>,
    text: String,
}

/// The message currently being streamed and the last answer a turn finished.
#[derive(Debug, Clone, Default)]
pub struct MessageAssembly {
    open: Option<OpenMessage>,
    last_answer: Option<String>,
}

impl MessageAssembly {
    /// Append a chunk, closing the open message first when the chunk starts a new one.
    pub fn push(&mut self, stream: MessageStream, message_id: Option<&str>, text: &str) {
        let continues = self.open.as_ref().is_some_and(|open| {
            open.stream == stream
                && match (open.message_id.as_deref(), message_id) {
                    (Some(open_id), Some(chunk_id)) => open_id == chunk_id,
                    (None, _) | (Some(_), None) => true,
                }
        });
        if !continues {
            self.close();
        }
        let open = self.open.get_or_insert_with(|| OpenMessage {
            stream,
            message_id: message_id.map(str::to_owned),
            text: String::new(),
        });
        if open.message_id.is_none() {
            open.message_id = message_id.map(str::to_owned);
        }
        open.text.push_str(text);
    }

    /// Close the open message. Call at every boundary that is not a chunk of it: a tool call, a
    /// plan, a permission request, a notice, the end of the turn.
    pub fn close(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        if open.stream == MessageStream::Answer && !open.text.is_empty() {
            self.last_answer = Some(open.text);
        }
    }

    /// The last complete answer, closing the open message first.
    pub fn finish(&mut self) -> Option<String> {
        self.close();
        self.last_answer.take()
    }
}
