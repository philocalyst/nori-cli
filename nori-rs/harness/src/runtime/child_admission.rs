//! Ordered admission of controls for each child, independent of other children.
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::backend::AcpBackend;

type Reply = oneshot::Sender<anyhow::Result<()>>;

pub(super) enum ChildCommand {
    Send(Vec<acp::ContentBlock>, Reply),
    Steer(Vec<acp::ContentBlock>, Reply),
    Promote(Reply),
    Cancel(Reply),
}

struct AdmissionMailboxes<K, C> {
    workers: HashMap<K, mpsc::UnboundedSender<C>>,
}

impl<K: Eq + Hash, C> AdmissionMailboxes<K, C> {
    fn enqueue(
        &mut self,
        id: K,
        command: C,
        start: impl FnOnce() -> mpsc::UnboundedSender<C>,
    ) -> Result<(), C> {
        self.workers
            .entry(id)
            .or_insert_with(start)
            .send(command)
            .map_err(|error| error.0)
    }
}

pub(super) struct ChildAdmissions {
    mailboxes: AdmissionMailboxes<acp::SessionId, ChildCommand>,
}

impl Default for ChildAdmissions {
    fn default() -> Self {
        Self {
            mailboxes: AdmissionMailboxes {
                workers: HashMap::new(),
            },
        }
    }
}

impl ChildAdmissions {
    /// Called synchronously by the harness actor in journal order. A worker
    /// waits only for each operation's admission, never for a completed turn.
    pub(super) fn send(
        &mut self,
        backend: &Arc<AcpBackend>,
        id: acp::SessionId,
        command: ChildCommand,
    ) {
        let result = self.mailboxes.enqueue(id.clone(), command, || {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let backend = Arc::clone(backend);
            tokio::spawn(async move {
                while let Some(command) = rx.recv().await {
                    match command {
                        ChildCommand::Send(content, reply) => {
                            let _ = reply
                                .send(backend.send_subagent_content(id.clone(), content).await);
                        }
                        ChildCommand::Steer(content, reply) => {
                            let _ = reply
                                .send(backend.steer_subagent_content(id.clone(), content).await);
                        }
                        ChildCommand::Promote(reply) => {
                            let _ = reply.send(backend.promote_subagent_queue(id.clone()).await);
                        }
                        ChildCommand::Cancel(reply) => {
                            let _ = reply.send(backend.cancel_subagent(id.clone()).await);
                        }
                    }
                }
            });
            tx
        });
        if let Err(command) = result {
            let reply = match command {
                ChildCommand::Send(_, reply)
                | ChildCommand::Steer(_, reply)
                | ChildCommand::Promote(reply)
                | ChildCommand::Cancel(reply) => reply,
            };
            let _ = reply.send(Err(anyhow::anyhow!("child admission worker stopped")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AdmissionMailboxes;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn child_mailboxes_preserve_burst_order_without_blocking_other_children() {
        let mut boxes = AdmissionMailboxes {
            workers: HashMap::new(),
        };
        let (events, mut received) = mpsc::unbounded_channel();
        let (release, gate) = oneshot::channel::<()>();
        let (first_tx, mut first_rx) = mpsc::unbounded_channel();
        let first_events = events.clone();
        tokio::spawn(async move {
            let first = first_rx.recv().await.expect("first command");
            first_events.send((1, first)).expect("record first");
            let _ = gate.await;
            while let Some(command) = first_rx.recv().await {
                first_events.send((1, command)).expect("record next");
            }
        });
        boxes.enqueue(1, "send A", || first_tx).expect("enqueue A");
        for command in ["send B", "promote", "steer", "cancel"] {
            boxes
                .enqueue(1, command, || panic!("reuse worker"))
                .expect("enqueue command");
        }
        let other_events = events.clone();
        boxes
            .enqueue(2, "other child", || {
                let (tx, mut rx) = mpsc::unbounded_channel();
                tokio::spawn(async move {
                    if let Some(command) = rx.recv().await {
                        other_events.send((2, command)).expect("other event");
                    }
                });
                tx
            })
            .expect("enqueue other child");
        let mut before_release = vec![
            received.recv().await.expect("first event"),
            received.recv().await.expect("second event"),
        ];
        before_release.sort();
        assert_eq!(before_release, vec![(1, "send A"), (2, "other child")]);
        release.send(()).expect("release first child");
        let mut remaining = Vec::new();
        for _ in 0..4 {
            remaining.push(received.recv().await.expect("remaining event"));
        }
        assert_eq!(
            remaining,
            vec![(1, "send B"), (1, "promote"), (1, "steer"), (1, "cancel")]
        );
    }
}
