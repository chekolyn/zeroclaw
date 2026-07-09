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
        "Store a fact, preference, or note in long-term memory. Use category 'core' for permanent facts, 'daily' for session notes, 'conversation' for chat context, or a custom category name."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Unique key for this memory (e.g. 'user_lang', 'project_stack')"
                },
                "content": {
                    "type": "string",
                    "description": "The information to remember"
                },
                "category": {
                    "type": "string",
                    "description": "Memory category: 'core' (permanent), 'daily' (session), 'conversation' (chat), or a custom category name. Defaults to 'core'."
                },
                "append": {
                    "type": "boolean",
                    "description": "If true, append to any existing value instead of overwriting. JSON-array values get a new element; text values are newline-joined."
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
            self.memory
                .store_append(key, content, category, None)
                .await
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
    use tempfile::TempDir;
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::policy::SecurityPolicy;
    use zeroclaw_memory::SqliteMemory;

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
        assert!(
            entry.content == "line1\nline2"
                || entry.content == "line2\nline1"
        );
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
}
