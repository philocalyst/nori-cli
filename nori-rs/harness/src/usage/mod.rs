//! Usage accounting: what agents report about tokens and quota, kept on disk and summarized for
//! the TUI's usage view.
//!
//! Agents attach a [`nori_protocol::UsageReport`] to `_meta.nori` of their usage updates. A
//! [`UsageTap`] per session turns each report into a [`UsageSample`] (the delta since the last
//! report, never a running total) and a [`UsageWriter`] appends it to
//! `$NORI_HOME/usage/YYYY-MM.jsonl` off the session's task. [`UsageStore::summary`] reads the
//! months a period touches and folds the samples into a [`UsageSummary`]: time buckets per
//! provider, per-model totals priced by the [`PriceTable`], and rate-limit windows pooled across
//! the sessions that share an account.

mod account;
mod build;
mod limits;
mod pricing;
mod sample;
mod store;
mod summary;
mod tap;

pub use account::AccountId;
pub use account::AccountResolver;
pub use limits::AccountLimits;
pub use limits::PooledWindow;
pub use pricing::ModelPrice;
pub use pricing::PriceRow;
pub use pricing::PriceSource;
pub use pricing::PriceTable;
pub use sample::UsageSample;
pub use sample::provider_for;
pub use sample::provider_title;
pub use store::UsageStore;
pub use store::UsageWriter;
pub use store::subscribe_samples;
pub use summary::AccountModelUsage;
pub use summary::AccountUsage;
pub use summary::BucketValue;
pub use summary::CategoryCost;
pub use summary::Constraint;
pub use summary::ModelUsage;
pub use summary::ProviderUsage;
pub use summary::TimeBucket;
pub use summary::UsagePeriod;
pub use summary::UsageSummary;
pub use summary::UsageTotals;
pub use tap::UsageTap;
