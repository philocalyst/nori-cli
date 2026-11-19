//! The PreToolUse hook: permission checking for the CLI's tool calls, asking the client when
//! the rules say to.

mod output;
mod pre_tool_use;

pub use pre_tool_use::create_pre_tool_use_hook;
