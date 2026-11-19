//! The turn ledger.
//!
//! A session has at most one turn (see [`crate::normalized::turn`]). The modules here are its
//! rules, split by the question each one answers:
//!
//! - [`ledger`]: how a turn opens and ends, what ending closes, what carries over.
//! - [`queue`]: which prompt starts when.
//! - [`cancel`]: what cancelling does to each kind of turn.
//! - [`lifecycle`]: the requests that own a turn: prompt, load, connection.
//! - [`updates`] and [`metadata`]: agent activity and the session facts it carries.
//! - [`permissions`]: requests the agent waits on, valid in every turn.
//! - [`quiescence`]: when an agent turn is over.
//!
//! The driver-side halves, which perform effects, are [`dispatch`], [`send_prompt`],
//! [`approvals`], [`completion`] and [`failure`].

pub(super) mod approvals;
pub(super) mod cancel;
pub(super) mod completion;
pub(super) mod dispatch;
pub(super) mod failure;
pub(super) mod ledger;
pub(super) mod lifecycle;
pub(super) mod metadata;
pub(super) mod permissions;
pub(super) mod queue;
pub(super) mod quiescence;
pub(super) mod send_prompt;
pub(super) mod updates;
