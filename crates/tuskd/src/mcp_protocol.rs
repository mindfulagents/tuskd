//! Hand-rolled MCP JSON-RPC core (DECISIONS D3): initialize, tools/list,
//! tools/call, ping — transport-agnostic; stdio and HTTP both feed
//! `handle_value`.

use serde_json::{json, Value};
use tusk_mcp::ToolRegistry;

pub const SERVER_NAME: &str = "tuskd";
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-03-26";

pub struct McpSession {
    registry: ToolRegistry,
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

impl McpSession {
    pub fn new(registry: ToolRegistry) -> McpSession {
        McpSession { registry }
    }

    /// Handle one raw JSON line; `None` for notifications.
    pub fn handle_line(&self, raw: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    rpc_error(Value::Null, -32700, format!("parse error: {e}")).to_string(),
                )
            }
        };
        self.handle_value(&msg).map(|v| v.to_string())
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub fn handle_value(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = match msg.get("method").and_then(|m| m.as_str()) {
            Some(m) => m,
            None => {
                return id.map(|id| rpc_error(id, -32600, "missing method".into()));
            }
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        match (method, id) {
            ("initialize", Some(id)) => {
                let version = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                Some(rpc_result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    }),
                ))
            }
            ("ping", Some(id)) => Some(rpc_result(id, json!({}))),
            ("tools/list", Some(id)) => {
                let tools: Vec<Value> = self
                    .registry
                    .list_tools()
                    .into_iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema,
                        })
                    })
                    .collect();
                Some(rpc_result(id, json!({"tools": tools})))
            }
            ("tools/call", Some(id)) => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = self.registry.call(name, &args);
                Some(rpc_result(
                    id,
                    json!({
                        "content": [{"type": "text", "text": result.text}],
                        "isError": result.is_error,
                    }),
                ))
            }
            (_, None) => None, // unknown notification: ignore
            (other, Some(id)) => Some(rpc_error(id, -32601, format!("unknown method {other:?}"))),
        }
    }
}

/// True when the message is a notification (no response expected).
pub fn is_notification(msg: &Value) -> bool {
    msg.get("id").is_none() && msg.get("method").is_some()
}
