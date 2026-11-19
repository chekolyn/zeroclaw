use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::policy::ToolOperation;
use zeroclaw_memory::{Memory, MemoryCategory};

/// Let the agent store memories — its own brain writes
pub struct MemoryStoreTool {
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl MemoryStoreTool {
    pub fn new(memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self { memory, security }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store a fact in long-term memory"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string"
                },
                "content": {
                    "type": "string"
                },
                "category": {
                    "type": "string",
                    "description": "core, daily, conversation, or custom (default core)"
                },
                "append": {
                    "type": "boolean"
                }
            },
            "required": ["key", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"param": "key"})),
                "memory_store: missing key parameter"
            );
            anyhow::Error::msg("Missing 'key' parameter")
        })?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"param": "content"})),
                    "memory_store: missing content parameter"
                );
                anyhow::Error::msg("Missing 'content' parameter")
            })?;

        let category = match args.get("category").and_then(|v| v.as_str()) {
            Some("core") | None => MemoryCategory::Core,
            Some("daily") => MemoryCategory::Daily,
            Some("conversation") => MemoryCategory::Conversation,
            Some(other) => MemoryCategory::Custom(other.to_string()),
        };

        let append = args
            .get("append")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "memory_store")
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            });
        }

        let result = if append {
            self.memory.store_append(key, content, category, None).await
        } else {
            self.memory.store(key, content, category, None).await
        };

        match result {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Stored memory: {key}").into(),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Failed to store memory: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::policy::SecurityPolicy;
    use zeroclaw_memory::{MemoryEntry, SqliteMemory};

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn test_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        assert_eq!(tool.name(), "memory_store");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["key"].is_object());
        assert!(schema["properties"]["content"].is_object());
    }

    #[test]
    fn eager_schema_is_slim() {
        // `memory_store` is in the runtime's EAGER_BUILTINS: its spec rides
        // the wire on every call. The slimmed eager wire keeps the SHAPE
        // (names/types/required); only the category value space stays as a
        // one-liner (it is not inferable from the name alone).
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        assert!(
            tool.description().chars().count() <= 60,
            "eager tool description must stay a short clause: {}",
            tool.description()
        );
        let schema = tool.parameters_schema();
        let wire = serde_json::to_string(&schema).unwrap();
        assert!(
            wire.len() <= 320,
            "eager schema wire must stay under 320 bytes, got {}: {wire}",
            wire.len()
        );
        assert_eq!(schema["required"], json!(["key", "content"]));
        let props = schema["properties"].as_object().expect("properties object");
        assert_eq!(props.len(), 4, "property set must not drift");
        assert_eq!(props["key"]["type"], "string");
        assert_eq!(props["content"]["type"], "string");
        assert_eq!(props["category"]["type"], "string");
        assert_eq!(props["append"]["type"], "boolean");
        for (name, prop) in props {
            if let Some(desc) = prop.get("description").and_then(|d| d.as_str()) {
                assert!(
                    desc.chars().count() <= 64,
                    "property `{name}` description must stay a one-liner: {desc}"
                );
            }
            assert!(
                prop.get("default").is_none(),
                "property `{name}` must not carry a default on the eager wire"
            );
            assert!(
                prop.get("examples").is_none(),
                "property `{name}` must not carry examples on the eager wire"
            );
        }
    }

    #[tokio::test]
    async fn store_core() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("lang"));

        let entry = mem.get("lang").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "Prefers Rust");
    }

    #[tokio::test]
    async fn store_with_category() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "note", "content": "Fixed bug", "category": "daily"}))
            .await
            .unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn store_with_custom_category() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(
                json!({"key": "proj_note", "content": "Uses async runtime", "category": "project"}),
            )
            .await
            .unwrap();
        assert!(result.success);

        let entry = mem.get("proj_note").await.unwrap().unwrap();
        assert_eq!(entry.content, "Uses async runtime");
        assert_eq!(entry.category, MemoryCategory::Custom("project".into()));
    }

    #[tokio::test]
    async fn store_missing_key() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        let result = tool.execute(json!({"content": "no key"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn store_missing_content() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        let result = tool.execute(json!({"key": "no_content"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn store_blocked_in_readonly_mode() {
        let (_tmp, mem) = test_mem();
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = MemoryStoreTool::new(mem.clone(), readonly);
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("read-only mode")
        );
        assert!(mem.get("lang").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn store_append_text_concatenates() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        // First store "line1"
        let result1 = tool
            .execute(json!({"key": "append_test", "content": "line1"}))
            .await
            .unwrap();
        assert!(result1.success);

        // Append "line2"
        let result2 = tool
            .execute(json!({"key": "append_test", "content": "line2", "append": true}))
            .await
            .unwrap();
        assert!(result2.success);

        // Verify content contains both lines
        let entry = mem.get("append_test").await.unwrap().unwrap();
        assert!(entry.content.contains("line1"));
        assert!(entry.content.contains("line2"));
        // Newline separator between them
        assert!(entry.content == "line1\nline2" || entry.content == "line2\nline1");
    }

    #[tokio::test]
    async fn store_append_json_array_pushes() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        // Store a JSON array
        let result1 = tool
            .execute(json!({"key": "arr_test", "content": "[\"a\"]"}))
            .await
            .unwrap();
        assert!(result1.success);

        // Append a JSON value (string "b")
        let result2 = tool
            .execute(json!({"key": "arr_test", "content": "\"b\"", "append": true}))
            .await
            .unwrap();
        assert!(result2.success);

        // Verify resulting content is JSON array ["a","b"]
        let entry = mem.get("arr_test").await.unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&entry.content).unwrap();
        assert!(parsed.is_array());
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "a");
        assert_eq!(arr[1], "b");
    }

    #[tokio::test]
    async fn store_append_missing_key_behaves_like_store() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        // Append on an absent key should behave like store
        let result = tool
            .execute(json!({"key": "brand_new", "content": "first entry", "append": true}))
            .await
            .unwrap();
        assert!(result.success);

        let entry = mem.get("brand_new").await.unwrap().unwrap();
        assert_eq!(entry.content, "first entry");
    }

    #[tokio::test]
    async fn store_non_append_still_overwrites() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        let r1 = tool
            .execute(json!({"key": "ow", "content": "first"}))
            .await
            .unwrap();
        assert!(r1.success);

        let r2 = tool
            .execute(json!({"key": "ow", "content": "second"}))
            .await
            .unwrap();
        assert!(r2.success);

        let entry = mem.get("ow").await.unwrap().unwrap();
        assert_eq!(entry.content, "second");
    }

    #[tokio::test]
    async fn store_blocked_when_rate_limited() {
        let (_tmp, mem) = test_mem();
        let limited = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = MemoryStoreTool::new(mem.clone(), limited);
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Rate limit exceeded")
        );
        assert!(mem.get("lang").await.unwrap().is_none());
    }

    /// The tool-plane session-scope guard (the fusion session-scope program,
    /// Candidate B's pins, 2026-11-18): the `memory_store` tool is
    /// SESSION-LESS — its execute passes `session_id = None` to the backend
    /// on both the plain-store and the append path, and its schema exposes
    /// no `session_id` parameter. A session-NULL `core`/`daily` row is the
    /// durable-global knowledge shape; a tool-authored session binding would
    /// silently strip rows of their carve-out eligibility.
    struct StoreSessionRecorder {
        recorded: Mutex<Vec<Option<String>>>,
    }

    impl StoreSessionRecorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Memory for StoreSessionRecorder {
        fn name(&self) -> &str {
            "store-session-recording"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.recorded
                .lock()
                .unwrap()
                .push(session_id.map(String::from));
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

        async fn store_append(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.store(key, content, category, session_id).await
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
    impl ::zeroclaw_api::attribution::Attributable for StoreSessionRecorder {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "StoreSessionRecorder"
        }
    }

    #[tokio::test]
    async fn store_execute_passes_session_none_to_the_backend_on_both_paths() {
        let mem = StoreSessionRecorder::new();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        tool.execute(json!({"key": "k1", "content": "c"}))
            .await
            .unwrap();
        tool.execute(json!({"key": "k2", "content": "c", "append": true}))
            .await
            .unwrap();

        let recorded = mem.recorded.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![None, None],
            "the store tool must pass session_id=None on both the store and the append path"
        );
    }

    #[test]
    fn store_tool_schema_hides_session_scope() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        assert!(
            tool.parameters_schema()["properties"]
                .get("session_id")
                .is_none(),
            "the tool plane is session-less — the schema must not expose a session_id parameter, got: {}",
            tool.parameters_schema()
        );
    }
}
