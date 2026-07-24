#![forbid(unsafe_code)]

//! tusk-mcp: MCP tool registry bound to an agent identity; transport-agnostic
//! (spec §5). DENIED errors as `isError` text; success = pretty JSON.

mod context;
mod tools;

pub use context::TuskContext;
pub use tools::{record_json, ToolDef, ToolRegistry, ToolResult};
