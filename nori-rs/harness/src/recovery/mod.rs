//! Recovery from failed requests: classification feeds a declarative policy (`recovery { ... }`
//! in Pkl, typed in `nori-workflow`), which one engine executes.
//!
//! - [`engine`]: the pure decision, retry / fall back / surface.
//! - [`plan`] and [`ranking`]: which routes a fallback may use, in what order. The same
//!   ranking orders every model picker.
//! - [`catalog`] and [`identity`]: the routes Nori knows (installed agents, advertised models,
//!   known failures) and when two of them are the same model.
//! - [`ledger`]: what a conversation has learned about routes that failed it.
//!
//! Executing decisions (waiting, switching models, asking the front end to switch agents) is
//! `backend/prompt_recovery.rs`, which owns the session.

mod catalog;
pub(crate) mod engine;
pub(crate) mod handoff;
mod headroom;
mod identity;
pub(crate) mod ledger;
pub(crate) mod plan;
mod ranking;

pub use catalog::Availability;
pub use catalog::CatalogAgent;
pub use catalog::CatalogModel;
pub use catalog::installed_agents;
pub use catalog::is_debug_fixture;
pub use catalog::route_catalog;
pub use headroom::AccountHeadroom;
pub use headroom::Room;
pub use headroom::SPENT_BELOW_PERCENT;
pub use identity::ModelFamily;
pub use identity::ModelIdentity;
pub use identity::display_name;
pub use identity::model_label;
pub use identity::normalized;
pub use ranking::RouteKey;
pub use ranking::RouteRanking;
pub use ranking::is_gateway;
