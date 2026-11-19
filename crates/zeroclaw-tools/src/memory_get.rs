use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::Memory;

/// Fetch ONE memory's full content by its exact key — the read-side
/// companion to `memory_recall`.
///
/// Why this tool exists: `memory_recall` returns listing lines, and the
/// agent loop's universal tool-output cap (`[risk_profiles.*]
/// max_tool_result_chars`, applied by the history module's
/// `truncate_tool_result` before any tool result enters the context
/// window) clips oversized tool results — so a long memory can arrive
/// truncated inside a preview line. This tool returns the entry's stored
/// content VERBATIM: the key, the full content, and the row's metadata.
///
/// Honesty note: this tool adds NO truncation of its own, but the
/// universal cap still applies to THIS tool's output — a single memory
/// larger than the configured cap is clipped by the loop, not here. The
/// knob is config-exposed (`max_tool_result_chars`, default 50,000, with
/// the per-agent runtime-profile override), so the operator, not the
/// tool, owns that bound.
///
/// The two-step flow the tools teach: recall (previews) to find keys,
/// then `memory_get` for one key's full text. An unknown key is a clear
/// "not found" success (the agent decides what to do next), not an
/// error; a backend error surfaces. The fetch is session-less like the
/// sibling tools — the exact-key fetch rides the `Memory::get` contract.
pub struct MemoryGetTool {
    memory: Arc<dyn Memory>,
}

