//! Modified for Nori. Derived from helix-stdx of github.com/helix-editor/helix (commit
//! 9de242bfac2aac9dfd0e4496eb25190e85c24e69), Mozilla Public License 2.0 (see ../../LICENSE).
//!
//! Only the RFC3986 `Url` newtype that `helix-lsp-types` is written against is kept; the rest of
//! Helix's standard library extensions (ropes, paths, regex-cursor, faccess, ...) is dropped.

pub mod uri;

pub use uri::Url;
