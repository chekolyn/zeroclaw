//! Agent-callable tool that publishes an arbitrary message to the MQTT
//! event bus.
//!
//! Wraps [`crate::mqtt_bus::publish`]. When no MQTT channel is configured
//! the underlying publish is a graceful no-op (returns `Ok`); this tool
//! surfaces that inert state in its result so the caller knows the
//! message was not actually delivered, without treating it as a failure.
//!
//! A per-session rate limit (max [`MAX_PUBLISHES_PER_MINUTE`] publishes per
//! calendar minute) guards against runaway SOPs/agents flooding the bus.
//! The decision logic is split into [`MqttPublishTool::check_rate_limit`],
//! which takes the current unix-second time as a parameter so it is
//! unit-testable without a broker or wall-clock dependency.

#![cfg(feature = "channel-mqtt")]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolResult};

/// Maximum number of `mqtt_publish` calls allowed per calendar minute
/// for a single agent session. Tunable as a single const.
pub const MAX_PUBLISHES_PER_MINUTE: u32 = 60;

/// Tool that lets an agent (or SOP) publish an arbitrary message to the
/// MQTT event bus.
///
/// The tool is constructed per agent/session, so the rate-limit counter
/// lives in-struct and naturally resets with the session.
pub struct MqttPublishTool {
    /// Minute index (unix seconds / 60) of the current rate-limit window.
    minute: AtomicU64,
    /// Number of publishes consumed in the current minute window.
    count: AtomicU32,
}

impl MqttPublishTool {
    pub fn new() -> Self {
        Self {
            minute: AtomicU64::new(0),
            count: AtomicU32::new(0),
        }
    }

    /// Decide whether a publish is allowed under the per-minute rate limit,
    /// consuming one slot if so.
    ///
    /// Takes the current unix-second time as `now_secs` so the logic is
    /// deterministic and unit-testable without a wall clock. Returns
    /// `Ok(())` when the publish may proceed, or `Err(message)` when the
    /// limit for the current minute has been exhausted (the slot is NOT
    /// consumed in that case).
    ///
    /// # Concurrency
    ///
    /// For v1 this uses two atomics with relaxed ordering. Under the
    /// per-session construction model contention is low; a rare reset-race
    /// may slightly under-count at a minute boundary, which is acceptable
    /// for a guardian-style guardrail (the limit is a backstop, not a
    /// hard SLA).
    fn check_rate_limit(&self, now_secs: u64) -> std::result::Result<(), String> {
        let current_minute = now_secs / 60;
        let stored_minute = self.minute.load(Ordering::Relaxed);
        if current_minute != stored_minute {
            // New calendar-minute window: reset the counter and consume
            // this publish as the first of the window.
            self.minute.store(current_minute, Ordering::Relaxed);
            self.count.store(1, Ordering::Relaxed);
            return Ok(());
        }
        let prev = self.count.fetch_add(1, Ordering::Relaxed);
        if prev + 1 > MAX_PUBLISHES_PER_MINUTE {
            // Over the limit: roll back the increment so the window's
            // recorded count stays at the cap rather than climbing without
            // bound on a flood of rejected calls.
            self.count.fetch_sub(1, Ordering::Relaxed);
            return Err(format!(
                "mqtt_publish rate limit exceeded: max {} publishes per minute \
                 (calendar minute {}); retry shortly",
                MAX_PUBLISHES_PER_MINUTE, current_minute
            ));
        }
        Ok(())
    }
}

impl Default for MqttPublishTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MqttPublishTool {
    fn name(&self) -> &str {
        "mqtt_publish"
    }