impl MemoryGetTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Fetch one memory's full content by exact key"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The exact memory key to fetch"
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::Error::msg("Missing 'key' parameter"))?;

        match self.memory.get(key).await {
            Ok(Some(entry)) => {
                // The full stored content, verbatim — no truncation here
                // (the loop's universal cap still applies downstream).
                let mut output = format!("Memory '{}':\n", entry.key);
                let _ = writeln!(output, "content: {}", entry.content);
                let _ = writeln!(output, "category: {}", entry.category);
                let _ = writeln!(output, "recorded: {}", entry.timestamp);
                let _ = writeln!(output, "namespace: {}", entry.namespace);
                if let Some(session) = entry.session_id.as_deref() {
                    let _ = writeln!(output, "session: {session}");
                }
                if let Some(importance) = entry.importance {
                    let _ = writeln!(output, "importance: {importance:.2}");
                }
                if entry.pinned {
                    let _ = writeln!(output, "pinned: true");
                }
                if let Some(superseded_by) = entry.superseded_by.as_deref() {
                    let _ = writeln!(output, "superseded_by: {superseded_by}");
                }
                if let Some(agent_alias) = entry.agent_alias.as_deref() {
                    let _ = writeln!(output, "agent: {agent_alias}");
                }
                Ok(ToolResult {
                    success: true,
                    output: output.into(),
                    error: None,
                })
            }
            Ok(None) => Ok(ToolResult {
                success: true,
                output: format!("No memory found with key: {key}").into(),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Memory get failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zeroclaw_memory::{MemoryCategory, MemoryEntry};

    /// A fake whose `get` returns a canned outcome — one fully-populated
    /// entry, a not-found None, or a backend error — so every contract path
    /// is pinned deterministically without a storage engine.
    struct FakeMemory {
        entry: Option<MemoryEntry>,
        error: Option<String>,
    }

    impl FakeMemory {
        fn with_entry(entry: MemoryEntry) -> Arc<Self> {
            Arc::new(Self {
                entry: Some(entry),
                error: None,
            })
        }

        fn empty() -> Arc<Self> {
            Arc::new(Self {
                entry: None,
                error: None,
            })
        }

        fn failing(message: &str) -> Arc<Self> {
            Arc::new(Self {
                entry: None,
                error: Some(message.to_string()),
            })
        }
    }

    fn populated_entry(key: &str, content: String) -> MemoryEntry {
        MemoryEntry {
            principal_id: None,
            id: "row:1".into(),
            key: key.into(),
            content,
            category: MemoryCategory::Custom("workstream".into()),
            timestamp: "2026-11-19T00:00:00Z".into(),
            session_id: Some("sess-42".into()),
            score: None,
            namespace: "default".into(),
            importance: Some(0.8),
            superseded_by: Some("proj:deploy:v2".into()),
            kind: None,
            pinned: true,
            tenant_id: None,
            agent_alias: Some("rocky".into()),
            agent_id: None,
        }
    }

    #[async_trait]
    impl Memory for FakeMemory {
        fn name(&self) -> &str {
            "fake-get"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            if let Some(error) = &self.error {
                anyhow::bail!("{error}");
            }
            Ok(self.entry.clone())
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        async fn store_with_agent(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.recall(query, limit, session_id, since, until).await
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for FakeMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "FakeMemory"
        }
    }

    /// THE load-bearing contract: the entry's FULL stored content is
    /// rendered, not a preview. `memory_recall` returns listing lines and
    /// the agent loop's universal tool-output cap truncates oversized tool
    /// results, so a long memory can arrive clipped in a preview; this
    /// tool must hand the complete content to that cap. A preview would
    /// clip the tail — both ends of a multi-KB body must survive
    /// verbatim, and the row's metadata must ride along.
    #[tokio::test]
    async fn memory_get_returns_full_content_for_known_key() {
        let content = format!("HEAD-marker:{}", "x".repeat(8192)) + ":TAIL-marker";
        let entry = populated_entry("proj:deploy", content.clone());
        let tool = MemoryGetTool::new(FakeMemory::with_entry(entry));

        let result = tool.execute(json!({"key": "proj:deploy"})).await.unwrap();

        assert!(result.success, "a known key must succeed");
        let output = result.output.to_string();
        assert!(
            output.contains(&content),
            "the FULL stored content must be rendered verbatim, not a preview"
        );
        assert!(
            output.contains("HEAD-marker:"),
            "the content head must be rendered: {output}"
        );
        assert!(
            output.contains(":TAIL-marker"),
            "the content tail must survive — a preview would clip it: {}",
            &output[output.len().saturating_sub(160)..]
        );
        assert!(output.contains("proj:deploy"), "the key must be rendered");
        assert!(
            output.contains("category: workstream"),
            "the category metadata must be rendered: {output}"
        );
        assert!(
            output.contains("recorded: 2026-11-19T00:00:00Z"),
            "the timestamp metadata must be rendered: {output}"
        );
        assert!(
            output.contains("namespace: default"),
            "the namespace metadata must be rendered: {output}"
        );
        assert!(
            output.contains("session: sess-42"),
            "the session metadata must be rendered: {output}"
        );
        assert!(
            output.contains("importance: 0.80"),
            "the importance metadata must be rendered: {output}"
        );
        assert!(
            output.contains("pinned: true"),
            "the pinned metadata must be rendered: {output}"
        );
        assert!(
            output.contains("superseded_by: proj:deploy:v2"),
            "the superseded_by metadata must be rendered: {output}"
        );
        assert!(
            output.contains("agent: rocky"),
            "the agent metadata must be rendered: {output}"
        );
    }

    /// An unknown key is a clear not-found SUCCESS (the agent decides what
    /// to do next — search, re-store, or give up), never an error.
    #[tokio::test]
    async fn memory_get_unknown_key_reports_not_found() {
        let tool = MemoryGetTool::new(FakeMemory::empty());

        let result = tool.execute(json!({"key": "no-such-key"})).await.unwrap();

        assert!(
            result.success,
            "a not-found must be a success the agent can act on, not an error"
        );
        assert!(
            result
                .output
                .contains("No memory found with key: no-such-key"),
            "the not-found message must name the key: {}",
            result.output
        );
        assert!(result.error.is_none(), "no error on a clean not-found");
    }

    /// A backend error surfaces (success: false with the message) — a
    /// silent empty result would masquerade as a not-found.
    #[tokio::test]
    async fn memory_get_backend_error_surfaces() {
        let tool = MemoryGetTool::new(FakeMemory::failing("backend down"));

        let result = tool.execute(json!({"key": "proj:deploy"})).await.unwrap();

        assert!(!result.success, "a backend error must not claim success");
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            error.contains("Memory get failed") && error.contains("backend down"),
            "the backend error must surface with its message, got: {error}"
        );
    }

    /// The missing-parameter path mirrors the sibling `memory_forget`'s
    /// convention: the call fails (the schema marks `key` required; this
    /// is the belt for direct callers).
    #[tokio::test]
    async fn memory_get_missing_key_fails_the_call() {
        let tool = MemoryGetTool::new(FakeMemory::empty());

        let result = tool.execute(json!({})).await;

        assert!(
            result.is_err(),
            "a missing key parameter must fail the call"
        );
    }

    /// The schema pin: {key} only — the exact-key fetch takes no session,
    /// scope, or limit parameters (the sibling tools are session-less by
    /// the session-scope contract; no session_id creep).
    #[test]
    fn memory_get_schema_exposes_key_only() {
        let tool = MemoryGetTool::new(FakeMemory::empty());
        assert_eq!(tool.name(), "memory_get");

        let schema = tool.parameters_schema();
        let props = schema["properties"].as_object().expect("properties object");
        assert_eq!(
            props.len(),
            1,
            "the schema must expose exactly one property, got: {props:?}"
        );
        assert!(props.contains_key("key"), "the sole property must be `key`");
        assert_eq!(schema["properties"]["key"]["type"], "string");
        assert_eq!(
            schema["required"],
            json!(["key"]),
            "`key` must be the required parameter"
        );
    }
}
