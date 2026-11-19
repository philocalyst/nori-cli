//! In-process ACP adapter backed by the official Gemini Core execution engine.
mod auth;
mod engine;
mod provider;
mod tool_output;
pub use provider::registration;
pub use provider::serve_over_pipe;