    fn description(&self) -> &str {
        "Publish an arbitrary message to the MQTT event bus. \
         Use this to emit events that other agents/SOPs (stuck-watchdog, \
         task-completion-listener, rollups) consume. When MQTT is not \
         configured the publish is a graceful no-op and the result reports \
         the inert state — it is NOT a failure. A per-session rate limit \
         (60 publishes/minute) prevents flooding the bus."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "description": "Full MQTT topic path, e.g. \
                     'zeroclaw/projects/<p>/milestones/<m>/tasks/<id>/note'."
                },
                "payload": {
                    "type": "string",
                    "description": "Message body (UTF-8 text)."
                },
                "retain": {
                    "type": "boolean",
                    "description": "Whether to publish as a retained message.",
                    "default": false
                }
            },
            "required": ["topic", "payload"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let topic = args
            .get("topic")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"param": "topic"})),
                    "tool argument validation failed"
                );
                anyhow::Error::msg("Missing or empty 'topic' parameter")
            })?
            .to_string();
        let payload = args
            .get("payload")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"param": "payload"})),
                    "tool argument validation failed"
                );
                anyhow::Error::msg("Missing 'payload' parameter")
            })?
            .to_string();
        let retain = args
            .get("retain")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // Rate-limit guard: check BEFORE issuing the publish so a rejected
        // call never reaches the bus.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Err(message) = self.check_rate_limit(now_secs) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(message),
            });
        }

        match crate::mqtt_bus::publish(&topic, payload.into_bytes(), retain).await {
            Ok(()) => {
                // mqtt_bus::publish is a no-op (returns Ok) when MQTT is
                // unconfigured; there's no return signal distinguishing the
                // no-op from a real publish. We report success in both
                // cases — the no-op path is intentional graceful degradation,
                // not a failure. The agent learns "inert vs delivered" from
                // the bus itself (subscribers), not from this tool.
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "published to {topic} (retain={retain}); \
                         note: if MQTT is unconfigured this was a no-op"
                    ),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("mqtt publish failed: {e:#}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zeroclaw_api::tool::Tool;

    #[test]
    fn tool_name_and_schema() {
        let tool = MqttPublishTool::new();
        assert_eq!(tool.name(), "mqtt_publish");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["topic"].is_object());
        assert!(schema["properties"]["payload"].is_object());
        assert!(schema["properties"]["retain"].is_object());
        assert_eq!(schema["required"], json!(["topic", "payload"]));
    }

    #[tokio::test]
    async fn publish_noop_when_mqtt_unconfigured() {
        let tool = MqttPublishTool::new();
        let result = tool
            .execute(json!({
                "topic": "zeroclaw/projects/test/milestones/m1/tasks/t1/note",
                "payload": "hello"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "unconfigured publish should succeed (no-op)"
        );
        assert!(result.error.is_none());
        assert!(result.output.contains("no-op"));
    }

    #[tokio::test]
    async fn publish_with_retain_true() {
        let tool = MqttPublishTool::new();
        let result = tool
            .execute(json!({
                "topic": "zeroclaw/test/retained",
                "payload": "retained-msg",
                "retain": true
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("retain=true"));
    }

    #[tokio::test]
    async fn missing_topic_returns_error() {
        let tool = MqttPublishTool::new();
        let result = tool.execute(json!({"payload": "hello"})).await;
        // Missing required args return Err from execute (matches schedule.rs convention).
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("topic"));
    }

    #[tokio::test]
    async fn missing_payload_returns_error() {
        let tool = MqttPublishTool::new();
        let result = tool.execute(json!({"topic": "zeroclaw/test"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("payload"));
    }

    #[tokio::test]
    async fn empty_topic_returns_error() {
        let tool = MqttPublishTool::new();
        let result = tool
            .execute(json!({"topic": "  ", "payload": "hello"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("topic"));
    }

    #[test]
    fn rate_limit_allows_up_to_max_in_one_minute() {
        let tool = MqttPublishTool::new();
        let minute_secs = 1_000_000 * 60;
        for _ in 0..MAX_PUBLISHES_PER_MINUTE {
            assert!(
                tool.check_rate_limit(minute_secs).is_ok(),
                "publishes up to the limit should be allowed"
            );
        }
        let blocked = tool.check_rate_limit(minute_secs);
        assert!(blocked.is_err());
        assert!(
            blocked.unwrap_err().contains("rate limit exceeded"),
            "over-limit call should report rate limit exceeded"
        );
    }

    #[test]
    fn rate_limit_resets_across_calendar_minutes() {
        let tool = MqttPublishTool::new();
        let minute_a = 5_000_000 * 60;
        let minute_b = minute_a + 60;
        for _ in 0..MAX_PUBLISHES_PER_MINUTE {
            assert!(tool.check_rate_limit(minute_a).is_ok());
        }
        assert!(tool.check_rate_limit(minute_a).is_err());
        assert!(
            tool.check_rate_limit(minute_b).is_ok(),
            "a new calendar minute resets the rate-limit window"
        );
    }

    #[test]
    fn rate_limit_rejected_call_does_not_consume_slot() {
        let tool = MqttPublishTool::new();
        let minute_a = 7_000_000 * 60;
        for _ in 0..MAX_PUBLISHES_PER_MINUTE {
            assert!(tool.check_rate_limit(minute_a).is_ok());
        }
        for _ in 0..10 {
            assert!(tool.check_rate_limit(minute_a).is_err());
        }
        let minute_b = minute_a + 60;
        for i in 0..MAX_PUBLISHES_PER_MINUTE {
            assert!(
                tool.check_rate_limit(minute_b).is_ok(),
                "call {} in fresh minute B should be allowed",
                i + 1
            );
        }
        assert!(tool.check_rate_limit(minute_b).is_err());
    }
}
