use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::{Memory, RecallExcludes};

/// Let the agent search its own memory
pub struct MemoryRecallTool {
    memory: Arc<dyn Memory>,
    /// Three-axis exclude set sourced from `[memory] exclude_namespaces /
    /// exclude_categories / exclude_key_prefixes`. Applied when the caller
    /// does not opt out via `scope = "all"` (the audit/debug escape hatch).
    excludes: RecallExcludes,
}

impl MemoryRecallTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            excludes: RecallExcludes::default(),
        }
    }

    /// Construct with the config-sourced three-axis excludes. Used by the
    /// agent runtime where the live `MemoryConfig` is available; the bare
    /// [`new`](Self::new) constructor is kept for tests and config-less
    /// callers (empty excludes → no-op filter).
    pub fn new_with_excludes(memory: Arc<dyn Memory>, excludes: RecallExcludes) -> Self {
        Self { memory, excludes }
    }

    /// Handle prefix recall mode: list all entries whose key starts with `prefix`.
    async fn handle_prefix_recall(&self, prefix: &str, limit: usize) -> anyhow::Result<ToolResult> {
        let entries = self.memory.list(None, None).await?;
        let matched: Vec<_> = entries
            .into_iter()
            .filter(|e| e.key.starts_with(prefix))
            .take(limit)
            .collect();

        if matched.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No memories found.".into(),
                error: None,
            });
        }

        let mut output = format!("Found {} memories:\n", matched.len());
        for entry in &matched {
            let _ = writeln!(
                output,
                "- [{}] {}: {}",
                entry.category, entry.key, entry.content
            );
        }
        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance. Supports keyword search, recent recall with omitted query or bare '*', time-only query (since/until), or both."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or phrase to search for in memory. Omit or pass bare '*' to return recent memories; non-bare wildcard terms remain keyword searches."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default: 5 for keyword, 100 for prefix)"
                },
                "since": {
                    "type": "string",
                    "description": "Filter memories created at or after this time (RFC 3339, e.g. 2025-03-01T00:00:00Z)"
                },
                "until": {
                    "type": "string",
                    "description": "Filter memories created at or before this time (RFC 3339)"
                },
                "search_mode": {
                    "type": "string",
                    "enum": ["bm25", "embedding", "hybrid"],
                    "description": "Search strategy: bm25 (keyword), embedding (semantic), or hybrid (both). Defaults to config value."
                },
                "category": {
                    "type": "string",
                    "description": "Filter by memory category (core, workstream, daily, conversation)"
                },
                "namespace": {
                    "type": "string",
                    "description": "Filter by namespace (e.g. 'telemetry' for audit, 'ws' for workstream)"
                },
                "key_prefix": {
                    "type": "string",
                    "description": "Filter by key prefix (e.g. 'ws:infra:id:' for a workstream enumeration)"
                },
                "scope": {
                    "type": "string",
                    "enum": ["filtered", "all"],
                    "description": "'filtered' (default, applies config exclusion) or 'all' (no exclusion). Use 'all' for audit/debug."
                },
                "prefix": {
                    "type": "string",
                    "description": "If set, return ALL entries whose key starts with this prefix (unranked, complete list), ignoring query/since/until/search_mode."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let since = args.get("since").and_then(|v| v.as_str());
        let until = args.get("until").and_then(|v| v.as_str());

        // Validate date strings
        if let Some(s) = since
            && chrono::DateTime::parse_from_rfc3339(s).is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid 'since' date: {s}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
            });
        }
        if let Some(u) = until
            && chrono::DateTime::parse_from_rfc3339(u).is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid 'until' date: {u}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
            });
        }
        if let (Some(s), Some(u)) = (since, until)
            && let (Ok(s_dt), Ok(u_dt)) = (
                chrono::DateTime::parse_from_rfc3339(s),
                chrono::DateTime::parse_from_rfc3339(u),
            )
            && s_dt >= u_dt
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'since' must be before 'until'".into()),
            });
        }

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5, |v| v as usize);

        // Prefix mode: list all entries whose key starts with the given prefix
        if let Some(prefix) = args.get("prefix").and_then(|v| v.as_str()) {
            let prefix = prefix.trim();
            if prefix.is_empty() {
                return Ok(ToolResult {
                    success: true,
                    output: "No memories found.".into(),
                    error: None,
                });
            }
            return self.handle_prefix_recall(prefix, limit).await;
        }

        // `scope = "all"` is the audit/debug escape hatch: skip the
        // config-sourced excludes and recall verbatim. Default ("filtered")
        // applies the three-axis excludes via `recall_filtered`.
        let scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or("filtered");
        let skip_excludes = scope.eq_ignore_ascii_case("all");
        let recall_result = if skip_excludes {
            self.memory
                .recall(query, limit, None, since, until)
                .await
        } else {
            self.memory
                .recall_filtered(query, limit, None, since, until, &self.excludes)
                .await
        };

        match recall_result {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: "No memories found.".into(),
                error: None,
            }),
            Ok(entries) => {
                // Three-axis inclusion filtering (category + namespace + key_prefix)
                let category_f = args.get("category").and_then(|v| v.as_str());
                let namespace_f = args.get("namespace").and_then(|v| v.as_str());
                let key_prefix_f = args.get("key_prefix").and_then(|v| v.as_str());
                let entries: Vec<_> = entries
                    .into_iter()
                    .filter(|e| category_f.map_or(true, |c| e.category.to_string().to_lowercase() == c.to_lowercase()))
                    .filter(|e| namespace_f.map_or(true, |ns| e.namespace == ns))
                    .filter(|e| key_prefix_f.map_or(true, |kp| e.key.starts_with(kp)))
                    .collect();
                if entries.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: "No memories found.".into(),
                        error: None,
                    });
                }
                let mut output = format!("Found {} memories:\n", entries.len());
                for entry in &entries {
                    let score = entry
                        .score
                        .map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
                    let _ = writeln!(
                        output,
                        "- [{}] {}: {}{score}",
                        entry.category, entry.key, entry.content
                    );
                }
                Ok(ToolResult {
                    success: true,
                    output: output.into(),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Memory recall failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use zeroclaw_memory::{MemoryCategory, MemoryEntry, SqliteMemory, is_recent_recall_query};

    fn seeded_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    struct QueryEchoMemory {
        last_query: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl Memory for QueryEchoMemory {
        fn name(&self) -> &str {
            "query_echo"
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
            query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            *self.last_query.lock().unwrap() = Some(query.to_string());
            if is_recent_recall_query(query) {
                Ok(vec![MemoryEntry {
                    id: "recent".into(),
                    key: "recent".into(),
                    content: "recent memory".into(),
                    category: MemoryCategory::Core,
                    timestamp: "2026-05-03T00:00:00Z".into(),
                    session_id: None,
                    score: None,
                    namespace: "default".into(),
                    importance: None,
                    superseded_by: None,
                    kind: None,
                    pinned: false,
                    tenant_id: None,
                    agent_alias: None,
                    agent_id: None,
                }])
            } else {
                Ok(Vec::new())
            }
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
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
    impl ::zeroclaw_api::attribution::Attributable for QueryEchoMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "QueryEchoMemory"
        }
    }

    #[tokio::test]
    async fn recall_empty() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"query": "anything"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memories found"));
    }

    #[tokio::test]
    async fn recall_finds_match() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("tz", "Timezone is EST", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"query": "Rust"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(result.output.contains("Found 1"));
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let (_tmp, mem) = seeded_mem();
        for i in 0..10 {
            mem.store(
                &format!("k{i}"),
                &format!("Rust fact {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"query": "Rust", "limit": 3}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 3"));
    }

    #[tokio::test]
    async fn bare_recall_returns_recent_entries() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 1"));
        assert!(result.output.contains("Rust"));
    }

    #[tokio::test]
    async fn recall_star_query_returns_recent_entries() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("tz", "Timezone is EST", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"query": "*"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 2"));
        assert!(result.output.contains("Rust"));
        assert!(result.output.contains("EST"));
    }

    #[tokio::test]
    async fn recall_star_query_uses_backend_recent_query_contract() {
        let last_query = Arc::new(Mutex::new(None));
        let mem = Arc::new(QueryEchoMemory {
            last_query: last_query.clone(),
        });
        let tool = MemoryRecallTool::new(mem);

        let result = tool.execute(json!({"query": "*"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("recent memory"));
        assert_eq!(*last_query.lock().unwrap(), Some("*".into()));
    }

    #[tokio::test]
    async fn recall_time_only_returns_entries() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryRecallTool::new(mem);
        // Time-only: since far in past
        let result = tool
            .execute(json!({"since": "2020-01-01T00:00:00Z", "limit": 5}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 1"));
        assert!(result.output.contains("Rust"));
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        assert_eq!(tool.name(), "memory_recall");
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
    }

    #[test]
    fn score_formatted_as_percent() {
        let score: Option<f64> = Some(0.63);
        let formatted = score.map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
        assert_eq!(formatted, " [63%]");

        let score: Option<f64> = Some(0.42);
        let formatted = score.map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
        assert_eq!(formatted, " [42%]");

        let score: Option<f64> = Some(1.0);
        let formatted = score.map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
        assert_eq!(formatted, " [100%]");

        let score: Option<f64> = Some(0.0);
        let formatted = score.map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
        assert_eq!(formatted, " [0%]");

        let score: Option<f64> = None;
        let formatted = score.map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
        assert_eq!(formatted, "");
    }

    #[tokio::test]
    async fn recall_prefix_returns_only_matching_keys() {
        let (_tmp, mem) = seeded_mem();
        mem.store("proj:a:1", "first", MemoryCategory::Core, None).await.unwrap();
        mem.store("proj:a:2", "second", MemoryCategory::Core, None).await.unwrap();
        mem.store("proj:b:1", "third", MemoryCategory::Core, None).await.unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"prefix": "proj:a:"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("proj:a:1"));
        assert!(result.output.contains("proj:a:2"));
        assert!(!result.output.contains("proj:b:1"));
    }

    #[tokio::test]
    async fn recall_prefix_respects_limit() {
        let (_tmp, mem) = seeded_mem();
        for i in 0..5 {
            mem.store(
                &format!("proj:a:{i}"),
                &format!("entry {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"prefix": "proj:a:", "limit": 2}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 2"));
    }

    #[tokio::test]
    async fn recall_prefix_empty_returns_nothing() {
        let (_tmp, mem) = seeded_mem();
        mem.store("some_key", "value", MemoryCategory::Core, None).await.unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"prefix": "nonexistent:"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memories found"));
    }

    #[test]
    fn schema_includes_search_mode_parameter() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        let schema = tool.parameters_schema();
        let search_mode = &schema["properties"]["search_mode"];
        assert_eq!(search_mode["type"], "string");
        let enum_values = search_mode["enum"].as_array().unwrap();
        assert_eq!(enum_values.len(), 3);
        assert!(enum_values.contains(&json!("bm25")));
        assert!(enum_values.contains(&json!("embedding")));
        assert!(enum_values.contains(&json!("hybrid")));
    }

    #[tokio::test]
    async fn filtered_scope_applies_config_excludes() {
        let (_tmp, mem) = seeded_mem();
        // telemetry entry should be excluded by namespace prefix "telemetry".
        mem.store_with_metadata(
            "sop:1",
            "SOP runbook",
            MemoryCategory::Custom("sop".into()),
            None,
            Some("telemetry_sop"),
            None,
        )
        .await
        .unwrap();
        mem.store("core:1", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let excludes = RecallExcludes {
            namespaces: vec!["telemetry".into()],
            ..Default::default()
        };
        let tool = MemoryRecallTool::new_with_excludes(mem, excludes);
        // Default scope is "filtered" → telemetry_sop entry is dropped.
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(!result.output.contains("SOP runbook"));
    }

    #[tokio::test]
    async fn scope_all_bypasses_config_excludes() {
        let (_tmp, mem) = seeded_mem();
        mem.store_with_metadata(
            "sop:1",
            "SOP runbook",
            MemoryCategory::Custom("sop".into()),
            None,
            Some("telemetry_sop"),
            None,
        )
        .await
        .unwrap();
        mem.store("core:1", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let excludes = RecallExcludes {
            namespaces: vec!["telemetry".into()],
            ..Default::default()
        };
        let tool = MemoryRecallTool::new_with_excludes(mem, excludes);
        // scope=all is the audit escape hatch: excludes are NOT applied.
        let result = tool.execute(json!({"scope": "all"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(
            result.output.contains("SOP runbook"),
            "scope=all must return the excluded entry for audit/debug"
        );
    }

    #[tokio::test]
    async fn filtered_scope_category_exclude_exact_match() {
        let (_tmp, mem) = seeded_mem();
        mem.store("sop:1", "SOP runbook", MemoryCategory::Custom("sop".into()), None)
            .await
            .unwrap();
        mem.store("core:1", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let excludes = RecallExcludes {
            categories: vec!["sop".into()],
            ..Default::default()
        };
        let tool = MemoryRecallTool::new_with_excludes(mem, excludes);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(!result.output.contains("SOP runbook"));
    }
}
