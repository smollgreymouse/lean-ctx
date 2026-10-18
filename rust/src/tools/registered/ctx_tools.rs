use rmcp::ErrorData;
use rmcp::model::Tool;
use serde_json::{Map, Value, json};

use crate::server::tool_trait::{McpTool, ToolContext, ToolOutput};
use crate::tool_defs::tool_def;

/// `ctx_tools` — MCP Tool-Catalog Gateway (#210). Aggregates downstream MCP
/// servers and returns a per-query top-N shortlist instead of injecting every
/// downstream schema, then proxies the real call.
pub struct CtxToolsTool;

impl McpTool for CtxToolsTool {
    fn name(&self) -> &'static str {
        "ctx_tools"
    }

    fn tool_def(&self) -> Tool {
        tool_def(
            "ctx_tools",
            "Gateway to downstream MCP servers — unlimited external tools at ~constant context cost.\n\
             actions: find (query → top-N relevant tools) | call (proxy a server::tool) |\n\
             list (servers+counts) | refresh.\n\
             WORKFLOW: find to discover, then call the chosen server::tool.\n\
             ANTIPATTERN: not for built-in tools — use those directly.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["find", "call", "list", "refresh"],
                        "description": "find|call|list|refresh"
                    },
                    "query": { "type": "string", "description": "What you want to do (for find)" },
                    "tool": { "type": "string", "description": "`server::tool` handle (for call)" },
                    "arguments": { "type": "object", "description": "Arguments for downstream tool (call)" }
                },
                "allOf": [
                    {
                        "if": { "properties": { "action": { "const": "call" } }, "required": ["action"] },
                        "then": { "required": ["action", "tool"] }
                    }
                ]
            }),
        )
    }

    fn handle(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ErrorData> {
        // `project_root` is threaded through so the gateway's L3 consolidation
        // (#1095) can write addon output into the project's BM25/graph/knowledge
        // stores. Empty (one-shot CLI ctx) disables project-scoped indexing.
        let progress_sender = ctx
            .progress_sender
            .as_ref()
            .and_then(|shared| shared.lock().ok().and_then(|sender| sender.clone()));

        let progress = progress_sender.clone().map(|sender| {
            std::sync::Arc::new(
                move |update: crate::core::mcp_catalog::client::ProgressUpdate| {
                    sender.send(update.progress, update.total, update.message);
                },
            ) as crate::core::mcp_catalog::client::ProgressCallback
        });

        let result = crate::tools::ctx_tools::run_with_progress(args, &ctx.project_root, progress);

        // ProgressSender drains on the async server runtime while this handler
        // runs in spawn_blocking. Flush before returning so the terminal
        // tools/call response cannot overtake queued UI progress notifications.
        if let Some(sender) = progress_sender {
            if !sender.flush_blocking(std::time::Duration::from_secs(5)) {
                tracing::debug!("[ctx_tools] progress flush timed out before terminal result");
            }
        }

        match result {
            Ok(text) => Ok(ToolOutput::simple(text)),
            Err(e) => Err(ErrorData::invalid_params(e, None)),
        }
    }
}
