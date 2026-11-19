//! The tool runtime seam: how a program's declared tools run.
//!
//! `host` tools go to the [`ProgramHost`](crate::host::ProgramHost). `wasm` tools go to a
//! [`ToolRuntime`]: a sandboxed WebAssembly engine that calls the tool's exported function
//! with JSON arguments and only the capabilities its grants name. This build has no
//! WebAssembly engine; [`UnavailableToolRuntime`] fails every wasm call honestly, naming the
//! tool and what is missing, so a program that needs one stops at load time instead of
//! pretending.

use async_trait::async_trait;

use crate::error::ConduitError;
use crate::host::ToolOutcome;
use crate::schemas::ToolSource;
use crate::schemas::ToolSpec;
use crate::value::ConduitValue;

/// One invocation of a declared tool.
pub struct ToolCall<'a> {
    pub tool: &'a ToolSpec,
    /// JSON arguments, shaped by the tool's `args_schema`.
    pub args: &'a ConduitValue,
    /// Directory the module path is relative to (the program file's directory).
    pub base_dir: &'a std::path::Path,
}

#[async_trait]
pub trait ToolRuntime: Send + Sync {
    /// Whether this runtime can run `tool` at all; checked before a program starts so an
    /// unrunnable tool is reported once, up front, with the tool's name.
    fn check(&self, tool: &ToolSpec) -> Result<(), ConduitError>;

    async fn invoke(&self, call: ToolCall<'_>) -> Result<ToolOutcome, ConduitError>;
}

/// The runtime of builds without WebAssembly: runs nothing, says why.
pub struct UnavailableToolRuntime;

fn unavailable(tool: &ToolSpec) -> ConduitError {
    let module = match &tool.source {
        ToolSource::Wasm { file, .. } => file.as_str(),
        ToolSource::Host => "(host tool)",
    };
    ConduitError::Unsupported {
        what: format!("wasm tool `{}` (module {module})", tool.name),
        reason: "this build of Nori has no WebAssembly runtime; remove the call, or run the \
                 program on a Nori build that provides one"
            .to_owned(),
    }
}

#[async_trait]
impl ToolRuntime for UnavailableToolRuntime {
    fn check(&self, tool: &ToolSpec) -> Result<(), ConduitError> {
        match tool.source {
            ToolSource::Host => Ok(()),
            ToolSource::Wasm { .. } => Err(unavailable(tool)),
        }
    }

    async fn invoke(&self, call: ToolCall<'_>) -> Result<ToolOutcome, ConduitError> {
        Err(unavailable(call.tool))
    }
}
